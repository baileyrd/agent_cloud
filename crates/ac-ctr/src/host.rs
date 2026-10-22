//! Host adapters for the watchdog (ADR-012): `ctr` exec clients found in `/proc`, SIGKILL through
//! `kill`, pressure from `/proc/loadavg` and `/proc/pressure`, and a monotonic clock. The parsing
//! is pure and tested here; the reads are thin.

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use ac_domain::health::{ExecClient, Pressure};
use ac_domain::AgentId;
use ac_ports::{Clock, HostLoad, PortError, Watch};
use platform::process::{ExitStatus, Spawner};

use crate::{failure, os, CtrRuntime};

/// `/proc` reports times in USER_HZ, which is 100 on Linux whatever the kernel's HZ.
const USER_HZ: u64 = 100;

impl<S: Spawner> Watch for CtrRuntime<S> {
    fn probe(&self, id: &AgentId, budget: Duration) -> bool {
        CtrRuntime::probe(self, id, budget)
    }

    fn exec_clients(&self, id: &AgentId) -> Result<Vec<ExecClient>, PortError> {
        let fail = |cause: String| PortError {
            op: "exec_clients",
            cause,
        };
        let uptime = std::fs::read_to_string("/proc/uptime").map_err(|e| fail(e.to_string()))?;
        let now = uptime_ticks(&uptime).ok_or_else(|| fail(format!("/proc/uptime: {uptime:?}")))?;
        let dir = std::fs::read_dir("/proc").map_err(|e| fail(e.to_string()))?;
        let mut found = Vec::new();
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            // A process may exit between the listing and these reads: skip it.
            let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let argv: Vec<String> = raw
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            let Some((exec_id, agent)) = exec_of(&argv) else {
                continue;
            };
            let Some(start) = std::fs::read_to_string(entry.path().join("stat"))
                .ok()
                .and_then(|s| start_ticks(&s))
            else {
                continue;
            };
            if agent == id.as_str() {
                found.push(ExecClient {
                    pid,
                    exec_id: exec_id.to_owned(),
                    age: Duration::from_millis(now.saturating_sub(start) * 1000 / USER_HZ),
                    cmdline: argv.join(" "),
                });
            }
        }
        found.sort_by_key(|c| c.pid);
        Ok(found)
    }

    fn kill(&self, pids: &[u32]) -> Result<(), PortError> {
        if pids.is_empty() {
            return Ok(());
        }
        let mut args = vec!["-KILL".to_owned()];
        args.extend(pids.iter().map(u32::to_string));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        // Exit 1: one of them had already gone, which is what was wanted.
        match self.exec("kill", OsStr::new("kill"), &os(&args), None)? {
            (ExitStatus::Code(0 | 1), _, _) => Ok(()),
            (status, stdout, stderr) => Err(failure("kill", status, &stdout, &stderr)),
        }
    }
}

/// The exec id and agent of a `ctr [global flags] task exec [flags] <agent> ...` command line.
/// `argv[0]` must be `ctr` or a path to it, so a shell or `timeout` wrapping one never matches.
pub fn exec_of<'a>(argv: &[&'a str]) -> Option<(&'a str, &'a str)> {
    let (first, rest) = argv.split_first()?;
    if first.rsplit('/').next() != Some("ctr") {
        return None;
    }
    let at = rest.windows(2).position(|w| w == ["task", "exec"])?;
    let mut args = rest[at + 2..].iter();
    let mut exec_id = None;
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--exec-id=") {
            exec_id = Some(v);
        } else if matches!(
            *a,
            "--exec-id" | "--cwd" | "--fifo-dir" | "--log-uri" | "--user" | "-u"
        ) {
            let v = args.next()?;
            if *a == "--exec-id" {
                exec_id = Some(*v);
            }
        } else if !a.starts_with('-') {
            return Some((exec_id?, a));
        }
    }
    None
}

/// Seconds since boot from `/proc/uptime`, in USER_HZ ticks.
fn uptime_ticks(uptime: &str) -> Option<u64> {
    let secs = uptime.split_whitespace().next()?;
    let (whole, frac) = secs.split_once('.').unwrap_or((secs, "0"));
    let hundredths: u64 = format!("{frac:0<2}").get(..2)?.parse().ok()?;
    Some(whole.parse::<u64>().ok()? * USER_HZ + hundredths)
}

/// Field 22 (`starttime`) of `/proc/<pid>/stat`, after the parenthesised command name.
fn start_ticks(stat: &str) -> Option<u64> {
    stat.rsplit_once(") ")?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// [`HostLoad`] from `/proc`.
pub struct ProcPressure;

impl HostLoad for ProcPressure {
    fn pressure(&self) -> Result<Pressure, PortError> {
        let read = |path: &str| {
            std::fs::read_to_string(path).map_err(|e| PortError {
                op: "pressure",
                cause: format!("{path}: {e}"),
            })
        };
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        pressure(
            &read("/proc/loadavg")?,
            &read("/proc/pressure/cpu")?,
            &read("/proc/pressure/io")?,
            u32::try_from(cores).unwrap_or(u32::MAX),
        )
        .ok_or_else(|| PortError {
            op: "pressure",
            cause: "unexpected /proc/loadavg or /proc/pressure format".into(),
        })
    }
}

/// Load and PSI `some avg10` from the three files' text.
pub fn pressure(loadavg: &str, cpu: &str, io: &str, cores: u32) -> Option<Pressure> {
    let some = |psi: &str| -> Option<f64> {
        let line = psi.lines().find(|l| l.starts_with("some "))?;
        line.split_whitespace()
            .find_map(|f| f.strip_prefix("avg10="))?
            .parse()
            .ok()
    };
    Some(Pressure {
        load1: loadavg.split_whitespace().next()?.parse().ok()?,
        cores,
        cpu_some: some(cpu)?,
        io_some: some(io)?,
    })
}

/// [`Clock`] on [`Instant`].
pub struct Monotonic(Instant);

impl Monotonic {
    /// Starting now.
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for Monotonic {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for Monotonic {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_command_lines() {
        let line = |s: &'static str| s.split(' ').collect::<Vec<_>>();
        let gateway = line("ctr -n agents task exec --exec-id acprobe-a1-77-3 a1 true");
        assert_eq!(exec_of(&gateway), Some(("acprobe-a1-77-3", "a1")));
        let path = line("/usr/bin/ctr -n agents task exec -t --user 0 --exec-id x a2 sh -c id");
        assert_eq!(exec_of(&path), Some(("x", "a2")));
        assert_eq!(
            exec_of(&line("ctr task exec --exec-id=y a3 sh")),
            Some(("y", "a3"))
        );
        // Not a ctr exec client: the wrapping timeout, a shell, other ctr commands, no exec id.
        let wrapped = line("timeout -s TERM -k 3 10 ctr -n agents task exec --exec-id x a1 true");
        assert_eq!(exec_of(&wrapped), None);
        assert_eq!(exec_of(&line("sh -c ctr task exec --exec-id x a1")), None);
        assert_eq!(exec_of(&line("ctr -n agents task ls")), None);
        assert_eq!(exec_of(&line("ctr task exec a1 sh")), None);
        assert_eq!(exec_of(&line("ctr task exec --exec-id")), None);
    }

    #[test]
    fn proc_times() {
        assert_eq!(uptime_ticks("12345.67 99.0\n"), Some(1_234_567));
        assert_eq!(uptime_ticks("10.5 1"), Some(1050));
        assert_eq!(uptime_ticks("7 1"), Some(700));
        let stat = "4242 (ctr task) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 9 0 \
                    987654 123 456";
        assert_eq!(start_ticks(stat), Some(987_654));
        assert_eq!(start_ticks("1 (x) S"), None);
    }

    #[test]
    fn pressure_files() {
        let cpu = "some avg10=12.50 avg60=3.00 avg300=1.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let io = "some avg10=41.02 avg60=1 avg300=1 total=1\nfull avg10=40.00 avg60=1 avg300=1 total=1\n";
        assert_eq!(
            pressure("3.10 2.00 1.00 2/600 999\n", cpu, io, 4),
            Some(Pressure {
                load1: 3.1,
                cores: 4,
                cpu_some: 12.5,
                io_some: 41.02
            })
        );
        assert_eq!(pressure("", cpu, io, 4), None);
        assert_eq!(pressure("1.0", "full avg10=1", io, 4), None);
    }
}
