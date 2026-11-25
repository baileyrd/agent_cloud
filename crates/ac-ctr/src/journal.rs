//! The egress proxy's journal for a time window (ADR-029), for a job's witness record. Only
//! the read is here; counting the lines is [`ac_domain::witness::decisions`].

use std::ffi::OsStr;

use ac_ports::PortError;
use platform::process::Spawner;

use crate::{os, CtrRuntime};

/// The proxy's unit.
pub const EGRESS_UNIT: &str = "ac-egress.service";
/// Most journal text read for one window. Past it the read fails, and the record says egress
/// is unavailable rather than counting part of it.
pub const JOURNAL_CAP: usize = 16 * 1024 * 1024;

/// `journalctl` arguments for the proxy's messages from `since_ms` to `until_ms`, widened to
/// whole seconds (the journal's `@` times are seconds).
pub fn journal_args(since_ms: u64, until_ms: u64) -> Vec<String> {
    let since = since_ms / 1000;
    let until = until_ms.div_ceil(1000);
    vec![
        "--quiet".into(),
        "--no-pager".into(),
        "--output=cat".into(),
        format!("--unit={EGRESS_UNIT}"),
        format!("--since=@{since}"),
        format!("--until=@{until}"),
    ]
}

impl<S: Spawner> CtrRuntime<S> {
    /// The proxy's journal messages from `since_ms` to `until_ms`, at most [`JOURNAL_CAP`] bytes.
    pub fn egress_journal(&self, since_ms: u64, until_ms: u64) -> Result<String, PortError> {
        let args = journal_args(since_ms, until_ms);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.run_capped(
            "egress journal",
            OsStr::new("journalctl"),
            &os(&args),
            None,
            JOURNAL_CAP,
        )?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_widened_to_whole_seconds() {
        let args = journal_args(1_700_000_000_250, 1_700_000_009_001);
        assert!(args.contains(&"--since=@1700000000".to_owned()), "{args:?}");
        assert!(args.contains(&"--until=@1700000010".to_owned()), "{args:?}");
        assert!(args.contains(&"--unit=ac-egress.service".to_owned()));
        assert_eq!(journal_args(5_000, 5_000)[5], "--until=@5");
    }
}
