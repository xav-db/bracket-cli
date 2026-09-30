//! What bracket remembers between runs, kept in its config directory
//! (`~/Library/Application Support/bracket` on macOS, `~/.config/bracket` on Linux).

use std::{fs, io, path::PathBuf};

const LAST_REGION: &str = "last-region";

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// A store under the platform's config directory, if it has one.
    pub fn from_config_dir() -> Option<Self> {
        dirs::config_dir().map(|dir| Store::at(dir.join("bracket")))
    }

    pub fn at(dir: PathBuf) -> Self {
        Store { dir }
    }

    /// The region `bracket list ec2` showed last. A missing or blank file means none yet.
    pub fn last_region(&self) -> Option<String> {
        fs::read_to_string(self.dir.join(LAST_REGION))
            .ok()
            .map(|region| region.trim().to_string())
            .filter(|region| !region.is_empty())
    }

    pub fn set_last_region(&self, region: &str) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::write(self.dir.join(LAST_REGION), region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_the_last_region() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("nested").join("bracket"));
        assert_eq!(store.last_region(), None);

        store.set_last_region("eu-west-2").unwrap();
        assert_eq!(store.last_region().as_deref(), Some("eu-west-2"));

        store.set_last_region("us-east-2").unwrap();
        assert_eq!(store.last_region().as_deref(), Some("us-east-2"));
    }

    #[test]
    fn blank_files_mean_no_region() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LAST_REGION), " \n").unwrap();
        assert_eq!(Store::at(dir.path().to_path_buf()).last_region(), None);
    }
}
