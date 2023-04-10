use std::thread::JoinHandle;

use anyhow::{bail, Context};
use borgbackup::asynchronous::CreateProgress;
// use once_cell::sync::OnceCell;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use tracing_subscriber::FmtSubscriber;

use crate::borg::MyCreateProgress;
use crate::borgtui::{BorgTui, Command, CommandResponse};
use crate::cli::Action;
use crate::profiles::Profile;
use crate::types::BorgResult;

mod borg;
mod borgtui;
mod cli;
mod profiles;
mod types;

const QUEUE_SIZE: usize = 1000;

fn try_get_initial_repo_password() -> BorgResult<Option<String>> {
    if atty::is(atty::Stream::Stdin) {
        rpassword::read_password()
            .with_context(|| "Failed to read password from tty")
            .map(|pass| if pass.is_empty() { None } else { Some(pass) })
    } else {
        bail!("Password must be provided via an interactive terminal!")
    }
}

async fn handle_tui_command(
    command: Command,
    command_response_send: mpsc::Sender<CommandResponse>,
) -> BorgResult<bool> {
    match command {
        Command::CreateBackup(profile) => {
            let (send, mut recv) = mpsc::channel::<MyCreateProgress>(QUEUE_SIZE);
            tokio::spawn(async move {
                while let Some(msg) = recv.recv().await {
                    let res = command_response_send
                        .send(CommandResponse::CreateProgress(msg))
                        .await;
                    if let Err(e) = res {
                        error!("Failed to send create progress: {}", e);
                    }
                }
            });
            borg::create_backup(&profile, send).await?;
            Ok(false)
        }
        Command::Quit => Ok(true),
    }
}

async fn setup_tui() -> BorgResult<JoinHandle<()>> {
    let profile = Profile::try_open_profile_or_create_default(&None).await?;
    let (command_send, mut command_recv) = mpsc::channel::<Command>(QUEUE_SIZE);
    let (response_send, response_recv) = mpsc::channel::<CommandResponse>(QUEUE_SIZE);
    let res = std::thread::spawn(move || {
        let mut tui = BorgTui::new(profile, command_send, response_recv);
        if let Err(e) = tui.run() {
            error!("Failed to run tui: {}", e);
        }
    });
    while let Some(command) = command_recv.recv().await {
        match handle_tui_command(command, response_send.clone()).await {
            Ok(true) => return Ok(res),
            Err(e) => error!("Failed to handle tui command: {}", e),
            _ => {}
        }
    }
    Ok(res)
}

async fn handle_action(action: Action) -> BorgResult<()> {
    match action {
        Action::Init => {
            todo!()
        }
        Action::Create { profile } => {
            let profile = Profile::try_open_profile_or_create_default(&profile).await?;
            info!("Creating backup for profile {}", profile);
            let (sender, mut receiver) = mpsc::channel::<MyCreateProgress>(QUEUE_SIZE);
            tokio::spawn(async move {
                while let Some(msg) = receiver.recv().await {
                    match msg.create_progress {
                        CreateProgress::Progress {
                            original_size,
                            compressed_size,
                            deduplicated_size,
                            nfiles,
                            path,
                        } => info!(
                            "{}: {} -> {} -> {} ({} files)",
                            path, original_size, compressed_size, deduplicated_size, nfiles
                        ),
                        CreateProgress::Finished => info!("Finished backup"),
                    }
                }
            });
            borg::create_backup(&profile, sender).await?;
            Ok(())
        }
        Action::Add { directory, profile } => {
            let mut profile = Profile::try_open_profile_or_create_default(&profile).await?;
            profile.add_backup_path(directory.clone()).await?;
            profile.save_profile().await?;
            info!("Added {} to profile {}", directory, profile);
            Ok(())
        }
        Action::Remove { directory, profile } => {
            let mut profile = Profile::try_open_profile_or_create_default(&profile).await?;
            profile.remove_backup_path(&directory);
            profile.save_profile().await?;
            info!("Added {} to profile {}", directory, profile);
            Ok(())
        }
        Action::AddRepo {
            repository,
            profile,
            no_encryption,
            encryption_passphrase,
            store_passphase_in_cleartext,
        } => {
            // TODO: Check if repo is valid
            let mut profile = Profile::try_open_profile_or_create_default(&profile).await?;
            if profile.has_repository(&repository) {
                bail!(
                    "Repository {} already exists in profile {}",
                    repository,
                    profile
                );
            }
            let passphrase = match encryption_passphrase {
                Some(passphrase) => Some(passphrase),
                None => {
                    if no_encryption {
                        None
                    } else {
                        try_get_initial_repo_password()?
                    }
                }
            };
            profile.add_repository(repository.clone(), passphrase, store_passphase_in_cleartext)?;
            profile.save_profile().await?;
            info!("Added repository {} to profile {}", repository, profile);
            Ok(())
        }
        Action::List { profile } => {
            let profile = Profile::try_open_profile_or_create_default(&profile).await?;
            for repo in profile.repositories() {
                borg::list_archives(repo).await?;
            }
            Ok(())
        }
    }
}

fn main() -> BorgResult<()> {
    let args = cli::get_args();
    let file_appender = tracing_appender::rolling::hourly("/tmp", "borgtui.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(non_blocking)
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .with_context(|| "setting default subscriber failed")?;

    info!("test");

    let mut tui_join_handle = None;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let res = match args.action {
                Some(action) => handle_action(action).await,
                // TODO: Is failing to join here a bad idea?
                None => {
                    match setup_tui().await {
                        Ok(join_handle) => tui_join_handle = Some(join_handle),
                        Err(e) => error!("Failed to setup tui: {}", e),
                    }
                    Ok(())
                }
            };

            if let Err(e) = res {
                error!("Error: {}", e);
                std::process::exit(1);
            }
        });
    debug!("below tokio runtime");
    if let Some(join_handle) = tui_join_handle {
        join_handle.join().unwrap();
    }
    Ok(())
}
