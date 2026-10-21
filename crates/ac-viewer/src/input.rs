//! Viewer input as tmux commands for the take-over client. Key names come from a fixed set and
//! are plain words, so nothing a browser sends is parsed by tmux as command syntax (`;`, quotes,
//! `#{}`); typed and pasted text goes as hex bytes (`send-keys -H`).

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

const NAMED: [&str; 17] = [
    "Enter", "Tab", "BTab", "BSpace", "Escape", "Space", "Up", "Down", "Left", "Right", "Home",
    "End", "PPage", "NPage", "IC", "DC", "KPEnter",
];

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

/// Space-separated key names (`Enter`, `C-c`, `M-x`, `F5`, `S-Up` ...) as one command line.
pub fn keys(body: &str) -> Result<String, InputError> {
    let names: Vec<&str> = body.split_whitespace().collect();
    if names.is_empty() {
        return Err(InputError::Empty);
    }
    if names.len() > MAX_KEYS {
        return Err(InputError::TooLarge);
    }
    if let Some(bad) = names.iter().find(|k| !valid_key(k)) {
        return Err(InputError::BadKey((*bad).chars().take(32).collect()));
    }
    Ok(format!("send-keys -t main {}\n", names.join(" ")))
}

fn valid_key(name: &str) -> bool {
    let mut base = name;
    let mut mods = 0;
    while let Some(rest) = ["C-", "M-", "S-"].iter().find_map(|m| base.strip_prefix(m)) {
        base = rest;
        mods += 1;
    }
    let function = base
        .strip_prefix('F')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n));
    let modified_char =
        mods > 0 && base.len() == 1 && base.bytes().all(|b| b.is_ascii_alphanumeric());
    mods <= 3 && (NAMED.contains(&base) || function || modified_char)
}

/// Typed bytes, sent exactly.
pub fn text(bytes: &[u8]) -> Result<Vec<String>, InputError> {
    if bytes.is_empty() {
        return Err(InputError::Empty);
    }
    if bytes.len() > MAX_TEXT {
        return Err(InputError::TooLarge);
    }
    Ok(hex_commands(bytes))
}

/// Pasted bytes. Paste-end markers inside are removed (they would let pasted text escape
/// bracketed paste and run as typed input); when the program asked for bracketed paste the
/// whole is wrapped in the markers.
pub fn paste(bytes: &[u8], bracketed: bool) -> Result<Vec<String>, InputError> {
    if bytes.is_empty() {
        return Err(InputError::Empty);
    }
    if bytes.len() > MAX_PASTE {
        return Err(InputError::TooLarge);
    }
    let mut body = bytes.to_vec();
    for marker in [&b"\x1b[200~"[..], b"\x1b[201~"] {
        while let Some(at) = body.windows(marker.len()).position(|w| w == marker) {
            body.drain(at..at + marker.len());
        }
    }
    // Removing one marker can join two halves into another; repeat until none is left.
    if body
        .windows(6)
        .any(|w| w == b"\x1b[200~" || w == b"\x1b[201~")
    {
        return paste(&body, bracketed);
    }
    if bracketed {
        body.splice(0..0, b"\x1b[200~".iter().copied());
        body.extend_from_slice(b"\x1b[201~");
    }
    Ok(hex_commands(&body))
}

/// Size the take-over client (and so the window) to the viewer.
pub fn resize(cols: u16, rows: u16) -> Result<String, InputError> {
    if !(COLS.0..=COLS.1).contains(&cols) || !(ROWS.0..=ROWS.1).contains(&rows) {
        return Err(InputError::BadSize);
    }
    Ok(format!("refresh-client -C {cols}x{rows}\n"))
}

fn hex_commands(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(HEX_CHUNK)
        .map(|chunk| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            format!("send-keys -t main -H {}\n", hex.join(" "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names() {
        assert_eq!(
            keys("Enter C-c M-x S-Up F12 C-M-a").as_deref(),
            Ok("send-keys -t main Enter C-c M-x S-Up F12 C-M-a\n")
        );
        for bad in [
            "a",
            ";",
            "C-;",
            "Enter;kill-server",
            "F13",
            "C-",
            "#{pane_pid}",
            "\"",
            "C-C-C-C-a",
            "Up\nkill-server",
        ] {
            assert!(matches!(keys(bad), Err(InputError::BadKey(_))), "{bad:?}");
        }
        assert_eq!(keys("  "), Err(InputError::Empty));
        assert_eq!(keys(&"Up ".repeat(MAX_KEYS + 1)), Err(InputError::TooLarge));
    }

    #[test]
    fn text_is_hex_only() {
        let cmds = text("é;\n".as_bytes()).unwrap_or_default();
        assert_eq!(cmds, ["send-keys -t main -H c3 a9 3b 0a\n"]);
        assert_eq!(text(&vec![b'x'; HEX_CHUNK + 1]).map(|c| c.len()), Ok(2));
        assert_eq!(text(&vec![b'x'; MAX_TEXT + 1]), Err(InputError::TooLarge));
        assert_eq!(text(b""), Err(InputError::Empty));
    }

    #[test]
    fn paste_cannot_escape_brackets() {
        let hex = |s: &str| {
            s.bytes()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let cmds = paste(b"a\x1b[201~rm -rf ~\n", true).unwrap_or_default();
        assert_eq!(
            cmds,
            [format!(
                "send-keys -t main -H {}\n",
                hex("\x1b[200~arm -rf ~\n\x1b[201~")
            )]
        );
        let nested = paste(b"\x1b[20\x1b[201~1~x", false).unwrap_or_default();
        assert_eq!(nested, [format!("send-keys -t main -H {}\n", hex("x"))]);
        assert_eq!(
            paste(&vec![b'x'; MAX_PASTE + 1], true),
            Err(InputError::TooLarge)
        );
    }

    #[test]
    fn resize_bounds() {
        assert_eq!(resize(120, 40).as_deref(), Ok("refresh-client -C 120x40\n"));
        for (c, r) in [(19, 40), (501, 40), (120, 4), (120, 201)] {
            assert_eq!(resize(c, r), Err(InputError::BadSize));
        }
    }
}
