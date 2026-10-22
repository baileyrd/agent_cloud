//! The proxy end to end over loopback sockets. Names resolve through a fake [`Upstream`]: the
//! public address 1.1.1.1 is wired to a local echo server, so the real vetting still applies.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ac_domain::Allowlist;
use ac_egress::{Limits, Proxy, Upstream};

const PUBLIC: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1));

struct Fake {
    names: HashMap<&'static str, IpAddr>,
    echo: SocketAddr,
}

impl Upstream for Fake {
    fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        let ip = self.names.get(host).ok_or(io::ErrorKind::NotFound)?;
        Ok(vec![SocketAddr::new(*ip, port)])
    }

    fn connect(&self, addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
        if addr.ip() != PUBLIC {
            return Err(io::ErrorKind::ConnectionRefused.into());
        }
        TcpStream::connect_timeout(&self.echo, timeout)
    }
}

/// Echoes every byte back until the peer closes.
fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut out = conn.try_clone().unwrap_or_else(|e| panic!("{e}"));
                let _ = io::copy(&mut &conn, &mut out);
            });
        }
    });
    addr
}

fn start(limits: Limits) -> SocketAddr {
    let names = HashMap::from([
        ("allowed.test", PUBLIC),
        ("lan.test", IpAddr::from([192, 168, 50, 1])),
        ("meta.test", IpAddr::from([169, 254, 169, 254])),
    ]);
    let list = Allowlist::new(["allowed.test", "lan.test", "meta.test", "gone.test"])
        .unwrap_or_else(|e| panic!("{e}"));
    let fake = Fake {
        names,
        echo: echo_server(),
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let addr = listener.local_addr().unwrap_or_else(|e| panic!("{e}"));
    let proxy = Arc::new(Proxy::new(list, fake, limits));
    thread::spawn(move || proxy.serve(listener));
    addr
}

fn connect(proxy: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(proxy).unwrap_or_else(|e| panic!("{e}"));
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    s
}

/// Read up to the end of the response head; returns the status code and the head.
fn response(s: &mut TcpStream) -> (u16, String) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match s.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

fn status_of(request: &str) -> u16 {
    let mut s = connect(start(Limits::default()));
    let _ = s.write_all(request.as_bytes());
    response(&mut s).0
}

#[test]
fn connect_tunnels_bytes_both_ways() {
    let mut s = connect(start(Limits::default()));
    let _ =
        s.write_all(b"CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\nearly");
    assert_eq!(response(&mut s).0, 200);
    let _ = s.write_all(b"+ping");
    let mut got = [0u8; 10];
    let _ = s.read_exact(&mut got);
    assert_eq!(&got, b"early+ping");
}

#[test]
fn plain_http_is_forwarded_in_origin_form_with_our_host_header() {
    let mut s = connect(start(Limits::default()));
    let request = "GET http://allowed.test/x?y=1 HTTP/1.1\r\nHost: evil.test\r\n\
                   Proxy-Authorization: Basic c2VjcmV0\r\nAccept: */*\r\n\r\n";
    let _ = s.write_all(request.as_bytes());
    // The echo server sends back exactly what reached upstream.
    let (_, echoed) = response(&mut s);
    assert_eq!(
        echoed,
        "GET /x?y=1 HTTP/1.1\r\nHost: allowed.test\r\nAccept: */*\r\n\r\n"
    );
}

#[test]
fn policy_denials_are_403() {
    for target in [
        "example.test:443",
        "allowed.test:22",
        "1.1.1.1:443",
        "lan.test:443",
        "meta.test:80",
        "localhost:443",
    ] {
        let req = format!("CONNECT {target} HTTP/1.1\r\n\r\n");
        assert_eq!(status_of(&req), 403, "{target}");
    }
    assert_eq!(status_of("GET http://lan.test/ HTTP/1.1\r\n\r\n"), 403);
}

#[test]
fn lookup_failure_is_403_and_hostile_heads_are_400_or_431() {
    assert_eq!(status_of("CONNECT gone.test:443 HTTP/1.1\r\n\r\n"), 403);
    assert_eq!(
        status_of("CONNECT u@allowed.test:443 HTTP/1.1\r\n\r\n"),
        400
    );
    assert_eq!(status_of("CONNECT [::1]:443 HTTP/1.1\r\n\r\n"), 400);
    assert_eq!(status_of("GET /relative HTTP/1.1\r\n\r\n"), 400);
    let huge = format!(
        "CONNECT allowed.test:443 HTTP/1.1\r\nX: {}\r\n\r\n",
        "a".repeat(10_000)
    );
    assert_eq!(status_of(&huge), 431);
}

#[test]
fn slow_heads_time_out_with_408() {
    let mut s = connect(start(Limits {
        head_timeout: Duration::from_millis(200),
        ..Limits::default()
    }));
    let _ = s.write_all(b"CONNECT allowed.test:443 HTTP/1.1\r\n");
    assert_eq!(response(&mut s).0, 408);
}

#[test]
fn idle_tunnels_are_closed() {
    let mut s = connect(start(Limits {
        idle: Duration::from_millis(300),
        ..Limits::default()
    }));
    let _ = s.write_all(b"CONNECT allowed.test:443 HTTP/1.1\r\n\r\n");
    assert_eq!(response(&mut s).0, 200);
    thread::sleep(Duration::from_millis(900));
    let mut buf = [0u8; 1];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)));
}

#[test]
fn connections_beyond_the_cap_get_503() {
    let proxy = start(Limits {
        max_conns: 2,
        ..Limits::default()
    });
    // Two clients that never send a head hold both slots until the head timeout.
    let _held = [connect(proxy), connect(proxy)];
    thread::sleep(Duration::from_millis(100));
    let mut third = connect(proxy);
    assert_eq!(response(&mut third).0, 503);
}
