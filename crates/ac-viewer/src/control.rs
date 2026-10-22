//! tmux control mode (`tmux -C`), one line at a time: pane output, command replies, and notices
//! that the session's windows changed.
//!
//! A reply is the lines between `%begin <time> <n> <flags>` and an `%end`/`%error` with the same
//! three words. Captured pane lines appear raw inside a reply, so a hostile program can print a
//! line that looks like an `%end`. Matching all three words makes that a guess, and blocks are
//! size-bounded; the worst a successful guess does is garble that agent's own viewer until the
//! next snapshot.

/// What one line meant.
#[derive(Debug, PartialEq, Eq)]
pub enum Notice {
    /// Bytes the pane's program wrote (`%output`), unescaped.
    Output(Vec<u8>),
    /// The reply to one of this client's commands, in the order they were sent.
    Reply {
        /// `%end` (true) or `%error` (false).
        ok: bool,
        /// The reply's lines.
        lines: Vec<String>,
    },
    /// A window was added, closed, renamed or made current, or a window's active pane or layout
    /// changed: the window list (and what `main` shows) may differ now.
    Windows,
    /// The client is detaching, or the server is gone (`%exit`).
    Exit,
}

/// Notifications that mean [`Notice::Windows`]. (Outside a reply block every line is tmux's own:
/// pane output arrives escaped in `%output`, so a program cannot forge one.)
const WINDOW_NOTICES: [&str; 9] = [
    "%window-add ",
    "%window-close ",
    "%window-renamed ",
    "%window-pane-changed ",
    "%unlinked-window-add ",
    "%unlinked-window-close ",
    "%unlinked-window-renamed ",
    "%session-window-changed ",
    "%layout-change ",
];

/// Lines kept per reply; the rest are dropped.
pub const MAX_REPLY_LINES: usize = 2048;
/// Bytes kept per line.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

struct Block {
    args: Vec<u8>,
    ours: bool,
    lines: Vec<String>,
}

/// Turns control-mode lines into [`Notice`]s.
#[derive(Default)]
pub struct Parser {
    block: Option<Block>,
}

impl Parser {
    /// Feed one line (with or without its newline). `None` for lines that mean nothing to us.
    pub fn line(&mut self, line: &[u8]) -> Option<Notice> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        if self.block.is_some() {
            return self.in_block(line);
        }
        if let Some(args) = line.strip_prefix(b"%begin ") {
            // Flags 1: the command came from this client (0 is tmux's own, such as the attach).
            let ours = args.split(|b| *b == b' ').nth(2) == Some(b"1");
            self.block = Some(Block {
                args: args.to_vec(),
                ours,
                lines: Vec::new(),
            });
            return None;
        }
        if let Some(rest) = line.strip_prefix(b"%output ") {
            let data = rest.splitn(2, |b| *b == b' ').nth(1).unwrap_or_default();
            return Some(Notice::Output(unescape(data)));
        }
        if line == b"%exit" || line.starts_with(b"%exit ") {
            return Some(Notice::Exit);
        }
        if WINDOW_NOTICES
            .iter()
            .any(|n| line.starts_with(n.as_bytes()))
        {
            return Some(Notice::Windows);
        }
        None
    }

    fn in_block(&mut self, line: &[u8]) -> Option<Notice> {
        let end = match (line.strip_prefix(b"%end "), line.strip_prefix(b"%error ")) {
            (Some(args), _) => Some((true, args)),
            (_, Some(args)) => Some((false, args)),
            _ => None,
        };
        let block = self.block.as_mut()?;
        if let Some((ok, args)) = end {
            if args == block.args.as_slice() {
                let block = self.block.take()?;
                return block.ours.then_some(Notice::Reply {
                    ok,
                    lines: block.lines,
                });
            }
        }
        if block.lines.len() < MAX_REPLY_LINES {
            let kept = &line[..line.len().min(MAX_LINE_BYTES)];
            block.lines.push(String::from_utf8_lossy(kept).into_owned());
        }
        None
    }
}

/// Undo `%output` escaping: `\ooo` (three octal digits) is one byte.
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let octal = data.get(i + 1..i + 4).filter(|d| {
            d.iter().all(|b| (b'0'..=b'7').contains(b)) && d[0] <= b'3' && data[i] == b'\\'
        });
        match octal {
            Some(d) => {
                out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
                i += 4;
            }
            None => {
                out.push(data[i]);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(p: &mut Parser, text: &str) -> Vec<Notice> {
        text.lines().filter_map(|l| p.line(l.as_bytes())).collect()
    }

    #[test]
    fn output_is_unescaped() {
        let mut p = Parser::default();
        assert_eq!(
            p.line(br"%output %0 \033[31mred\033[0m h\303\251 \134 \015\012"),
            Some(Notice::Output(
                b"\x1b[31mred\x1b[0m h\xc3\xa9 \\ \r\n".to_vec()
            ))
        );
        assert_eq!(p.line(b"%output %0"), Some(Notice::Output(Vec::new())));
        assert_eq!(unescape(br"\9\07x\777"), b"\\9\\07x\\777");
    }

    #[test]
    fn replies_only_for_our_commands() {
        let mut p = Parser::default();
        let got = feed(
            &mut p,
            "%begin 100 1 0\n%end 100 1 0\n%session-changed $0 main\n%begin 100 2 1\n80 24\n%end 100 2 1\n\
             %begin 100 3 1\nclient is read-only\n%error 100 3 1\n%exit",
        );
        assert_eq!(
            got,
            [
                Notice::Reply {
                    ok: true,
                    lines: vec!["80 24".into()]
                },
                Notice::Reply {
                    ok: false,
                    lines: vec!["client is read-only".into()]
                },
                Notice::Exit,
            ]
        );
    }

    #[test]
    fn pane_lines_that_look_like_protocol_stay_inside_the_reply() {
        let mut p = Parser::default();
        let got = feed(
            &mut p,
            "%begin 100 7 1\n%end 100 6 1\n%error 99 7 1\n%output %0 spoof\n%exit\n%begin 1 1 1\nok\n%end 100 7 1",
        );
        assert_eq!(got.len(), 1);
        let Notice::Reply { ok, lines } = &got[0] else {
            panic!("{got:?}")
        };
        assert!(ok);
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[3], "%exit");
    }

    #[test]
    fn window_changes_are_noticed_outside_replies_only() {
        let mut p = Parser::default();
        let got = feed(
            &mut p,
            "%window-add @3\n%window-close @3\n%unlinked-window-close @9\n%window-renamed @1 job-x\n\
             %session-window-changed $0 @1\n%window-pane-changed @1 %4\n%layout-change @1 b25d,80x24,0,0,0\n\
             %window-addx\n%output %0 %window-add @7\n%begin 1 1 1\n%window-close @0\n%end 1 1 1",
        );
        assert_eq!(got.len(), 9, "{got:?}");
        assert!(got[..7].iter().all(|n| *n == Notice::Windows), "{got:?}");
        assert_eq!(got[7], Notice::Output(b"%window-add @7".to_vec()));
        assert_eq!(
            got[8],
            Notice::Reply {
                ok: true,
                lines: vec!["%window-close @0".into()]
            }
        );
    }

    #[test]
    fn replies_and_lines_are_bounded() {
        let mut p = Parser::default();
        assert_eq!(p.line(b"%begin 1 1 1"), None);
        let long = vec![b'x'; MAX_LINE_BYTES * 2];
        for _ in 0..MAX_REPLY_LINES + 10 {
            assert_eq!(p.line(&long), None);
        }
        let Some(Notice::Reply { lines, .. }) = p.line(b"%end 1 1 1") else {
            panic!()
        };
        assert_eq!(lines.len(), MAX_REPLY_LINES);
        assert_eq!(lines[0].len(), MAX_LINE_BYTES);
    }
}
