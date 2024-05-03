use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, bail, Context};
use borgbackup::asynchronous::CreateProgress;
use chrono::Duration;
use notify::Watcher;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info};
use tracing_subscriber::FmtSubscriber;
use types::{log_on_error, show_notification, DirectoryFinder, EXTENDED_NOTIFICATION_DURATION};
use walkdir::WalkDir;

use crate::borgtui::{BorgTui, Command, CommandResponse};
use crate::cli::Action;
use crate::profiles::{Encryption, Profile};
use crate::types::{send_error, send_info, BorgResult, PrettyBytes};

mod borg;
mod borgtui;
mod cli;
mod profiles;
mod types;

const QUEUE_SIZE: usize = 1000;

/// Open a file path in a detached GUI file manager.
fn open_path_in_gui_file_manager<P: AsRef<Path>>(path: P) -> BorgResult<()> {
    open::that_detached(path.as_ref())?;
    Ok(())
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
                send_info!(
                    command_response_send,
                    format!("Compacting {}", repo),
                    "Failed to send start compacting info: {}"
                );
                if let Err(e) = borg::compact(&repo, command_response_send.clone()).await {
                    send_error!(command_response_send, format!("Failed to compact: {}", e));
                } else {
                    send_info!(command_response_send, format!("Compacted {}", repo));
                }
            });
            Ok(false)
        }
        Command::Prune(repo, prune_options) => {
            tokio::spawn(async move {
                send_info!(
                    command_response_send,
                    format!("Pruning {}", repo),
                    "Failed to send start prune info: {}"
                );
                if let Err(e) =
                    borg::prune(&repo, prune_options, command_response_send.clone()).await
                {
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
                    dir_finder.suggestions(&directory, 30),
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
        Command::Mount(repo, repo_or_archive, mountpoint) => {
            let mountpoint_p = PathBuf::from(mountpoint.clone());
            tokio::spawn(async move {
                if let Err(e) =
                    borg::mount(&repo, repo_or_archive.clone(), mountpoint_p.clone()).await
                {
                    send_error!(command_response_send, format!("Failed to mount: {}", e))
                } else {
                    send_info!(
                        command_response_send,
                        format!("Successfully mounted {}", repo_or_archive)
                    );
                    log_on_error!(
                        command_response_send
                            .send(CommandResponse::MountResult(repo_or_archive, mountpoint))
                            .await,
                        "Failed to send suggestion results: {}"
                    );
                    if let Err(e) = open_path_in_gui_file_manager(mountpoint_p) {
                        send_error!(
                            command_response_send,
                            format!("Failed to open file manager: {}", e.to_string())
                        );
                    }
                }
            });
            Ok(false)
        }
        Command::Unmount(mountpoint) => {
            // TODO: Properly join all of this.
            tokio::spawn(async move {
                match borg::umount(PathBuf::from(mountpoint.clone())).await {
                    Ok(_) => {
                        send_info!(
                            command_response_send,
                            format!("Successfully unmounted {}", mountpoint)
                        )
                    }
                    Err(e) => {
                        send_error!(
                            command_response_send,
                            format!("Failed to unmount: {}", e.to_string())
                        );
                    }
                }
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
    let profile = Profile::open_or_create(&profile).await?;
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
                    "[{}] {}: {} -> {} -> {} ({} files)",
                    msg.repository,
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
            CommandResponse::MountResult(_, _) => {
                error!("Received MountResult in non-interactive!")
            }
            CommandResponse::Error(error_message) => error!(error_message),
            CommandResponse::ProfileUpdated(_profile) => info!("Profile updated."),
        }
    }
}

fn generate_system_unit(profile_name: &str, timer: bool, action: &str, calendar: &str) -> String {
    if timer {
        format!(
            "[Unit]
Description=Run BorgTUI {action} on a schedule (\"{calendar}\")

[Timer]
OnCalendar={calendar}
Persistent=true

[Install]
WantedBy=timers.target"
        )
    } else {
        format!(
            "[Unit]
Description=BorgTUI {action} for profile `{profile_name}`

[Service]
Type=simple
ExecStart=borgtui -p {profile_name} {action}

[Install]
WantedBy=default.target
"
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
            passphrase_loc,
            location,
            rsh,
        } => {
            let mut profile = Profile::open_or_create(&profile_name).await?;
            let borg_passphrase = passphrase_loc.get_passphrase()?;
            borg::init(borg_passphrase.clone(), location.clone(), rsh.clone()).await?;
            profile.add_repository(location.clone(), passphrase_loc, rsh)?;
            profile.save_profile().await?;
            info!("Initialized Repository '{}' in {}", location, profile);
            Ok(())
        }
        Action::Create => {
            let profile = Profile::open_or_create(&profile_name).await?;
            info!("Creating backup for profile {}", profile);
            let handle =
                borg::create_backup_with_notification(&profile, command_response_send).await?;
            handle.await?;
            Ok(())
        }
        Action::Add { directory } => {
            let mut profile = Profile::open_or_create(&profile_name).await?;
            profile.add_backup_path(directory.clone()).await?;
            profile.save_profile().await?;
            info!("Added {} to profile {}", directory.display(), profile);
            Ok(())
        }
        Action::Remove { directory } => {
            let mut profile = Profile::open_or_create(&profile_name).await?;
            profile.remove_backup_path(&directory);
            profile.save_profile().await?;
            info!("Removed {} from profile {}", directory.display(), profile);
            Ok(())
        }
        Action::AddRepo {
            repository,
            passphrase_loc,
            rsh,
        } => {
            // TODO: Check if repo is valid (maybe once "borg info" or something works)
            let mut profile = Profile::open_or_create(&profile_name).await?;
            if profile.has_repository(&repository) {
                bail!(
                    "Repository {} already exists in profile {}",
                    repository,
                    profile
                );
            }
            profile.add_repository(repository.clone(), passphrase_loc, rsh)?;
            profile.save_profile().await?;
            info!("Added repository {} to profile {}", repository, profile);
            Ok(())
        }
        Action::List {
            repository,
            all,
            count,
        } => {
            let profile = Profile::open_or_create(&profile_name).await?;
            let timeout_duration_secs = profile.action_timeout_seconds() as i64;
            for repo in profile.active_repositories().filter(|repo| {
                repository
                    .as_ref()
                    .map(|rr| rr.as_str() == repo.path.as_str())
                    .unwrap_or(true)
            }) {
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
                let mut to_skip = 0;
                if !all {
                    to_skip = list_archives_per_repo.archives.len().saturating_sub(count);
                }
                for archives in list_archives_per_repo.archives.iter().skip(to_skip) {
                    info!("{}::{}", repo, archives.name);
                }
            }
            Ok(())
        }
        Action::ListRepos => {
            let profile = Profile::open_or_create(&profile_name).await?;
            for repo in profile.repositories() {
                let mut extra_info = "";
                if repo.disabled() {
                    extra_info = " (DISABLED)";
                }
                println!("{}{}", &repo.path, extra_info);
            }
            Ok(())
        }
        Action::SetPassword {
            repo,
            passphrase_loc,
        } => {
            let mut profile = Profile::open_or_create(&profile_name).await?;
            let borg_passphrase = passphrase_loc.get_passphrase()?;
            let encryption = Encryption::from_passphrase_loc(passphrase_loc)?;
            profile.update_repository_password(&repo, encryption.clone(), borg_passphrase)?;
            profile.save_profile().await?;
            info!("Updated password for {} (method: {:?})", repo, encryption);
            Ok(())
        }
        Action::Compact => {
            let profile = Profile::open_or_create(&profile_name).await?;
            for repo in profile.active_repositories() {
                borg::compact(repo, command_response_send.clone()).await?;
                info!("Finished compacting {}", repo);
            }
            Ok(())
        }
        Action::Prune => {
            let profile = Profile::open_or_create(&profile_name).await?;
            for repo in profile.active_repositories() {
                borg::prune(repo, profile.prune_options(), command_response_send.clone()).await?;
                info!("Finished pruning {}", repo);
            }
            Ok(())
        }
        Action::Check { only_these_repos } => {
            let profile = Profile::open_or_create(&profile_name).await?;
            let check_semaphore = Arc::new(Semaphore::new(0));
            let successful = Arc::new(AtomicBool::new(true));
            for repo in profile.active_repositories() {
                let should_check = only_these_repos
                    .as_ref()
                    .map(|repos_to_check| repos_to_check.contains(&repo.path()))
                    .unwrap_or(true);
                if !should_check {
                    tracing::info!("Skipping verification of {}", repo.path());
                    check_semaphore.add_permits(1);
                    continue;
                }
                tracing::info!("Starting verification of {}", repo.path());
                let successful_clone = successful.clone();
                let check_semaphore_clone = check_semaphore.clone();
                let repo_clone = repo.clone();
                tokio::spawn(async move {
                    let _guard = repo_clone.lock.lock().await;
                    let res = match borg::check_with_notification(&repo_clone).await {
                        Ok(res) => res,
                        Err(e) => {
                            error!("Verification failed: {e}");
                            false
                        }
                    };
                    successful_clone.fetch_and(res, Ordering::SeqCst);
                    check_semaphore_clone.add_permits(1);
                });
            }
            let _ = check_semaphore
                .acquire_many(profile.num_active_repositories() as u32)
                .await?;
            let title = if successful.load(Ordering::SeqCst) {
                "Backup Verification Successful!"
            } else {
                "Backup Verification FAILED!"
            };
            let message = format!("Profile: {}", profile.name());
            info!("{}", message);
            show_notification(title, &message, EXTENDED_NOTIFICATION_DURATION).await?;
            Ok(())
        }
        Action::AddProfile { name } => {
            let profile = match Profile::open_profile(&name).await {
                Ok(Some(profile)) => bail!("Error: {} already exists", profile),
                Ok(None) => Profile::create_profile(&name).await?,
                Err(e) => bail!(
                    "[{}] exists but encountered an error while reading: {:#}",
                    Profile::profile_path_for_name(&name)?.to_string_lossy(),
                    e
                ),
            };
            info!(
                "Created {} ({})",
                profile,
                profile
                    .profile_path()
                    .unwrap_or("unknown_path".into())
                    .to_string_lossy()
            );
            Ok(())
        }
        Action::Mount {
            repository_path,
            mountpoint,
            do_not_open_in_gui_file_manager,
        } => {
            let profile = Profile::open_or_create(&profile_name).await?;
            let repo = profile.find_repo_from_mount_src(&repository_path)?;
            borg::mount(&repo, repository_path, mountpoint.clone()).await?;
            if !do_not_open_in_gui_file_manager {
                if let Err(e) = open_path_in_gui_file_manager(mountpoint) {
                    error!("Failed to open GUI file manager: {}", e);
                }
            }
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
            check_unit,
        } => {
            let profile = Profile::open_or_create(&profile_name).await?;
            let (action, calendar) = if check_unit {
                ("check", "monthly")
            } else {
                ("create", "*-*-* 21:00:00")
            };
            let systemd_unit_contents =
                generate_system_unit(profile.name(), timer, action, calendar);
            let extension = if timer { "timer" } else { "service" };
            if install || install_path.is_some() {
                let home_dir = dirs::home_dir()
                    .ok_or_else(|| anyhow!("Couldn't find a home directory. Is $HOME set?"))?;
                let install_path = install_path.unwrap_or_else(|| {
                    home_dir.join(format!(
                        ".config/systemd/user/borgtui-{}-{}.{}",
                        action,
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

        Action::ConfigPath => {
            let profile = Profile::open_or_create(&profile_name).await?;
            println!(
                "{}",
                Profile::profile_path_for_name(profile.name())?.to_string_lossy()
            );
            Ok(())
        }
        Action::ShellCompletion { shell } => {
            cli::print_shell_completion(&shell)?;
            Ok(())
        }
        Action::InstallManPages { man_root } => cli::print_manpage(man_root).await,
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
