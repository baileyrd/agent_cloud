//! The agent's proxy credential (ADR-016), sans I/O: parsing `Proxy-Authorization`. Every value
//! here is hostile input; anything but exactly one well-formed `Basic` credential for a valid
//! agent id and a 64-hex secret is [`Presented::Malformed`], which the proxy answers 407.

use std::fmt;

use ac_domain::AgentId;

/// Longest header value accepted: `Basic ` and base64 of a 32-byte id, `:` and 64 hex digits
/// fit in 140.
pub const MAX_VALUE: usize = 256;

/// A header value or secret that must never be printed. `Debug` shows only its length.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap `s`.
    pub fn new(s: String) -> Self {
        Self(s)
    }

    /// The value, for comparing and hashing only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

/// What the client presented.
#[derive(Debug, PartialEq, Eq)]
pub enum Presented {
    /// No `Proxy-Authorization`: an agent without a credential (base list only).
    None,
    /// A well-formed credential, not yet checked against the agents file.
    Basic {
        /// The agent it claims to be.
        agent: AgentId,
        /// The secret (64 lowercase hex digits).
        secret: Secret,
    },
    /// Present but unusable; why, for the log.
    Malformed(&'static str),
}

/// Parse every `Proxy-Authorization` value of one request head.
pub fn parse(values: &[Secret]) -> Presented {
    let bad = Presented::Malformed;
    let value = match values {
        [] => return Presented::None,
        [one] => one.expose(),
        _ => return bad("more than one Proxy-Authorization header"),
    };
    if value.len() > MAX_VALUE {
        return bad("Proxy-Authorization too long");
    }
    let value = value.trim_matches([' ', '\t']);
    let Some((scheme, token)) = value.split_once(' ') else {
        return bad("Proxy-Authorization: want Basic <base64>");
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return bad("Proxy-Authorization: scheme is not Basic");
    }
    let Ok(raw) = rusty_base64::decode_standard(token) else {
        return bad("Proxy-Authorization: bad base64");
    };
    let Ok(text) = String::from_utf8(raw) else {
        return bad("Proxy-Authorization: not UTF-8");
    };
    let Some((user, pass)) = text.split_once(':') else {
        return bad("Proxy-Authorization: no colon");
    };
    let Ok(agent) = AgentId::parse(user) else {
        return bad("Proxy-Authorization: user is not an agent id");
    };
    if pass.len() != 64 || !pass.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return bad("Proxy-Authorization: secret is not 64 hex digits");
    }
    Presented::Basic {
        agent,
        secret: Secret(pass.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn basic(raw: &str) -> Secret {
        Secret(format!(
            "Basic {}",
            rusty_base64::encode_standard(raw.as_bytes())
        ))
    }

    fn reason(values: &[Secret]) -> &'static str {
        match parse(values) {
            Presented::Malformed(why) => why,
            other => panic!("accepted {values:?} as {other:?}"),
        }
    }

    #[test]
    fn none_and_a_good_credential() {
        assert_eq!(parse(&[]), Presented::None);
        let good = parse(&[basic(&format!("a1:{SECRET}"))]);
        assert!(
            matches!(&good, Presented::Basic { agent, secret }
                if agent.as_str() == "a1" && secret.expose() == SECRET),
            "{good:?}"
        );
        // Scheme case and optional whitespace around the value (OWS) are fine.
        let token = rusty_base64::encode_standard(format!("a1:{SECRET}").as_bytes());
        for v in [format!("basic {token}"), format!(" \tBASIC {token} ")] {
            assert!(matches!(parse(&[Secret(v)]), Presented::Basic { .. }));
        }
    }

    #[test]
    fn debug_never_shows_the_secret() {
        let shown = format!("{:?}", parse(&[basic(&format!("a1:{SECRET}"))]));
        assert!(
            !shown.contains(SECRET) && shown.contains("64 bytes"),
            "{shown}"
        );
        assert!(!format!("{:?}", basic("a1:x")).contains("YTE6"));
    }

    #[test]
    fn hostile_values() {
        let token = rusty_base64::encode_standard(format!("a1:{SECRET}").as_bytes());
        let good = basic(&format!("a1:{SECRET}"));
        let cases: Vec<(Vec<Secret>, &str)> = vec![
            (vec![good.clone(), good.clone()], "more than one"),
            (
                vec![Secret(format!("Basic {}", "A".repeat(300)))],
                "too long",
            ),
            (vec![Secret("Basic".into())], "want Basic"),
            (vec![Secret(format!("Bearer {token}"))], "not Basic"),
            (vec![Secret(format!("Basic  {token}"))], "bad base64"),
            (vec![Secret(format!("Basic\t{token}"))], "want Basic"),
            (vec![Secret(format!("Basic {token} x"))], "bad base64"),
            (vec![Secret("Basic !!!!".into())], "bad base64"),
            (
                vec![Secret(format!("Basic {}={}", &token[..8], &token[8..]))],
                "bad base64",
            ),
            (vec![Secret(format!("Basic {token}=="))], "bad base64"),
            (
                vec![Secret(format!(
                    "Basic {}",
                    rusty_base64::encode_standard(b"\xff\xfe:x")
                ))],
                "not UTF-8",
            ),
            (vec![basic(&format!("a1{SECRET}"))], "no colon"),
            (vec![basic(&format!(":{SECRET}"))], "not an agent id"),
            (vec![basic(&format!("A1:{SECRET}"))], "not an agent id"),
            (vec![basic(&format!("a1\0:{SECRET}"))], "not an agent id"),
            (vec![basic(&format!("a1 :{SECRET}"))], "not an agent id"),
            (vec![basic(&format!("../a1:{SECRET}"))], "not an agent id"),
            (
                vec![basic(&format!("{}:{SECRET}", "a".repeat(33)))],
                "not an agent id",
            ),
            (vec![basic("a1:")], "not 64 hex"),
            (
                vec![basic(&format!("a1:{}", SECRET.to_uppercase()))],
                "not 64 hex",
            ),
            (vec![basic(&format!("a1:{SECRET}\0"))], "not 64 hex"),
            (vec![basic(&format!("a1:{SECRET} "))], "not 64 hex"),
            (vec![basic(&format!("a1:{SECRET}:x"))], "not 64 hex"),
            (vec![basic(&format!("a1:{}", &SECRET[1..]))], "not 64 hex"),
        ];
        for (values, want) in cases {
            let got = reason(&values);
            assert!(got.contains(want), "{values:?}: {got}");
        }
    }
}
