use std::{path::PathBuf, sync::Arc};

// use tokio::fs::meta
use crate::types::BorgResult;
use anyhow::Context;
use borgbackup::common::CreateOptions;
use keyring::Entry;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

// TODO: This debug impl is a security concern.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum Encryption {
    None,
    Raw(String),
    Keyring,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Repository {
    pub(crate) path: String,
    encryption: Encryption,
    #[serde(skip)]
    pub(crate) lock: Arc<Mutex<()>>,
}

impl std::fmt::Display for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Repository<{}>", self.path)
    }
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
        Self {
            path,
            encryption,
            lock: Default::default(),
        }
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

    pub(crate) fn get_path(&self) -> String {
        self.path.clone()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
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
    pub(crate) async fn try_open_profile_or_create_default(
        profile: &Option<String>,
    ) -> BorgResult<Self> {
        match profile {
            Some(profile_name) => Profile::open_profile(profile_name)
                .await
                .with_context(|| format!("Failed to open profile {}", profile_name))?
                .ok_or_else(|| anyhow::anyhow!("Profile {} does not exist", profile_name)),
            None => Profile::open_or_create_default_profile().await,
        }
    }

    fn blank(name: &str) -> Self {
        Self {
            name: name.to_string(),
            backup_paths: vec![],
            repos: vec![],
        }
    }

    pub(crate) async fn open_or_create_default_profile() -> BorgResult<Self> {
        if let Some(profile) = Self::open_profile(Self::DEFAULT_PROFILE_NAME).await? {
            Ok(profile)
        } else {
            let profile = Self::blank(Self::DEFAULT_PROFILE_NAME);
            profile.save_profile().await?;
            Ok(profile)
        }
    }

    pub(crate) async fn open_profile(name: &str) -> BorgResult<Option<Self>> {
        let blank = Self::blank(name);
        // TODO: This is a bit of a hack; make this less janky lol
        let profile_path = blank.profile_path()?;
        if !profile_path.exists() {
            return Ok(None);
        }
        let profile = tokio::fs::read_to_string(profile_path)
            .await
            .with_context(|| format!("Failed to read profile {}", name))?;
        serde_json::from_str(&profile)
            .with_context(|| format!("Failed to deserialize profile {}", name))
            .map(Some)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn repos(&self) -> &[Repository] {
        &self.repos
    }

    pub(crate) fn num_repos(&self) -> usize {
        self.repos.len()
    }

    pub(crate) fn repositories(&self) -> &[Repository] {
        &self.repos
    }

    pub(crate) fn serialize(&self) -> BorgResult<String> {
        serde_json::to_string_pretty(self)
            .with_context(|| format!("Failed to serialize profile {}", self.name()))
    }

    pub(crate) fn borg_create_options(
        &self,
        archive_name: String,
    ) -> BorgResult<Vec<(CreateOptions, Repository)>> {
        if self.repos.is_empty() {
            return Err(anyhow::anyhow!(
                "No repositories configured for profile {}",
                self.name
            ));
        }
        let mut create_options_list = Vec::new();
        for repo in self.repos.clone() {
            let mut create_options = CreateOptions::new(
                repo.path.clone(),
                archive_name.clone(),
                self.backup_paths.clone(),
                vec![],
            );
            create_options.passphrase = repo.get_passphrase()?;
            create_options_list.push((create_options, repo));
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

    pub(crate) async fn save_profile(&self) -> BorgResult<()> {
        let profile_path = self.profile_path()?;
        if let Some(parent) = profile_path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!(
                    "Failed to create parent directory for profile {}",
                    self.name
                )
            })?
        }
        let profile = self.serialize()?;
        tokio::fs::write(profile_path, profile)
            .await
            .with_context(|| format!("Failed to write profile {}", self.name))
    }

    pub(crate) fn has_repository(&self, path: &str) -> bool {
        // TODO: Handle trailing slashes or other weirdness
        self.repos.iter().any(|r| r.path == path)
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

    pub(crate) async fn add_backup_path(&mut self, path: String) -> BorgResult<()> {
        // TODO: Handle trailing slashes or other weirdness
        if self.backup_paths.contains(&path) {
            return Err(anyhow::anyhow!(
                "Path {} already exists in profile {}",
                path,
                self.name
            ));
        }
        tokio::fs::metadata(&path).await.with_context(|| {
            format!(
                "Failed to get metadata for path {} when adding to profile {}. Does the path exist?",
                path, self.name
            )
        })?;
        self.backup_paths.push(path);
        Ok(())
    }

    pub(crate) fn remove_backup_path(&mut self, path: &str) {
        // TODO: Handle trailing slashes or other weirdness
        self.backup_paths.retain(|p| p != path);
    }
}
