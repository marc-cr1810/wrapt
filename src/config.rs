//! User configuration from `~/.config/wrapt/config.toml` (or `$WRAPT_CONFIG`).
//! Every field is optional; anything unset falls back to a built-in default,
//! and any explicit CLI flag overrides the config.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
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
}

impl Config {
    /// Load the config file if present; a missing file is not an error, but a
    /// malformed one is surfaced so the user can fix it.
    pub fn load() -> anyhow::Result<Config> {
        let Some(path) = config_path() else {
            return Ok(Config::default());
        };
        if !is_trusted(&path) {
            crate::ui::warn(&format!(
                "ignoring {}: it is writable by other users, or owned by \
                 someone other than you or root",
                path.display()
            ));
            return Ok(Config::default());
        }
        let cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("invalid config at {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
        };
        // Catch a misspelled policy rather than silently falling back to auto.
        if let Some(c) = cfg.color.as_deref()
            && !matches!(c, "auto" | "always" | "never")
        {
            anyhow::bail!(
                "invalid config at {}: color must be \"auto\", \"always\", or \"never\" (got {c:?})",
                path.display()
            );
        }
        // A blank entry would silently match nothing; say so rather than
        // leaving the user believing a service is protected.
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
        if let Some(cc) = cfg.mirror_country.as_deref()
            && !(cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic()))
        {
            anyhow::bail!(
                "invalid config at {}: mirror_country must be a two-letter \
                 country code like \"AU\" (got {cc:?})",
                path.display()
            );
        }
        Ok(cfg)
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
