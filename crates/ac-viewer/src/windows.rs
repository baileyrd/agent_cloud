//! The windows of session `main`, and which one a viewer watches. Window and pane ids are parsed
//! into numbers (`@<digits>`, `%<digits>`) and only ever rendered back from those numbers, so no
//! viewer-supplied text can reach a tmux command; window names (chosen by the agent) are stripped
//! to plain text and only ever sent to the page as JSON strings.

use std::fmt;

use crate::{json, text};

/// `list-windows -F` format whose output [`parse_list`] reads; the name last, as it may hold spaces.
pub const LIST_FORMAT: &str =
    "#{window_id} #{pane_id} #{window_index} #{window_active} #{window_name}";
/// Windows kept from a listing.
pub const MAX_WINDOWS: usize = 64;
/// Characters kept of a window name.
const MAX_NAME: usize = 64;
/// Digits in an id: below `u32::MAX`, and refuses absurd input before parsing it.
const MAX_DIGITS: usize = 9;

/// A tmux window id, `@<digits>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

/// A tmux pane id, `%<digits>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u32);

impl WindowId {
    /// Exactly `@` and 1 to 9 ASCII digits, no leading zero (so it renders back as given);
    /// anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        digits(s, '@').map(Self)
    }
}

impl PaneId {
    /// Exactly `%` and 1 to 9 ASCII digits, no leading zero; anything else is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        digits(s, '%').map(Self)
    }
}

fn digits(s: &str, sigil: char) -> Option<u32> {
    let d = s.strip_prefix(sigil)?;
    let ok = !d.is_empty()
        && d.len() <= MAX_DIGITS
        && d.bytes().all(|b| b.is_ascii_digit())
        && (d == "0" || !d.starts_with('0'));
    ok.then(|| d.parse().ok()).flatten()
}

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.0)
    }
}

impl fmt::Display for PaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

/// What a viewer watches: the window the agent has active in `main` (the default), or one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Session `main`'s current window, whichever it is.
    Active,
    /// This window, wherever the agent is.
    Window(WindowId),
}

impl fmt::Display for Target {
    /// The tmux target: `main` or the window id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("main"),
            Self::Window(w) => w.fmt(f),
        }
    }
}

/// One window of `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Its id.
    pub id: WindowId,
    /// Its active pane.
    pub pane: PaneId,
    /// Its number in the session.
    pub index: u32,
    /// The agent's current window.
    pub active: bool,
    /// Its name, as plain text.
    pub name: String,
}

impl Window {
    /// A task API job's window (`job-<id>`, ADR-010).
    pub fn is_job(&self) -> bool {
        self.name.starts_with("job-")
    }
}

/// Windows from `list-windows -F` [`LIST_FORMAT`] lines; lines that do not fit are skipped.
pub fn parse_list(lines: &[String]) -> Vec<Window> {
    lines
        .iter()
        .filter_map(|l| {
            let mut f = l.splitn(5, ' ');
            let id = WindowId::parse(f.next()?)?;
            let pane = PaneId::parse(f.next()?)?;
            let index = f.next()?.parse().ok()?;
            let active = match f.next()? {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            let name: String = text::strip(f.next().unwrap_or_default().as_bytes())
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_NAME)
                .collect();
            Some(Window {
                id,
                pane,
                index,
                active,
                name,
            })
        })
        .take(MAX_WINDOWS)
        .collect()
}

/// `{"t":"windows","list":[{"id","index","name","active","job"}...]}`.
pub fn message(list: &[Window]) -> String {
    let mut out = String::from("{\"t\":\"windows\",\"list\":[");
    for (k, w) in list.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"id\":\"{}\",\"index\":{},\"name\":",
            w.id, w.index
        ));
        json::string(&mut out, &w.name);
        out.push_str(&format!(
            ",\"active\":{},\"job\":{}}}",
            w.active,
            w.is_job()
        ));
    }
    out.push_str("]}\n");
    out
}

/// Which window this viewer now watches: `{"t":"target","window":"@3"}`, or `null` for the agent's.
pub fn target(t: Target) -> String {
    match t {
        Target::Active => "{\"t\":\"target\",\"window\":null}\n".into(),
        Target::Window(w) => format!("{{\"t\":\"target\",\"window\":\"{w}\"}}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn ids_are_exactly_a_sigil_and_digits() {
        assert_eq!(
            WindowId::parse("@0").map(|w| w.to_string()),
            Some("@0".into())
        );
        assert_eq!(
            WindowId::parse("@123456789").map(|w| w.to_string()),
            Some("@123456789".into())
        );
        assert_eq!(
            PaneId::parse("%7").map(|p| p.to_string()),
            Some("%7".into())
        );
        for bad in [
            "",
            "@",
            "@1;kill-server",
            "@1 ",
            " @1",
            "@1\n",
            "%1",
            "1",
            "@-1",
            "@+1",
            "@1.5",
            "@0x1",
            "@1234567890",
            "@01",
            "@00",
            "@99999999999999999999999",
            "@١",
            "@１",
            "＠1",
            "@1\u{0}",
            "main",
        ] {
            assert_eq!(WindowId::parse(bad), None, "{bad:?}");
        }
        assert_eq!(PaneId::parse("@1"), None);
        assert_eq!(PaneId::parse("%1;x"), None);
    }

    #[test]
    fn targets_render_only_from_numbers() {
        assert_eq!(Target::Active.to_string(), "main");
        let w = WindowId::parse("@42").map(Target::Window);
        assert_eq!(w.map(|t| t.to_string()), Some("@42".into()));
    }

    #[test]
    fn list_parses_and_strips_names() {
        let got = parse_list(&raw(&[
            "@0 %0 0 1 bash",
            "@3 %5 1 0 job-6b941feec45a4c25",
            "@4 %6 2 0 two words \x1b]0;title\x07\u{202e}x\ty",
            "@5 %7 3 0 ",
            "@1;kill-server %1 4 0 x",
            "@6 %8 x 0 bad index",
            "@7 %9 5 2 bad flag",
            "garbage",
            "",
        ]));
        let ids: Vec<String> = got.iter().map(|w| w.id.to_string()).collect();
        assert_eq!(ids, ["@0", "@3", "@4", "@5"]);
        assert!(got[0].active && !got[0].is_job());
        assert!(got[1].is_job() && got[1].pane.to_string() == "%5" && got[1].index == 1);
        assert_eq!(got[2].name, "two words xy");
        assert_eq!(got[3].name, "");
        let long = format!("@1 %1 0 0 {}", "n".repeat(500));
        assert_eq!(parse_list(&raw(&[&long]))[0].name.len(), MAX_NAME);
        let many: Vec<String> = (0..200).map(|i| format!("@{i} %{i} {i} 0 w")).collect();
        assert_eq!(parse_list(&many).len(), MAX_WINDOWS);
    }

    #[test]
    fn messages() {
        let list = parse_list(&raw(&["@0 %0 0 1 ba\"sh", "@3 %5 1 0 job-1"]));
        assert_eq!(
            message(&list),
            "{\"t\":\"windows\",\"list\":[{\"id\":\"@0\",\"index\":0,\"name\":\"ba\\\"sh\",\"active\":true,\"job\":false},\
             {\"id\":\"@3\",\"index\":1,\"name\":\"job-1\",\"active\":false,\"job\":true}]}\n"
        );
        assert_eq!(
            target(Target::Active),
            "{\"t\":\"target\",\"window\":null}\n"
        );
        let w = WindowId::parse("@3").map(Target::Window);
        assert_eq!(
            w.map(target),
            Some("{\"t\":\"target\",\"window\":\"@3\"}\n".into())
        );
    }
}
