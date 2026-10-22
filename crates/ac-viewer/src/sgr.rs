//! `capture-pane -e` lines into styled runs. SGR (colours and attributes) is the only escape
//! understood; every other escape sequence (OSC 8 hyperlinks, titles, OSC 52 clipboard, DCS,
//! APC, cursor movement), every C0/C1 control and every bidi override is dropped. The browser
//! gets plain text plus numbers, so a hostile agent cannot make the viewer do anything but show
//! coloured characters.

/// Default colour.
pub const DEFAULT: i32 = -1;
/// Added to `0xRRGGBB` for a truecolour value; values `0..=255` are palette indices.
pub const RGB: i32 = 0x0100_0000;

/// Attribute bits.
pub const BOLD: u8 = 1;
/// Faint.
pub const DIM: u8 = 2;
/// Italic.
pub const ITALIC: u8 = 4;
/// Underline (any style).
pub const UNDERLINE: u8 = 8;
/// Swap foreground and background.
pub const REVERSE: u8 = 16;
/// Invisible.
pub const HIDDEN: u8 = 32;
/// Crossed out.
pub const STRIKE: u8 = 64;

/// Characters kept per line.
pub const MAX_LINE_CHARS: usize = 1000;

/// A run's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    /// Foreground: [`DEFAULT`], a palette index, or [`RGB`] plus `0xRRGGBB`.
    pub fg: i32,
    /// Background, likewise.
    pub bg: i32,
    /// Attribute bits ([`BOLD`] ...).
    pub attrs: u8,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: DEFAULT,
            bg: DEFAULT,
            attrs: 0,
        }
    }
}

/// Text in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// Printable text only.
    pub text: String,
    /// Its look.
    pub style: Style,
}

/// One screen line.
pub type Line = Vec<Run>;

/// Converts lines in order; the style carries from one line to the next, as tmux emits it.
#[derive(Default)]
pub struct Painter {
    style: Style,
}

impl Painter {
    /// One raw line into runs.
    pub fn line(&mut self, raw: &str) -> Line {
        let mut runs: Line = Vec::new();
        let mut chars = raw.chars().peekable();
        let mut kept = 0;
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut fin = None;
                        for c in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&c) {
                                fin = Some(c);
                                break;
                            }
                            if params.len() < 64 {
                                params.push(c);
                            }
                        }
                        let sgr = params
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == ';' || c == ':');
                        if fin == Some('m') && sgr {
                            self.apply(&params);
                        }
                    }
                    // OSC, DCS, SOS, PM, APC: skip to the string terminator (ESC \ or BEL).
                    Some(']' | 'P' | 'X' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if !printable(c) || kept >= MAX_LINE_CHARS {
                continue;
            }
            kept += 1;
            match runs.last_mut() {
                Some(run) if run.style == self.style => run.text.push(c),
                _ => runs.push(Run {
                    text: c.to_string(),
                    style: self.style,
                }),
            }
        }
        runs
    }

    fn apply(&mut self, params: &str) {
        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let mut sub = parts[i].split(':');
            let n: u32 = sub.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let s = &mut self.style;
            match n {
                0 => *s = Style::default(),
                1 => s.attrs |= BOLD,
                2 => s.attrs |= DIM,
                3 => s.attrs |= ITALIC,
                4 if sub.next() == Some("0") => s.attrs &= !UNDERLINE,
                4 | 21 => s.attrs |= UNDERLINE,
                7 => s.attrs |= REVERSE,
                8 => s.attrs |= HIDDEN,
                9 => s.attrs |= STRIKE,
                22 => s.attrs &= !(BOLD | DIM),
                23 => s.attrs &= !ITALIC,
                24 => s.attrs &= !UNDERLINE,
                27 => s.attrs &= !REVERSE,
                28 => s.attrs &= !HIDDEN,
                29 => s.attrs &= !STRIKE,
                30..=37 => s.fg = (n - 30) as i32,
                39 => s.fg = DEFAULT,
                40..=47 => s.bg = (n - 40) as i32,
                49 => s.bg = DEFAULT,
                90..=97 => s.fg = (n - 90 + 8) as i32,
                100..=107 => s.bg = (n - 100 + 8) as i32,
                38 | 48 | 58 => {
                    // Colon form carries its own arguments; semicolon form takes the next ones.
                    let colon: Vec<&str> = sub.collect();
                    let args: Vec<&str> = if colon.is_empty() {
                        let kind = parts.get(i + 1).copied();
                        let take = if kind == Some("2") { 4 } else { 2 };
                        let end = (i + 1 + take).min(parts.len());
                        let a = parts[i + 1..end].to_vec();
                        i = end - 1;
                        a
                    } else {
                        colon
                    };
                    let colour = extended(&args);
                    match (n, colour) {
                        (38, Some(c)) => s.fg = c,
                        (48, Some(c)) => s.bg = c,
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// `5;n` or `2;r;g;b` (also `2::r:g:b`, with an empty colour-space id).
fn extended(args: &[&str]) -> Option<i32> {
    let num = |s: &&str| s.parse::<u8>().ok();
    match args.first().copied() {
        Some("5") => args.get(1).and_then(num).map(i32::from),
        Some("2") => {
            let rgb: Vec<u8> = args[1..]
                .iter()
                .filter(|s| !s.is_empty())
                .filter_map(num)
                .collect();
            match rgb[..] {
                [r, g, b, ..] => {
                    Some(RGB + (i32::from(r) << 16 | i32::from(g) << 8 | i32::from(b)))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Visible characters: no C0/C1 controls, no DEL, no bidi embedding/override/isolate marks.
fn printable(c: char) -> bool {
    !(c.is_control()
        || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn colours_and_attributes() {
        let mut p = Painter::default();
        let line = p.line("a\x1b[1;31mred\x1b[0m \x1b[38;5;208mo\x1b[48;2;1;2;3mx\x1b[38:2::255:0:0my\x1b[22;39;49m.");
        let styles: Vec<(String, Style)> = line.iter().map(|r| (r.text.clone(), r.style)).collect();
        assert_eq!(styles[0], ("a".into(), Style::default()));
        assert_eq!(
            styles[1].1,
            Style {
                fg: 1,
                bg: DEFAULT,
                attrs: BOLD
            }
        );
        assert_eq!(styles[3].1.fg, 208);
        assert_eq!(styles[4].1.bg, RGB + 0x010203);
        assert_eq!(styles[5].1.fg, RGB + 0xff0000);
        assert_eq!(styles[6].1, Style::default());
        assert_eq!(text(&line), "ared oxy.");
    }

    #[test]
    fn style_carries_across_lines() {
        let mut p = Painter::default();
        let _ = p.line("\x1b[32mgreen");
        let next = p.line("still");
        assert_eq!(next[0].style.fg, 2);
    }

    #[test]
    fn hostile_sequences_never_survive() {
        let hostile = concat!(
            "\x1b]52;c;c2VjcmV0\x07",              // OSC 52 clipboard write
            "\x1b]8;;https://evil.example/\x1b\\", // OSC 8 hyperlink open
            "click\x1b]8;;\x1b\\",                 // ... and close
            "\x1b]0;fake title\x07",               // window title
            "\x1bP1$qm\x1b\\",                     // DCS
            "\x1b_Gf=100;AAAA\x1b\\",              // APC (kitty graphics)
            "\x1b^pm\x1b\\\x1bXsos\x1b\\",         // PM, SOS
            "\x1b[2J\x1b[H\x1b[?1049h\x1b[6n",     // clear, home, alt screen, cursor report
            "\u{9b}31m\u{9d}0;t\u{7}",             // 8-bit CSI / OSC
            "\u{202e}gnp.exe\u{202c}",             // bidi override
            "\r\x08\x7f\x00ok"
        );
        let mut p = Painter::default();
        let line = p.line(hostile);
        let all = text(&line);
        assert_eq!(all, "click31m0;tgnp.exeok");
        assert!(all.chars().all(printable));
        assert!(line.iter().all(|r| r.style == Style::default()));
        // An unterminated OSC swallows the rest of the line rather than leaking it.
        assert_eq!(text(&p.line("a\x1b]8;;http://x/ b")), "a");
    }

    #[test]
    fn long_lines_are_cut() {
        let mut p = Painter::default();
        let line = p.line(&"x".repeat(100_000));
        assert_eq!(text(&line).len(), MAX_LINE_CHARS);
    }

    #[test]
    fn malformed_sgr_is_harmless() {
        let mut p = Painter::default();
        for raw in [
            "\x1b[38;5m",
            "\x1b[38;2;1m",
            "\x1b[48;5;999m",
            "\x1b[99999999999m",
            "\x1b[38:2m",
            "\x1b[",
        ] {
            let _ = p.line(raw);
        }
        assert_eq!(text(&p.line("ok")), "ok");
    }
}
