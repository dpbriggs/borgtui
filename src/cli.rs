use std::path::{Path, PathBuf};

use async_recursion::async_recursion;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, shells};
use clap_mangen;
use tokio::io::AsyncWriteExt;

use crate::types::BorgResult;

const ABOUT: &str = "Like borgomatic, but with a TUI to help automate borg backups :^)";

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
    #[arg(env, short = 'p', long = "profile")]
    pub(crate) borgtui_profile: Option<String>,

    /// Watch for changes in the profile and automatically reload on modify.
    /// This feature is experimental.
    #[arg(short, long)]
    pub(crate) watch_profile: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub(crate) enum Action {
    /// Initialize a new borg repository and add it to a profile
    Init {
        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(env, short = 'e', long)]
        borg_passphrase: String,

        /// The repo location
        #[arg(short, long)]
        location: String,

        /// Do not store the passphrase in the keyring
        #[arg(short, long)]
        do_not_store_in_keyring: bool,

        /// SSH command to use when connecting to the repo
        #[arg(short, long)]
        rsh: Option<String>,
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

        // TODO: Simplify these options!
        /// The encryption passphrase to use. If not specified and borgtui
        /// called in an interactive terminal, the user will be prompted.
        #[arg(short, long, default_value = "true")]
        no_encryption: bool,

        /// The profile to use. If not specified, the default profile
        /// will be used.
        #[arg(env, short, long)]
        borg_passphrase: Option<String>,

        /// If true, store the encryption passphrase in cleartext in the
        /// configuration file. This is not recommended.
        #[arg(short, long, default_value = "false")]
        store_passphase_in_cleartext: bool,
    },
    /// Mount a mounted Borg repo or archive as a FUSE filesystem.
    Mount {
        /// The directory or file path to add to backup
        repository_path: String,
        /// The mount point
        mountpoint: PathBuf,
    },
    /// Unmount a mounted Borg repo or archive
    Umount {
        /// The mount point
        mountpoint: PathBuf,
    },
    /// List the archives in a directory
    List,
    /// Compact a borg repo
    Compact,
    /// Prune a borg repo
    Prune,
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
        /// If set, save the save the unit to the path specified. This option implies
        /// --install
        #[arg(long)]
        install_path: Option<PathBuf>,
        #[arg(long)]
        timer: bool,
    },
    /// Generate shell completion scripts (printed to stdout)
    ShellCompletion {
        /// Type of shell to print completions for the specified shell. Defaults to zsh.
        ///
        /// Allowed options: "zsh", "bash", "fish", "elvish", "powershell"
        #[arg(long, default_value = "zsh")]
        shell: String,
    },
    /// Generate man pages for borgtui
    ManPage {
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
