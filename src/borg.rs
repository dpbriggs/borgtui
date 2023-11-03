use std::{path::PathBuf, sync::Arc, time::Instant};

use crate::{
    borgtui::CommandResponse,
    profiles::{Passphrase, Profile, Repository},
    types::{log_on_error, send_error, send_info, take_repo_lock, BorgResult},
};
use anyhow::{anyhow, bail};
use borgbackup::{
    asynchronous as borg_async,
    common::{
        CommonOptions, CompactOptions, EncryptionMode, InitOptions, ListOptions, MountOptions,
        MountSource, PruneOptions,
    },
    output::list::ListRepository,
};
use notify_rust::Notification;
use tokio::{
    sync::{mpsc, Semaphore},
    task::JoinHandle,
};
use tracing::{error, info};

fn archive_name(name: &str) -> String {
    format!(
        "{}-{}",
        name,
        chrono::Local::now().format("%Y-%m-%d:%H:%M:%S")
    )
}

#[derive(Debug)]
pub(crate) struct BorgCreateProgress {
    pub(crate) repository: String,
    pub(crate) create_progress: borg_async::CreateProgress,
}

pub(crate) async fn init(
    borg_passphrase: Passphrase,
    repo_loc: String,
    rsh: Option<String>,
) -> BorgResult<()> {
    let init_options = InitOptions::new(repo_loc, EncryptionMode::Repokey(borg_passphrase.inner()));
    borg_async::init(
        &init_options,
        &CommonOptions {
            rsh,
            ..Default::default()
        },
    )
    .await
    .map_err(|e| anyhow!("Failed to init repo: {}", e))?;
    Ok(())
}

pub(crate) async fn create_backup_with_notification(
    profile: &Profile,
    progress_channel: mpsc::Sender<CommandResponse>,
) -> BorgResult<JoinHandle<()>> {
    let completion_semaphore = Arc::new(Semaphore::new(0));
    let num_repos = profile.num_repos();
    let profile_name = format!("{}", profile);
    let completion_semaphore_clone = completion_semaphore.clone();
    let join_handle = tokio::spawn(async move {
        let start_time = Instant::now();
        if let Err(e) = completion_semaphore_clone
            .acquire_many(num_repos as u32)
            .await
        {
            error!("Failed to wait on completion semaphore: {}", e);
        } else {
            let elapsed_duration = start_time.elapsed();
            let nicely_formatted = format!(
                "{:0>2}:{:0>2}:{:0>2}",
                elapsed_duration.as_secs() / 60 / 60,
                elapsed_duration.as_secs() / 60 % 60,
                elapsed_duration.as_secs() % 60
            );
            info!(
                "Completed backup for profile {} in {}",
                profile_name, nicely_formatted
            );
            log_on_error!(
                Notification::new()
                    .summary(&format!("Backup complete for {}", profile_name))
                    .body(&format!("Completed in {}", nicely_formatted))
                    .show_async()
                    .await,
                "Failed to show notification: {}"
            );
        }
    });
    create_backup_internal(profile, progress_channel, completion_semaphore).await?;
    Ok(join_handle)
}
pub(crate) async fn create_backup(
    profile: &Profile,
    progress_channel: mpsc::Sender<CommandResponse>,
) -> BorgResult<()> {
    create_backup_internal(profile, progress_channel, Arc::new(Semaphore::new(0))).await
}

pub(crate) async fn create_backup_internal(
    profile: &Profile,
    progress_channel: mpsc::Sender<CommandResponse>,
    completion_semaphore: Arc<Semaphore>,
) -> BorgResult<()> {
    let archive_name = archive_name(profile.name());
    for (create_option, repo) in profile.borg_create_options(archive_name)? {
        info!(
            "Creating archive {} in repository {}",
            create_option.archive, create_option.repository
        );
        let (create_progress_send, mut create_progress_recv) =
            mpsc::channel::<borg_async::CreateProgress>(100);

        let common_options = repo.common_options();

        let repo_name_clone = create_option.repository.clone();
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
                let create_progress = BorgCreateProgress {
                    repository: repo_name_clone.clone(),
                    create_progress: progress,
                };
                if let Err(e) = progress_channel_task
                    .send(CommandResponse::CreateProgress(create_progress))
                    .await
                {
                    error!("Failed to send CreateProgress update: {}", e);
                }
            }
        });

        let progress_channel_clone = progress_channel.clone();
        let completion_semaphore_clone = completion_semaphore.clone();
        tokio::spawn(async move {
            let res =
                borg_async::create_progress(&create_option, &common_options, create_progress_send)
                    .await;
            completion_semaphore_clone.add_permits(1);
            match res {
                Ok(c) => info!("Archive created successfully: {:?}", c.archive.stats),
                // TODO: Send this error message along that channel
                Err(e) => send_error!(
                    progress_channel_clone,
                    format!(
                        "Failed to create archive {} in repo {}: {:?}",
                        create_option.archive, create_option.repository, e
                    )
                ),
            };
        });
    }
    Ok(())
}

pub(crate) async fn list_archives(repo: &Repository) -> BorgResult<ListRepository> {
    let list_options = ListOptions {
        repository: repo.path(),
        passphrase: repo.get_passphrase()?,
    };
    borg_async::list(&list_options, &repo.common_options())
        .await
        .map_err(|e| anyhow!("Failed to list archives in repo {}: {:?}", repo.path(), e))
}

pub(crate) async fn compact(
    repo: &Repository,
    progress_channel: mpsc::Sender<CommandResponse>,
) -> BorgResult<()> {
    let compact_options = CompactOptions {
        repository: repo.path(),
    };
    take_repo_lock!(progress_channel, repo);
    borg_async::compact(&compact_options, &repo.common_options())
        .await
        .map_err(|e| anyhow!("Failed to compact repo {}: {:?}", repo.path(), e))
}

pub(crate) async fn prune(
    repo: &Repository,
    prune_options: crate::profiles::PruneOptions,
    progress_channel: mpsc::Sender<CommandResponse>,
) -> BorgResult<()> {
    take_repo_lock!(progress_channel, repo);
    let mut compact_options = PruneOptions::new(repo.path());
    compact_options.passphrase = repo.get_passphrase()?;
    compact_options.keep_daily = Some(prune_options.keep_daily);
    compact_options.keep_weekly = Some(prune_options.keep_weekly);
    compact_options.keep_monthly = Some(prune_options.keep_monthly);
    compact_options.keep_yearly = Some(prune_options.keep_yearly);
    borg_async::prune(&compact_options, &repo.common_options())
        .await
        .map_err(|e| anyhow!("Failed to prune repo {}: {:?}", repo.path(), e))
}

pub(crate) async fn mount(
    repo: &Repository,
    // This could be a repo path (/backup/borgrepo) or an archive (/backup/borgrepo::archive_name)
    given_repository_path: String,
    mountpoint: PathBuf,
) -> BorgResult<()> {
    if repo.disabled() {
        bail!("Attempted to mount disabled repo: {}", repo);
    }
    // See if the path exists, and if not, try to make it
    if let Ok(false) = tokio::fs::try_exists(&mountpoint).await {
        info!(
            "Attempting to create directory for mounting: {}",
            mountpoint.to_string_lossy()
        );
        tokio::fs::create_dir_all(&mountpoint).await?;
    }
    let mount_source = if given_repository_path.contains("::") {
        MountSource::Archive {
            archive_name: given_repository_path,
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
    mount_options.passphrase = repo.get_passphrase()?;
    borg_async::mount(&mount_options, &repo.common_options())
        .await
        .map_err(|e| anyhow!("Failed to mount repo {}: {}", repo.path(), e))
}

pub(crate) async fn umount(mount_point: PathBuf) -> BorgResult<()> {
    borg_async::umount(
        mount_point.to_string_lossy().to_string(),
        &CommonOptions::default(),
    )
    .await
    .map_err(|e| anyhow!("Failed to umount path {:?}: {}", mount_point, e))
}

pub(crate) async fn check_with_notification(repo: &Repository) -> BorgResult<()> {
    let repo_path = repo.path();
    let rsh = repo.rsh();
    let passphrase = repo.get_passphrase()?;
    let exit = tokio::process::Command::new("borg")
        .env("BORG_PASSPHRASE", passphrase.unwrap_or_default())
        .args(
            rsh.map(|r| vec!["--rsh".to_string(), r])
                .unwrap_or_default(),
        )
        .arg("--progress")
        .arg("check")
        .arg(repo_path)
        .spawn()?
        .wait()
        .await?;
    if !exit.success() {
        error!("Verification failed for repository: {}", repo);
        Notification::new()
            .summary(&format!("Verification Failed for {}!", repo))
            .body("Please check BorgTUI's logs for more information.")
            .show_async()
            .await?;
    } else {
        info!("Verification succeeded for repository: {}", repo);
    }
    Ok(())
}
