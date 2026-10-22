//! Reading and parsing a proxy request head (`CONNECT host:port` or an absolute `http://` URI),
//! with hard limits on line length, total size and header count.

use std::fmt;
use std::io::{self, BufRead, Read};

/// Bounds on what a client may send before the proxy decides.
#[derive(Debug, Clone)]
pub struct HeadLimits {
    /// Longest request or header line, without its line ending.
    pub max_line: usize,
    /// Most bytes in the whole head.
    pub max_head: usize,
    /// Most header lines.
    pub max_headers: usize,
}

impl Default for HeadLimits {
    fn default() -> Self {
        Self {
            max_line: 4096,
            max_head: 8192,
            max_headers: 64,
        }
    }
}

/// What the client asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    /// `CONNECT`: a byte tunnel (HTTPS).
    Connect,
    /// A plain-HTTP request, to forward with this origin-form request line (`GET /x HTTP/1.1`).
    Http {
        /// The rewritten request line.
        request_line: String,
    },
}

/// A parsed request head.
#[derive(Debug)]
pub struct Head {
    /// Target hostname, lowercased. Not yet validated: see [`crate::policy::decide`].
    pub host: String,
    /// Target port.
    pub port: u16,
    /// Tunnel or plain request.
    pub kind: Kind,
    /// Header lines to forward with a plain request: everything but `Host` and `Proxy-*`.
    pub headers: Vec<String>,
}

impl Head {
    /// The head to send upstream for a plain request: the origin-form request line, a `Host`
    /// naming the vetted target (never the client's, which could name another site on a shared
    /// address), then the client's other headers. `None` for a tunnel.
    pub fn upstream(&self) -> Option<String> {
        let Kind::Http { request_line } = &self.kind else {
            return None;
        };
        let mut out = format!("{request_line}\r\nHost: {}", self.host);
        if self.port != 80 {
            out.push_str(&format!(":{}", self.port));
        }
        out.push_str("\r\n");
        for h in &self.headers {
            out.push_str(h);
            out.push_str("\r\n");
        }
        out.push_str("\r\n");
        Some(out)
    }
}

/// Why a head was refused.
#[derive(Debug)]
pub enum HeadError {
    /// The client closed the connection before sending anything.
    Closed,
    /// A line exceeded [`HeadLimits::max_line`].
    LineTooLong,
    /// The head exceeded [`HeadLimits::max_head`] or [`HeadLimits::max_headers`].
    TooLarge,
    /// Not a request this proxy serves.
    Malformed(&'static str),
    /// Reading failed (including the head timeout).
    Io(io::Error),
}

impl HeadError {
    /// HTTP status to answer with; `None` when there is no one to answer.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Closed => None,
            Self::LineTooLong | Self::TooLarge => Some(431),
            Self::Malformed(_) => Some(400),
            Self::Io(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Some(408)
            }
            Self::Io(_) => None,
        }
    }
}

impl fmt::Display for HeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("closed before a request"),
            Self::LineTooLong => f.write_str("line too long"),
            Self::TooLarge => f.write_str("request head too large"),
            Self::Malformed(what) => write!(f, "bad request: {what}"),
            Self::Io(e) => write!(f, "read: {e}"),
        }
    }
}

/// Read and parse one request head from `r`, leaving any bytes after it buffered in `r`.
pub fn read_head(r: &mut impl BufRead, limits: &HeadLimits) -> Result<Head, HeadError> {
    let line = read_line(r, limits.max_line)?.ok_or(HeadError::Closed)?;
    let mut total = line.len();
    let (host, port, kind) = request_line(&line)?;
    let mut headers = Vec::new();
    loop {
        let line = read_line(r, limits.max_line)?
            .ok_or(HeadError::Malformed("connection closed in head"))?;
        total += line.len() + 2;
        if total > limits.max_head {
            return Err(HeadError::TooLarge);
        }
        if line.is_empty() {
            break;
        }
        if headers.len() == limits.max_headers {
            return Err(HeadError::TooLarge);
        }
        let Some((name, _)) = line.split_once(':') else {
            return Err(HeadError::Malformed("header without a colon"));
        };
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(HeadError::Malformed("header name"));
        }
        let name = name.to_ascii_lowercase();
        if name != "host" && !name.starts_with("proxy-") {
            headers.push(line);
        }
    }
    Ok(Head {
        host,
        port,
        kind,
        headers,
    })
}

/// One line without its CRLF (or bare LF). `None` at EOF before any byte. Control characters
/// other than tab are refused, so nothing forwarded can smuggle a line break.
fn read_line(r: &mut impl BufRead, max: usize) -> Result<Option<String>, HeadError> {
    let mut buf = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(2);
    let n = r
        .by_ref()
        .take(limit)
        .read_until(b'\n', &mut buf)
        .map_err(HeadError::Io)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(if buf.len() > max {
            HeadError::LineTooLong
        } else {
            HeadError::Malformed("connection closed mid-line")
        });
    }
    buf.pop();
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    if buf.len() > max {
        return Err(HeadError::LineTooLong);
    }
    if buf.iter().any(|&b| b.is_ascii_control() && b != b'\t') {
        return Err(HeadError::Malformed("control character"));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| HeadError::Malformed("not UTF-8"))
}

fn request_line(line: &str) -> Result<(String, u16, Kind), HeadError> {
    let bad = HeadError::Malformed;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("request line"));
    };
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(bad("HTTP version"));
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err(bad("method"));
    }
    if method == "CONNECT" {
        let (host, port) = authority(target, None)?;
        return Ok((host, port, Kind::Connect));
    }
    let rest = target
        .strip_prefix("http://")
        .ok_or(bad("need CONNECT or an absolute http:// URI"))?;
    let (auth, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let (host, port) = authority(auth, Some(80))?;
    let path = path.split('#').next().unwrap_or("");
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let request_line = format!("{method} {path} {version}");
    Ok((host, port, Kind::Http { request_line }))
}

/// `host:port` (port optional when `default` is given). Userinfo and IPv6 literals are refused
/// here; IPv4 literals parse, and [`crate::policy::decide`] refuses them.
fn authority(s: &str, default: Option<u16>) -> Result<(String, u16), HeadError> {
    let bad = HeadError::Malformed;
    if s.contains('@') {
        return Err(bad("userinfo in authority"));
    }
    if s.contains(['[', ']']) {
        return Err(bad("IPv6 literal"));
    }
    let (host, port) = match s.rsplit_once(':') {
        Some((host, port)) => (host, port_number(port)?),
        None => (s, default.ok_or(bad("port required"))?),
    };
    if host.is_empty() || host.contains(':') {
        return Err(bad("host"));
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn port_number(s: &str) -> Result<u16, HeadError> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HeadError::Malformed("port"));
    }
    s.parse().map_err(|_| HeadError::Malformed("port"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<Head, HeadError> {
        read_head(&mut raw.as_bytes(), &HeadLimits::default())
    }

    fn refusal(raw: &str) -> String {
        match parse(raw) {
            Ok(h) => panic!("accepted {raw:?} as {h:?}"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn connect_with_headers() {
        let h = parse("CONNECT API.anthropic.com:443 HTTP/1.1\r\nHost: x\r\nUser-Agent: c\r\n\r\n")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            (h.host.as_str(), h.port, &h.kind),
            ("api.anthropic.com", 443, &Kind::Connect)
        );
        assert_eq!(h.upstream(), None);
    }

    #[test]
    fn absolute_http_is_rewritten_with_our_host_and_without_proxy_headers() {
        let raw = "GET http://Example.com/a/b?c=1#frag HTTP/1.1\r\nHost: evil.test\r\n\
                   Proxy-Authorization: Basic eDp5\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
        let h = parse(raw).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((h.host.as_str(), h.port), ("example.com", 80));
        assert_eq!(
            h.upstream().as_deref(),
            Some("GET /a/b?c=1 HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n")
        );
        let h = parse("HEAD http://x.com:443?q HTTP/1.0\n\n").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            h.upstream().as_deref(),
            Some("HEAD /?q HTTP/1.0\r\nHost: x.com:443\r\n\r\n")
        );
    }

    #[test]
    fn leaves_bytes_after_the_head_buffered() {
        let mut r = std::io::BufReader::new("CONNECT a.com:443 HTTP/1.1\r\n\r\nTLS".as_bytes());
        assert!(read_head(&mut r, &HeadLimits::default()).is_ok());
        assert_eq!(r.buffer(), b"TLS");
    }

    #[test]
    fn hostile_request_lines() {
        for raw in [
            "CONNECT a.com:443\r\n\r\n",
            "CONNECT  a.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT a.com:443 HTTP/1.1 x\r\n\r\n",
            "CONNECT a.com:443 HTTP/2\r\n\r\n",
            "connect a.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT a.com HTTP/1.1\r\n\r\n",
            "CONNECT a.com: HTTP/1.1\r\n\r\n",
            "CONNECT a.com:99999 HTTP/1.1\r\n\r\n",
            "CONNECT a.com:+443 HTTP/1.1\r\n\r\n",
            "CONNECT :443 HTTP/1.1\r\n\r\n",
            "CONNECT user:pw@a.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT [::1]:443 HTTP/1.1\r\n\r\n",
            "CONNECT ::1:443 HTTP/1.1\r\n\r\n",
            "GET /path HTTP/1.1\r\n\r\n",
            "GET https://a.com/ HTTP/1.1\r\n\r\n",
            "GET http://u@a.com/ HTTP/1.1\r\n\r\n",
            "GET http://[fe80::1]/ HTTP/1.1\r\n\r\n",
            "CONNECT a.com:443 HTTP/1.1\r\nX-A: b\rc\r\n\r\n",
            "CONNECT a.com:443 HTTP/1.1\r\n folded\r\n\r\n",
            "CONNECT a.com:443 HTTP/1.1\r\nno colon\r\n\r\n",
            "CONNECT a.com:443 HTTP/1.1\r\nX-A: b",
            "CONNECT a.com:443 HTTP/1.1\r\n",
        ] {
            assert!(refusal(raw).starts_with("bad request"), "{raw:?}");
        }
        assert!(matches!(parse(""), Err(HeadError::Closed)));
    }

    #[test]
    fn oversized_heads_are_refused() {
        let long_line = format!("CONNECT {}.com:443 HTTP/1.1\r\n\r\n", "a".repeat(5000));
        assert!(matches!(parse(&long_line), Err(HeadError::LineTooLong)));
        let pad = format!("X-Pad: {}\r\n", "a".repeat(1000));
        let big = format!("CONNECT a.com:443 HTTP/1.1\r\n{}\r\n", pad.repeat(10));
        assert!(matches!(parse(&big), Err(HeadError::TooLarge)));
        let many = format!(
            "CONNECT a.com:443 HTTP/1.1\r\n{}\r\n",
            "A: b\r\n".repeat(65)
        );
        assert!(matches!(parse(&many), Err(HeadError::TooLarge)));
        assert_eq!(HeadError::TooLarge.status(), Some(431));
    }
}
