use std::path::PathBuf;

use crate::types::BorgResult;
use anyhow::Context;
use borgbackup::common::CreateOptions;
use keyring::Entry;

use serde::{Deserialize, Serialize};

// TODO: This debug impl is a security concern.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Encryption {
    None,
    Raw(String),
    Keyring,
}
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Repository {
    path: String,
    encryption: Encryption,
}

fn get_keyring_entry(repo_path: &str) -> BorgResult<Entry> {
    Entry::new("borgtui", repo_path).with_context(|| {
        format!(
            "Failed to create keyring entry for repository {}",
            repo_path
        )
    })
}

impl Repository {
    pub(crate) fn new(path: String, encryption: Encryption) -> Self {
        Self { path, encryption }
    }

    pub(crate) fn get_passphrase(&self) -> BorgResult<Option<String>> {
        match &self.encryption {
            Encryption::None => Ok(None),
            Encryption::Raw(passphrase) => Ok(Some(passphrase.clone())),
            Encryption::Keyring => get_keyring_entry(&self.path)?
                .get_password()
                .map_err(|e| anyhow::anyhow!("Failed to get passphrase from keyring: {}", e))
                .map(Some),
        }
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
            Some(profile_name) => Profile::open_profile(profile_name)
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
            Ok(profile)
        } else {
            let profile = Self::blank(Self::DEFAULT_PROFILE_NAME);
            profile.save_profile()?;
            Ok(profile)
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
        let mut create_options_list = Vec::new();
        for repo in &self.repos {
            let mut create_options = CreateOptions::new(
                repo.path.clone(),
                archive_name.clone(),
                self.backup_paths.clone(),
                vec![],
            );
            create_options.passphrase = repo.get_passphrase()?;
            create_options_list.push(create_options);
        }
        Ok(create_options_list)
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
                "Failed to get metadata for path {} when adding to profile {}. Does the path exist?",
                path, self.name
            )
        })?;
        self.backup_paths.push(path);
        Ok(())
    }

    pub(crate) fn add_repository(
        &mut self,
        path: String,
        borg_passphrase: Option<String>,
        store_passphase_in_cleartext: bool,
    ) -> BorgResult<()> {
        let encryption = match borg_passphrase {
            Some(borg_passphrase) => {
                // TODO: Refactor this into a separate function
                if store_passphase_in_cleartext {
                    Encryption::Raw(borg_passphrase)
                } else {
                    let entry = get_keyring_entry(&path)?;
                    entry.set_password(&borg_passphrase).with_context(|| {
                        format!(
                            "Failed to set password for repository {} in profile {}",
                            path, &self
                        )
                    })?;
                    assert!(entry.get_password().is_ok());
                    Encryption::Keyring
                }
            }
            None => Encryption::None,
        };
        self.repos.push(Repository::new(path, encryption));
        Ok(())
    }

    pub(crate) fn remove_backup_path(&mut self, path: &str) {
        // TODO: Handle trailing slashes
        self.backup_paths.retain(|p| p != path);
    }
}
