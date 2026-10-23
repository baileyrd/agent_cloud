//! Settings derived from the host (ADR-014): what a host is ([`facts`]), what it should be set to
//! for a workload profile ([`rules`]), and the line edits that set it ([`edit`]). Pure; the
//! collector and `ac-ctl doctor`/`tune` are the I/O around it.

pub mod edit;
pub mod facts;
pub mod rules;

pub use facts::{Fact, HostFacts};
pub use rules::{recommend, Confidence, Finding, Profile, Status};

#[cfg(test)]
mod tests;
