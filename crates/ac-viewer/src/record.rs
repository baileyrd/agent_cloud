//! What a viewed session records per agent: output chunks and take-over events, timestamped.
//! Keystrokes are never recorded (they can hold secrets typed during a take-over); a take-over
//! appears only as its start and end.
//!
//! Payload: one tag byte (`o` output, `e` event), the time as `u64` little-endian milliseconds
//! since the Unix epoch, then the bytes.

use crate::text;

/// One recorded item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// Bytes the pane's program wrote.
    Output {
        /// Milliseconds since the epoch.
        ms: u64,
        /// Raw bytes.
        bytes: Vec<u8>,
    },
    /// A take-over event ([`crate::registry::Event`] as text).
    Event {
        /// Milliseconds since the epoch.
        ms: u64,
        /// What happened.
        text: String,
    },
}

impl Record {
    /// The log payload.
    pub fn encode(&self) -> Vec<u8> {
        let (tag, ms, body) = match self {
            Self::Output { ms, bytes } => (b'o', ms, bytes.as_slice()),
            Self::Event { ms, text } => (b'e', ms, text.as_bytes()),
        };
        let mut out = Vec::with_capacity(9 + body.len());
        out.push(tag);
        out.extend_from_slice(&ms.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    /// Back from a payload; `None` if it is not one.
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let ms = u64::from_le_bytes(payload.get(1..9)?.try_into().ok()?);
        let body = &payload[9..];
        match payload[0] {
            b'o' => Some(Self::Output {
                ms,
                bytes: body.to_vec(),
            }),
            b'e' => Some(Self::Event {
                ms,
                text: String::from_utf8_lossy(body).into_owned(),
            }),
            _ => None,
        }
    }
}

/// Records as plain text: output with every escape stripped, each run of output headed by its
/// start time, events on their own lines.
pub fn render(records: &[Record]) -> String {
    let mut out = String::new();
    let mut run: Vec<u8> = Vec::new();
    let flush = |out: &mut String, run: &mut Vec<u8>| {
        if !run.is_empty() {
            out.push_str(&text::strip(run));
            if !out.ends_with('\n') {
                out.push('\n');
            }
            run.clear();
        }
    };
    for r in records {
        match r {
            Record::Output { ms, bytes } => {
                if run.is_empty() {
                    out.push_str(&format!("--- {} output\n", utc(*ms)));
                }
                run.extend_from_slice(bytes);
            }
            Record::Event { ms, text } => {
                flush(&mut out, &mut run);
                out.push_str(&format!(
                    "--- {} {}\n",
                    utc(*ms),
                    text::strip(text.as_bytes())
                ));
            }
        }
    }
    flush(&mut out, &mut run);
    out
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (`rusty_time`); past its range (a year beyond `i32`), the raw
/// milliseconds as `@<ms>ms`.
pub fn utc(ms: u64) -> String {
    let Some(t) = i64::try_from(ms / 1000)
        .ok()
        .and_then(|s| rusty_time::DateTime::from_unix_secs(s).ok())
    else {
        return format!("@{ms}ms");
    };
    let (d, h) = (t.date(), t.time());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        d.year(),
        d.month(),
        d.day(),
        h.hour(),
        h.minute(),
        h.second(),
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for r in [
            Record::Output {
                ms: 1,
                bytes: b"\x1b[31mx".to_vec(),
            },
            Record::Event {
                ms: u64::MAX,
                text: "takeover viewer=v1".into(),
            },
        ] {
            assert_eq!(Record::decode(&r.encode()), Some(r));
        }
        assert_eq!(Record::decode(b"o123"), None);
        assert_eq!(Record::decode(b"z12345678"), None);
    }

    #[test]
    fn time() {
        assert_eq!(utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(1_792_540_201_123), "2026-10-20T23:50:01.123Z");
        assert_eq!(utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn render_strips_and_interleaves() {
        let recs = [
            Record::Output {
                ms: 1000,
                bytes: b"\x1b[1mcount\x1b[0m 1\r\n".to_vec(),
            },
            Record::Output {
                ms: 1500,
                bytes: b"2\r\n\x1b]52;c;eA==\x07".to_vec(),
            },
            Record::Event {
                ms: 2000,
                text: "takeover viewer=v1".into(),
            },
            Record::Output {
                ms: 3000,
                bytes: b"3".to_vec(),
            },
        ];
        assert_eq!(
            render(&recs),
            "--- 1970-01-01T00:00:01.000Z output\ncount 1\n2\n\
             --- 1970-01-01T00:00:02.000Z takeover viewer=v1\n\
             --- 1970-01-01T00:00:03.000Z output\n3\n"
        );
    }
}
