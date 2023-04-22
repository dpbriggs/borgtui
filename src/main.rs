use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{bail, Context};
use borgbackup::asynchronous::CreateProgress;
use tokio::sync::mpsc;
use tracing::{debug, error, info};
use tracing_subscriber::FmtSubscriber;
use types::{log_on_error, DirectoryFinder};
use walkdir::WalkDir;

use crate::borgtui::{BorgTui, Command, CommandResponse};
use crate::cli::Action;
use crate::profiles::Profile;
use crate::types::{send_error, send_info, BorgResult, PrettyBytes};

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

fn determine_directory_size(path: PathBuf, byte_count: Arc<AtomicU64>) {
    for entry in WalkDir::new(path) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                error!("Failed to read entry: {}", e);
                continue;
            }
        };
        match entry.metadata() {
            Ok(metadata) => {
                byte_count.fetch_add(metadata.len(), Ordering::SeqCst);
            }
            Err(e) => {
                error!("Failed to obtain metadata for entry {:?}: {}", entry, e);
            }
        }
    }
}

/// Returns Ok(true) to exit the program.
async fn handle_tui_command(
    command: Command,
    command_response_send: mpsc::Sender<CommandResponse>,
    directory_finder: Arc<Mutex<DirectoryFinder>>,
) -> BorgResult<bool> {
    match command {
        Command::CreateBackup(profile) => {
            send_info!(
                command_response_send,
                format!("Starting backup of profile {}", &profile),
                "Failed to send backup start signal: {}"
            );
            borg::create_backup(&profile, command_response_send).await?;
            Ok(false)
        }
        Command::SaveProfile(profile) => {
            send_info!(
                command_response_send,
                format!("Saved profile '{}'", profile.name()),
                "Failed to save profile: {}"
            );
            if let Err(e) = profile.save_profile().await {
                send_error!(
                    command_response_send,
                    format!("Failed to save profile: {}", e)
                );
            };
            Ok(false)
        }

        Command::DetermineDirectorySize(path, byte_count_atomic) => {
            tokio::task::spawn_blocking(|| determine_directory_size(path, byte_count_atomic));
            Ok(false)
        }
        Command::ListArchives(repo) => {
            tokio::spawn(async move {
                match borg::list_archives(&repo).await {
                    Ok(res) => {
                        if let Err(e) = command_response_send
                            .send(CommandResponse::ListArchiveResult(res))
                            .await
                        {
                            error!("Failed to send ListArchiveResult for {}: {}", repo, e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to list archives for {}: {}", repo, e);
                    }
                }
            });
            Ok(false)
        }
        Command::GetDirectorySuggestionsFor(directory) => {
            // TODO: This blocks command handling, right?
            tokio::task::spawn_blocking(move || {
                let mut dir_finder =
                    log_on_error!(directory_finder.lock(), "failed to lock dir_finder: {}");
                log_on_error!(
                    dir_finder.update_guess(&directory),
                    "failed to update guess: {}"
                );
                let suggestions = log_on_error!(
                    dir_finder.suggestions(&directory, 20),
                    "failed to obtain suggestions: {}"
                );
                log_on_error!(
                    command_response_send
                        .blocking_send(CommandResponse::SuggestionResults(suggestions)),
                    "Failed to send suggestion results: {}"
                );
            });
            Ok(false)
        }
        Command::AddBackupPathAndSave(mut profile, path, successfully_saved) => {
            let path = PathBuf::try_from(path)?;
            profile.add_backup_path(path).await?;
            profile.save_profile().await?;
            successfully_saved.store(true, Ordering::SeqCst);
            if let Err(e) = command_response_send
                .send(CommandResponse::ProfileUpdated(profile))
                .await
            {
                error!("Failed to send updated profile: {}", e)
            }
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
    let dir_finder = Arc::new(Mutex::new(DirectoryFinder::new()));
    while let Some(command) = command_recv.recv().await {
        match handle_tui_command(command, response_send.clone(), dir_finder.clone()).await {
            Ok(true) => return Ok(res),
            Err(e) => {
                error!("Failed to handle tui command: {}", e);
                send_error!(response_send, format!("{}", e));
            }
            _ => {}
        }
    }
    Ok(res)
}

async fn handle_command_response(command_response_recv: mpsc::Receiver<CommandResponse>) {
    let mut command_response_recv = command_response_recv;
    while let Some(message) = command_response_recv.recv().await {
        match message {
            CommandResponse::CreateProgress(msg) => match msg.create_progress {
                CreateProgress::Progress {
                    original_size,
                    compressed_size,
                    deduplicated_size,
                    nfiles,
                    path,
                } => info!(
                    "{}: {} -> {} -> {} ({} files)",
                    path,
                    PrettyBytes(original_size),
                    PrettyBytes(compressed_size),
                    PrettyBytes(deduplicated_size),
                    nfiles
                ),
                CreateProgress::Finished => {
                    info!("Finished backup for {}", msg.repository)
                }
            },
            CommandResponse::Info(info_log) => info!(info_log),
            CommandResponse::ListArchiveResult(list_archive_result) => {
                // TODO: Print this out in a more informative way
                info!("{:?}", list_archive_result)
            }
            CommandResponse::SuggestionResults(_) => {
                error!("Received SuggestionResults in non-interactive!")
            }
            CommandResponse::Error(error_message) => error!(error_message),
            CommandResponse::ProfileUpdated(_profile) => info!("Profile updated."),
        }
    }
}

async fn handle_action(
    action: Action,
    command_response_send: mpsc::Sender<CommandResponse>,
) -> BorgResult<()> {
    match action {
        Action::Init => {
            todo!()
        }
        Action::Create { profile } => {
            let profile = Profile::try_open_profile_or_create_default(&profile).await?;
            info!("Creating backup for profile {}", profile);
            borg::create_backup(&profile, command_response_send).await?;
            Ok(())
        }
        Action::Add { directory, profile } => {
            let mut profile = Profile::try_open_profile_or_create_default(&profile).await?;
            profile.add_backup_path(directory.clone()).await?;
            profile.save_profile().await?;
            info!("Added {} to profile {}", directory.display(), profile);
            Ok(())
        }
        Action::Remove { directory, profile } => {
            let mut profile = Profile::try_open_profile_or_create_default(&profile).await?;
            profile.remove_backup_path(&directory);
            profile.save_profile().await?;
            info!("Removed {} from profile {}", directory.display(), profile);
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
    let is_noninteractive = args.action.is_some();
    let file_appender = tracing_appender::rolling::hourly("/tmp", "borgtui.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = FmtSubscriber::builder().with_max_level(tracing::Level::DEBUG);
    if is_noninteractive {
        tracing::subscriber::set_global_default(subscriber.finish())
            .with_context(|| "setting default subscriber failed")?;
    } else {
        tracing::subscriber::set_global_default(subscriber.with_writer(non_blocking).finish())
            .with_context(|| "setting default subscriber failed")?;
    }

    let mut tui_join_handle = None;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let res = match args.action {
                Some(action) => {
                    let (send, recv) = mpsc::channel::<CommandResponse>(QUEUE_SIZE);
                    let handle = tokio::spawn(async move { handle_command_response(recv).await });
                    if let Err(e) = handle_action(action, send).await {
                        error!("Error handling CLI action: {}", e)
                    };
                    handle.await
                }
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
