use std::{path::PathBuf, process::Stdio};

use anyhow::anyhow;
use async_trait::async_trait;
use borgbackup::{
    asynchronous as borg_async,
    common::{
        CommonOptions, EncryptionMode, InitOptions, MountOptions, MountSource,
        PruneOptions as BorgLibPruneOptions,
    },
    output::list::ListRepository as BorgLibListRepository,
};
use tracing::info;

use crate::{
    borgtui::CommandResponse,
    profiles::{Passphrase, Repository, RepositoryOptions},
    types::{
        send_check_complete, send_check_progress, send_error, send_info, take_repo_lock, Archive,
        BackupCreateProgress, BackupCreationProgress, BorgResult, CommandResponseSender,
        RepositoryArchives,
    },
};

impl From<BorgLibListRepository> for RepositoryArchives {
    fn from(value: BorgLibListRepository) -> Self {
        RepositoryArchives {
            path: value.repository.location,
            archives: value
                .archives
                .into_iter()
                .map(|archive| Archive {
                    name: archive.name,
                    creation_date: archive.start,
                })
                .collect(),
        }
    }
}

impl From<borg_async::CreateProgress> for BackupCreationProgress {
    fn from(value: borg_async::CreateProgress) -> Self {
        match value {
            borg_async::CreateProgress::Progress {
                original_size,
                compressed_size,
                deduplicated_size,
                nfiles,
                path,
            } => BackupCreationProgress::InProgress {
                original_size,
                compressed_size,
                deduplicated_size,
                num_files: nfiles,
                current_path: path,
            },
            borg_async::CreateProgress::Finished => BackupCreationProgress::Finished,
        }
    }
}

fn make_common_options(repo: &Repository) -> BorgResult<CommonOptions> {
    let borg_options = repo.borg_options()?;
    Ok(CommonOptions {
        rsh: borg_options.rsh.clone(),
        remote_path: borg_options.remote_path.clone(),
        ..Default::default()
    })
}

/// TODO: tie this into the repo which was mounted!
pub(crate) async fn hack_unmount(mountpoint: PathBuf) -> BorgResult<()> {
    let mut exit = tokio::process::Command::new("umount")
        .arg(mountpoint)
        .spawn()?;
    tracing::info!("unmount finished with exitcode {:?}", exit.wait().await?);
    Ok(())
}

async fn borg_check(
    repo: &Repository,
    passphrase: Option<Passphrase>,
    progress_channel: CommandResponseSender,
    repair: bool,
) -> BorgResult<bool> {
    let repo_path = repo.path();
    let rsh = repo.borg_options()?.rsh.clone();
    let mut extra_args = vec![];
    if repair {
        extra_args.push("--repair");
    }
    let mut process = tokio::process::Command::new("borg")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env(
            "BORG_PASSPHRASE",
            passphrase.map(|p| p.inner()).unwrap_or_default(),
        )
        .env("BORG_CHECK_I_KNOW_WHAT_I_AM_DOING", "YES")
        .args(
            rsh.map(|r| vec!["--rsh".to_string(), r])
                .unwrap_or_default(),
        )
        .arg("--progress")
        .arg("--log-json")
        .arg("check")
        .args(extra_args)
        .arg(repo_path.clone())
        .spawn()?;

    if let Some(reader) = process.stderr.take() {
        let progress_channel_clone = progress_channel.clone();
        let repo_loc = repo.path();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let bb = tokio::io::BufReader::new(reader);
            let mut lines = bb.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let msg = line
                    .parse::<serde_json::Value>()
                    .ok()
                    .and_then(|jj| jj.get("message").cloned());
                if let Some(msg) = msg {
                    let msg = format!("{}", msg);
                    send_check_progress!(progress_channel_clone, repo_loc.clone(), msg);
                }
            }
        });
    }

    let exit = process.wait().await?;
    if !exit.success() {
        let err = format!("Borg check failed for {repo_path}");
        send_check_complete!(progress_channel, repo_path, Some(err));
    } else {
        send_check_complete!(progress_channel, repo_path, None);
        send_info!(
            progress_channel,
            format!("Verification succeeded for repository: {}", repo)
        );
    }
    Ok(exit.success())
}

use super::backup_provider::BackupProvider;

pub(crate) struct BorgProvider;

impl BorgProvider {}

#[async_trait]
impl BackupProvider for BorgProvider {
    async fn create_backup(
        &self,
        archive_name: String,
        backup_paths: &[PathBuf],
        exclude_patterns: &[String],
        exclude_caches: bool,
        repo: Repository,
        progress_channel: CommandResponseSender,
        completion_semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    ) -> BorgResult<()> {
        // CreateOptions
        let backup_paths = backup_paths
            .iter()
            .map(|path| format!("'{}'", path.to_string_lossy()))
            .collect::<Vec<String>>();

        let mut create_option = borgbackup::common::CreateOptions::new(
            repo.path(),
            archive_name.clone(),
            backup_paths.to_vec(),
            vec![],
        );
        create_option.passphrase = repo.get_passphrase()?.map(|p| p.inner());
        create_option.excludes = exclude_patterns
            .iter()
            .cloned()
            .map(borgbackup::common::Pattern::Shell)
            .collect();
        create_option.exclude_caches = exclude_caches;

        // Convert borgs create progress into ours

        let (create_progress_send, mut create_progress_recv) =
            tokio::sync::mpsc::channel::<borg_async::CreateProgress>(200);

        let common_options = make_common_options(&repo)?;

        let repo_name_clone = repo.path();
        let progress_channel_task = progress_channel.clone();
        tokio::spawn(async move {
            take_repo_lock!(
                progress_channel_task,
                repo,
                "A backup is already in progress for {}, waiting..."
            );
            send_info!(
                progress_channel_task,
                format!("Grabbed repo lock, starting the backup for {}", repo)
            );
            // TODO: I think the UI doesn't update if you issue two backups in a row
            while let Some(progress) = create_progress_recv.recv().await {
                let create_progress = BackupCreateProgress {
                    repository: repo_name_clone.clone(),
                    create_progress: progress.into(),
                };
                if let Err(e) = progress_channel_task
                    .send(CommandResponse::CreateProgress(create_progress))
                    .await
                {
                    tracing::error!("Failed to send CreateProgress update: {}", e);
                }
            }
        });

        // Actually spawn the borg backup

        let progress_channel_clone = progress_channel.clone();
        let completion_semaphore_clone = completion_semaphore.clone();
        tokio::spawn(async move {
            let res =
                borg_async::create_progress(&create_option, &common_options, create_progress_send)
                    .await;
            completion_semaphore_clone.add_permits(1);
            match res {
                Ok(c) => info!(
                    "Archive created successfully in repo {}: {:?}",
                    c.repository.location, c.archive.stats
                ),
                Err(e) => send_error!(
                    progress_channel_clone,
                    format!(
                        "Failed to create archive {} in repo {}: {:?}",
                        create_option.archive, create_option.repository, e
                    )
                ),
            };
        });
        Ok(())
    }

    async fn list_archives(&self, repo: &Repository) -> BorgResult<RepositoryArchives> {
        let list_options = borgbackup::common::ListOptions {
            repository: repo.path(),
            passphrase: repo.get_passphrase()?.map(|p| p.inner()),
        };
        let res = borg_async::list(&list_options, &make_common_options(repo)?)
            .await
            .map_err(|e| anyhow!("Failed to list archives in repo {}: {:?}", repo.path(), e))?;
        Ok(res.into())
    }
    async fn init_repo(
        &self,
        repo_loc: String,
        passphrase: Option<Passphrase>,
        config: RepositoryOptions,
    ) -> BorgResult<()> {
        let encryption_mode = match passphrase {
            Some(passphrase) => EncryptionMode::Repokey(passphrase.inner()),
            None => EncryptionMode::None,
        };
        let init_options = InitOptions::new(repo_loc, encryption_mode);
        borg_async::init(
            &init_options,
            &CommonOptions {
                rsh: config.borg_options()?.rsh.clone(),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow!("Failed to init repo: {}", e))?;
        Ok(())
    }

    async fn mount(
        &self,
        repo: &Repository,
        given_repository_path: String,
        mountpoint: PathBuf,
    ) -> BorgResult<()> {
        if repo.disabled() {
            anyhow::bail!("Attempted to mount disabled repo: {}", repo);
        }
        // See if the path exists, and if not, try to make it
        if let Ok(false) = tokio::fs::try_exists(&mountpoint).await {
            info!(
                "Attempting to create directory for mounting: {}",
                mountpoint.to_string_lossy()
            );
            tokio::fs::create_dir_all(&mountpoint).await?;
        }
        // TODO: Check if this is already mounted!
        let mount_source = if given_repository_path.contains("::") {
            MountSource::Archive {
                archive_name: given_repository_path.clone(),
            }
        } else {
            MountSource::Repository {
                name: repo.path(),
                first_n_archives: None,
                last_n_archives: None,
                glob_archives: None,
            }
        };
        let mut mount_options =
            MountOptions::new(mount_source, mountpoint.to_string_lossy().to_string());
        mount_options.passphrase = repo.get_passphrase()?.map(|p| p.inner());
        borg_async::mount(&mount_options, &make_common_options(repo)?)
            .await
            .map_err(|e| anyhow!("Failed to mount repo {}: {}", repo.path(), e))?;
        info!(
            "Successfully mounted {} at {:?}",
            given_repository_path, mountpoint
        );
        Ok(())
    }

    // TODO: Figure out unused
    #[allow(unused)]
    async fn unmount(&self, mountpoint: PathBuf) -> BorgResult<()> {
        borg_async::umount(
            mountpoint.to_string_lossy().to_string(),
            &CommonOptions::default(),
        )
        .await
        .map_err(|e| anyhow!("Failed to umount path {:?}: {}", mountpoint, e))
    }

    async fn prune(
        &self,
        repo: &Repository,
        prune_options: crate::profiles::PruneOptions,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        take_repo_lock!(progress_channel, repo);
        let mut compact_options = BorgLibPruneOptions::new(repo.path());
        compact_options.passphrase = repo.get_passphrase()?.map(|p| p.inner());
        compact_options.keep_daily = Some(prune_options.keep_daily);
        compact_options.keep_weekly = Some(prune_options.keep_weekly);
        compact_options.keep_monthly = Some(prune_options.keep_monthly);
        compact_options.keep_yearly = Some(prune_options.keep_yearly);
        borg_async::prune(&compact_options, &make_common_options(repo)?)
            .await
            .map_err(|e| anyhow!("Failed to prune repo {}: {:?}", repo.path(), e))?;
        Ok(())
    }
    async fn compact(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<()> {
        let compact_options = borgbackup::common::CompactOptions {
            repository: repo.path(),
        };
        take_repo_lock!(progress_channel, repo);
        borg_async::compact(&compact_options, &make_common_options(repo)?)
            .await
            .map_err(|e| anyhow!("Failed to compact repo {}: {:?}", repo.path(), e))?;
        Ok(())
    }
    async fn check(
        &self,
        repo: &Repository,
        // TODO: Use this
        progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        take_repo_lock!(progress_channel, repo);
        borg_check(repo, repo.get_passphrase()?, progress_channel, false).await
    }
    async fn repair(
        &self,
        repo: &Repository,
        progress_channel: CommandResponseSender,
    ) -> BorgResult<bool> {
        take_repo_lock!(progress_channel, repo);
        borg_check(repo, repo.get_passphrase()?, progress_channel, true).await
    }
}
