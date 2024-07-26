use std::{
    io::Write,
    num::NonZeroU16,
    os::unix::prelude::PermissionsExt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use crate::{
    backends::{
        backup_provider::BackupProvider, borg_provider::BorgProvider,
        rustic_provider::RusticProvider,
    },
    cli::PassphraseSource,
    types::{
        log_on_error, show_notification, BorgResult, CommandResponseSender, RepositoryArchives,
        SHORT_NOTIFICATION_DURATION,
    },
};
use anyhow::anyhow;
use anyhow::{bail, Context};
use keyring::Entry;
use std::fs;
use tracing::info;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Passphrase(String);

impl Passphrase {
    pub(crate) fn inner(&self) -> String {
        self.0.clone()
    }

    pub(crate) fn inner_ref(&self) -> &str {
        &self.0
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

impl From<&str> for Passphrase {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum Encryption {
    None,
    Raw(Passphrase),
    Keyring,
    Keyfile(PathBuf),
}

impl Encryption {
    pub(crate) fn from_passphrase_loc(passphrase_loc: PassphraseSource) -> BorgResult<Self> {
        if passphrase_loc.raw {
            if let Some(borg_passphrase) = passphrase_loc.borg_passphrase {
                return Ok(Encryption::Raw(borg_passphrase));
            }
        }
        match (passphrase_loc.keyfile, passphrase_loc.borg_passphrase) {
            (Some(keyfile), None) => Ok(Encryption::Keyfile(keyfile)),
            (Some(keyfile), Some(borg_passphrase)) => {
                let keyfile_path = Path::new(&keyfile);
                if !keyfile_path.exists() {
                    let mut file = std::fs::File::create(keyfile_path)?;
                    let mut permissions = file.metadata()?.permissions();
                    permissions.set_mode(0o600);
                    file.set_permissions(permissions)?;
                    file.write_all(borg_passphrase.inner_ref().as_bytes())?;
                } else {
                    tracing::warn!("Keyfile exists and BORG_PASSPHRASE set in the environment. Ignoring BORG_PASSPHRASE and not updating the keyfile!");
                }
                Ok(Encryption::Keyfile(keyfile))
            }
            (None, Some(_borg_passphrase)) => Ok(Encryption::Keyring),
            (None, None) => Ok(Encryption::None),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
pub(crate) enum RepositoryKind {
    Borg,
    Rustic,
}

impl std::fmt::Display for RepositoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sort = match self {
            RepositoryKind::Borg => "borg".to_string(),
            RepositoryKind::Rustic => "rustic".to_string(),
        };
        write!(f, "{sort}")
    }
}

impl FromStr for RepositoryKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "borg" => Ok(RepositoryKind::Borg),
            "rustic" => Ok(RepositoryKind::Rustic),
            otherwise => Err(anyhow::anyhow!("Unknown repository kind: {otherwise}")),
        }
    }
}

const fn default_repository_kind() -> RepositoryKind {
    RepositoryKind::Borg
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
    #[serde(default = "default_repository_kind")]
    kind: RepositoryKind,
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
    pub(crate) fn new(
        path: String,
        encryption: Encryption,
        kind: RepositoryKind,
        rsh: Option<String>,
    ) -> Self {
        Self {
            path,
            encryption,
            kind,
            rsh,
            disabled: false,
            lock: Default::default(),
        }
    }

    pub(crate) fn get_passphrase(&self) -> BorgResult<Option<Passphrase>> {
        match &self.encryption {
            Encryption::None => Ok(None),
            Encryption::Raw(passphrase) => Ok(Some(passphrase.clone())),
            Encryption::Keyring => get_keyring_entry(&self.path)?
                .get_password()
                .map_err(|e| anyhow::anyhow!("Failed to get passphrase from keyring: {}", e))
                .map(|v| Some(Passphrase(v))),
            Encryption::Keyfile(filepath) => {
                let passphrase = fs::read_to_string(filepath)
                    .with_context(|| format!("Failed to read {filepath:?}. Does the file exist?"))?
                    .trim()
                    .to_string();
                Ok(Some(Passphrase(passphrase)))
            }
        }
    }

    pub(crate) fn set_passphrase(
        &mut self,
        encryption: Encryption,
        borg_passphrase: Option<Passphrase>,
    ) -> BorgResult<()> {
        self.encryption = encryption;
        if matches!(self.encryption, Encryption::Keyring) {
            let entry = get_keyring_entry(&self.path)?;
            let borg_passphrase = borg_passphrase.ok_or_else(|| {
                anyhow!(
                    "Keyring encryption is being used in {} but BORG_PASSPHRASE is unset!",
                    self
                )
            })?;
            entry
                .set_password(borg_passphrase.inner_ref())
                .with_context(|| {
                    format!(
                        "Failed to set password in keyring for repository {} in profile {}",
                        &self.path, &self
                    )
                })?;
            assert!(entry.get_password().is_ok());
        }
        Ok(())
    }

    /// If true, the repo has been disabled and actions will
    /// not be performed on it
    pub(crate) fn disabled(&self) -> bool {
        self.disabled
    }

    pub(crate) fn path(&self) -> String {
        self.path.clone()
    }

    pub(crate) fn rsh(&self) -> Option<String> {
        self.rsh.clone()
    }

    pub(crate) async fn list_archives(&self) -> BorgResult<RepositoryArchives> {
        self.backup_provider().list_archives(self).await
    }

    pub(crate) async fn init(&self) -> BorgResult<()> {
        self.backup_provider()
            .init_repo(self.path(), self.get_passphrase()?, self.rsh())
            .await?;
        Ok(())
    }

    pub(crate) async fn mount(
        &self,
        given_repository_path: String,
        mountpoint: PathBuf,
    ) -> BorgResult<()> {
        self.backup_provider()
            .mount(self, given_repository_path, mountpoint)
            .await?;
        Ok(())
    }

    pub(crate) async fn prune(
        &self,
        prune_options: PruneOptions,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        info!("Starting to prune {}", self);
        self.backup_provider()
            .prune(self, prune_options, progress_channel)
            .await
    }

    pub(crate) async fn compact(&self, progress_channel: CommandResponseSender) -> BorgResult<()> {
        self.backup_provider().compact(self, progress_channel).await
    }

    pub(crate) async fn check(&self, progress_channel: CommandResponseSender) -> BorgResult<bool> {
        self.backup_provider().check(self, progress_channel).await
    }

    pub(crate) fn backup_provider(&self) -> Box<dyn BackupProvider> {
        match self.kind {
            RepositoryKind::Borg => Box::new(BorgProvider {}),
            RepositoryKind::Rustic => Box::new(RusticProvider {}),
        }
    }

    pub(crate) fn repo_kind_name(&self) -> &'static str {
        match self.kind {
            RepositoryKind::Borg => "Borg",
            RepositoryKind::Rustic => "Rustic",
        }
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
            keep_daily: NonZeroU16::new(64).unwrap(),
            keep_weekly: NonZeroU16::new(128).unwrap(),
            keep_monthly: NonZeroU16::new(64).unwrap(),
            keep_yearly: NonZeroU16::new(32).unwrap(),
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

// Necessary for serde(default)
const fn default_exclude_caches() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct Profile {
    name: String,
    backup_paths: Vec<PathBuf>,
    #[serde(default)]
    exclude_patterns: Vec<String>,
    #[serde(default = "default_exclude_caches")]
    exclude_caches: bool,
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
    pub(crate) async fn open_or_create(profile: &Option<String>) -> BorgResult<Self> {
        let profile_name = profile
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or(Self::DEFAULT_PROFILE_NAME);
        if let Some(profile) = Self::open_profile(profile_name).await? {
            Ok(profile)
        } else {
            Self::create_profile(profile_name).await
        }
    }

    fn blank(name: &str) -> Self {
        Self {
            name: name.to_string(),
            exclude_patterns: vec![],
            exclude_caches: true,
            backup_paths: vec![],
            prune_options: Default::default(),
            repos: vec![],
            action_timeout_seconds: default_action_timeout_seconds(),
        }
    }

    pub(crate) async fn create_profile(name: &str) -> BorgResult<Self> {
        if let Ok(Some(_)) = Self::open_profile(name).await {
            anyhow::bail!("Cannot create profile '{}' as it already exists!", name);
        }
        let profile = Self::blank(name);
        profile.save_profile().await?;
        info!(
            "Created {} ({})",
            profile,
            profile
                .profile_path()
                .unwrap_or("unknown_path".into())
                .to_string_lossy()
        );
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

    pub(crate) fn find_repo_from_mount_src(&self, repo_or_archive: &str) -> BorgResult<Repository> {
        let repo_name = match repo_or_archive.find("::") {
            Some(loc) => repo_or_archive[..loc].to_string(),
            None => repo_or_archive.to_string(),
        };
        tracing::debug!("Figured repo name is: {}", repo_name);
        self.active_repositories()
            .find(|repo| repo.path == repo_name)
            .ok_or_else(|| anyhow::anyhow!("Could not find repo: {}", repo_or_archive))
            .cloned()
    }

    pub(crate) async fn create_backup_with_notification(
        &self,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<tokio::task::JoinHandle<()>> {
        let completion_semaphore = Arc::new(Semaphore::new(0));
        let num_active_repos = self.num_active_repos();
        let self_name = format!("{}", self);
        let completion_semaphore_clone = completion_semaphore.clone();
        let join_handle = tokio::spawn(async move {
            let start_time = Instant::now();
            if let Err(e) = completion_semaphore_clone
                .acquire_many(num_active_repos as u32)
                .await
            {
                tracing::error!("Failed to wait on completion semaphore: {}", e);
            } else {
                let elapsed_duration = start_time.elapsed();
                let nicely_formatted = format!(
                    "{:0>2}:{:0>2}:{:0>2}",
                    elapsed_duration.as_secs() / 60 / 60,
                    elapsed_duration.as_secs() / 60 % 60,
                    elapsed_duration.as_secs() % 60
                );
                tracing::info!("Completed backup for {} in {}", self_name, nicely_formatted);
                log_on_error!(
                    show_notification(
                        &format!("Backup complete for {}", self_name),
                        &format!("Completed in {}", nicely_formatted),
                        SHORT_NOTIFICATION_DURATION
                    )
                    .await,
                    "Failed to show notification: {}"
                );
            }
        });
        self.create_backup_internal(progress_channel, completion_semaphore)
            .await?;
        Ok(join_handle)
    }

    pub(crate) async fn create_backup(
        &self,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        self.create_backup_internal(progress_channel, Arc::new(Semaphore::new(0)))
            .await
    }

    async fn create_backup_internal(
        &self,
        progress_channel: CommandResponseSender,
        completion_semaphore: Arc<Semaphore>,
    ) -> BorgResult<()> {
        let archive_name = format!(
            "{}-{}",
            self.name(),
            chrono::Local::now().format("%Y-%m-%d:%H:%M:%S")
        );
        for repo in self.active_repositories() {
            let backup_provider = repo.backup_provider();
            backup_provider
                .create_backup(
                    archive_name.clone(),
                    self.backup_paths(),
                    self.exclude_patterns(),
                    self.exclude_caches(),
                    repo.clone(),
                    progress_channel.clone(),
                    completion_semaphore.clone(),
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) fn active_repositories(&self) -> impl Iterator<Item = &Repository> {
        self.repositories().iter().filter(|repo| !repo.disabled)
    }

    pub(crate) fn num_active_repositories(&self) -> usize {
        self.active_repositories().count()
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

    pub(crate) fn num_active_repos(&self) -> usize {
        self.active_repositories().count()
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

    pub(crate) fn exclude_caches(&self) -> bool {
        self.exclude_caches
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

    pub(crate) fn add_repository(&mut self, repo: Repository) {
        self.repos.push(repo);
    }

    // TODO: Rewrite this in terms of PassphraseSource
    pub(crate) fn update_repository_password(
        &mut self,
        repo_path: &str,
        encryption: Encryption,
        borg_passphrase: Option<Passphrase>,
    ) -> BorgResult<()> {
        let self_str = format!("{}", self); // used in error message below
        let repo = self
            .repos
            .iter_mut()
            .find(|repo| repo.path == repo_path)
            .ok_or_else(|| {
                anyhow::anyhow!("Couldn't find repository {} in {}", repo_path, self_str)
            })?;
        repo.set_passphrase(encryption, borg_passphrase)?;
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
