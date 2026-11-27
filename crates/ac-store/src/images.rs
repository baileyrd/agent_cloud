//! [`Images`] as a plain-text file beside the agent list copy (`<data>/state/images`), in the
//! format of [`ac_domain::image::render`]. Rewritten whole on every change, atomically (temp file,
//! fsync, rename, mode 0600). Its writers are the process holding the data directory's lock (the
//! gateway, or `ac-ctl` while the gateway is stopped), so a read-modify-write cannot interleave
//! with another process's.

use std::path::{Path, PathBuf};

use ac_domain::image::{self, Images as Record};
use ac_domain::{AgentId, ImageRef};
use ac_ports::{Images, PortError};

use crate::err;
use crate::mirror::make_private_dir;

/// Where the record lives, under the data directory.
pub fn path(data: &Path) -> PathBuf {
    data.join("state").join("images")
}

/// The image record file.
pub struct ImageFile {
    path: PathBuf,
}

impl ImageFile {
    /// The record at `path`; a missing file is an empty record. Its directory is made (0700) on
    /// the first write.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read(&self) -> Result<Record, PortError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Record::new()),
            Err(e) => return Err(err("read images", format!("{}: {e}", self.path.display()))),
        };
        image::parse(&text).map_err(|e| err("read images", format!("{}: {e}", self.path.display())))
    }

    fn write(&self, all: &Record) -> Result<(), PortError> {
        if let Some(dir) = self.path.parent() {
            make_private_dir(dir).map_err(|e| err("write images", e.cause))?;
        }
        rusty_atomic_file::write_private(&self.path, image::render(all).as_bytes())
            .map_err(|e| err("write images", format!("{}: {e}", self.path.display())))
    }
}

impl Images for ImageFile {
    fn image(&self, id: &AgentId) -> Result<Option<ImageRef>, PortError> {
        Ok(self.read()?.remove(id.as_str()))
    }

    fn set_image(&self, id: &AgentId, image: Option<&ImageRef>) -> Result<(), PortError> {
        let mut all = self.read()?;
        let before = all.get(id.as_str()).cloned();
        if before.as_ref() == image {
            return Ok(());
        }
        match image {
            Some(i) => all.insert(id.to_string(), i.clone()),
            None => all.remove(id.as_str()),
        };
        self.write(&all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::tmp;

    fn id(s: &str) -> AgentId {
        AgentId::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn img(s: &str) -> ImageRef {
        ImageRef::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn missing_file_is_empty_and_records_survive_a_new_handle() {
        let dir = tmp();
        let file = ImageFile::new(path(&dir));
        assert_eq!(file.image(&id("d1")).ok(), Some(None));
        file.set_image(&id("d1"), Some(&img("agent-desktop:latest")))
            .unwrap_or_else(|e| panic!("{e}"));
        file.set_image(&id("a1"), Some(&img("agent-shell:latest")))
            .unwrap_or_else(|e| panic!("{e}"));
        let again = ImageFile::new(path(&dir));
        assert_eq!(
            again.image(&id("d1")).ok().flatten(),
            Some(img("agent-desktop:latest"))
        );
        let text = std::fs::read_to_string(path(&dir)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(text, "a1 agent-shell:latest\nd1 agent-desktop:latest\n");
        again
            .set_image(&id("d1"), None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(file.image(&id("d1")).ok(), Some(None));
        assert_eq!(
            file.image(&id("a1")).ok().flatten(),
            Some(img("agent-shell:latest"))
        );
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_an_empty_record() {
        let dir = tmp();
        let p = path(&dir);
        std::fs::create_dir_all(p.parent().unwrap_or(&dir)).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&p, "garbage\n").unwrap_or_else(|e| panic!("{e}"));
        let file = ImageFile::new(p);
        assert!(file.image(&id("d1")).is_err());
        assert!(file.set_image(&id("d1"), None).is_err());
    }
}
