//! Raw terminal output as plain text: every escape sequence and control character removed,
//! `\r\n` folded to `\n`. For the history endpoint, which never returns raw bytes.

/// Strip `bytes` (any number of whole or partial sequences) to printable text, tabs and newlines.
pub fn strip(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                // Two-byte sequences (charset selection and the like) take one more char.
                Some('(' | ')' | '*' | '+' | '#' | '%') => {
                    chars.next();
                }
                _ => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\t' => out.push(c),
            c if c.is_control()
                || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
                {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_survives() {
        assert_eq!(
            strip("héllo 世界 😀\r\n\tok\n".as_bytes()),
            "héllo 世界 😀\n\tok\n"
        );
    }

    #[test]
    fn every_escape_goes() {
        let raw = b"\x1b[1;31mred\x1b[0m \x1b]52;c;c2VjcmV0\x07\x1b]8;;http://evil/\x1b\\link\x1b]8;;\x1b\\ \
            \x1b]0;title\x1b\\\x1bP+q\x1b\\\x1b_APC\x1b\\\x1b[?1049h\x1b(B\x1b=\x08\x07\x00\r\xe2\x80\xaeend";
        assert_eq!(strip(raw), "red link end");
    }

    #[test]
    fn invalid_utf8_and_truncated_sequences() {
        assert_eq!(strip(b"a\xffb\x1b["), "a\u{fffd}b");
        assert_eq!(strip(b"a\x1b]8;;never ends"), "a");
    }
}
