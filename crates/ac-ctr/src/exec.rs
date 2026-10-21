//! A long-lived `ctr task exec` that never hands its guest process stdin EOF.
//!
//! Without `-t`, `ctr` never passes stdin EOF to the guest. When the host side of the pipe reaches
//! EOF while the guest process lives, the Kata shim blocks every other call on that task (new
//! execs, `task ls`, signal forwarding) until the guest process exits or the `ctr` client is
//! killed with SIGKILL (ADR-006). So [`Exec`] owns the child and its stdin together and has one
//! way to end, run by [`Exec::close`] and by `Drop` (so also on panic and early return): write
//! the quit command, wait, SIGTERM (`ctr` forwards it to the guest process), wait, SIGKILL (safe
//! for the shim), reap, and only then drop stdin.

use std::time::{Duration, Instant};

use platform::fs::File;
use platform::process::{Child, Signal};

/// How [`Exec::close`] ended the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// It had already exited.
    Gone,
    /// It exited after the quit command.
    Quit,
    /// It exited after SIGTERM.
    Term,
    /// It was killed with SIGKILL.
    Kill,
}

/// A running exec and its stdin. Stdin is private and dropped only after the process is reaped.
pub struct Exec {
    child: Option<Box<dyn Child>>,
    stdin: Option<Box<dyn File>>,
    quit: Vec<u8>,
    grace: Duration,
}

impl Exec {
    /// Take ownership of a spawned child (and its piped stdin). `quit` is written to make the
    /// guest process exit by itself; each later step waits up to `grace`.
    pub fn new(mut child: Box<dyn Child>, quit: &[u8], grace: Duration) -> Self {
        let stdin = child.take_stdin();
        Self {
            child: Some(child),
            stdin,
            quit: quit.to_vec(),
            grace,
        }
    }

    /// Write all of `bytes` to the process's stdin.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        match self.stdin.as_mut() {
            Some(stdin) => write_all(stdin.as_mut(), bytes),
            None => Err("stdin is not piped".into()),
        }
    }

    /// The read end of the process's stdout, if piped. Yields `Some` once.
    pub fn take_stdout(&mut self) -> Option<Box<dyn File>> {
        self.child.as_mut()?.take_stdout()
    }

    /// The host pid of the `ctr` client.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    /// End the process (see the module docs) and say how.
    pub fn close(mut self) -> Closed {
        self.finish()
    }

    fn finish(&mut self) -> Closed {
        let Some(mut child) = self.child.take() else {
            return Closed::Gone;
        };
        let how = if exited(child.as_mut(), Duration::ZERO) {
            Closed::Gone
        } else {
            if let Some(stdin) = self.stdin.as_mut() {
                let _ = write_all(stdin.as_mut(), &self.quit);
            }
            if exited(child.as_mut(), self.grace) {
                Closed::Quit
            } else {
                let _ = child.kill_single(Signal::Term);
                if exited(child.as_mut(), self.grace) {
                    Closed::Term
                } else {
                    let _ = child.kill_single(Signal::Kill);
                    Closed::Kill
                }
            }
        };
        // SIGKILL cannot be ignored, so this returns; for the other cases the status is stashed.
        let _ = child.wait();
        self.stdin = None;
        how
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Poll until the child has exited or `within` has passed. An error polling counts as running,
/// so the caller escalates instead of dropping stdin on a guess.
fn exited(child: &mut dyn Child, within: Duration) -> bool {
    let end = Instant::now() + within;
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_all(file: &mut dyn File, mut bytes: &[u8]) -> Result<(), String> {
    while !bytes.is_empty() {
        match file.write(bytes) {
            Ok(0) => return Err("write returned 0".into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) => return Err(e.to_string()),
        }
    }
    file.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use platform::error::{ErrorKind, OsCode, PlatformError};
    use platform::process::ExitStatus;

    use super::*;

    /// What the fake process gives in to.
    #[derive(Clone, Copy, PartialEq)]
    enum Yields {
        Quit,
        Term,
        Kill,
    }

    struct State {
        alive: bool,
        yields: Yields,
        log: Vec<String>,
    }

    type Shared = Rc<RefCell<State>>;

    struct FakeStdin(Shared);

    impl File for FakeStdin {
        fn read(&mut self, _: &mut [u8]) -> platform::error::Result<usize> {
            Ok(0)
        }
        fn write(&mut self, buf: &[u8]) -> platform::error::Result<usize> {
            let mut s = self.0.borrow_mut();
            if !s.alive {
                return Err(PlatformError::new(
                    ErrorKind::BrokenPipe,
                    OsCode::None,
                    "write",
                ));
            }
            let text = String::from_utf8_lossy(buf).trim().to_owned();
            if text == "quit" && s.yields == Yields::Quit {
                s.alive = false;
            }
            s.log.push(format!("write {text}"));
            Ok(buf.len())
        }
        fn flush(&mut self) -> platform::error::Result<()> {
            Ok(())
        }
        fn sync_all(&mut self) -> platform::error::Result<()> {
            Ok(())
        }
        fn try_clone(&self) -> platform::error::Result<Box<dyn File>> {
            Err(PlatformError::new(
                ErrorKind::Unsupported,
                OsCode::None,
                "clone",
            ))
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    impl Drop for FakeStdin {
        fn drop(&mut self) {
            let mut s = self.0.borrow_mut();
            let line = format!("stdin closed, alive={}", s.alive);
            s.log.push(line);
        }
    }

    struct FakeChild {
        state: Shared,
        stdin: Option<Box<dyn File>>,
    }

    impl Child for FakeChild {
        fn wait(self: Box<Self>) -> platform::error::Result<ExitStatus> {
            let s = self.state.borrow();
            assert!(!s.alive, "wait on a live child would block forever");
            Ok(ExitStatus::Code(0))
        }
        fn id(&self) -> u32 {
            7
        }
        fn kill_tree(&self, sig: Signal) -> platform::error::Result<()> {
            self.kill_single(sig)
        }
        fn kill_single(&self, sig: Signal) -> platform::error::Result<()> {
            let mut s = self.state.borrow_mut();
            s.log.push(format!("{sig:?}"));
            if matches!(sig, Signal::Kill)
                || (matches!(sig, Signal::Term) && s.yields == Yields::Term)
            {
                s.alive = false;
            }
            Ok(())
        }
        fn try_wait(&mut self) -> platform::error::Result<Option<ExitStatus>> {
            Ok((!self.state.borrow().alive).then_some(ExitStatus::Code(0)))
        }
        fn wait_job(&mut self) -> platform::error::Result<ExitStatus> {
            Ok(ExitStatus::Code(0))
        }
        fn try_wait_job(&mut self) -> platform::error::Result<Option<ExitStatus>> {
            self.try_wait()
        }
        fn take_stdin(&mut self) -> Option<Box<dyn File>> {
            self.stdin.take()
        }
        fn take_stdout(&mut self) -> Option<Box<dyn File>> {
            None
        }
        fn take_stderr(&mut self) -> Option<Box<dyn File>> {
            None
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    fn spawn(yields: Yields) -> (Exec, Shared) {
        let state = Rc::new(RefCell::new(State {
            alive: true,
            yields,
            log: Vec::new(),
        }));
        let child = FakeChild {
            state: Rc::clone(&state),
            stdin: Some(Box::new(FakeStdin(Rc::clone(&state)))),
        };
        let exec = Exec::new(Box::new(child), b"quit\n", Duration::from_millis(50));
        (exec, state)
    }

    fn log(state: &Shared) -> Vec<String> {
        state.borrow().log.clone()
    }

    /// The invariant: stdin is closed exactly once, last, after the process has exited.
    fn assert_closed_last(state: &Shared) {
        let log = log(state);
        let closes: Vec<&String> = log.iter().filter(|l| l.starts_with("stdin")).collect();
        assert_eq!(closes, ["stdin closed, alive=false"], "{log:?}");
        assert_eq!(
            log.last().map(String::as_str),
            Some("stdin closed, alive=false")
        );
    }

    #[test]
    fn quit_then_close_stdin() {
        let (mut exec, state) = spawn(Yields::Quit);
        exec.send(b"hello\n").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(exec.close(), Closed::Quit);
        assert_eq!(
            log(&state),
            ["write hello", "write quit", "stdin closed, alive=false"]
        );
    }

    #[test]
    fn ignores_quit_gets_sigterm() {
        let (exec, state) = spawn(Yields::Term);
        assert_eq!(exec.close(), Closed::Term);
        assert_eq!(
            log(&state),
            ["write quit", "Term", "stdin closed, alive=false"]
        );
    }

    #[test]
    fn ignores_sigterm_gets_sigkill() {
        let (exec, state) = spawn(Yields::Kill);
        assert_eq!(exec.close(), Closed::Kill);
        assert_eq!(
            log(&state),
            ["write quit", "Term", "Kill", "stdin closed, alive=false"]
        );
    }

    #[test]
    fn already_exited_just_closes() {
        let (exec, state) = spawn(Yields::Quit);
        state.borrow_mut().alive = false;
        assert_eq!(exec.close(), Closed::Gone);
        assert_eq!(log(&state), ["stdin closed, alive=false"]);
    }

    #[test]
    fn drop_runs_the_sequence() {
        let (exec, state) = spawn(Yields::Term);
        drop(exec);
        assert_closed_last(&state);
    }

    #[test]
    fn panic_unwinds_through_the_sequence() {
        let (exec, state) = spawn(Yields::Kill);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = exec;
            panic!("handler bug");
        }));
        assert!(r.is_err());
        assert_closed_last(&state);
        assert!(log(&state).contains(&"Kill".to_owned()));
    }

    #[test]
    fn early_return_runs_the_sequence() {
        fn work(mut exec: Exec) -> Result<(), String> {
            exec.send(b"one\n")?;
            Err::<(), String>("bad reply".into())?;
            exec.send(b"never\n")
        }
        let (exec, state) = spawn(Yields::Quit);
        assert!(work(exec).is_err());
        assert_eq!(
            log(&state),
            ["write one", "write quit", "stdin closed, alive=false"]
        );
    }

    #[test]
    fn send_after_exit_fails_without_closing_stdin() {
        let (mut exec, state) = spawn(Yields::Quit);
        state.borrow_mut().alive = false;
        assert!(exec.send(b"x\n").is_err());
        assert!(log(&state).is_empty(), "stdin must stay open until close");
        drop(exec);
        assert_closed_last(&state);
    }
}
