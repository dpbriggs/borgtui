use std::path::{Path, PathBuf};

use async_recursion::async_recursion;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, shells};
use tokio::io::AsyncWriteExt;

use crate::{
    profiles::{Passphrase, RepositoryKind},
    types::BorgResult,
};

const ABOUT: &str = "A TUI and CLI to help automate borg backups :^)";

#[derive(Parser, Debug, Clone)]
#[command(
    author = "David Briggs <david@dpbriggs.ca>",
    version,
    about = ABOUT,
    long_about = ABOUT
)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) action: Option<Action>,

    /// The profile to use. If not specified, the default profile
    /// will be used.
    #[arg(global = true, env, short = 'p', long = "profile")]
    pub(crate) borgtui_profile: Option<String>,

    /// Watch for changes in the profile and automatically reload on modify.
    /// This feature is experimental.
    #[arg(short, long)]
    pub(crate) watch_profile: bool,
}

#[derive(Parser, Debug, Clone)]
pub(crate) struct PassphraseSource {
    /// Use a keyfile (a file path containing the borg passphrase)
    #[arg(short, long)]
    pub(crate) keyfile: Option<PathBuf>,
    /// Store the borg passphrase in the config file.
    #[arg(short = 'c', long)]
    pub(crate) raw: bool,
    /// No borg passphrase associated with this repository.
    ///
    /// Implied if no other borg passphrase sources are specified.
    #[arg(short, long)]
    pub(crate) none: bool,
    /// Obtain the borg passphrase from the environment.
    #[arg(env)]
    pub(crate) borg_passphrase: Option<Passphrase>,
}

impl PassphraseSource {
    pub(crate) fn get_passphrase(&self) -> BorgResult<Option<Passphrase>> {
        if self.none {
            return Ok(None);
        }
        if let Some(borg_passphrase) = &self.borg_passphrase {
            Ok(Some(borg_passphrase.clone()))
        } else if let Some(keyfile) = &self.keyfile {
            let passphrase = std::fs::read_to_string(keyfile)?.trim().to_string();
            Ok(Some(passphrase.into()))
        } else {
            Ok(None)
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Action {
    /// Initialize a new borg repository and add it to a profile
    Init {
        /// SSH command to use when connecting to the repo
        #[arg(short, long)]
        rsh: Option<String>,

        /// The repo location. It should be a file path or ssh string.
        ///
        /// Examples:
        /// - /hdd3/NewBackup
        /// - /hdd2/NewBackup
        location: String,

        #[arg(short = 's', long, default_value = "borg")]
        kind: RepositoryKind,

        #[command(flatten)]
        passphrase_loc: PassphraseSource,
    },
    /// Create a new backup
    Create,
    /// Add a directory to the profile to backup
    Add {
        /// The directory or file path to add to backup
        directory: PathBuf,
    },
    /// Remove a directory from a profile (will no longer be backed up)
    Remove {
        /// The directory or file path to add to backup
        directory: PathBuf,
    },
    /// Add an existing repository to the profile.
    ///
    /// It's recommended to set BORG_PASSPHRASE in your environment and export it.
    AddRepo {
        /// The directory or file path to add to backup
        repository: String,

        /// SSH command to use when connecting to the repo
        #[arg(short, long)]
        rsh: Option<String>,

        /// Whether this is a "borg" or "rustic" repo.
        #[arg(short = 's', long, default_value = "borg")]
        kind: RepositoryKind,

        #[command(flatten)]
        passphrase_loc: PassphraseSource,
    },
    /// Create a new profile with a given name
    AddProfile { name: String },
    /// Mount a borg repo or archive as a FUSE filesystem.
    Mount {
        /// The directory or file path to add to backup
        repository_path: String,
        /// The mount point
        mountpoint: PathBuf,
        /// If set, don't open a GUI file manager to browse the mounted
        /// repository or archive.
        #[arg(short, long)]
        do_not_open_in_gui_file_manager: bool,
    },
    /// Unmount a mounted Borg repo or archive
    Umount {
        /// The mount point
        mountpoint: PathBuf,
    },
    /// List the archives in a directory.
    ///
    /// By default it'll print the three most recent archives.
    List {
        /// If specified, only list archives from this repository.
        repository: Option<String>,
        /// If set, show all archives from all repositories.
        #[arg(short, long)]
        all: bool,
        /// How many archives per repository to list
        #[arg(short, long, default_value = "3")]
        count: usize,
    },
    /// List the repositories associated with the profile.
    ListRepos,
    /// Set the password for a repository. By default it will read
    /// BORG_PASSPHRASE from the environment unless --keyfile is specified.
    SetPassword {
        /// Name of the repository (use `borgtui list-repos` to list)
        repo: String,

        #[command(flatten)]
        passphrase_loc: PassphraseSource,
    },
    /// Compact a borg repo
    Compact,
    /// Prune a borg repo
    Prune,
    /// Verify the integrity of all active repositories. This usually takes a long time.
    ///
    /// It will display a notification and log on failure.
    Check {
        /// If specified, only check these repositories. Consider using `list-repos` to get repo urls.
        only_these_repos: Option<Vec<String>>,
    },
    /// Create a systemd unit to create a backup
    ///
    /// If `--install` is specified it will install the unit as a user unit under
    /// ~/.config/systemd/user. To use this unit you will need to reload user
    /// units and start the unit.
    ///
    ///     $ systemctl --user daemon-reload
    ///
    ///     $ systemctl --user start borgtui-create-{profile_name}.service
    ///
    /// The default profile is aptly named "default" so the command used is:
    ///
    ///     $ systemctl --user start borgtui-create-default.service
    SystemdCreateUnit {
        /// If true, save the unit under ~/.config/systemd/user/
        #[arg(long)]
        install: bool,
        /// If set, make a check unit instead of a create unit.
        #[arg(long)]
        check_unit: bool,
        /// If set, save the save the unit to the path specified. This option implies
        /// --install
        #[arg(long)]
        install_path: Option<PathBuf>,
        /// If set, create a timer unit instead of a service unit.
        #[arg(long)]
        timer: bool,
    },
    /// Print the configuration file path for the given profile (defaults to "default")
    ///
    /// Note that this command will _open_ the profile to see if it exists.
    ConfigPath,
    /// Generate shell completion scripts (printed to stdout)
    ShellCompletion {
        /// Type of shell to print completions for the specified shell. Defaults to zsh.
        ///
        /// Allowed options: "zsh", "bash", "fish", "elvish", "powershell"
        #[arg(long, default_value = "zsh")]
        shell: String,
    },
    /// Install man pages in a target directory.
    InstallManPages {
        /// Path where man pages will be written. Several files will be written
        /// as borgtui uses subcommands.
        man_root: PathBuf,
    },
}

pub(crate) async fn print_manpage(man_root: PathBuf) -> BorgResult<()> {
    // Adapted from https://github.com/clap-rs/clap/discussions/3603#discussioncomment-3641542
    #[async_recursion]
    async fn write_man_page(dir: &Path, app: &clap::Command) -> BorgResult<()> {
        // `get_display_name()` is `Some` for all instances, except the root.
        let name = app.get_display_name().unwrap_or_else(|| app.get_name());
        let mut out = tokio::fs::File::create(dir.join(format!("{name}.1"))).await?;

        let mut buf = Vec::new();
        clap_mangen::Man::new(app.clone())
            .title(name)
            .render(&mut buf)?;
        out.write_all(buf.as_slice()).await?;
        out.flush().await?;

        for sub in app.get_subcommands() {
            write_man_page(dir, sub).await?;
        }
        Ok(())
    }
    let mut command = Args::command();
    command.build();
    write_man_page(man_root.as_path(), &command).await?;
    Ok(())
}

pub(crate) fn print_shell_completion(shell_kind: &str) -> BorgResult<()> {
    let shell = match shell_kind {
        "zsh" => shells::Shell::Zsh,
        "bash" => shells::Shell::Bash,
        "fish" => shells::Shell::Fish,
        "elvish" => shells::Shell::Elvish,
        "powershell" => shells::Shell::PowerShell,
        _ => {
            anyhow::bail!("Unknown shell kind {}, assuming zsh", shell_kind);
        }
    };
    generate(
        shell,
        &mut Args::command(),
        "borgtui",
        &mut std::io::stdout(),
    );
    Ok(())
}

pub(crate) fn get_args() -> Args {
    Args::parse()
}
