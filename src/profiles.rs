use std::path::PathBuf;

use crate::types::BorgResult;
use anyhow::Context;
use borgbackup::common::CreateOptions;
use dirs;

use serde::{Deserialize, Serialize};
use serde_json;

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Repository {
    path: String,
}

impl Repository {
    pub fn new(path: String) -> Self {
        Self { path }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Profile {
    name: String,
    backup_paths: Vec<String>,
    // TODO: A proper field for this
    repos: Vec<Repository>,
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Profile<{}>", self.name)
    }
}

impl Profile {
    pub(crate) const DEFAULT_PROFILE_NAME: &'static str = "default";
    pub(crate) fn try_open_profile_or_create_default(profile: &Option<String>) -> BorgResult<Self> {
        match profile {
            Some(profile_name) => Profile::open_profile(&profile_name)
                .with_context(|| format!("Failed to open profile {}", profile_name))?
                .ok_or_else(|| anyhow::anyhow!("Profile {} does not exist", profile_name)),
            None => Profile::open_or_create_default_profile(),
        }
    }

    fn blank(name: &str) -> Self {
        Self {
            name: name.to_string(),
            backup_paths: vec![],
            repos: vec![],
        }
    }

    pub(crate) fn open_or_create_default_profile() -> BorgResult<Self> {
        if let Some(profile) = Self::open_profile(Self::DEFAULT_PROFILE_NAME)? {
            return Ok(profile);
        } else {
            let profile = Self::blank(Self::DEFAULT_PROFILE_NAME);
            profile.save_profile()?;
            return Ok(profile);
        }
    }

    pub(crate) fn open_profile(name: &str) -> BorgResult<Option<Self>> {
        let blank = Self::blank(name);
        // TODO: This is a bit of a hack; make this less janky lol
        let profile_path = blank.profile_path()?;
        if !profile_path.exists() {
            return Ok(None);
        }
        let profile = std::fs::read_to_string(profile_path)
            .with_context(|| format!("Failed to read profile {}", name))?;
        serde_json::from_str(&profile)
            .with_context(|| format!("Failed to deserialize profile {}", name))
            .map(Some)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn serialize(&self) -> BorgResult<String> {
        serde_json::to_string_pretty(self)
            .with_context(|| format!("Failed to serialize profile {}", self.name()))
    }

    pub(crate) fn borg_create_options(
        &self,
        archive_name: String,
    ) -> BorgResult<Vec<CreateOptions>> {
        if self.repos.is_empty() {
            return Err(anyhow::anyhow!(
                "No repositories configured for profile {}",
                self.name
            ));
        }
        Ok(self
            .repos
            .iter()
            .map(|repo| {
                CreateOptions::new(
                    repo.path.clone(),
                    archive_name.clone(),
                    self.backup_paths.clone(),
                    vec![],
                )
            })
            .collect())
    }

    pub(crate) fn profile_path(&self) -> BorgResult<PathBuf> {
        let mut path = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Failed to get config directory. Is $HOME set?"))?;
        path.push("borgtui");
        path.push("profiles");
        path.push(&self.name);
        path.set_extension("json");
        Ok(path)
    }

    pub(crate) fn save_profile(&self) -> BorgResult<()> {
        let profile_path = self.profile_path()?;
        if let Some(parent) = profile_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create parent directory for profile {}",
                    self.name
                )
            })?
        }
        let profile = self.serialize()?;
        std::fs::write(profile_path, profile)
            .with_context(|| format!("Failed to write profile {}", self.name))
    }

    pub(crate) fn add_backup_path(&mut self, path: String) -> BorgResult<()> {
        // TODO: Handle duplicates
        std::fs::metadata(&path).with_context(|| {
            format!(
                "Failed to get metadata for path {} when adding to profile {}",
                path, self.name
            )
        })?;
        self.backup_paths.push(path);
        Ok(())
    }

    pub(crate) fn add_repository(&mut self, path: String) -> BorgResult<()> {
        // TODO: Can we handle errors here?
        self.repos.push(Repository::new(path));
        Ok(())
    }

    pub(crate) fn remove_backup_path(&mut self, path: &str) {
        // TODO: Handle trailing slashes
        self.backup_paths.retain(|p| p != path);
    }
}
