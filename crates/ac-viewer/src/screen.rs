//! Screen snapshots and the messages viewers receive, one JSON object per line: a full snapshot
//! on connect (with scrollback) and on resize, otherwise only the changed lines and the cursor.
//!
//! A line is an array of runs; a run is `[text, fg, bg, attrs]` (see [`crate::sgr`]).

use crate::json;
use crate::sgr::{Line, Painter};

/// Rows kept from a capture.
pub const MAX_ROWS: usize = 300;
/// Scrollback lines sent on connect.
pub const MAX_SCROLLBACK: usize = 1000;
/// `display -p` format whose output [`Meta::parse`] reads.
pub const DISPLAY_FORMAT: &str =
    "#{pane_width} #{pane_height} #{cursor_x} #{cursor_y} #{cursor_flag} #{bracket_paste_flag} #{keypad_cursor_flag}";

/// Pane size, cursor and modes, from [`DISPLAY_FORMAT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    /// Columns.
    pub width: u16,
    /// Rows.
    pub height: u16,
    /// Cursor column.
    pub cx: u16,
    /// Cursor row.
    pub cy: u16,
    /// Cursor visible.
    pub cursor: bool,
    /// The program asked for bracketed paste.
    pub bracketed_paste: bool,
    /// Application cursor-key mode (arrows send `ESC O x`).
    pub app_cursor: bool,
}

impl Meta {
    /// Parse one `display -p` reply line.
    pub fn parse(line: &str) -> Option<Self> {
        let v: Vec<u16> = line
            .split_whitespace()
            .map(|w| w.parse().ok())
            .collect::<Option<_>>()?;
        match v[..] {
            [width, height, cx, cy, cursor, paste, keys] if width > 0 && height > 0 => Some(Self {
                width,
                height,
                cx,
                cy,
                cursor: cursor == 1,
                bracketed_paste: paste == 1,
                app_cursor: keys == 1,
            }),
            _ => None,
        }
    }
}

/// One pane's visible screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Size and cursor.
    pub meta: Meta,
    /// Exactly `meta.height` lines (at most [`MAX_ROWS`]).
    pub lines: Vec<Line>,
}

impl Screen {
    /// From a `display` reply and the `capture-pane -p -e` lines that followed it.
    pub fn from_capture(meta: Meta, raw: &[String]) -> Self {
        let rows = usize::from(meta.height).min(MAX_ROWS);
        let mut lines = lines(raw);
        lines.resize(rows, Vec::new());
        Self { meta, lines }
    }
}

/// Sanitised lines from raw `capture-pane -p -e` output.
pub fn lines(raw: &[String]) -> Vec<Line> {
    let mut painter = Painter::default();
    raw.iter().map(|l| painter.line(l)).collect()
}

/// The first message on a stream: the viewer's id.
pub fn hello(viewer: &str) -> String {
    format!("{{\"t\":\"hello\",\"viewer\":{}}}\n", json::quoted(viewer))
}

/// Who holds the take-over lock, if anyone.
pub fn mode(owner: Option<&str>) -> String {
    match owner {
        Some(v) => format!("{{\"t\":\"mode\",\"owner\":{}}}\n", json::quoted(v)),
        None => "{\"t\":\"mode\",\"owner\":null}\n".into(),
    }
}

/// Keep-alive.
pub fn ping() -> String {
    "{\"t\":\"ping\"}\n".into()
}

/// The stream is ending.
pub fn bye(reason: &str) -> String {
    format!("{{\"t\":\"bye\",\"reason\":{}}}\n", json::quoted(reason))
}

/// The whole screen, plus scrollback (oldest first).
pub fn full(screen: &Screen, scroll: &[Line]) -> String {
    let mut out = String::from("{\"t\":\"full\"");
    let m = screen.meta;
    out.push_str(&format!(",\"w\":{},\"h\":{}", m.width, m.height));
    cursor(&mut out, &m);
    out.push_str(",\"scroll\":");
    array(&mut out, scroll.iter().rev().take(MAX_SCROLLBACK).rev());
    out.push_str(",\"lines\":");
    array(&mut out, screen.lines.iter());
    out.push_str("}\n");
    out
}

/// What changed from `prev` to `next`: `None` if nothing, a full snapshot if the size changed,
/// else the changed lines (`[[row, line], ...]`) and the cursor.
pub fn delta(prev: &Screen, next: &Screen) -> Option<String> {
    let (p, n) = (prev.meta, next.meta);
    if (p.width, p.height) != (n.width, n.height) {
        return Some(full(next, &[]));
    }
    let changed: Vec<(usize, &Line)> = next
        .lines
        .iter()
        .enumerate()
        .filter(|(i, l)| prev.lines.get(*i) != Some(*l))
        .collect();
    if changed.is_empty() && (p.cx, p.cy, p.cursor) == (n.cx, n.cy, n.cursor) {
        return None;
    }
    let mut out = String::from("{\"t\":\"delta\"");
    cursor(&mut out, &n);
    out.push_str(",\"lines\":[");
    for (k, (i, line)) in changed.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push_str(&format!("[{i},"));
        line_json(&mut out, line);
        out.push(']');
    }
    out.push_str("]}\n");
    Some(out)
}

fn cursor(out: &mut String, m: &Meta) {
    out.push_str(&format!(
        ",\"cx\":{},\"cy\":{},\"cv\":{}",
        m.cx, m.cy, m.cursor
    ));
}

fn array<'a>(out: &mut String, lines: impl Iterator<Item = &'a Line>) {
    out.push('[');
    for (k, line) in lines.enumerate() {
        if k > 0 {
            out.push(',');
        }
        line_json(out, line);
    }
    out.push(']');
}

fn line_json(out: &mut String, line: &Line) {
    out.push('[');
    for (k, run) in line.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push('[');
        json::string(out, &run.text);
        let s = run.style;
        out.push_str(&format!(",{},{},{}]", s.fg, s.bg, s.attrs));
    }
    out.push(']');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(w: u16, h: u16, cx: u16) -> Meta {
        Meta {
            width: w,
            height: h,
            cx,
            cy: 0,
            cursor: true,
            bracketed_paste: false,
            app_cursor: false,
        }
    }

    fn raw(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn meta_parses_display_output() {
        assert_eq!(
            Meta::parse("80 24 3 4 1 0 0"),
            Some(Meta {
                width: 80,
                height: 24,
                cx: 3,
                cy: 4,
                cursor: true,
                bracketed_paste: false,
                app_cursor: false,
            })
        );
        assert_eq!(
            Meta::parse("80 24 3 4 1 1 1").map(|m| (m.bracketed_paste, m.app_cursor)),
            Some((true, true))
        );
        for bad in [
            "",
            "80 24",
            "80 24 0 0 1 0",
            "0 24 0 0 1 0 0",
            "x 24 0 0 1 0 0",
            "80 24 0 0 1 0 0 9",
            "70000 1 0 0 0 0 0",
        ] {
            assert_eq!(Meta::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn capture_is_padded_and_cut_to_height() {
        let s = Screen::from_capture(meta(10, 3, 0), &raw(&["a"]));
        assert_eq!(s.lines.len(), 3);
        let s = Screen::from_capture(meta(10, 2, 0), &raw(&["a", "b", "c"]));
        assert_eq!(s.lines.len(), 2);
    }

    #[test]
    fn delta_sends_only_changed_lines() {
        let a = Screen::from_capture(meta(10, 3, 0), &raw(&["one", "two", "three"]));
        let b = Screen::from_capture(meta(10, 3, 2), &raw(&["one", "TWO", "three"]));
        assert_eq!(
            delta(&a, &b).as_deref(),
            Some("{\"t\":\"delta\",\"cx\":2,\"cy\":0,\"cv\":true,\"lines\":[[1,[[\"TWO\",-1,-1,0]]]]}\n")
        );
        assert_eq!(delta(&a, &a), None);
        let c = Screen::from_capture(meta(12, 3, 0), &raw(&["one", "two", "three"]));
        assert!(delta(&a, &c).is_some_and(|m| m.starts_with("{\"t\":\"full\"")));
    }

    #[test]
    fn full_has_scrollback_and_escaped_text() {
        let s = Screen::from_capture(meta(10, 1, 0), &raw(&["\x1b[31m\"q\"\x1b]8;;x\x1b\\"]));
        let msg = full(&s, &lines(&raw(&["old"])));
        assert_eq!(
            msg,
            "{\"t\":\"full\",\"w\":10,\"h\":1,\"cx\":0,\"cy\":0,\"cv\":true,\"scroll\":[[[\"old\",-1,-1,0]]],\"lines\":[[[\"\\\"q\\\"\",1,-1,0]]]}\n"
        );
        assert_eq!(msg.matches('\n').count(), 1);
        assert!(!msg.contains('\x1b'));
    }

    #[test]
    fn small_messages() {
        assert_eq!(hello("v1"), "{\"t\":\"hello\",\"viewer\":\"v1\"}\n");
        assert_eq!(mode(None), "{\"t\":\"mode\",\"owner\":null}\n");
        assert_eq!(mode(Some("v\"")), "{\"t\":\"mode\",\"owner\":\"v\\\"\"}\n");
        assert_eq!(bye("gone"), "{\"t\":\"bye\",\"reason\":\"gone\"}\n");
    }
}
