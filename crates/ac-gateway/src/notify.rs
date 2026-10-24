//! Readiness for systemd (`Type=notify`, ADR-015): `READY=1` once the gateway can serve,
//! `STOPPING=1` when told to stop, as sd_notify(3) datagrams to `NOTIFY_SOCKET`. Std only.
//!
//! With `NOTIFY_SOCKET` unset (run by hand, or under a `Type=simple` unit, which gets none)
//! nothing is sent. A failed send is logged as a warning and the gateway carries on.

use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::PathBuf;

/// The message that ends startup; `status` shows in `systemctl status` (`STATUS=`).
pub fn ready(status: &str) -> String {
    format!("READY=1\nSTATUS={}\n", one_line(status))
}

/// The message sent when a stop begins.
pub const STOPPING: &str = "STOPPING=1\n";

/// A newline would start another assignment.
fn one_line(s: &str) -> String {
    s.replace('\n', " ")
}

/// Where `NOTIFY_SOCKET` points: a filesystem path, or an abstract name written `@name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// A socket file, such as `/run/systemd/notify`.
    Path(PathBuf),
    /// An abstract socket name, without the `@`.
    Abstract(Vec<u8>),
}

impl Address {
    /// Parse a `NOTIFY_SOCKET` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.as_bytes() {
            [b'@', name @ ..] if !name.is_empty() => Ok(Self::Abstract(name.to_vec())),
            [b'/', ..] => Ok(Self::Path(value.into())),
            _ => Err(format!(
                "NOTIFY_SOCKET={value}: want an absolute path or @name"
            )),
        }
    }

    fn socket_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Path(p) => SocketAddr::from_pathname(p),
            Self::Abstract(name) => SocketAddr::from_abstract_name(name),
        }
    }
}

/// Delivers one message.
pub trait Notifier {
    /// Send `message` whole, or fail.
    fn notify(&self, message: &str) -> io::Result<()>;
}

/// The real sender: one datagram per message from an unbound socket.
#[derive(Debug)]
pub struct Socket(pub Address);

impl Notifier for Socket {
    fn notify(&self, message: &str) -> io::Result<()> {
        let sent =
            UnixDatagram::unbound()?.send_to_addr(message.as_bytes(), &self.0.socket_addr()?)?;
        if sent != message.len() {
            return Err(io::Error::other(format!(
                "sent {sent} of {} bytes",
                message.len()
            )));
        }
        Ok(())
    }
}

/// The gateway's notifications: none without a [`Notifier`].
pub struct Notify(Option<Box<dyn Notifier + Send + Sync>>);

impl Notify {
    /// From `NOTIFY_SOCKET`; a malformed value is logged and nothing is sent.
    pub fn from_env() -> Self {
        let Ok(value) = std::env::var("NOTIFY_SOCKET") else {
            return Self(None);
        };
        match Address::parse(&value) {
            Ok(a) => Self::with(Socket(a)),
            Err(e) => {
                eprintln!("ac-gateway: WARNING: no readiness notifications: {e}");
                Self(None)
            }
        }
    }

    /// Through `n`.
    pub fn with(n: impl Notifier + Send + Sync + 'static) -> Self {
        Self(Some(Box::new(n)))
    }

    /// Send `message`; a failure is logged, never returned.
    pub fn send(&self, message: &str) {
        let Some(n) = &self.0 else { return };
        if let Err(e) = n.notify(message) {
            let first = message.lines().next().unwrap_or_default();
            eprintln!("ac-gateway: WARNING: systemd notification {first} not sent: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn messages() {
        assert_eq!(
            ready("listening on 127.0.0.1:8780; 2 agents"),
            "READY=1\nSTATUS=listening on 127.0.0.1:8780; 2 agents\n"
        );
        assert_eq!(ready("a\nb"), "READY=1\nSTATUS=a b\n");
        assert_eq!(STOPPING, "STOPPING=1\n");
    }

    #[test]
    fn addresses() {
        assert_eq!(
            Address::parse("/run/systemd/notify"),
            Ok(Address::Path("/run/systemd/notify".into()))
        );
        assert_eq!(
            Address::parse("@sd/notify"),
            Ok(Address::Abstract(b"sd/notify".to_vec()))
        );
        for bad in ["", "@", "run/notify", "vsock:2:1234"] {
            assert!(Address::parse(bad).is_err(), "{bad}");
        }
    }

    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<String>>>);

    impl Notifier for Seen {
        fn notify(&self, message: &str) -> io::Result<()> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("poisoned"))?
                .push(message.to_owned());
            Ok(())
        }
    }

    struct Fails;

    impl Notifier for Fails {
        fn notify(&self, _: &str) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::ConnectionRefused))
        }
    }

    #[test]
    fn sends_through_the_notifier() {
        let seen = Seen::default();
        let n = Notify::with(seen.clone());
        n.send(&ready("up"));
        n.send(STOPPING);
        let got = seen.0.lock().map(|v| v.clone()).unwrap_or_default();
        assert_eq!(got, ["READY=1\nSTATUS=up\n", "STOPPING=1\n"]);
    }

    #[test]
    fn a_failed_send_returns() {
        Notify::with(Fails).send(STOPPING);
        Notify(None).send(STOPPING);
    }
}
