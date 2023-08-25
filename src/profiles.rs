use std::{
    num::NonZeroU16,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::types::BorgResult;
use anyhow::{bail, Context};
use borgbackup::common::{CommonOptions, CreateOptions, Pattern};
use keyring::Entry;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Passphrase(String);

impl Passphrase {
    pub(crate) fn inner(&self) -> String {
        self.0.clone()
    }
}

impl std::fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Passphrase<redacted>")
    }
}

impl AsRef<str> for Passphrase {
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for Passphrase {
    fn from(value: String) -> Self {
        Self(value)
    }
}

// TODO: This debug impl is a security concern.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum Encryption {
    None,
    Raw(Passphrase),
    Keyring,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Repository {
    pub(crate) path: String,
    /// SSH command to use when connecting
    #[serde(default)]
    pub(crate) rsh: Option<String>,
    encryption: Encryption,
    #[serde(default)]
    disabled: bool,
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
    pub(crate) fn new(path: String, encryption: Encryption, rsh: Option<String>) -> Self {
        Self {
            path,
            encryption,
            rsh,
            disabled: false,
            lock: Default::default(),
        }
    }

    pub(crate) fn get_passphrase(&self) -> BorgResult<Option<String>> {
        match &self.encryption {
            Encryption::None => Ok(None),
            Encryption::Raw(passphrase) => Ok(Some(passphrase.inner())),
            Encryption::Keyring => get_keyring_entry(&self.path)?
                .get_password()
                .map_err(|e| anyhow::anyhow!("Failed to get passphrase from keyring: {}", e))
                .map(Some),
        }
    }

    /// If true, the repo has been disabled and actions will
    /// not be performed on it
    pub(crate) fn disabled(&self) -> bool {
        self.disabled
    }

    pub(crate) fn get_path(&self) -> String {
        self.path.clone()
    }

    pub(crate) fn common_options(&self) -> CommonOptions {
        CommonOptions {
            rsh: self.rsh.clone(),
            ..Default::default()
        }
    }

    pub(crate) fn create_options(
        &self,
        archive_name: &str,
        backup_paths: &[String],
        excludes: &[String],
    ) -> BorgResult<CreateOptions> {
        let mut create_options = CreateOptions::new(
            self.path.clone(),
            archive_name.to_string(),
            backup_paths.to_vec(),
            vec![],
        );
        create_options.passphrase = self.get_passphrase()?;
        create_options.excludes = excludes.iter().cloned().map(Pattern::Shell).collect();
        Ok(create_options)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub(crate) struct PruneOptions {
    pub(crate) keep_daily: NonZeroU16,
    pub(crate) keep_weekly: NonZeroU16,
    pub(crate) keep_monthly: NonZeroU16,
    pub(crate) keep_yearly: NonZeroU16,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            keep_daily: NonZeroU16::new(4).unwrap(),
            keep_weekly: NonZeroU16::new(12).unwrap(),
            keep_monthly: NonZeroU16::new(12).unwrap(),
            keep_yearly: NonZeroU16::new(12).unwrap(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum ProfileOperation {
    AddBackupPath(PathBuf),
}

// Necessary for serde(default)
const fn default_action_timeout_seconds() -> u64 {
    30
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Profile {
    name: String,
    backup_paths: Vec<PathBuf>,
    #[serde(default)]
    exclude_patterns: Vec<String>,
    #[serde(default)]
    prune_options: PruneOptions,
    #[serde(default = "default_action_timeout_seconds")]
    action_timeout_seconds: u64,
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
            exclude_patterns: vec![],
            backup_paths: vec![],
            prune_options: Default::default(),
            repos: vec![],
            action_timeout_seconds: default_action_timeout_seconds(),
        }
    }

    pub(crate) async fn open_or_create_default_profile() -> BorgResult<Self> {
        if let Some(profile) = Self::open_profile(Self::DEFAULT_PROFILE_NAME).await? {
            Ok(profile)
        } else {
            Self::create_profile(Self::DEFAULT_PROFILE_NAME).await
        }
    }

    pub(crate) async fn create_profile(name: &str) -> BorgResult<Self> {
        let profile = Self::blank(name);
        profile.save_profile().await?;
        Ok(profile)
    }

    pub(crate) async fn open_profile(name: &str) -> BorgResult<Option<Self>> {
        let profile_path = Profile::profile_path_for_name(name)?;
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

    pub(crate) fn blocking_open_path<P: AsRef<Path>>(path: P) -> BorgResult<Self> {
        let profile = std::fs::read_to_string(path.as_ref()).with_context(|| {
            format!("Failed to read profile {}", path.as_ref().to_string_lossy())
        })?;
        serde_json::from_str(&profile).with_context(|| {
            format!(
                "Failed to deserialize profile {}",
                path.as_ref().to_string_lossy()
            )
        })
    }

    pub(crate) fn active_repositories(&self) -> impl Iterator<Item = &Repository> {
        self.repositories().iter().filter(|repo| !repo.disabled)
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn repositories(&self) -> &[Repository] {
        &self.repos
    }

    pub(crate) fn backup_paths(&self) -> &[PathBuf] {
        &self.backup_paths
    }

    pub(crate) fn num_repos(&self) -> usize {
        self.repos.len()
    }

    pub(crate) fn action_timeout_seconds(&self) -> u64 {
        self.action_timeout_seconds
    }

    pub(crate) fn prune_options(&self) -> PruneOptions {
        self.prune_options
    }

    pub(crate) fn exclude_patterns(&self) -> &[String] {
        &self.exclude_patterns
    }

    pub(crate) fn serialize(&self) -> BorgResult<String> {
        serde_json::to_string_pretty(self)
            .with_context(|| format!("Failed to serialize profile {}", self.name()))
    }

    pub(crate) async fn apply_operation(&mut self, op: ProfileOperation) -> BorgResult<()> {
        // This looks silly but I was intending to add more profile operations in the future :^)
        match op {
            ProfileOperation::AddBackupPath(path) => self.add_backup_path(path).await,
        }
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
        let backup_paths = self
            .backup_paths
            .iter()
            .map(|path| format!("'{}'", path.to_string_lossy()))
            .collect::<Vec<String>>();
        for repo in self.active_repositories() {
            match repo.create_options(&archive_name, &backup_paths, self.exclude_patterns()) {
                Ok(create_option) => create_options_list.push((create_option, repo.clone())),
                Err(e) => tracing::error!(
                    "Failed to make create options for {} in {}: {}",
                    self,
                    repo,
                    e
                ),
            };
        }
        Ok(create_options_list)
    }

    pub(crate) fn profile_path_for_name(name: &str) -> BorgResult<PathBuf> {
        let mut path = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("Failed to get config directory. Is $HOME set?"))?;
        path.push("borgtui");
        path.push("profiles");
        path.push(name);
        path.set_extension("json");
        Ok(path)
    }

    pub(crate) fn profile_path(&self) -> BorgResult<PathBuf> {
        Self::profile_path_for_name(&self.name)
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
        self.repos.iter().any(|r| r.path == path)
    }

    pub(crate) fn add_repository(
        &mut self,
        path: String,
        borg_passphrase: Option<String>,
        rsh: Option<String>,
        do_not_store_in_keyring: bool,
        store_passphase_in_cleartext: bool,
    ) -> BorgResult<()> {
        let encryption = match borg_passphrase {
            Some(borg_passphrase) => {
                // TODO: Refactor this into a separate function
                if store_passphase_in_cleartext {
                    Encryption::Raw(Passphrase(borg_passphrase))
                } else if !do_not_store_in_keyring {
                    let entry = get_keyring_entry(&path)?;
                    entry.set_password(&borg_passphrase).with_context(|| {
                        format!(
                            "Failed to set password for repository {} in profile {}",
                            path, &self
                        )
                    })?;
                    assert!(entry.get_password().is_ok());
                    Encryption::Keyring
                } else {
                    Encryption::None
                }
            }
            None => Encryption::None,
        };
        self.repos.push(Repository::new(path, encryption, rsh));
        Ok(())
    }

    pub(crate) async fn add_backup_path(&mut self, path: PathBuf) -> BorgResult<()> {
        if self.backup_paths.contains(&path) {
            return Err(anyhow::anyhow!(
                "Path {} already exists in profile {}",
                path.display(),
                self.name
            ));
        }
        tokio::fs::metadata(&path).await.with_context(|| {
            format!(
                "Failed to get metadata for path {} when adding to profile {}. Does the path exist?",
                path.display(), self.name
            )
        })?;
        let canonical_path = tokio::fs::canonicalize(&path).await.with_context(|| {
            format!(
                "Failed to canonicalize path {} when adding to profile {}. Does the path exist?",
                path.display(),
                self.name
            )
        })?;
        if canonical_path != path {
            bail!("Attempted to add relative path or path that contained symlinks. \nAttempted='{}',\nCanonical='{}'", path.to_string_lossy(), canonical_path.to_string_lossy());
        }
        self.backup_paths.push(path);
        Ok(())
    }

    pub(crate) fn remove_backup_path(&mut self, path: &Path) {
        self.backup_paths.retain(|p| p != path);
    }
}
