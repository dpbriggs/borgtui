use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc, RwLock,
    },
};

use async_trait::async_trait;
use fuse_mt::FuseMT;
use rustic_core::repofile::SnapshotFile;
use rustic_core::vfs::{FilePolicy, IdenticalSnapshot, Latest, Vfs};
use tokio::sync::Semaphore;

use crate::{
    backends::rustic_mount::FuseFS,
    borgtui::CommandResponse,
    profiles::{Passphrase, PruneOptions, Repository},
    types::{
        send_check_complete, send_error, send_info, take_repo_lock, Archive, BackupCreateProgress,
        BackupCreationProgress, BorgResult, CheckProgress, CommandResponseSender, PrettyBytes,
        RepositoryArchives,
    },
    RepositoryOptions,
};

use super::backup_provider::BackupProvider;

const RESTIC_PASSPHRASE_REQUIRED: &str = "Restic Repositories require a password! Please check your configuration using `borgtui config-path`";

fn passphrase_from_repo(repo: &Repository) -> BorgResult<Passphrase> {
    repo.get_passphrase()?
        .ok_or_else(|| anyhow::anyhow!(RESTIC_PASSPHRASE_REQUIRED))
}

#[derive(Debug, Clone)]
enum ProgressEmitterKind {
    Backup,
    Info,
    Check,
}

#[derive(Clone, Debug)]
struct ProgressEmitter {
    sender: CommandResponseSender,
    prefix: String,
    repo_path: String,
    title: Arc<RwLock<String>>,
    length: Arc<AtomicU64>,
    counter: Arc<AtomicU64>,
    last_sent_counter: Arc<AtomicU64>,
    last_time_sent: Arc<RwLock<std::time::Instant>>,
    force_emit_now: Arc<AtomicBool>,
    hidden: bool,
    kind: ProgressEmitterKind,
}

impl ProgressEmitter {
    fn create_backup(sender: CommandResponseSender, repo_path: String) -> Self {
        // TODO: fix ugly time
        let last_time_sent = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .unwrap();
        Self {
            sender,
            prefix: String::new(),
            repo_path,
            title: Arc::new(RwLock::new(String::new())),
            length: Arc::new(AtomicU64::new(0)),
            counter: Arc::new(AtomicU64::new(0)),
            last_sent_counter: Arc::new(AtomicU64::new(0)),
            last_time_sent: Arc::new(RwLock::new(last_time_sent)),
            force_emit_now: Arc::new(AtomicBool::new(false)),
            hidden: false,
            kind: ProgressEmitterKind::Backup,
        }
    }

    fn info(sender: CommandResponseSender, repo_path: String) -> Self {
        let mut ss = Self::create_backup(sender, repo_path);
        ss.kind = ProgressEmitterKind::Info;
        ss
    }

    fn check(sender: CommandResponseSender, repo_path: String) -> Self {
        let mut ss = Self::create_backup(sender, repo_path);
        ss.kind = ProgressEmitterKind::Check;
        ss
    }

    fn with_prefix(&self, prefix: String) -> Self {
        let mut ss = self.clone();
        ss.prefix = prefix;
        ss
    }

    fn send_check_progress(&self) {
        let counter = self.counter.load(std::sync::atomic::Ordering::SeqCst);
        self.last_sent_counter
            .store(counter, std::sync::atomic::Ordering::SeqCst);
        let msg = format!(
            "{} {} / {}",
            &self.prefix,
            PrettyBytes(counter),
            PrettyBytes(self.length.load(std::sync::atomic::Ordering::SeqCst))
        );
        let msg = CommandResponse::CheckProgress(CheckProgress::new(self.repo_path.clone(), msg));
        if let Err(e) = self.sender.blocking_send(msg) {
            tracing::error!("Failed to send check message: {e}");
        }
    }

    fn send_info_progress(&self) {
        let counter = self.counter.load(std::sync::atomic::Ordering::SeqCst);
        self.last_sent_counter
            .store(counter, std::sync::atomic::Ordering::SeqCst);
        let msg = format!(
            "[{}] {} {} / {}",
            &self.repo_path,
            &self.prefix,
            PrettyBytes(counter),
            PrettyBytes(self.length.load(std::sync::atomic::Ordering::SeqCst))
        );
        let msg = CommandResponse::Info(msg);
        if let Err(e) = self.sender.blocking_send(msg) {
            tracing::error!("Failed to send info message: {e}");
        }
    }

    fn send_create_progress(&self) {
        let byte_counter = self.counter.load(std::sync::atomic::Ordering::SeqCst);
        self.last_sent_counter
            .store(byte_counter, std::sync::atomic::Ordering::SeqCst);
        let total_size = self.length.load(std::sync::atomic::Ordering::SeqCst);
        let msg = format!(
            "{}: {} - {} / {}",
            &self.prefix,
            &*self.title.read().unwrap(),
            PrettyBytes(byte_counter),
            PrettyBytes(total_size)
        );
        let progress = BackupCreationProgress::InProgress {
            original_size: total_size,
            compressed_size: byte_counter,
            deduplicated_size: byte_counter,
            num_files: 0,
            current_path: msg,
        };
        let create_progress = BackupCreateProgress::new(self.repo_path.clone(), progress);
        let msg = CommandResponse::CreateProgress(create_progress);
        if let Err(e) = self.sender.blocking_send(msg) {
            tracing::error!("Failed to send create message: {e}");
        }
    }

    fn maybe_send_backup_create_finished(&self) {
        // Actually finish
        if matches!(self.kind, ProgressEmitterKind::Backup) {
            let finished = BackupCreateProgress::finished(self.repo_path.clone());
            let msg = CommandResponse::CreateProgress(finished);
            if let Err(e) = self.sender.blocking_send(msg) {
                tracing::error!("Failed to send finish message: {e}");
            }
        }
    }

    fn send_message(&self) {
        let mut last_time_guard = self.last_time_sent.write().unwrap();
        *last_time_guard = std::time::Instant::now();
        match self.kind {
            ProgressEmitterKind::Backup => self.send_create_progress(),
            ProgressEmitterKind::Info => self.send_info_progress(),
            ProgressEmitterKind::Check => self.send_check_progress(),
        }
    }
}

const SUBSTANTIAL_CHANGE_THRESHOLD: f64 = 0.10;
const SUBSTANTIAL_CHANGE_THRESHOLD_TIME: std::time::Duration =
    std::time::Duration::from_millis(200);

impl rustic_core::Progress for ProgressEmitter {
    fn is_hidden(&self) -> bool {
        self.hidden
    }

    fn set_length(&self, len: u64) {
        self.length.store(len, std::sync::atomic::Ordering::SeqCst);
    }

    fn set_title(&self, title: &'static str) {
        let mut guard = self.title.write().unwrap();
        *guard = title.to_string();
        self.force_emit_now
            .store(true, std::sync::atomic::Ordering::SeqCst)
    }

    fn inc(&self, inc: u64) {
        let old = self
            .counter
            .fetch_add(inc, std::sync::atomic::Ordering::SeqCst);
        let new_value = old + inc;
        let last_sent_size = self
            .last_sent_counter
            .load(std::sync::atomic::Ordering::SeqCst);
        let is_substantial_change =
            (1.0 - last_sent_size as f64 / (new_value as f64)) >= SUBSTANTIAL_CHANGE_THRESHOLD;
        let is_substantial_time_diff = (std::time::Instant::now()
            - *self.last_time_sent.read().unwrap())
            >= SUBSTANTIAL_CHANGE_THRESHOLD_TIME;
        let force_emit_now = self
            .force_emit_now
            .load(std::sync::atomic::Ordering::SeqCst);
        if is_substantial_change || is_substantial_time_diff || force_emit_now {
            self.send_message();
        }
    }

    fn finish(&self) {
        if self.hidden {
            return;
        }
        // Ensure the last entry is always sent
        self.send_message();
    }
}

impl rustic_core::ProgressBars for ProgressEmitter {
    type P = ProgressEmitter;

    fn progress_hidden(&self) -> Self::P {
        self.clone()
    }

    fn progress_spinner(&self, prefix: impl Into<std::borrow::Cow<'static, str>>) -> Self::P {
        self.with_prefix(prefix.into().to_string())
    }

    fn progress_counter(&self, prefix: impl Into<std::borrow::Cow<'static, str>>) -> Self::P {
        self.with_prefix(prefix.into().to_string())
    }

    fn progress_bytes(&self, prefix: impl Into<std::borrow::Cow<'static, str>>) -> Self::P {
        self.with_prefix(prefix.into().to_string())
    }
}

fn rustic_cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|mut p| {
        p.push("borgtui");
        p.push("rustic");
        p
    })
}

pub(crate) struct RusticProvider;

#[async_trait]
impl BackupProvider for RusticProvider {
    async fn create_backup(
        &self,
        archive_name: String,
        backup_paths: &[PathBuf],
        exclude_patterns: &[String],
        exclude_caches: bool,
        repo: Repository,
        progress_channel: CommandResponseSender,
        completion_semaphore: Arc<Semaphore>,
    ) -> BorgResult<()> {
        let backup_paths: Vec<_> = backup_paths
            .iter()
            .map(|bp| bp.to_string_lossy().to_string())
            .collect();

        let mut filter_opts = rustic_core::LocalSourceFilterOptions::default();
        if exclude_caches {
            filter_opts = filter_opts.exclude_if_present(["CACHEDIR.TAG".to_string()]);
        }
        filter_opts = filter_opts.globs(
            exclude_patterns
                .iter()
                .map(|exclude_pattern| format!("!{exclude_pattern}"))
                .collect::<Vec<_>>(),
        );

        let fully_qualified_name = format!("{}::{}", repo.path(), &archive_name);
        send_info!(
            progress_channel,
            format!("Starting rustic backup of {fully_qualified_name}")
        );
        let pb = ProgressEmitter::create_backup(progress_channel.clone(), repo.path());
        let handle = tokio::task::spawn_blocking(move || -> BorgResult<()> {
            // Backend
            let repo_loc = repo.path();
            let backends = rustic_backend::BackendOptions::default()
                .repository(repo_loc)
                .to_backends()?;

            // Passphrase
            let passphrase = passphrase_from_repo(&repo)?;
            // Actually open the connection
            let mut repo_opts =
                rustic_core::RepositoryOptions::default().password(passphrase.inner());
            if let Some(cache_dir) = rustic_cache_dir() {
                repo_opts = repo_opts.cache_dir(cache_dir);
                repo_opts.no_cache = false;
            }
            let rustic_repo =
                rustic_core::Repository::new_with_progress(&repo_opts, &backends, pb.clone())?
                    .open()?
                    .to_indexed_ids()?;
            let backup_opts = rustic_core::BackupOptions::default().ignore_filter_opts(filter_opts);
            let sources = rustic_core::PathList::from_iter(backup_paths);
            let mut snap = rustic_core::SnapshotOptions::default()
                .add_tags("borgtui")?
                .to_snapshot()?;
            snap.label = archive_name;
            let snap = rustic_repo.backup(&backup_opts, &sources, snap)?;
            tracing::info!("Snapshot taken! {}", snap.label);
            pb.maybe_send_backup_create_finished();
            Ok(())
        });

        tokio::spawn(async move {
            match handle.await {
                Ok(Ok(_)) => {
                    send_info!(
                        progress_channel,
                        format!("Completed rustic backup for {}", fully_qualified_name)
                    );
                }
                Ok(Err(e)) => send_error!(progress_channel, format!("Rustic backup failed: {e}")),
                Err(e) => send_error!(
                    progress_channel,
                    format!("Failed to spawn thread for Rustic backup: {e}")
                ),
            }
            completion_semaphore.add_permits(1);
        });
        Ok(())
    }
    async fn list_archives(&self, repo: &Repository) -> BorgResult<RepositoryArchives> {
        let repo_loc = repo.path();
        let passphrase = repo.get_passphrase()?;
        let backends = rustic_backend::BackendOptions::default()
            .repository(&repo_loc)
            .to_backends()?;

        let mut repo_opts = rustic_core::RepositoryOptions::default();
        if let Some(passphrase) = passphrase {
            repo_opts = repo_opts.password(passphrase.inner())
        }
        let res = tokio::task::spawn_blocking(move || -> BorgResult<RepositoryArchives> {
            let snapshots = rustic_core::Repository::new(&repo_opts, &backends)?
                .open()?
                .get_all_snapshots()?;
            let mut archives: Vec<Archive> = snapshots
                .iter()
                .map(|snapshot| Archive {
                    name: snapshot.label.clone(),
                    creation_date: snapshot.time.date_naive().into(),
                })
                .collect();
            // Sort so the most recent archive is the last (borg behaviour)
            archives.sort_by(|left, right| {
                left.creation_date
                    .partial_cmp(&right.creation_date)
                    .unwrap()
            });
            Ok(RepositoryArchives::new(repo_loc, archives))
        })
        // TODO: This one actually blocks?
        .await??;
        Ok(res)
    }

    async fn init_repo(
        &self,
        repo_loc: String,
        passphrase: Option<Passphrase>,
        _config: RepositoryOptions,
    ) -> BorgResult<()> {
        let passphrase = match passphrase {
            Some(passphrase) => passphrase,
            None => anyhow::bail!(
                "Restic repositories require a password. Please provide one. See `borgtui init -h`."
            ),
        };
        let backends = rustic_backend::BackendOptions::default()
            .repository(&repo_loc)
            .to_backends()?;

        let repo_opts = rustic_core::RepositoryOptions::default().password(passphrase.inner());
        let key_opts = rustic_core::KeyOptions::default();
        let config_opts = rustic_core::ConfigOptions::default();
        tracing::info!("Initializing rustic repo: {repo_loc}");
        rustic_core::Repository::new(&repo_opts, &backends)?.init(&key_opts, &config_opts)?;
        tracing::info!("Successfully initialized rustic repo: {repo_loc}");
        Ok(())
    }

    async fn mount(
        &self,
        repo: &Repository,
        // TODO: support mounting particular snapshots
        given_repository_path: String,
        mountpoint: PathBuf,
    ) -> BorgResult<()> {
        if repo.disabled() {
            anyhow::bail!("Attempted to mount disabled repo: {}", repo);
        }
        // See if the path exists, and if not, try to make it
        if let Ok(false) = tokio::fs::try_exists(&mountpoint).await {
            tracing::info!(
                "Attempting to create directory for mounting: {}",
                mountpoint.to_string_lossy()
            );
            tokio::fs::create_dir_all(&mountpoint).await?;
        }

        let passphrase = passphrase_from_repo(repo)?;
        let repo_loc = repo.path();
        let backends = rustic_backend::BackendOptions::default()
            .repository(&repo_loc)
            .to_backends()?;

        let repo_opts = rustic_core::RepositoryOptions::default().password(passphrase.inner());

        // TODO: do something with this join handle
        let _join_handle = tokio::task::spawn_blocking(move || -> BorgResult<()> {
            let rustic_repo = rustic_core::Repository::new(&repo_opts, &backends)?.open()?;

            let (_repo_path, snapshot_label) =
                given_repository_path.split_once("::").unwrap_or_default();

            let sn_filter = |sn: &SnapshotFile| {
                if snapshot_label.is_empty() {
                    return true;
                }
                sn.label == snapshot_label
            };

            let vfs = Vfs::from_snapshots(
                rustic_repo.get_matching_snapshots(sn_filter)?,
                "[{hostname}]/[{label}]",
                // TODO: Make this a borgtui constant (we use it in the archive name)
                "%Y-%m-%d:%H:%M:%S",
                Latest::AsLink,
                IdenticalSnapshot::AsLink,
            )?;
            let file_policy = FilePolicy::Read; // TODO: I should probably be smarter here

            tracing::info!(
                "Mounting rustic repo: {} at {}",
                repo_loc,
                mountpoint.to_string_lossy(),
            );
            tracing::info!("BorgTUI will block until the filesystem is unmounted.");
            tracing::info!("Make sure to run `umount {mountpoint:?}` to unmount the filesystem once you are done.");
            let fuse_mt = FuseMT::new(FuseFS::new(rustic_repo.to_indexed()?, vfs, file_policy), 1);
            fuse_mt::mount(fuse_mt, &mountpoint, &[])?;
            Ok(())
        });
        Ok(())
    }

    // TODO: Wire unmounting in with repositories
    async fn unmount(&self, mountpoint: PathBuf) -> BorgResult<()> {
        let exit = tokio::process::Command::new("umount")
            .arg(mountpoint)
            .spawn()?
            .wait()
            .await?;
        tracing::info!("umount finished with exitcode {exit:?}");
        Ok(())
    }

    async fn prune(
        &self,
        repo: &Repository,
        prune_options: PruneOptions,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        take_repo_lock!(progress_channel, repo);

        let repo_loc = repo.path();

        let backends = rustic_backend::BackendOptions::default()
            .repository(&repo_loc)
            .to_backends()?;
        let passphrase = passphrase_from_repo(repo)?;

        let pb = ProgressEmitter::info(progress_channel.clone(), repo_loc.clone());
        let handle = tokio::task::spawn_blocking(move || {
            // Actually open the connection
            let repo_opts = rustic_core::RepositoryOptions::default().password(passphrase.inner());
            let rustic_repo =
                rustic_core::Repository::new_with_progress(&repo_opts, &backends, pb)?.open()?;
            let keep_options = rustic_core::KeepOptions::default()
                .keep_daily(prune_options.keep_daily.get() as i32)
                .keep_weekly(prune_options.keep_weekly.get() as i32)
                .keep_monthly(prune_options.keep_monthly.get() as i32)
                .keep_yearly(prune_options.keep_yearly.get() as i32);
            let forget_ids = rustic_repo
                .get_forget_snapshots(
                    &keep_options,
                    rustic_core::SnapshotGroupCriterion::default()
                        .hostname(false)
                        .label(false)
                        .paths(false)
                        .tags(false),
                    |_| true,
                )?
                .into_forget_ids();
            // TODO: use send_info_blocking (and write that macro)
            tracing::info!(
                "Removing {} rustic snapshots in {}.",
                forget_ids.len(),
                repo_loc,
            );
            rustic_repo.delete_snapshots(&forget_ids)?;
            let prune_opts = rustic_core::PruneOptions::default().ignore_snaps(forget_ids);
            let prune_plan = rustic_repo.prune_plan(&prune_opts)?;
            // TODO: use send_info_blocking
            tracing::info!("Pruning {}...", repo_loc);
            rustic_repo.prune(&prune_opts, prune_plan)?;
            Ok::<(), anyhow::Error>(())
        });
        let repo_loc_clone = repo.path();
        tokio::spawn(async move {
            match handle.await {
                Ok(Ok(_)) => send_info!(
                    progress_channel,
                    format!("Successfully pruned {repo_loc_clone}")
                ),
                Ok(Err(e)) => send_error!(progress_channel, format!("Rustic prune failed: {e}")),
                Err(e) => send_error!(progress_channel, format!("Failed to spawn thread: {e}")),
            }
        });
        Ok(())
    }
    async fn compact(
        &self,
        _repo: &Repository,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        tracing::warn!(
            "BorgTUI's implementation of Rustic repositories automatically compact when pruning!"
        );
        Ok(())
    }

    async fn check(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        take_repo_lock!(progress_channel, repo);
        let repo_loc = repo.path();

        let backends = rustic_backend::BackendOptions::default()
            .repository(&repo_loc)
            .to_backends()?;
        let passphrase = passphrase_from_repo(repo)?;

        let progress_channel_clone = progress_channel.clone();
        let res = tokio::task::spawn_blocking(move || {
            let pb = ProgressEmitter::check(progress_channel_clone, repo_loc.clone());
            let repo_opts = rustic_core::RepositoryOptions::default().password(passphrase.inner());
            let rustic_repo =
                rustic_core::Repository::new_with_progress(&repo_opts, &backends, pb)?.open()?;
            let check_options = rustic_core::CheckOptions::default();
            rustic_repo.check(check_options)
        })
        .await?;
        match res {
            Ok(_) => {
                send_check_complete!(progress_channel, repo.path(), None);
                send_info!(
                    progress_channel,
                    format!("Verification succeeded for repository: {repo}")
                );
                Ok(true)
            }
            Err(e) => {
                let err_msg = format!("Rustic check failed: {e}");
                send_check_complete!(progress_channel, repo.path(), Some(err_msg.clone()));
                send_error!(progress_channel, err_msg.clone());
                Ok(false)
            }
        }
    }
    async fn repair(
        &self,
        _repo: &Repository,
        _progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        // TODO: Implement this!
        anyhow::bail!("Unimplemented")
    }
}
