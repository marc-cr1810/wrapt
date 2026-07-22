use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
    name = "wrapt",
    version,
    about = "A faster, prettier front-end for apt"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Number of parallel downloads (default 5, or config `parallel`)
    #[arg(short = 'j', long, global = true)]
    pub parallel: Option<usize>,

    /// Show apt's raw output instead of the clean progress display
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Emit machine-readable JSON (search, list, why, history, doctor, held, provides)
    #[arg(long, global = true)]
    pub json: bool,

    /// Show what a command would do, then stop without changing anything
    #[arg(short = 'n', long, global = true)]
    pub dry_run: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Refresh the package lists
    Update,
    /// Upgrade all installed packages
    Upgrade {
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
        /// Allow installing/removing packages to satisfy the upgrade (dist-upgrade)
        #[arg(long)]
        full: bool,
        /// Only apply upgrades from a security pocket
        #[arg(long)]
        security_only: bool,
    },
    /// Install packages
    Install {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove packages
    Remove {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
        /// Also delete configuration files
        #[arg(long)]
        purge: bool,
    },
    /// Reinstall packages, fetching their current version again
    Reinstall {
        #[arg(required = true)]
        packages: Vec<String>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove packages that are no longer needed
    Autoremove {
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Download package .debs into the current directory without installing
    Download {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// List installed, upgradable, or manually-installed packages
    List {
        /// Only packages with a newer version available
        #[arg(long)]
        upgradable: bool,
        /// Only packages you installed on purpose (not pulled in as deps)
        #[arg(long)]
        manual: bool,
        /// Filter to packages whose name contains this text
        pattern: Option<String>,
    },
    /// Preview what installing packages would do, without installing
    Plan {
        #[arg(required = true)]
        packages: Vec<String>,
    },
    /// Free disk space by clearing the downloaded-package cache
    Clean {
        /// Remove every cached .deb, not just ones that can't be re-downloaded
        #[arg(long)]
        all: bool,
        /// Purge old kernels, keeping the running one and the newest installed
        #[arg(long)]
        kernels: bool,
    },
    /// Benchmark apt mirrors and switch to the fastest (Ubuntu)
    Fetch {
        /// Write the fastest mirror into apt's sources (needs root)
        #[arg(long)]
        apply: bool,
        /// How many top mirrors to display
        #[arg(long, default_value_t = 10)]
        count: usize,
        /// Two-letter country code to pull the mirror list from (e.g. US, DE)
        #[arg(long)]
        country: Option<String>,
    },
    /// Suggest a package for an unrecognised command (used by the shell hook)
    #[allow(clippy::enum_variant_names)]
    CommandNotFound {
        /// The command that wasn't found
        command: Option<String>,
        /// Print the shell hook for SHELL instead of looking up a command
        #[arg(long, value_name = "SHELL")]
        init: Option<Shell>,
    },
    /// Explain why a package can't be installed
    WhyNot { package: String },
    /// Show a package's changelog, highlighting security fixes
    Changelog { package: String },
    /// Manage apt software sources and PPAs
    Repo {
        #[command(subcommand)]
        action: RepoCmd,
    },
    /// Show the transaction history
    History {
        /// Show the details of one transaction
        id: Option<u64>,
    },
    /// Undo a past transaction (the most recent one by default)
    Undo {
        /// The transaction to undo (see `wrapt history`)
        id: Option<u64>,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Re-apply a past transaction
    Redo {
        /// The transaction to redo (see `wrapt history`)
        id: u64,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Undo every transaction after the given one, restoring that state
    Rollback {
        /// Roll back to the state just after this transaction id
        id: u64,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Hold packages at their current version (exclude from upgrades)
    Hold { packages: Vec<String> },
    /// Release held packages
    Unhold { packages: Vec<String> },
    /// List held packages
    Held,
    /// Show the effective configuration, or create a starter config file
    Config {
        /// Write a commented starter config to your user config path
        #[arg(long)]
        init: bool,
        /// Print only the paths wrapt reads configuration from
        #[arg(long)]
        path: bool,
    },
    /// Review configuration files left behind by upgrades (*.dpkg-dist)
    ConfigDiff,
    /// Generate a shell completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
    /// Print the man page (roff format)
    Man,
    /// Search for packages
    Search { query: String },
    /// Show detailed information about a package
    Show { package: String },
    /// Explain why a package is installed
    Why {
        package: String,
        /// List every manually-installed package that pulls it in
        #[arg(short, long)]
        all: bool,
    },
    /// Find which package provides a file or command
    Provides { pattern: String },
    /// Check the system for common package problems
    Doctor,
    /// Update wrapt itself to the latest published release
    SelfUpdate {
        /// Only report whether an update is available; don't install it
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
pub enum RepoCmd {
    /// List the configured software sources
    List,
    /// Add a repository (e.g. `ppa:user/ppa` or a full `deb` line)
    Add {
        /// The repository spec, as understood by add-apt-repository
        repo: String,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Remove a previously-added repository
    Remove {
        /// The repository spec to remove
        repo: String,
        /// Assume yes to all prompts
        #[arg(short, long)]
        yes: bool,
    },
}
