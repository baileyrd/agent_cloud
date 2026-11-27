//! Which image each agent boots from (card #1627). An agent's image is recorded when it is
//! created and reused by every resume and fork, so a desktop agent stays a desktop agent. An
//! agent with no record boots on the host's default image. The record is a plain-text file, one
//! `<agent-id> <image>` line per agent, sorted by id; these rules read and write it.

use std::collections::BTreeMap;
use std::fmt;

use crate::AgentId;

/// Most bytes in an image reference.
pub const MAX_LEN: usize = 255;

/// An image reference (`docker.io/library/agent-desktop:latest`): safe as one `ctr` argument and
/// one word of the record file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageRef(String);

/// A string that is not a valid image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadImage(pub String);

impl fmt::Display for BadImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "image {:?}: want 1 to {MAX_LEN} of [A-Za-z0-9._/:@-], not starting with -",
            self.0.chars().take(40).collect::<String>()
        )
    }
}

impl std::error::Error for BadImage {}

impl ImageRef {
    /// Parse and validate external input.
    pub fn parse(s: &str) -> Result<Self, BadImage> {
        let ok_char = |c: char| c.is_ascii_alphanumeric() || "._/:@-".contains(c);
        let ok =
            !s.is_empty() && s.len() <= MAX_LEN && !s.starts_with('-') && s.chars().all(ok_char);
        match ok {
            true => Ok(Self(s.to_owned())),
            false => Err(BadImage(s.to_owned())),
        }
    }

    /// The validated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every agent's recorded image.
pub type Images = BTreeMap<String, ImageRef>;

/// A line of the record file that is not `<agent-id> <image>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadImageLine(pub usize);

impl fmt::Display for BadImageLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "image record line {} is not `<agent-id> <image>`",
            self.0
        )
    }
}

impl std::error::Error for BadImageLine {}

/// The record file: one `<agent-id> <image>` line per agent, sorted by id.
pub fn render(images: &Images) -> String {
    images
        .iter()
        .map(|(id, image)| format!("{id} {image}\n"))
        .collect()
}

/// Read a record file written by [`render`]; blank lines are skipped.
pub fn parse(text: &str) -> Result<Images, BadImageLine> {
    let mut out = Images::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = || BadImageLine(i + 1);
        let (id, image) = line.split_once(' ').ok_or_else(bad)?;
        let id = AgentId::parse(id).map_err(|_| bad())?;
        out.insert(id.to_string(), ImageRef::parse(image).map_err(|_| bad())?);
    }
    Ok(out)
}

/// The operator's allowlist of images a create may name (`AC_IMAGES`): comma-separated
/// `<name>=<image>` pairs, each name following the agent-id rule. Empty text is an empty list.
pub fn parse_allowlist(text: &str) -> Result<BTreeMap<String, ImageRef>, String> {
    let mut out = BTreeMap::new();
    for pair in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, image) = pair
            .split_once('=')
            .ok_or_else(|| format!("{pair:?} is not <name>=<image>"))?;
        crate::id::check(name).map_err(|e| format!("image name {name:?}: {e}"))?;
        let image = ImageRef::parse(image).map_err(|e| e.to_string())?;
        if out.insert(name.to_owned(), image).is_some() {
            return Err(format!("image name {name:?} is listed twice"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(s: &str) -> ImageRef {
        ImageRef::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn image_refs_are_checked() {
        for ok in [
            "agent-desktop:latest",
            "docker.io/library/a:1",
            "a@sha256:ab12",
        ] {
            assert_eq!(img(ok).as_str(), ok);
        }
        let long = "a".repeat(MAX_LEN + 1);
        for bad in ["", "-a", "a b", "a\nb", "a;b", long.as_str()] {
            assert!(ImageRef::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn record_round_trips_sorted() {
        let mut all = Images::new();
        all.insert("d1".into(), img("agent-desktop:latest"));
        all.insert("a1".into(), img("agent-shell:latest"));
        let text = render(&all);
        assert_eq!(text, "a1 agent-shell:latest\nd1 agent-desktop:latest\n");
        assert_eq!(parse(&text), Ok(all));
        assert_eq!(parse("\n"), Ok(Images::new()));
    }

    #[test]
    fn bad_record_lines_are_named() {
        assert_eq!(parse("a1 x\nnospace\n"), Err(BadImageLine(2)));
        assert_eq!(parse("A1 x\n"), Err(BadImageLine(1)));
        assert_eq!(parse("a1 x y\n"), Err(BadImageLine(1)));
    }

    #[test]
    fn allowlist_parses_and_refuses() {
        let list = parse_allowlist(" desktop=agent-desktop:latest , shell=agent-shell:1")
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(list.get("desktop"), Some(&img("agent-desktop:latest")));
        assert_eq!(list.len(), 2);
        assert_eq!(parse_allowlist(""), Ok(BTreeMap::new()));
        for bad in ["desktop", "Desk=a", "d=a b", "d=a,d=b"] {
            assert!(parse_allowlist(bad).is_err(), "{bad:?}");
        }
    }
}
