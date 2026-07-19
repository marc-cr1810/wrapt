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
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("invalid config at {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("cannot read {}: {e}", path.display())),
        }
    }

    /// Apply the colour policy to the global colouring override. "auto" leaves
    /// owo-colors to honour tty detection and the NO_COLOR convention.
    pub fn apply_color(&self) {
        match self.color.as_deref() {
            Some("always") => owo_colors::set_override(true),
            Some("never") => owo_colors::set_override(false),
            _ if std::env::var_os("NO_COLOR").is_some() => owo_colors::set_override(false),
            _ => {}
        }
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
