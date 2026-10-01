//! Login autostart through an XDG autostart entry.
//!
//! `animatrix.service` hangs off `graphical-session.target`, which only
//! systemd-managed sessions (GNOME, Plasma) start; Cinnamon, XFCE and MATE
//! never do. They all run `~/.config/autostart`, so the entry starts the
//! service from there. Where the target does start, starting the running
//! service again does nothing. Without the unit (a source build) the entry
//! runs the binary itself.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;

const FILE_NAME: &str = "animatrix.desktop";

const ENTRY: &str = "[Desktop Entry]
Type=Application
Name=Animatrix
Comment=Start Animatrix at login
Exec=sh -c \"systemctl --user start animatrix.service || exec animatrix --minimized\"
Icon=net._512mb.Animatrix
Terminal=false
NoDisplay=true
X-GNOME-Autostart-enabled=true
";

#[derive(Clone, Debug)]
pub struct Autostart {
    path: PathBuf,
}

impl Autostart {
    pub fn discover() -> Result<Self> {
        let base = BaseDirs::new().context("could not determine the XDG config directory")?;
        Ok(Self::in_dir(&base.config_dir().join("autostart")))
    }

    pub fn in_dir(dir: &Path) -> Self {
        Self { path: dir.join(FILE_NAME) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_installed(&self) -> bool {
        self.path.is_file()
    }

    /// Writes the entry unless it is already there unchanged.
    pub fn ensure(&self) -> Result<()> {
        if fs::read_to_string(&self.path).is_ok_and(|current| current == ENTRY) {
            return Ok(());
        }
        self.install()
    }

    pub fn install(&self) -> Result<()> {
        let parent = self.path.parent().context("autostart path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::write(&self.path, ENTRY)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    pub fn uninstall(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("failed to remove {}", self.path.display()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use super::*;

    #[test]
    fn install_ensure_uninstall() {
        let dir = tempdir().unwrap();
        let autostart = Autostart::in_dir(&dir.path().join("autostart"));
        assert!(!autostart.is_installed());
        autostart.ensure().unwrap();
        assert!(autostart.is_installed());
        fs::write(autostart.path(), "stale").unwrap();
        autostart.ensure().unwrap();
        assert_eq!(fs::read_to_string(autostart.path()).unwrap(), ENTRY);
        autostart.uninstall().unwrap();
        assert!(!autostart.is_installed());
        autostart.uninstall().unwrap();
    }
}
