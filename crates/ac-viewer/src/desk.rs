//! The desktop viewer without I/O (ADR-006, desktop addendum): JPEG frames split out of the frame
//! client's stdout, and the take-over client's closed command set.
//!
//! The frame client is `ffmpeg -f x11grab ... -f image2pipe -c:v mjpeg -`, which writes JPEG
//! after JPEG to one pipe. A JPEG starts with SOI (`FF D8`) and ends with EOI (`FF D9`); inside
//! the entropy-coded data every `FF` is followed by `00` (byte stuffing), so the first EOI after
//! an SOI ends the frame.
//!
//! The take-over client is a constant shell loop in the guest that runs one `xdotool` per line.
//! Every line it is sent is rendered by [`DeskInput::line`] from numbers and key names the gateway
//! checked: digits, ASCII letters, `_` and `+` only, so no viewer text can be read as anything
//! but a coordinate, a button or a key.

use std::fmt;

/// Largest frame kept; a longer one is dropped and the splitter waits for the next SOI.
pub const MAX_FRAME: usize = 8 << 20;
/// Largest screen coordinate accepted.
pub const MAX_COORD: u16 = 8192;
/// Key names in one request.
pub const MAX_KEYS: usize = 64;
/// Longest key name or combination (`ctrl+shift+Escape`).
pub const MAX_KEY_LEN: usize = 32;
/// Characters typed in one request.
pub const MAX_TEXT: usize = 1024;

const SOI: [u8; 2] = [0xFF, 0xD8];
const EOI: [u8; 2] = [0xFF, 0xD9];

/// Splits a stream of concatenated JPEGs into frames.
#[derive(Debug, Default)]
pub struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    /// An empty splitter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `bytes`; returns every frame now complete, oldest first. Bytes before an SOI are
    /// dropped, as is a frame past [`MAX_FRAME`].
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            let Some(start) = find(&self.buf, SOI, 0) else {
                // Keep a trailing FF: it may begin the next SOI.
                let keep = usize::from(self.buf.last() == Some(&0xFF));
                self.buf.drain(..self.buf.len() - keep);
                return out;
            };
            self.buf.drain(..start);
            match find(&self.buf, EOI, 2) {
                Some(end) => {
                    let frame: Vec<u8> = self.buf.drain(..end + 2).collect();
                    if frame.len() <= MAX_FRAME {
                        out.push(frame);
                    }
                }
                None if self.buf.len() > MAX_FRAME => {
                    // Too long to be a frame: resynchronise after this SOI.
                    self.buf.drain(..2);
                }
                None => return out,
            }
        }
    }
}

fn find(buf: &[u8], pat: [u8; 2], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(2)
        .position(|w| w == pat)
        .map(|i| i + from)
}

/// A mouse button: 1 left, 2 middle, 3 right, 4 and 5 the wheel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Button(u8);

impl Button {
    /// `1` to `5`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "1" | "2" | "3" | "4" | "5" => s.parse().ok().map(Self),
            _ => None,
        }
    }
}

/// One take-over command: the whole vocabulary of the desktop's input client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskInput {
    /// Move the pointer to `x`, `y` (screen pixels).
    Move(u16, u16),
    /// Press a button.
    Down(Button),
    /// Release a button.
    Up(Button),
    /// Press and release a button (the wheel: 4 up, 5 down).
    Click(Button),
    /// Press and release each key or combination in turn.
    Keys(Vec<String>),
}

/// Why input was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskError {
    /// Not one of the forms the route takes.
    Malformed(&'static str),
    /// Over a cap.
    TooLarge,
}

impl fmt::Display for DeskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(why) => f.write_str(why),
            Self::TooLarge => f.write_str("input too large"),
        }
    }
}

impl DeskInput {
    /// A pointer request body: `move X Y`, `down B`, `up B` or `click B`.
    pub fn pointer(body: &[u8]) -> Result<Self, DeskError> {
        let bad = DeskError::Malformed("use move X Y, down B, up B or click B");
        let text = std::str::from_utf8(body).map_err(|_| bad.clone())?;
        let parts: Vec<&str> = text.split(' ').collect();
        let button = |s: &str| Button::parse(s).ok_or(bad.clone());
        match parts.as_slice() {
            ["move", x, y] => Ok(Self::Move(
                coord(x).ok_or(bad.clone())?,
                coord(y).ok_or(bad)?,
            )),
            ["down", b] => Ok(Self::Down(button(b)?)),
            ["up", b] => Ok(Self::Up(button(b)?)),
            ["click", b] => Ok(Self::Click(button(b)?)),
            _ => Err(bad),
        }
    }

    /// A keys request body: key names or combinations separated by spaces (`ctrl+l Return`).
    /// Each is ASCII letters, digits, `_` and `+`, as X names keys.
    pub fn keys(body: &[u8]) -> Result<Self, DeskError> {
        let bad = DeskError::Malformed("keys are names like Return, ctrl+c or F5");
        let text = std::str::from_utf8(body).map_err(|_| bad.clone())?;
        let keys: Vec<String> = text.split(' ').map(str::to_owned).collect();
        if keys.len() > MAX_KEYS {
            return Err(DeskError::TooLarge);
        }
        if keys.iter().all(|k| key_name(k)) {
            Ok(Self::Keys(keys))
        } else {
            Err(bad)
        }
    }

    /// Text to type, as one key per character: letters and digits by name, newline as
    /// `Return`, tab as `Tab`, anything else printable as its Unicode keysym (`U0021`).
    pub fn text(body: &[u8]) -> Result<Self, DeskError> {
        let text = std::str::from_utf8(body).map_err(|_| DeskError::Malformed("not UTF-8"))?;
        if text.is_empty() {
            return Err(DeskError::Malformed("nothing to type"));
        }
        if text.chars().count() > MAX_TEXT {
            return Err(DeskError::TooLarge);
        }
        let keys = text
            .chars()
            .map(|c| match c {
                '\n' => Ok("Return".to_owned()),
                '\t' => Ok("Tab".to_owned()),
                c if c.is_ascii_alphanumeric() => Ok(c.to_string()),
                c if c.is_control() => Err(DeskError::Malformed("control characters are keys")),
                c => Ok(format!("U{:04X}", u32::from(c))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::Keys(keys))
    }

    /// The input client's lines for this command, each ending in a newline. Keys go
    /// [`MAX_KEYS`] to a line.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Move(x, y) => vec![format!("move {x} {y}\n")],
            Self::Down(b) => vec![format!("down {}\n", b.0)],
            Self::Up(b) => vec![format!("up {}\n", b.0)],
            Self::Click(b) => vec![format!("click {}\n", b.0)],
            Self::Keys(keys) => keys
                .chunks(MAX_KEYS)
                .map(|c| format!("key {}\n", c.join(" ")))
                .collect(),
        }
    }
}

fn coord(s: &str) -> Option<u16> {
    let ok = !s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit());
    s.parse().ok().filter(|n| ok && *n <= MAX_COORD)
}

fn key_name(k: &str) -> bool {
    let chars_ok = k
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'+');
    let parts_ok = k.split('+').all(|p| !p.is_empty());
    !k.is_empty() && k.len() <= MAX_KEY_LEN && chars_ok && parts_ok
}

/// The NDJSON message carrying one frame, its JPEG in standard base64 (computed by the caller,
/// so this crate needs no encoder).
pub fn frame_message(base64: &str) -> String {
    format!("{{\"t\":\"frame\",\"jpeg\":\"{base64}\"}}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(body: &[u8]) -> Vec<u8> {
        [&SOI[..], body, &EOI[..]].concat()
    }

    #[test]
    fn frames_split_at_eoi_across_reads_and_skip_junk() {
        let a = jpeg(&[1, 0xFF, 0x00, 2]);
        let b = jpeg(&[3]);
        let stream = [&b"junk"[..], &a, &b].concat();
        let mut f = Frames::new();
        let mut got = Vec::new();
        for chunk in stream.chunks(3) {
            got.extend(f.feed(chunk));
        }
        assert_eq!(got, [a, b]);
        assert!(f.feed(&[0xFF]).is_empty());
        assert_eq!(f.feed(&[0xD8, 7, 0xFF, 0xD9]), [jpeg(&[7])]);
    }

    #[test]
    fn an_endless_frame_is_dropped_and_the_next_one_kept() {
        let mut f = Frames::new();
        let mut huge = SOI.to_vec();
        huge.resize(MAX_FRAME + 10, 0);
        assert!(f.feed(&huge).is_empty());
        assert_eq!(f.feed(&jpeg(&[9])), [jpeg(&[9])]);
    }

    #[test]
    fn pointer_takes_only_its_four_forms() {
        assert_eq!(
            DeskInput::pointer(b"move 10 2000"),
            Ok(DeskInput::Move(10, 2000))
        );
        assert_eq!(
            DeskInput::pointer(b"click 4").map(|i| i.lines()),
            Ok(vec!["click 4\n".to_owned()])
        );
        for bad in [
            &b"move 1"[..],
            b"move -1 2",
            b"move 1 2 3",
            b"move 99999 1",
            b"move +1 2",
            b"down 6",
            b"down 0",
            b"up 1;reboot",
            b"click  1",
            b"wiggle 1",
            b"",
        ] {
            assert!(
                DeskInput::pointer(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn keys_are_x_names_only() {
        let k = DeskInput::keys(b"ctrl+l Return F5").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(k.lines(), ["key ctrl+l Return F5\n"]);
        for bad in [
            &b""[..],
            b"a  b",
            b"$(id)",
            b"a;b",
            b"ctrl+",
            b"+",
            b"--window",
            b"\n",
        ] {
            assert!(
                DeskInput::keys(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let many = vec!["a"; MAX_KEYS + 1].join(" ");
        assert_eq!(DeskInput::keys(many.as_bytes()), Err(DeskError::TooLarge));
    }

    #[test]
    fn text_becomes_one_key_per_character() {
        let t = DeskInput::text("Hi $é\n".as_bytes()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(t.lines(), ["key H i U0020 U0024 U00E9 Return\n"]);
        assert!(DeskInput::text(b"\x1b[A").is_err());
        assert!(DeskInput::text(b"").is_err());
        let long = "a".repeat(MAX_TEXT + 1);
        assert_eq!(DeskInput::text(long.as_bytes()), Err(DeskError::TooLarge));
        let lines = DeskInput::text("a".repeat(MAX_KEYS + 1).as_bytes()).map(|t| t.lines().len());
        assert_eq!(lines, Ok(2));
    }
}
