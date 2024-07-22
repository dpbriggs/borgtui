use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::sync::{mpsc, Semaphore};

use crate::{
    borgtui::CommandResponse,
    profiles::{Passphrase, PruneOptions, Repository},
    types::{Archive, BorgResult, RepositoryArchives},
};

use super::backup_provider::BackupProvider;

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
        // TODO: use progress channel
        _progress_channel: mpsc::Sender<CommandResponse>,
        completion_semaphore: Arc<Semaphore>,
    ) -> BorgResult<()> {
        let repo_loc = repo.path();
        let passphrase = repo.get_passphrase()?;
        let backends = rustic_backend::BackendOptions::default()
            .repository(&repo_loc)
            .to_backends()?;

        let mut repo_opts = rustic_core::RepositoryOptions::default();
        if let Some(passphrase) = passphrase {
            repo_opts = repo_opts.password(passphrase.inner())
        }

        let backup_paths: Vec<_> = backup_paths
            .iter()
            .map(|bp| bp.to_string_lossy().to_string())
            .collect();

        let mut filter_opts = rustic_core::LocalSourceFilterOptions::default();
        if exclude_caches {
            filter_opts = filter_opts.exclude_if_present(["CACHEDIR.TAG".to_string()]);
        }
        filter_opts = filter_opts.glob(
            exclude_patterns
                .iter()
                .map(|exclude_pattern| format!("!{exclude_pattern}"))
                .collect::<Vec<_>>(),
        );

        tracing::info!(
            "Starting rustic backup of {}::{}",
            repo.path(),
            &archive_name
        );
        let res = tokio::task::spawn_blocking(move || -> BorgResult<()> {
            let rustic_repo = rustic_core::Repository::new(&repo_opts, backends)?
                .open()?
                .to_indexed_ids()?;
            let backup_opts = rustic_core::BackupOptions::default().ignore_filter_opts(filter_opts);
            let sources = rustic_core::PathList::from_strings(backup_paths);
            let mut snap = rustic_core::SnapshotOptions::default()
                .add_tags("borgtui")?
                .to_snapshot()?;
            snap.label = archive_name;
            let snap = rustic_repo.backup(&backup_opts, &sources, snap)?;
            tracing::info!("Completed rustic backup: {:#?}", snap);
            Ok(())
        })
        .await?;
        // BUG: failing to create a thread will cause this semaphore to not increment!
        // TODO: Fix the bug above
        completion_semaphore.add_permits(1);
        res
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
            let snapshots = rustic_core::Repository::new(&repo_opts, backends)?
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
        .await??;
        Ok(res)
    }
    async fn init_repo(
        &self,
        repo_loc: String,
        passphrase: Option<Passphrase>,
        _rsh: Option<String>,
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
        // TODO: Use the progress bar one!
        rustic_core::Repository::new(&repo_opts, backends)?.init(&key_opts, &config_opts)?;
        Ok(())
    }
    async fn mount(
        &self,
        repo: &Repository,
        // TODO: support mounting particular snapshots
        _given_repository_path: String,
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
        let mountpoint_s = mountpoint.to_string_lossy().to_string();
        let repo_path = repo.path();
        let passphrase = repo.get_passphrase()?;
        let exit = tokio::process::Command::new("restic")
            .env(
                "RESTIC_PASSWORD",
                passphrase.map(|p| p.inner()).unwrap_or_default(),
            )
            .arg("-r")
            .arg(repo_path)
            .arg("mount")
            .arg(&mountpoint_s)
            .spawn()?
            .wait()
            .await?;
        tracing::info!("Successfully mounted at {mountpoint_s}");
        tracing::info!("Restic exited with code {exit:?}");
        Ok(())
    }
    // TODO: Figure out unmounting
    #[allow(unused)]
    async fn unmount(&self, mountpoint: PathBuf) -> BorgResult<()> {
        todo!()
    }
    async fn prune(
        &self,
        _repo: &Repository,
        _prune_options: PruneOptions,
        _progress_channel: mpsc::Sender<CommandResponse>,
    ) -> BorgResult<()> {
        todo!()
    }
    async fn compact(
        &self,
        _repo: &Repository,
        _progress_channel: mpsc::Sender<CommandResponse>,
    ) -> BorgResult<()> {
        todo!()
    }
    async fn check(&self, _repo: &Repository) -> BorgResult<bool> {
        todo!()
    }
}
