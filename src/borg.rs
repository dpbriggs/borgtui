use tokio::sync::mpsc;

use crate::{
    borgtui::CommandResponse,
    profiles::{Profile, Repository},
    types::BorgResult,
};
use anyhow::bail;
use borgbackup::{
    asynchronous as borg_async,
    common::{CommonOptions, ListOptions},
};
use tracing::{error, info};

fn archive_name(name: &str) -> String {
    format!(
        "{}-{}",
        name,
        chrono::Local::now().format("%Y-%m-%d:%H:%M:%S")
    )
}

// TODO: Better name
#[derive(Debug)]
pub(crate) struct BorgCreateProgress {
    pub(crate) repository: String,
    pub(crate) create_progress: borg_async::CreateProgress,
}

pub(crate) async fn create_backup(
    profile: &Profile,
    progress_channel: mpsc::Sender<CommandResponse>,
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
        let progress_channel = progress_channel.clone();
        tokio::spawn(async move {
            // TODO:
            if repo.lock.try_lock().is_err() {
                progress_channel
                    .send(CommandResponse::Info(format!(
                        "A backup is already in progress for {}, waiting...",
                        repo
                    )))
                    .await
                    .unwrap();
            }
            let _backup_guard = repo.lock.lock().await;
            progress_channel
                .send(CommandResponse::Info(format!(
                    "Grabbed repo lock, starting the backup for {}",
                    repo
                )))
                .await
                .unwrap();
            while let Some(progress) = create_progress_recv.recv().await {
                let create_progress = BorgCreateProgress {
                    repository: repo_name_clone.clone(),
                    create_progress: progress,
                };
                progress_channel
                    .send(CommandResponse::CreateProgress(create_progress))
                    .await
                    .unwrap();
            }
        });

        tokio::spawn(async move {
            match borgbackup::asynchronous::create_progress(
                &create_option,
                &CommonOptions::default(),
                create_progress_send,
            )
            .await
            {
                Ok(c) => info!("Archive created successfully: {:?}", c.archive.stats),
                // TODO: Send this error message along that channel
                Err(e) => error!(
                    "Failed to create archive {} in repo {}: {:?}",
                    create_option.archive, create_option.repository, e
                ),
            };
        });
    }
    Ok(())
}

pub(crate) async fn list_archives(repo: &Repository) -> BorgResult<()> {
    let list_options = ListOptions {
        repository: repo.get_path(),
        passphrase: repo.get_passphrase()?,
    };
    match borg_async::list(&list_options, &CommonOptions::default()).await {
        Ok(l) => {
            info!("Archives in repo {}: {:?}", repo.get_path(), l);
            // TODO: This is a bug in borgbackup, these fields should be public
            // for archive in l.archives {
            //     info!("Archive: {:?}", archive);
            // }
        }
        Err(e) => bail!(
            "Failed to list archives in repo {}: {:?}",
            repo.get_path(),
            e
        ),
    }
    Ok(())
}
