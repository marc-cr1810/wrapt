//! Configuration, read from two optional files and merged.
//!
//! `/etc/wrapt/config.toml` sets machine-wide defaults (a package or an admin
//! can ship it); `~/.config/wrapt/config.toml` — or `$WRAPT_CONFIG` — is the
//! user's own and overrides it key by key — except `never_restart`, where the
//! two lists are unioned. Every field is optional, anything unset falls back to
//! a built-in default, and an explicit CLI flag beats both files.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Machine-wide config, overridable for tests.
fn system_path() -> PathBuf {
    std::env::var_os("WRAPT_SYSTEM_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/wrapt/config.toml"))
}

/// Where a setting's value came from, for `wrapt config --show`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Default,
    System,
    User,
    /// Both layers contributed — only possible for a setting that is unioned
    /// rather than overridden.
    Both,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::System => "system",
            Source::User => "user",
            Source::Both => "system+user",
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Default number of parallel downloads.
    pub parallel: Option<usize>,
    /// Assume "yes" on prompts without needing -y.
    pub assume_yes: Option<bool>,
    /// Show apt's raw output by default.
    pub verbose: Option<bool>,
    /// Colour policy: "auto" (default), "always", or "never".
    pub color: Option<String>,
    /// GitHub `owner/name` that `self-update` pulls releases from.
    pub repo: Option<String>,
    /// After `wrapt upgrade`, check whether a newer wrapt is available.
    pub notify_updates: Option<bool>,
    /// Services wrapt must never restart after a transaction, on top of the
    /// session-critical ones it detects itself. For daemons that are safe to
    /// restart but costly to interrupt — a database, a container runtime.
    /// Names may be given with or without the `.service` suffix.
    pub never_restart: Option<Vec<String>>,
    /// What to do about services using upgraded-out libraries: "ask" (default),
    /// "auto" to restart the safe ones without prompting, or "never" to only
    /// report them.
    pub restart: Option<String>,
    /// How many kernels `clean --kernels` keeps, newest first (default 2). The
    /// running kernel is always kept on top of this.
    pub keep_kernels: Option<usize>,
    /// Two-letter country code for `fetch` to pull its mirror list from, for
    /// when geolocation guesses wrong.
    pub mirror_country: Option<String>,
    /// How many transactions to keep in the history (default 1000). Worth
    /// lowering on a machine that runs many transactions and never undoes
    /// them, like a CI runner or a container build.
    pub history_limit: Option<usize>,
}

impl Config {
    /// The effective configuration: system defaults with the user's own laid
    /// over the top. A missing file is not an error, but a malformed one is
    /// surfaced so the user can fix it.
    pub fn load() -> anyhow::Result<Config> {
        let (system, user) = Self::layers()?;
        Ok(Config::merge(system, user))
    }

    /// The two layers, unmerged, so `wrapt config --show` can say where each
    /// setting came from.
    pub fn layers() -> anyhow::Result<(Config, Config)> {
        let system = read_file(&system_path())?;
        let user = match config_path() {
            Some(p) => read_file(&p)?,
            None => Config::default(),
        };
        Ok((system, user))
    }

    /// The effective config from layers already read, so a caller that needs
    /// both the layers and the result doesn't read the files twice.
    pub fn effective(system: &Config, user: &Config) -> Config {
        Config::merge(system.clone(), user.clone())
    }

    /// Lay `over` on top of `base`, key by key.
    ///
    /// `never_restart` is the exception: the two layers are unioned rather than
    /// replaced. Forgetting to restate a machine-wide entry would silently drop
    /// a protection, and the costs are lopsided — over-deferring leaves a stale
    /// library until reboot, while under-deferring is what ends a session. The
    /// union can only ever protect more, never less, which is the same bias the
    /// restart guard itself is built on.
    fn merge(base: Config, over: Config) -> Config {
        Config {
            parallel: over.parallel.or(base.parallel),
            assume_yes: over.assume_yes.or(base.assume_yes),
            verbose: over.verbose.or(base.verbose),
            color: over.color.or(base.color),
            repo: over.repo.or(base.repo),
            notify_updates: over.notify_updates.or(base.notify_updates),
            never_restart: union(base.never_restart, over.never_restart),
            restart: over.restart.or(base.restart),
            keep_kernels: over.keep_kernels.or(base.keep_kernels),
            mirror_country: over.mirror_country.or(base.mirror_country),
            history_limit: over.history_limit.or(base.history_limit),
        }
    }

    /// Apply the colour policy. "auto" (the default) honours the NO_COLOR
    /// convention and otherwise colours only when stdout is a terminal, so
    /// piped or redirected output stays plain.
    pub fn apply_color(&self) {
        use std::io::IsTerminal;
        let on = match self.color.as_deref() {
            Some("always") => true,
            Some("never") => false,
            _ if std::env::var_os("NO_COLOR").is_some() => false,
            _ => std::io::stdout().is_terminal(),
        };
        crate::ui::set_color(on);
    }
}

/// Every setting, its effective value, and which layer supplied it. Built by
/// pairing each field of the merged config with the layers it came from, so
/// `--show` can't drift out of step with what `load` actually returns.
pub fn describe(merged: &Config, system: &Config, user: &Config) -> Vec<(String, String, Source)> {
    /// Which layer won for one field, given whether each layer set it.
    fn source(user_set: bool, system_set: bool) -> Source {
        if user_set {
            Source::User
        } else if system_set {
            Source::System
        } else {
            Source::Default
        }
    }

    macro_rules! rows {
        ($($field:ident => $default:expr),* $(,)?) => {{
            // An exhaustive destructure, so adding a field to Config stops this
            // compiling until the field is given a default and listed below.
            // A macro alone wouldn't catch that — this is what makes `--show`
            // impossible to leave incomplete.
            let Config { $($field),* } = merged;
            vec![$((
                stringify!($field).to_string(),
                $field
                    .as_ref()
                    .map(Render::render)
                    .unwrap_or_else(|| $default.to_string()),
                source(user.$field.is_some(), system.$field.is_some()),
            )),*]
        }};
    }

    // Listed in the order they're shown, which needn't match the struct.
    let mut rows = rows! {
        parallel => "5",
        assume_yes => "false",
        verbose => "false",
        color => "auto",
        restart => "ask",
        never_restart => "(none)",
        keep_kernels => "2",
        history_limit => "1000",
        mirror_country => "(geolocated)",
        repo => crate::selfupdate::DEFAULT_REPO,
        notify_updates => "false",
    };

    // `never_restart` is unioned, so when both layers set it the value really
    // did come from both. The "user wins" label every other setting uses would
    // point someone debugging at the wrong file.
    if user.never_restart.is_some()
        && system.never_restart.is_some()
        && let Some(row) = rows.iter_mut().find(|(name, ..)| name == "never_restart")
    {
        row.2 = Source::Both;
    }
    rows
}

/// Combine two optional lists, keeping every entry from both. `None` on either
/// side leaves the other untouched.
fn union(base: Option<Vec<String>>, over: Option<Vec<String>>) -> Option<Vec<String>> {
    match (base, over) {
        (Some(mut a), Some(b)) => {
            a.extend(b);
            a.sort();
            a.dedup();
            Some(a)
        }
        (a, b) => b.or(a),
    }
}

/// How a setting's value is displayed by `wrapt config`.
trait Render {
    fn render(&self) -> String;
}

impl Render for usize {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for bool {
    fn render(&self) -> String {
        self.to_string()
    }
}

impl Render for String {
    fn render(&self) -> String {
        self.clone()
    }
}

impl Render for Vec<String> {
    fn render(&self) -> String {
        if self.is_empty() {
            "(none)".to_string()
        } else {
            self.join(", ")
        }
    }
}

/// The commented starter file `wrapt config --init` writes. Every key is
/// present but commented out, so the file documents itself without freezing
/// today's defaults into a user's config.
const TEMPLATE: &str = "\
# wrapt configuration. Uncomment a line to change it; anything left commented
# uses the built-in default. A command-line flag always wins over this file.
#
# A machine-wide file at /etc/wrapt/config.toml is read first, and anything
# set here overrides it.

# Number of packages to download at once.
#parallel = 5

# Skip confirmation prompts, as if -y were always passed.
#assume_yes = false

# Show apt's raw output instead of wrapt's progress display.
#verbose = false

# Colour: \"auto\" follows the terminal and NO_COLOR, or force \"always\"/\"never\".
#color = \"auto\"

# Services still using upgraded-out libraries after an upgrade:
#   \"ask\"   prompt to restart them (default)
#   \"auto\"  restart them without asking
#   \"never\" only report them
# wrapt never restarts your display manager, the unit owning a live login
# session, dbus, systemd-logind or polkit, whatever this is set to.
#restart = \"ask\"

# Extra services to leave alone -- safe to restart, but costly to interrupt.
# Names work with or without the .service suffix.
#never_restart = [\"docker\", \"postgresql\"]

# How many kernels `clean --kernels` keeps, newest first. The running kernel
# is always kept as well. Below 2 you have no fallback if a kernel won't boot.
#keep_kernels = 2

# How many transactions `wrapt history` keeps. Older ones are dropped as new
# ones arrive. Worth lowering on a machine that runs many transactions and
# never undoes them, like a CI runner.
#history_limit = 1000

# Two-letter country code for `fetch` to pull its mirror list from. Worth
# setting if `fetch` finds only a handful of mirrors.
#mirror_country = \"AU\"

# Where `self-update` looks for new releases, as owner/name.
#repo = \"marc-cr1810/wrapt\"

# Mention a newer wrapt after `upgrade`.
#notify_updates = false
";

/// The user config path, for `wrapt config`. `None` when no home is resolvable.
pub fn user_config_path() -> Option<PathBuf> {
    config_path()
}

/// The machine-wide config path, for `wrapt config`.
pub fn machine_config_path() -> PathBuf {
    system_path()
}

/// Write the commented starter config to `path`, creating parent directories.
/// Refuses to overwrite an existing file. When run under sudo, the result is
/// handed to the invoking user so they can edit it without root.
pub fn write_template(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        anyhow::bail!(
            "{} already exists — edit it, or delete it first",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
        chown_to_invoker(parent);
    }
    std::fs::write(path, TEMPLATE)
        .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", path.display()))?;
    chown_to_invoker(path);
    Ok(())
}

/// Give a path back to the user who invoked sudo. Without this, `sudo wrapt
/// config --init` would leave a root-owned file in their home that they'd need
/// root to edit. Best-effort: a failure here isn't worth failing the command.
fn chown_to_invoker(path: &Path) {
    let (Some(uid), Some(gid)) = (env_id("SUDO_UID"), env_id("SUDO_GID")) else {
        return;
    };
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    // SAFETY: c_path is a valid NUL-terminated string for the duration of the
    // call; chown only reads it.
    unsafe {
        libc::chown(c_path.as_ptr(), uid, gid);
    }
}

fn env_id(var: &str) -> Option<u32> {
    std::env::var(var).ok()?.parse().ok()
}

/// Warn about an untrusted config once per path, per process. The config is
/// read more than once in a single command — at startup, and again by `wrapt
/// config` — and repeating one warning three times reads like three problems.
fn warn_untrusted(path: &Path) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static SEEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let Ok(mut seen) = SEEN.get_or_init(|| Mutex::new(HashSet::new())).lock() else {
        return;
    };
    if seen.insert(path.to_path_buf()) {
        crate::ui::warn(&format!(
            "ignoring {}: it is writable by other users, or owned by \
             someone other than you or root",
            path.display()
        ));
    }
}

/// Read and validate one config file. Absent (or untrusted) yields defaults.
fn read_file(path: &Path) -> anyhow::Result<Config> {
    if !is_trusted(path) {
        warn_untrusted(path);
        return Ok(Config::default());
    }
    let cfg: Config = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("invalid config at {}: {e}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
    };
    validate(&cfg, path)?;
    Ok(cfg)
}

/// Reject values that would otherwise fail silently or dangerously later.
fn validate(cfg: &Config, path: &Path) -> anyhow::Result<()> {
    // Catch a misspelled policy rather than silently falling back to auto.
    if let Some(c) = cfg.color.as_deref()
        && !matches!(c, "auto" | "always" | "never")
    {
        anyhow::bail!(
            "invalid config at {}: color must be \"auto\", \"always\", or \"never\" (got {c:?})",
            path.display()
        );
    }
    // A blank entry would silently match nothing; say so rather than leaving
    // the user believing a service is protected.
    if let Some(names) = cfg.never_restart.as_deref()
        && names.iter().any(|n| n.trim().is_empty())
    {
        anyhow::bail!(
            "invalid config at {}: never_restart must not contain empty names",
            path.display()
        );
    }
    if let Some(r) = cfg.restart.as_deref()
        && !matches!(r, "ask" | "auto" | "never")
    {
        anyhow::bail!(
            "invalid config at {}: restart must be \"ask\", \"auto\", or \"never\" (got {r:?})",
            path.display()
        );
    }
    // Zero would mean purging every kernel, including one you can boot.
    if cfg.keep_kernels == Some(0) {
        anyhow::bail!(
            "invalid config at {}: keep_kernels must be at least 1",
            path.display()
        );
    }
    // Zero would record a transaction and immediately discard it, leaving
    // `undo` with nothing to undo — say so rather than silently doing that.
    if cfg.history_limit == Some(0) {
        anyhow::bail!(
            "invalid config at {}: history_limit must be at least 1",
            path.display()
        );
    }
    if let Some(cc) = cfg.mirror_country.as_deref()
        && !(cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic()))
    {
        anyhow::bail!(
            "invalid config at {}: mirror_country must be a two-letter \
             country code like \"AU\" (got {cc:?})",
            path.display()
        );
    }
    Ok(())
}

fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WRAPT_CONFIG") {
        return Some(PathBuf::from(p));
    }
    // Most state-changing commands run under sudo, which resets HOME to root's.
    // Without this the user's config would be invisible for exactly the
    // commands it configures — restarts, kernel cleanup, mirrors.
    if let Some(home) = sudo_user_home() {
        return Some(home.join(".config").join("wrapt").join("config.toml"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("wrapt").join("config.toml"))
}

/// Apply [`trusted_owner`] to a real file. Only enforced when running as root,
/// since an unprivileged run can only ever read files that user could write
/// anyway. A missing file is "trusted" — `load` handles it as absent.
fn is_trusted(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: geteuid is always safe; it reads the calling process's euid.
    if unsafe { libc::geteuid() } != 0 {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    let invoking_uid = std::env::var("SUDO_UID")
        .ok()
        .and_then(|u| u.parse::<u32>().ok());
    trusted_owner(meta.uid(), meta.mode(), invoking_uid)
}

/// The home directory of the user who invoked sudo, if we're running under it.
fn sudo_user_home() -> Option<PathBuf> {
    let user = std::env::var("SUDO_USER").ok()?;
    if user.is_empty() || user == "root" {
        return None;
    }
    // `getent` resolves users from every NSS source, not just /etc/passwd, so
    // LDAP and SSSD accounts work too.
    let out = std::process::Command::new("getent")
        .args(["passwd", &user])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    home_from_passwd(&String::from_utf8_lossy(&out.stdout)).map(PathBuf::from)
}

/// The home field (6th, colon-separated) of a passwd entry.
fn home_from_passwd(line: &str) -> Option<String> {
    let home = line.lines().next()?.split(':').nth(5)?.trim();
    (!home.is_empty()).then(|| home.to_string())
}

/// Whether a config file is safe for root to read. Running as root, we'd
/// otherwise honour a file any other user could rewrite — and `repo` decides
/// where `self-update` fetches a `.deb` to install, so that would be a route to
/// running attacker code as root.
///
/// Split out from the filesystem call so the policy can be tested directly.
fn trusted_owner(file_uid: u32, mode: u32, invoking_uid: Option<u32>) -> bool {
    // Writable by group or other: anyone in that set could have authored it.
    if mode & 0o022 != 0 {
        return false;
    }
    file_uid == 0 || invoking_uid == Some(file_uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_fields() {
        let c: Config =
            toml::from_str("parallel = 8\nassume_yes = true\nverbose = false\ncolor = \"never\"\n")
                .unwrap();
        assert_eq!(c.parallel, Some(8));
        assert_eq!(c.assume_yes, Some(true));
        assert_eq!(c.verbose, Some(false));
        assert_eq!(c.color.as_deref(), Some("never"));
    }

    #[test]
    fn empty_config_is_all_none() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.parallel.is_none() && c.assume_yes.is_none());
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(toml::from_str::<Config>("parralel = 8\n").is_err());
    }

    #[test]
    fn user_layer_overrides_system_key_by_key() {
        let system: Config =
            toml::from_str("parallel = 8\nkeep_kernels = 4\nrestart = \"auto\"\n").unwrap();
        let user: Config = toml::from_str("keep_kernels = 2\n").unwrap();
        let merged = Config::merge(system, user);
        // Overridden by the user.
        assert_eq!(merged.keep_kernels, Some(2));
        // Untouched by the user, so the system value survives.
        assert_eq!(merged.parallel, Some(8));
        assert_eq!(merged.restart.as_deref(), Some("auto"));
        // Set by neither: still unset, for the built-in default to fill.
        assert_eq!(merged.mirror_country, None);
    }

    #[test]
    fn never_restart_unions_both_layers() {
        // A user adding to the list must not silently drop what the machine
        // protects — the merge can only ever protect more, never less.
        let system: Config = toml::from_str("never_restart = [\"docker\"]\n").unwrap();
        let user: Config = toml::from_str("never_restart = [\"nginx\"]\n").unwrap();
        let merged = Config::merge(system, user);
        assert_eq!(
            merged.never_restart.as_deref(),
            Some(["docker".to_string(), "nginx".to_string()].as_slice())
        );
        // And it must be attributed to both files, not just the user's.
        let system: Config = toml::from_str("never_restart = [\"docker\"]\n").unwrap();
        let user: Config = toml::from_str("never_restart = [\"nginx\"]\n").unwrap();
        let rows = describe(&merged, &system, &user);
        let (_, value, source) = rows
            .iter()
            .find(|(n, ..)| n == "never_restart")
            .expect("never_restart is shown");
        assert_eq!(value, "docker, nginx");
        assert_eq!(*source, Source::Both);
    }

    #[test]
    fn union_deduplicates_and_tolerates_one_sided_lists() {
        let both = union(
            Some(vec!["docker".into(), "nginx".into()]),
            Some(vec!["nginx".into(), "redis".into()]),
        );
        assert_eq!(
            both.as_deref(),
            Some(
                [
                    "docker".to_string(),
                    "nginx".to_string(),
                    "redis".to_string()
                ]
                .as_slice()
            )
        );
        assert_eq!(union(None, None), None);
        assert_eq!(
            union(Some(vec!["a".into()]), None).as_deref(),
            Some(["a".to_string()].as_slice())
        );
        assert_eq!(
            union(None, Some(vec!["b".into()])).as_deref(),
            Some(["b".to_string()].as_slice())
        );
    }

    #[test]
    fn describe_attributes_each_setting_to_its_layer() {
        let system: Config = toml::from_str("parallel = 8\nrestart = \"auto\"\n").unwrap();
        let user: Config = toml::from_str("restart = \"never\"\n").unwrap();
        let merged = Config::merge(
            toml::from_str("parallel = 8\nrestart = \"auto\"\n").unwrap(),
            toml::from_str("restart = \"never\"\n").unwrap(),
        );
        let rows = describe(&merged, &system, &user);
        let find = |name: &str| {
            rows.iter()
                .find(|(n, ..)| n == name)
                .map(|(_, v, s)| (v.clone(), *s))
                .unwrap()
        };
        assert_eq!(find("restart"), ("never".to_string(), Source::User));
        assert_eq!(find("parallel"), ("8".to_string(), Source::System));
        let (value, source) = find("keep_kernels");
        assert_eq!(source, Source::Default);
        assert_eq!(value, "2");
    }

    #[test]
    fn extracts_home_from_passwd_entry() {
        assert_eq!(
            home_from_passwd("marc:x:1000:1000:Marc:/home/marc:/usr/bin/zsh\n").as_deref(),
            Some("/home/marc")
        );
        // Only the first entry is considered, and an empty home is no home.
        assert_eq!(home_from_passwd("bad:x:1:1:x::/bin/sh"), None);
        assert_eq!(home_from_passwd(""), None);
        assert_eq!(home_from_passwd("truncated:x:1"), None);
    }

    #[test]
    fn root_only_trusts_config_it_or_the_invoking_user_owns() {
        let me = Some(1000);
        // Owned by the invoking user, not writable by anyone else: fine.
        assert!(trusted_owner(1000, 0o100644, me));
        // Owned by root: fine.
        assert!(trusted_owner(0, 0o100644, me));
        // Owned by a third party: refused, even with tight permissions.
        assert!(!trusted_owner(1001, 0o100644, me));
        // Group- or world-writable: refused however it's owned, since anyone
        // in that set could have written the file.
        assert!(!trusted_owner(1000, 0o100664, me));
        assert!(!trusted_owner(1000, 0o100646, me));
        assert!(!trusted_owner(0, 0o100662, me));
        // No SUDO_UID (a direct root login): only root's own file is trusted.
        assert!(trusted_owner(0, 0o100644, None));
        assert!(!trusted_owner(1000, 0o100644, None));
    }

    #[test]
    fn parses_new_scalar_fields() {
        let c: Config =
            toml::from_str("restart = \"never\"\nkeep_kernels = 3\nmirror_country = \"AU\"\n")
                .unwrap();
        assert_eq!(c.restart.as_deref(), Some("never"));
        assert_eq!(c.keep_kernels, Some(3));
        assert_eq!(c.mirror_country.as_deref(), Some("AU"));
    }

    #[test]
    fn parses_never_restart_list() {
        let c: Config =
            toml::from_str("never_restart = [\"docker\", \"postgresql.service\"]\n").unwrap();
        assert_eq!(
            c.never_restart.as_deref(),
            Some(["docker".to_string(), "postgresql.service".to_string()].as_slice())
        );
    }
}
