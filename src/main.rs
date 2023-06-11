use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, bail, Context};
use borgbackup::asynchronous::CreateProgress;
use chrono::Duration;
use notify::Watcher;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tracing::{error, info};
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

fn determine_directory_size(
    path: PathBuf,
    byte_count: Arc<AtomicU64>,
    exclude_patterns: Vec<String>,
) {
    let patterns = exclude_patterns
        .iter()
        .map(|s| glob::Pattern::new(s.as_str()))
        .collect::<Result<Vec<_>, _>>();
    let patterns = match patterns {
        Ok(pat) => pat,
        Err(e) => {
            error!(
                "Failed to create glob patterns from exclude_patterns: {}",
                e
            );
            vec![]
        }
    };
    let all_files = WalkDir::new(path).into_iter().filter_entry(|entry| {
        !patterns
            .iter()
            .any(|pattern| pattern.matches_path(entry.path()))
    });
    for entry in all_files {
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
        Command::UpdateProfileAndSave(mut profile, op, signal_success) => {
            profile.apply_operation(op).await?;
            profile.save_profile().await?;
            send_info!(
                command_response_send,
                format!("Saved profile '{}'", profile.name()),
                "Failed to send 'Saved profile' message: {}"
            );
            command_response_send
                .send(CommandResponse::ProfileUpdated(profile))
                .await?;
            signal_success.store(true, Ordering::SeqCst);
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
        Command::DetermineDirectorySize(path, byte_count_atomic, exclude_patterns) => {
            tokio::task::spawn_blocking(|| {
                determine_directory_size(path, byte_count_atomic, exclude_patterns)
            });
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
        Command::Compact(repo) => {
            tokio::spawn(async move {
                if let Err(e) = borg::compact(&repo).await {
                    send_error!(command_response_send, format!("Failed to compact: {}", e));
                } else {
                    send_info!(command_response_send, format!("Compacted {}", repo));
                }
            });
            Ok(false)
        }
        Command::Prune(repo, prune_options) => {
            tokio::spawn(async move {
                if let Err(e) = borg::prune(&repo, prune_options).await {
                    send_error!(command_response_send, format!("Failed to prune: {}", e))
                } else {
                    send_info!(command_response_send, format!("Pruned {}", repo));
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
        Command::Quit => Ok(true),
    }
}

fn watch_profile_for_changes(
    profile_path: PathBuf,
    response_send: mpsc::Sender<CommandResponse>,
) -> BorgResult<()> {
    let profile_path_clone = profile_path.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, _>| match res {
            Ok(event) => {
                let is_modify = event.kind.is_modify();
                tracing::debug!("is_modify: {}", is_modify);
                if is_modify {
                    match Profile::blocking_open_path(profile_path_clone.clone()) {
                        Ok(profile) => {
                            if let Err(e) = response_send
                                .blocking_send(CommandResponse::ProfileUpdated(profile))
                            {
                                error!("Failed to send update profile message: {}", e)
                            }
                        }
                        Err(e) => error!("Failed to read profile after modify update: {}", e),
                    }
                }
            }
            Err(e) => error!(
                "Error while watching path <{}>: {}",
                profile_path_clone.to_string_lossy(),
                e
            ),
        })?;

    std::thread::spawn(move || loop {
        if let Err(e) = watcher.watch(&profile_path, notify::RecursiveMode::NonRecursive) {
            error!("Failed to watcher.watch: {}", e)
        }
    });

    Ok(())
}

async fn setup_tui(profile: Option<String>, watch_profile: bool) -> BorgResult<JoinHandle<()>> {
    let profile = Profile::try_open_profile_or_create_default(&profile).await?;
    let (command_send, mut command_recv) = mpsc::channel::<Command>(QUEUE_SIZE);
    let (response_send, response_recv) = mpsc::channel::<CommandResponse>(QUEUE_SIZE);

    // Profile watcher (sends updates when the config file is manually edited)
    if watch_profile {
        watch_profile_for_changes(profile.profile_path()?, response_send.clone())?;
    }

    // Directory Finder
    let mut dir_finder = DirectoryFinder::new();
    if let Err(e) = dir_finder.seed_exclude_patterns(profile.exclude_patterns()) {
        error!("Failed to add exclude patterns: {}", e);
    }
    let dir_finder = Arc::new(Mutex::new(dir_finder));
    let res = std::thread::spawn(move || {
        // TODO: Run watcher task here
        let mut tui = BorgTui::new(profile, command_send, response_recv);
        if let Err(e) = tui.run() {
            error!("Failed to run tui: {}", e);
        }
    });
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

fn generate_system_create_unit(profile_name: &str, timer: bool) -> String {
    if timer {
        "[Unit]
Description=Run borgtui create every day at 9PM

[Timer]
OnCalendar=*-*-* 21:00:00
Persistent=true

[Install]
WantedBy=timers.target"
            .to_string()
    } else {
        format!(
            "[Unit]
Description=BorgTui Create Backup for Profile `{profile_name}`

[Service]
Type=simple
ExecStart=borgtui -p {profile_name} create

[Install]
WantedBy=default.target
",
            profile_name = profile_name,
        )
    }
}

async fn handle_action(
    action: Action,
    profile_name: Option<String>,
    command_response_send: mpsc::Sender<CommandResponse>,
) -> BorgResult<()> {
    match action {
        Action::Init {
            borg_passphrase,
            location,
            rsh,
            do_not_store_in_keyring,
        } => {
            let _ = do_not_store_in_keyring;
            let mut profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            borg::init(borg_passphrase.clone(), location.clone(), rsh.clone()).await?;
            profile.add_repository(location.clone(), Some(borg_passphrase), rsh, false)?;
            profile.save_profile().await?;
            info!("Added repo: {}", location);
            Ok(())
        }
        Action::Create => {
            let profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            info!("Creating backup for profile {}", profile);
            let handle =
                borg::create_backup_with_notification(&profile, command_response_send).await?;
            handle.await?;
            Ok(())
        }
        Action::Add { directory } => {
            let mut profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            profile.add_backup_path(directory.clone()).await?;
            profile.save_profile().await?;
            info!("Added {} to profile {}", directory.display(), profile);
            Ok(())
        }
        Action::Remove { directory } => {
            let mut profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            profile.remove_backup_path(&directory);
            profile.save_profile().await?;
            info!("Removed {} from profile {}", directory.display(), profile);
            Ok(())
        }
        Action::AddRepo {
            repository,
            no_encryption,
            borg_passphrase,
            rsh,
            store_passphase_in_cleartext,
        } => {
            // TODO: Check if repo is valid
            let mut profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            if profile.has_repository(&repository) {
                bail!(
                    "Repository {} already exists in profile {}",
                    repository,
                    profile
                );
            }
            let passphrase = match borg_passphrase {
                Some(passphrase) => Some(passphrase),
                None => {
                    if no_encryption {
                        None
                    } else {
                        try_get_initial_repo_password()?
                    }
                }
            };
            profile.add_repository(
                repository.clone(),
                passphrase,
                rsh,
                store_passphase_in_cleartext,
            )?;
            profile.save_profile().await?;
            info!("Added repository {} to profile {}", repository, profile);
            Ok(())
        }
        Action::List => {
            let profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            let timeout_duration_secs = profile.action_timeout_seconds() as i64;
            for repo in profile.repositories() {
                let list_archives_per_repo = match tokio::time::timeout(
                    Duration::seconds(timeout_duration_secs).to_std().unwrap(),
                    borg::list_archives(repo),
                )
                .await
                {
                    Ok(list_archive_result) => list_archive_result?,
                    Err(_timeout_error) => {
                        error!(
                            "Timeout ({}s) while attempting to list repo {}",
                            timeout_duration_secs, repo
                        );
                        continue;
                    }
                };
                let repo = list_archives_per_repo.repository.location;
                for archives in list_archives_per_repo.archives {
                    info!("{}::{}", repo, archives.name);
                }
            }
            Ok(())
        }
        Action::Compact => {
            let profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            for repo in profile.repositories() {
                borg::compact(repo).await?;
                info!("Finished compacting {}", repo);
            }
            Ok(())
        }
        Action::Prune => {
            let profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            for repo in profile.repositories() {
                borg::prune(repo, profile.prune_options()).await?;
                info!("Finished pruning {}", repo);
            }
            Ok(())
        }
        Action::Mount {
            repository_path,
            mountpoint,
        } => {
            let profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            let repo_name = match repository_path.find("::") {
                Some(loc) => repository_path[..loc].to_string(),
                None => repository_path.to_string(),
            };
            tracing::debug!("Figured repo name is: {}", repo_name);
            let repo = profile
                .repositories()
                .iter()
                .find(|repo| repo.path == repo_name)
                .ok_or_else(|| anyhow!("Could not find repo: {}", repository_path))?;
            borg::mount(repo, repository_path, mountpoint).await?;
            Ok(())
        }
        Action::Umount { mountpoint } => {
            borg::umount(mountpoint.clone()).await?;
            info!("Successfully unmounted {}", mountpoint.to_string_lossy());
            Ok(())
        }
        Action::SystemdCreateUnit {
            install,
            install_path,
            timer,
        } => {
            let profile = Profile::try_open_profile_or_create_default(&profile_name).await?;
            let systemd_unit_contents = generate_system_create_unit(profile.name(), timer);
            let extension = if timer { "timer" } else { "service" };
            if install || install_path.is_some() {
                let home_dir = dirs::home_dir()
                    .ok_or_else(|| anyhow!("Couldn't find a home directory. Is $HOME set?"))?;
                let install_path = install_path.unwrap_or_else(|| {
                    home_dir.join(format!(
                        ".config/systemd/user/borgtui-create-{}.{}",
                        profile.name(),
                        extension
                    ))
                });
                info!("{:?}", install_path);
                if let Some(parent_path) = install_path.parent() {
                    tokio::fs::create_dir_all(parent_path).await?;
                }
                tokio::fs::File::create(&install_path)
                    .await?
                    .write_all(systemd_unit_contents.as_bytes())
                    .await?;
                let unit_type = if timer { "timer unit" } else { "create unit" };
                info!(
                    "Installed systemd {} for {} at {}",
                    unit_type,
                    profile,
                    install_path.to_string_lossy()
                );
            } else {
                println!("{}", systemd_unit_contents)
            }
            Ok(())
        }
        Action::ShellCompletion { shell } => {
            cli::print_shell_completion(&shell)?;
            Ok(())
        }
        Action::ManPage { man_root } => cli::print_manpage(man_root).await,
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
                    if let Err(e) = handle_action(action, args.borgtui_profile, send).await {
                        error!("Error handling CLI action: {}", e)
                    };
                    handle.await
                }
                // TODO: Is failing to join here a bad idea?
                None => {
                    match setup_tui(args.borgtui_profile, args.watch_profile).await {
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
    if let Some(join_handle) = tui_join_handle {
        join_handle.join().unwrap();
    }
    Ok(())
}
