use std::{num::NonZeroU16, sync::Arc, time::Instant};

use crate::{
    borgtui::CommandResponse,
    profiles::{Profile, Repository},
    types::{log_on_error, send_error, send_info, BorgResult},
};
use anyhow::anyhow;
use borgbackup::{
    asynchronous as borg_async,
    common::{
        CommonOptions, CompactOptions, EncryptionMode, InitOptions, ListOptions, PruneOptions,
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

// TODO: Make a wrapper type for the passphrase
pub(crate) async fn init(borg_passphrase: String, repo_loc: String) -> BorgResult<()> {
    let init_options = InitOptions::new(repo_loc, EncryptionMode::Repokey(borg_passphrase));
    borg_async::init(&init_options, &CommonOptions::default())
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

        let repo_name_clone = create_option.repository.clone();
        let progress_channel_task = progress_channel.clone();
        tokio::spawn(async move {
            if repo.lock.try_lock().is_err() {
                send_info!(
                    progress_channel_task,
                    format!("A backup is already in progress for {}, waiting...", repo)
                );
            }
            let _backup_guard = repo.lock.lock().await;
            send_info!(
                progress_channel_task,
                format!("Grabbed repo lock, starting the backup for {}", repo)
            );
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
            let res = borg_async::create_progress(
                &create_option,
                &CommonOptions::default(),
                create_progress_send,
            )
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
        repository: repo.get_path(),
        passphrase: repo.get_passphrase()?,
    };
    borg_async::list(&list_options, &CommonOptions::default())
        .await
        .map_err(|e| {
            anyhow!(
                "Failed to list archives in repo {}: {:?}",
                repo.get_path(),
                e
            )
        })
}

pub(crate) async fn compact(repo: &Repository) -> BorgResult<()> {
    let compact_options = CompactOptions {
        repository: repo.get_path(),
    };
    borg_async::compact(&compact_options, &CommonOptions::default())
        .await
        .map_err(|e| anyhow!("Failed to compact repo {}: {:?}", repo.get_path(), e))
}

pub(crate) async fn prune(repo: &Repository) -> BorgResult<()> {
    let mut compact_options = PruneOptions::new(repo.get_path());
    compact_options.passphrase = repo.get_passphrase()?;
    compact_options.keep_daily = NonZeroU16::new(7);
    compact_options.keep_weekly = NonZeroU16::new(4);
    compact_options.keep_monthly = NonZeroU16::new(12);
    borg_async::prune(&compact_options, &CommonOptions::default())
        .await
        .map_err(|e| anyhow!("Failed to prune repo {}: {:?}", repo.get_path(), e))
}
