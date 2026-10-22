//! Viewer input for the take-over client, as an [`Input`] whose payloads can only be built here,
//! after validation. [`Input::lines`] is the one place that renders tmux commands, and it emits
//! viewer bytes only as hex (`send-keys -H`): no viewer byte can become a tmux command, a newline
//! or control-mode syntax. Key names are turned into the bytes a terminal would send, following
//! the pane's cursor-key mode.

use std::fmt;

/// Keys per request.
pub const MAX_KEYS: usize = 64;
/// Typed text per request, in bytes.
pub const MAX_TEXT: usize = 4096;
/// Pasted text per request, in bytes.
pub const MAX_PASTE: usize = 64 * 1024;
/// Bytes per `send-keys -H` command.
const HEX_CHUNK: usize = 512;
/// Smallest and largest size a viewer may set.
pub const COLS: (u16, u16) = (20, 500);
/// Rows, likewise.
pub const ROWS: (u16, u16) = (5, 200);

/// Why input was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum InputError {
    /// Larger than the per-request cap.
    TooLarge,
    /// Not a key name this accepts.
    BadKey(String),
    /// Nothing to send.
    Empty,
    /// Size out of range or malformed.
    BadSize,
}

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("input too large"),
            Self::BadKey(k) => write!(f, "unknown key {k:?}"),
            Self::Empty => f.write_str("no input"),
            Self::BadSize => f.write_str("size out of range"),
        }
    }
}

/// Bytes for the pane, already validated and capped. Built only by [`keys`], [`text`], [`paste`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytes(Vec<u8>);

/// A size within [`COLS`] and [`ROWS`]. Built only by [`resize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    cols: u16,
    rows: u16,
}

/// One request's worth of take-over input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Bytes to the pane.
    Bytes(Bytes),
    /// Size the window.
    Resize(Size),
}

impl Input {
    /// The tmux command lines for the take-over client: `send-keys -t main -H <hex>...` or
    /// `refresh-client -C <cols>x<rows>`, nothing else.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Self::Bytes(Bytes(b)) => b
                .chunks(HEX_CHUNK)
                .map(|chunk| {
                    let hex: Vec<String> = chunk.iter().map(|x| format!("{x:02x}")).collect();
                    format!("send-keys -t main -H {}\n", hex.join(" "))
                })
                .collect(),
            Self::Resize(Size { cols, rows }) => vec![format!("refresh-client -C {cols}x{rows}\n")],
        }
    }
}

/// Space-separated key names (`Enter`, `C-c`, `M-x`, `F5`, `S-Up` ...) as the bytes a terminal
/// sends for them. `app_cursor`: the pane is in application cursor-key mode.
pub fn keys(body: &str, app_cursor: bool) -> Result<Input, InputError> {
    let names: Vec<&str> = body.split_whitespace().collect();
    if names.is_empty() {
        return Err(InputError::Empty);
    }
    if names.len() > MAX_KEYS {
        return Err(InputError::TooLarge);
    }
    let mut out = Vec::new();
    for name in names {
        let bytes = key_bytes(name, app_cursor)
            .ok_or_else(|| InputError::BadKey(name.chars().take(32).collect()))?;
        out.extend(bytes);
    }
    Ok(Input::Bytes(Bytes(out)))
}

/// Modifier bits as xterm encodes them (`CSI 1 ; 1+bits X`).
const SHIFT: u8 = 1;
const ALT: u8 = 2;
const CTRL: u8 = 4;

fn key_bytes(name: &str, app_cursor: bool) -> Option<Vec<u8>> {
    let mut base = name;
    let mut mods = 0u8;
    while let Some((bit, rest)) = [("C-", CTRL), ("M-", ALT), ("S-", SHIFT)]
        .iter()
        .find_map(|(p, bit)| base.strip_prefix(p).map(|r| (*bit, r)))
    {
        if mods & bit != 0 {
            return None;
        }
        mods |= bit;
        base = rest;
    }
    // A letter or digit with Ctrl and/or Alt: the control byte, ESC-prefixed for Alt.
    if base.len() == 1 && mods & (CTRL | ALT) != 0 && mods & SHIFT == 0 {
        let c = base.as_bytes()[0].to_ascii_lowercase();
        if !c.is_ascii_alphanumeric() {
            return None;
        }
        let b = if mods & CTRL != 0 && c.is_ascii_lowercase() {
            c & 0x1f
        } else {
            c
        };
        return Some(if mods & ALT != 0 {
            vec![0x1b, b]
        } else {
            vec![b]
        });
    }
    // Cursor and editing keys: the final byte of the CSI/SS3 form, or a `CSI n ~` number.
    let csi = |fin: u8| -> Vec<u8> {
        if mods != 0 {
            format!("\x1b[1;{}{}", mods + 1, fin as char).into_bytes()
        } else if app_cursor {
            vec![0x1b, b'O', fin]
        } else {
            vec![0x1b, b'[', fin]
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if mods != 0 {
            format!("\x1b[{n};{}~", mods + 1).into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let alt = |b: &[u8]| -> Option<Vec<u8>> {
        match mods {
            0 => Some(b.to_vec()),
            ALT => Some([&[0x1b], b].concat()),
            _ => None,
        }
    };
    match base {
        "Up" => Some(csi(b'A')),
        "Down" => Some(csi(b'B')),
        "Right" => Some(csi(b'C')),
        "Left" => Some(csi(b'D')),
        // tmux's own key table, since panes run with TERM=tmux-256color (khome/kend).
        "Home" => Some(tilde(1)),
        "End" => Some(tilde(4)),
        "IC" => Some(tilde(2)),
        "DC" => Some(tilde(3)),
        "PPage" => Some(tilde(5)),
        "NPage" => Some(tilde(6)),
        "F1" | "F2" | "F3" | "F4" if mods == 0 => {
            Some(vec![0x1b, b'O', b'P' + base.as_bytes()[1] - b'1'])
        }
        "F5" => Some(tilde(15)),
        "F6" => Some(tilde(17)),
        "F7" => Some(tilde(18)),
        "F8" => Some(tilde(19)),
        "F9" => Some(tilde(20)),
        "F10" => Some(tilde(21)),
        "F11" => Some(tilde(23)),
        "F12" => Some(tilde(24)),
        "Enter" | "KPEnter" => alt(b"\r"),
        "Tab" => alt(b"\t"),
        "BTab" if mods == 0 => Some(b"\x1b[Z".to_vec()),
        "BSpace" => alt(b"\x7f"),
        "Escape" => alt(b"\x1b"),
        "Space" => alt(b" "),
        _ => None,
    }
}

/// Typed bytes, sent exactly.
pub fn text(bytes: &[u8]) -> Result<Input, InputError> {
    if bytes.is_empty() {
        return Err(InputError::Empty);
    }
    if bytes.len() > MAX_TEXT {
        return Err(InputError::TooLarge);
    }
    Ok(Input::Bytes(Bytes(bytes.to_vec())))
}

/// Pasted bytes. Paste markers inside are removed (an end marker would let pasted text escape
/// bracketed paste and run as typed input); when the program asked for bracketed paste the whole
/// is wrapped in the markers.
pub fn paste(bytes: &[u8], bracketed: bool) -> Result<Input, InputError> {
    if bytes.is_empty() {
        return Err(InputError::Empty);
    }
    if bytes.len() > MAX_PASTE {
        return Err(InputError::TooLarge);
    }
    let mut body = bytes.to_vec();
    // Removing one marker can join two halves into another; repeat until none is left.
    while let Some(at) = body
        .windows(6)
        .position(|w| w == b"\x1b[200~" || w == b"\x1b[201~")
    {
        body.drain(at..at + 6);
    }
    if bracketed {
        body.splice(0..0, b"\x1b[200~".iter().copied());
        body.extend_from_slice(b"\x1b[201~");
    }
    Ok(Input::Bytes(Bytes(body)))
}

/// Size the take-over client (and so the window) to the viewer.
pub fn resize(cols: u16, rows: u16) -> Result<Input, InputError> {
    if !(COLS.0..=COLS.1).contains(&cols) || !(ROWS.0..=ROWS.1).contains(&rows) {
        return Err(InputError::BadSize);
    }
    Ok(Input::Resize(Size { cols, rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(i: Result<Input, InputError>) -> Vec<u8> {
        match i {
            Ok(Input::Bytes(Bytes(b))) => b,
            other => panic!("{other:?}"),
        }
    }

    fn hex(s: &[u8]) -> String {
        s.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn key_names_become_terminal_bytes() {
        assert_eq!(
            bytes(keys("Enter C-c M-x BSpace Tab", false)),
            b"\r\x03\x1bx\x7f\t"
        );
        assert_eq!(bytes(keys("Up Left Home", false)), b"\x1b[A\x1b[D\x1b[1~");
        assert_eq!(
            bytes(keys("Up End", true)),
            b"\x1bOA\x1b[4~",
            "application cursor mode"
        );
        assert_eq!(
            bytes(keys("S-Up C-Right M-DC", true)),
            b"\x1b[1;2A\x1b[1;5C\x1b[3;3~"
        );
        assert_eq!(
            bytes(keys("F1 F4 F5 F12 S-F5 BTab PPage", false)),
            b"\x1bOP\x1bOS\x1b[15~\x1b[24~\x1b[15;2~\x1b[Z\x1b[5~"
        );
        assert_eq!(bytes(keys("C-M-a M-Enter C-1", false)), b"\x1b\x01\x1b\r1");
    }

    #[test]
    fn bad_keys_are_refused() {
        for bad in [
            "a",
            ";",
            "C-;",
            "Enter;kill-server",
            "F13",
            "C-",
            "#{pane_pid}",
            "\"",
            "C-C-a",
            "S-a",
            "C-Enter",
            "kill-server",
        ] {
            assert!(
                matches!(keys(bad, false), Err(InputError::BadKey(_))),
                "{bad:?}"
            );
        }
        assert_eq!(keys("  ", false), Err(InputError::Empty));
        assert_eq!(
            keys(&"Up ".repeat(MAX_KEYS + 1), false),
            Err(InputError::TooLarge)
        );
    }

    #[test]
    fn hostile_text_is_only_ever_hex() {
        let hostile: &[u8] =
            b"ls\nkill-server\n; run-shell 'rm -rf ~' \"q\" %begin 1 1 1\n%exit\0\x1b]52;c;x\x07\r\\";
        for input in [
            text(hostile),
            paste(hostile, true),
            paste(hostile, false),
            keys("Enter", false),
        ] {
            for line in input.unwrap_or_else(|e| panic!("{e}")).lines() {
                let rest = line
                    .strip_prefix("send-keys -t main -H ")
                    .and_then(|r| r.strip_suffix('\n'))
                    .unwrap_or_else(|| panic!("{line:?}"));
                assert!(
                    rest.split(' ')
                        .all(|h| h.len() == 2 && h.bytes().all(|c| c.is_ascii_hexdigit())),
                    "{line:?}"
                );
            }
        }
        assert_eq!(
            text(hostile).map(|i| i.lines()),
            Ok(vec![format!("send-keys -t main -H {}\n", hex(hostile))])
        );
    }

    #[test]
    fn text_chunks_and_caps() {
        assert_eq!(
            text(&vec![b'x'; HEX_CHUNK + 1]).map(|i| i.lines().len()),
            Ok(2)
        );
        assert_eq!(text(&vec![b'x'; MAX_TEXT + 1]), Err(InputError::TooLarge));
        assert_eq!(text(b""), Err(InputError::Empty));
    }

    #[test]
    fn paste_cannot_escape_brackets() {
        assert_eq!(
            bytes(paste(b"a\x1b[201~rm -rf ~\n", true)),
            b"\x1b[200~arm -rf ~\n\x1b[201~"
        );
        assert_eq!(bytes(paste(b"\x1b[20\x1b[201~1~x", false)), b"x");
        assert_eq!(
            paste(&vec![b'x'; MAX_PASTE + 1], true),
            Err(InputError::TooLarge)
        );
    }

    #[test]
    fn resize_bounds() {
        assert_eq!(
            resize(120, 40).map(|i| i.lines()),
            Ok(vec!["refresh-client -C 120x40\n".to_owned()])
        );
        for (c, r) in [(19, 40), (501, 40), (120, 4), (120, 201)] {
            assert_eq!(resize(c, r), Err(InputError::BadSize));
        }
    }
}
