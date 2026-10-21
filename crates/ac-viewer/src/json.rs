//! The little JSON the viewer writes: strings, escaped for any content.

/// Append `s` as a JSON string. Controls, DEL and U+2028/9 are `\u` escapes, so a value can never
/// end the string or the line it is on.
pub fn string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `s` as a JSON string.
pub fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    string(&mut out, s);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_everything_that_could_break_out() {
        assert_eq!(
            quoted("a\"b\\c\n\x1b\u{7f}\u{2028}é"),
            "\"a\\\"b\\\\c\\u000a\\u001b\\u007f\\u2028é\""
        );
    }
}
