//! The agents' egress proxy (ADR-007): an HTTP forward proxy that lets an agent reach only
//! allowlisted hostnames on ports 443 and 80, via `CONNECT` or a plain absolute-URI request.
//!
//! The decision ([`policy`]) and head parsing ([`head`]) are sans-I/O. [`proxy`] does the
//! blocking I/O, one thread per connection direction. Names are resolved on the host, every
//! answer must be a public address, and the proxy connects only to an address it vetted.
//! The firewall, not this proxy's environment variables, is what makes it the only way out.

pub mod allowfile;
pub mod auth;
pub mod head;
pub mod policy;
pub mod proxy;

pub use policy::{decide, is_public, vet, Decision, Reason};
pub use proxy::{Limits, Proxy, System, Upstream};
