use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Add a directory or file to backup
    Add {
        /// The directory or file path to add to backup
        directory: PathBuf,
    },
    /// Add a directory or file to backup
    Remove {
        /// The directory or file path to add to backup
        directory: PathBuf,
    },
    /// Add a directory or file to backup
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
    Mount {
        /// The directory or file path to add to backup
        repository_path: String,
        /// The mount point
        mountpoint: PathBuf,
    },
    Umount {
        /// The mount point
        mountpoint: PathBuf,
    },
    /// List the archives in a directory
    List,
    Compact,
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
    },
}

pub(crate) fn get_args() -> Args {
    Args::parse()
}
