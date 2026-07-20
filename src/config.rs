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
}

impl Config {
    /// Load the config file if present; a missing file is not an error, but a
    /// malformed one is surfaced so the user can fix it.
    pub fn load() -> anyhow::Result<Config> {
        let Some(path) = config_path() else {
            return Ok(Config::default());
        };
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
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("wrapt").join("config.toml"))
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
}
