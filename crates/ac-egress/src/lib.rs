//! The agents' egress proxy (ADR-007): an HTTP forward proxy that lets an agent reach only
//! allowlisted hostnames on ports 443 and 80, via `CONNECT` or a plain absolute-URI request.
//!
//! Every agent gets the base list. An agent that presents its proxy credential
//! (`Proxy-Authorization: Basic <agent>:<secret>`, ADR-016) also gets the profiles the operator
//! attached to it; a credential that does not match is refused with 407, never served the base
//! list.
//!
//! The decision ([`policy`]), credential parsing ([`auth`]) and head parsing ([`head`]) are
//! sans-I/O; [`allowfile`] and [`egressfile`] read the root-owned files. [`proxy`] does the
//! blocking I/O, one thread per connection direction. Names are resolved on the host, every
//! answer must be a public address, and the proxy connects only to an address it vetted.
//! The firewall, not this proxy's environment variables, is what makes it the only way out.

pub mod allowfile;
pub mod auth;
pub mod egressfile;
pub mod head;
pub mod policy;
pub mod proxy;

pub use policy::{
    decide, is_public, judge, vet, Caller, Decision, Policy, Reason, Refused, Verdict,
};
pub use proxy::{Limits, Proxy, System, Upstream};
