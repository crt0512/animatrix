use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;

use crate::model::AppConfig;

#[derive(Clone, Debug)]
pub struct ConfigStore {
    config_path: PathBuf,
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        let project = ProjectDirs::from("net", "512mb", "animatrix")
            .context("could not determine the XDG config directory")?;
        Ok(Self {
            config_path: project.config_dir().join("config.json"),
        })
    }

    #[cfg(test)]
    pub fn with_root(root: &std::path::Path) -> Self {
        Self {
            config_path: root.join("config.json"),
        }
    }

    pub fn load(&self) -> Result<AppConfig> {
        if !self.config_path.exists() {
            return Ok(AppConfig::default());
        }
        let data = fs::read(&self.config_path)
            .with_context(|| format!("failed to read {}", self.config_path.display()))?;
        serde_json::from_slice(&data)
            .with_context(|| format!("failed to parse {}", self.config_path.display()))
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        let parent = self.config_path.parent().context("configuration path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let temporary = self.config_path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(config)?)
            .with_context(|| format!("failed to write {}", temporary.display()))?;
        fs::rename(&temporary, &self.config_path)
            .with_context(|| format!("failed to replace {}", self.config_path.display()))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use super::*;

    #[test]
    fn config_round_trip_is_lossless() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::with_root(directory.path());
        let expected = AppConfig::default();
        store.save(&expected).unwrap();
        assert_eq!(store.load().unwrap(), expected);
    }

    #[test]
    fn missing_config_uses_defaults() {
        let directory = tempdir().unwrap();
        let store = ConfigStore::with_root(directory.path());
        let config = store.load().unwrap();
        assert!(config.enabled);
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.active_profile.as_deref(), Some(config.profiles[0].id.as_str()));
    }
}
