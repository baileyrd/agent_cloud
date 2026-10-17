//! Append-only segment log. One [`Log`] per stream (per agent).
//!
//! Layout: a directory of `<first_seq:020>.seg` files. Each record is
//! `len:u32le | checksum:u32le | payload`. Sequence numbers are implicit and
//! dense (record `n` of a segment has `seq = first_seq + n`). Payloads are
//! opaque bytes; the caller owns encoding.
//!
//! Durability: [`Log::append`] writes; [`Log::sync`] fsyncs. Batch appends,
//! then sync once. A torn tail record from a crash is truncated on open.
//! Reads scan a segment linearly; segments are size-bounded, so that is too.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const HEADER: usize = 8;
const MAX_RECORD: usize = 16 * 1024 * 1024;

/// Log failure, always with the operation that failed.
#[derive(Debug)]
pub enum LogError {
    /// Filesystem error.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A record failed its checksum or framing in a sealed segment.
    Corrupt {
        /// Segment file name.
        segment: String,
        /// Byte offset of the bad record.
        offset: usize,
    },
    /// Payload larger than the 16 MiB record limit.
    TooLarge(usize),
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { op, source } => write!(f, "{op}: {source}"),
            Self::Corrupt { segment, offset } => {
                write!(f, "corrupt record in {segment} at {offset}")
            }
            Self::TooLarge(n) => write!(f, "record of {n} bytes exceeds {MAX_RECORD}"),
        }
    }
}

impl std::error::Error for LogError {}

fn io(op: &'static str) -> impl FnOnce(std::io::Error) -> LogError {
    move |source| LogError::Io { op, source }
}

/// A numbered record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Dense, monotonically increasing sequence number.
    pub seq: u64,
    /// Opaque payload.
    pub payload: Vec<u8>,
}

/// An append-only log in one directory.
pub struct Log {
    dir: PathBuf,
    max_segment_bytes: u64,
    active: File,
    active_len: u64,
    next_seq: u64,
    recovered_bytes: u64,
}

fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(*b)).wrapping_mul(0x0100_0193)
    })
}

fn seg_name(first_seq: u64) -> String {
    format!("{first_seq:020}.seg")
}

/// Segment first-seqs in `dir`, ascending.
fn segments(dir: &Path) -> Result<Vec<u64>, LogError> {
    let mut v: Vec<u64> = fs::read_dir(dir)
        .map_err(io("list segments"))?
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .to_str()?
                .strip_suffix(".seg")?
                .parse()
                .ok()
        })
        .collect();
    v.sort_unstable();
    Ok(v)
}

/// Decode records from `buf`; returns entries and the offset of the first byte
/// that is not part of a valid record.
fn decode(buf: &[u8], first_seq: u64) -> (Vec<Entry>, usize) {
    let (mut out, mut at) = (Vec::new(), 0);
    while buf.len() - at >= HEADER {
        let len = u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) as usize;
        let sum = u32::from_le_bytes([buf[at + 4], buf[at + 5], buf[at + 6], buf[at + 7]]);
        let end = at + HEADER + len;
        if len > MAX_RECORD || end > buf.len() || checksum(&buf[at + HEADER..end]) != sum {
            break;
        }
        out.push(Entry {
            seq: first_seq + out.len() as u64,
            payload: buf[at + HEADER..end].to_vec(),
        });
        at = end;
    }
    (out, at)
}

impl Log {
    /// Open (creating `dir`), recovering from a torn tail in the newest segment.
    pub fn open(dir: impl Into<PathBuf>, max_segment_bytes: u64) -> Result<Self, LogError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(io("create log dir"))?;
        let (first, mut recovered_bytes) = (segments(&dir)?.pop().unwrap_or(0), 0);
        let path = dir.join(seg_name(first));
        let buf = fs::read(&path)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(Vec::new())
                } else {
                    Err(e)
                }
            })
            .map_err(io("read newest segment"))?;
        let (entries, valid) = decode(&buf, first);
        let active = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io("open segment"))?;
        if valid < buf.len() {
            recovered_bytes = (buf.len() - valid) as u64;
            active
                .set_len(valid as u64)
                .map_err(io("truncate torn tail"))?;
            active.sync_all().map_err(io("sync after truncate"))?;
        }
        Ok(Self {
            dir,
            max_segment_bytes,
            active,
            active_len: valid as u64,
            next_seq: first + entries.len() as u64,
            recovered_bytes,
        })
    }

    /// Sequence number the next append will receive.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Bytes of torn tail discarded by `open`.
    pub fn recovered_bytes(&self) -> u64 {
        self.recovered_bytes
    }

    /// Append one record, rotating first if the segment is full. Not fsynced.
    pub fn append(&mut self, payload: &[u8]) -> Result<u64, LogError> {
        if payload.len() > MAX_RECORD {
            return Err(LogError::TooLarge(payload.len()));
        }
        let size = (HEADER + payload.len()) as u64;
        if self.active_len > 0 && self.active_len + size > self.max_segment_bytes {
            self.rotate()?;
        }
        let mut rec = Vec::with_capacity(HEADER + payload.len());
        rec.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        rec.extend_from_slice(&checksum(payload).to_le_bytes());
        rec.extend_from_slice(payload);
        self.active.write_all(&rec).map_err(io("append"))?;
        self.active_len += size;
        self.next_seq += 1;
        Ok(self.next_seq - 1)
    }

    /// fsync appended data.
    pub fn sync(&mut self) -> Result<(), LogError> {
        self.active.sync_data().map_err(io("sync"))
    }

    fn rotate(&mut self) -> Result<(), LogError> {
        self.sync()?;
        let path = self.dir.join(seg_name(self.next_seq));
        self.active = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)
            .map_err(io("rotate"))?;
        self.active_len = 0;
        Ok(())
    }

    /// Up to `limit` entries with `seq >= from`, oldest first. Entries already
    /// pruned are skipped (the result starts at the oldest retained).
    pub fn read_from(&self, from: u64, limit: usize) -> Result<Vec<Entry>, LogError> {
        let segs = segments(&self.dir)?;
        let start = segs.iter().rposition(|s| *s <= from).unwrap_or(0);
        let mut out = Vec::new();
        for (i, first) in segs.iter().enumerate().skip(start) {
            let name = seg_name(*first);
            let buf = fs::read(self.dir.join(&name)).map_err(io("read segment"))?;
            let (entries, valid) = decode(&buf, *first);
            let is_newest = i + 1 == segs.len();
            if valid < buf.len() && !is_newest {
                return Err(LogError::Corrupt {
                    segment: name,
                    offset: valid,
                });
            }
            out.extend(entries.into_iter().filter(|e| e.seq >= from));
            if out.len() >= limit {
                out.truncate(limit);
                break;
            }
        }
        Ok(out)
    }

    /// The newest `n` entries.
    pub fn tail(&self, n: usize) -> Result<Vec<Entry>, LogError> {
        self.read_from(self.next_seq.saturating_sub(n as u64), n)
    }

    /// Delete oldest sealed segments until total size is at most `max_bytes`.
    /// The active segment is never deleted. Returns segments removed.
    pub fn prune(&mut self, max_bytes: u64) -> Result<usize, LogError> {
        let segs = segments(&self.dir)?;
        let sizes: Vec<u64> = segs
            .iter()
            .map(|s| {
                fs::metadata(self.dir.join(seg_name(*s)))
                    .map(|m| m.len())
                    .map_err(io("stat segment"))
            })
            .collect::<Result<_, _>>()?;
        let mut total: u64 = sizes.iter().sum();
        let mut removed = 0;
        for (first, size) in segs.iter().zip(&sizes).take(segs.len().saturating_sub(1)) {
            if total <= max_bytes {
                break;
            }
            fs::remove_file(self.dir.join(seg_name(*first))).map_err(io("remove segment"))?;
            total -= size;
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn tmp() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "ac-history-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn open(dir: &Path, max: u64) -> Log {
        Log::open(dir, max).unwrap_or_else(|e| panic!("{e}"))
    }

    fn payloads(es: &[Entry]) -> Vec<&[u8]> {
        es.iter().map(|e| e.payload.as_slice()).collect()
    }

    #[test]
    fn append_read_and_reopen() {
        let dir = tmp();
        let mut log = open(&dir, 1 << 20);
        assert_eq!(log.append(b"a").ok(), Some(0));
        assert_eq!(log.append(b"b").ok(), Some(1));
        assert!(log.sync().is_ok());
        drop(log);
        let log = open(&dir, 1 << 20);
        assert_eq!(log.next_seq(), 2);
        let all = log.read_from(0, 10).unwrap_or_default();
        assert_eq!(payloads(&all), [b"a".as_slice(), b"b"]);
        assert_eq!(log.tail(1).unwrap_or_default()[0].seq, 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rotates_and_reads_across_segments() {
        let dir = tmp();
        let mut log = open(&dir, 20);
        for i in 0..6u8 {
            assert!(log.append(&[i; 4]).is_ok());
        }
        assert!(segments(&dir).unwrap_or_default().len() > 1);
        let got = log.read_from(2, 3).unwrap_or_default();
        assert_eq!(got.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!(got[0].payload, [2; 4]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn torn_tail_is_truncated_on_open() {
        let dir = tmp();
        let mut log = open(&dir, 1 << 20);
        let _ = log.append(b"good");
        let _ = log.sync();
        drop(log);
        let seg = dir.join(seg_name(0));
        let mut f = OpenOptions::new()
            .append(true)
            .open(&seg)
            .unwrap_or_else(|e| panic!("{e}"));
        let _ = f.write_all(&[9, 0, 0, 0, 1]); // partial header + nothing
        drop(f);
        let mut log = open(&dir, 1 << 20);
        assert_eq!(log.recovered_bytes(), 5);
        assert_eq!(log.append(b"next").ok(), Some(1));
        assert_eq!(log.read_from(0, 10).unwrap_or_default().len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_sealed_segment_is_an_error() {
        let dir = tmp();
        let mut log = open(&dir, 20);
        for i in 0..4u8 {
            let _ = log.append(&[i; 4]);
        }
        let first = dir.join(seg_name(0));
        let mut bytes = fs::read(&first).unwrap_or_default();
        bytes[HEADER] ^= 0xff;
        let _ = fs::write(&first, bytes);
        assert!(matches!(
            log.read_from(0, 10),
            Err(LogError::Corrupt { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_drops_oldest_sealed_never_active() {
        let dir = tmp();
        let mut log = open(&dir, 20);
        for i in 0..8u8 {
            let _ = log.append(&[i; 4]);
        }
        let before = segments(&dir).unwrap_or_default().len();
        let removed = log.prune(0).unwrap_or(0);
        assert_eq!(removed, before - 1);
        let rest = log.read_from(0, 100).unwrap_or_default();
        assert_eq!(rest.last().map(|e| e.seq), Some(7));
        assert!(log.append(b"still works").is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn oversize_record_rejected() {
        let dir = tmp();
        let mut log = open(&dir, 1 << 20);
        assert!(matches!(
            log.append(&vec![0; MAX_RECORD + 1]),
            Err(LogError::TooLarge(_))
        ));
        let _ = fs::remove_dir_all(dir);
    }
}
