//! `wrapt repo`: list, add, and remove apt software sources / PPAs.
//!
//! Listing parses the sources files directly (both the classic one-line format
//! and the newer deb822 `.sources` stanzas). Adding and removing delegate to
//! `add-apt-repository`, which handles PPA key import and the file plumbing.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};
use owo_colors::OwoColorize;

use crate::cli::RepoCmd;
use crate::{apt, ui};

pub fn run(action: RepoCmd) -> Result<()> {
    match action {
        RepoCmd::List => list(),
        RepoCmd::Add { repo, yes } => modify(&repo, false, yes),
        RepoCmd::Remove { repo, yes } => modify(&repo, true, yes),
    }
}

/// One configured source line, however it was written.
pub(crate) struct Source {
    pub(crate) kind: String,       // deb | deb-src
    pub(crate) uri: String,        // http://…
    pub(crate) suite: String,      // noble, noble-updates, …
    pub(crate) components: String, // "main universe" (may be empty)
    pub(crate) enabled: bool,
}

pub(crate) fn apt_dir() -> PathBuf {
    // WRAPT_APT_DIR overrides /etc/apt for testing without touching the system.
    std::env::var_os("WRAPT_APT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/apt"))
}

/// Parse every source under `root` (both `sources.list` and the classic/deb822
/// files in `sources.list.d`), returning `(path, sources)` for each file that
/// has any. Shared by `repo list` and `doctor`'s duplicate-source check.
pub(crate) fn collect_sources(root: &std::path::Path) -> Vec<(PathBuf, Vec<Source>)> {
    let mut files: Vec<(PathBuf, Vec<Source>)> = Vec::new();

    let main_list = root.join("sources.list");
    if let Ok(text) = std::fs::read_to_string(&main_list) {
        let parsed = parse_one_line(&text);
        if !parsed.is_empty() {
            files.push((main_list, parsed));
        }
    }

    let d = root.join("sources.list.d");
    if let Ok(entries) = std::fs::read_dir(&d) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed = match path.extension().and_then(|e| e.to_str()) {
                Some("sources") => parse_deb822(&text),
                Some("list") => parse_one_line(&text),
                _ => continue,
            };
            if !parsed.is_empty() {
                files.push((path, parsed));
            }
        }
    }
    files
}

fn list() -> Result<()> {
    let root = apt_dir();
    let files = collect_sources(&root);

    if files.is_empty() {
        ui::warn(&format!(
            "No software sources found under {}.",
            root.display()
        ));
        return Ok(());
    }

    let total: usize = files.iter().map(|(_, s)| s.len()).sum();
    ui::header(&format!("Software sources ({total})"));
    for (path, sources) in &files {
        println!();
        println!("   {}", path.display().to_string().bold());
        for s in sources {
            let disabled = if s.enabled {
                String::new()
            } else {
                format!(" {}", "(disabled)".dimmed())
            };
            let components = if s.components.is_empty() {
                String::new()
            } else {
                format!(" {}", s.components.dimmed())
            };
            println!(
                "     {} {} {}{components}{disabled}",
                s.kind.dimmed(),
                s.uri.cyan(),
                s.suite.green(),
            );
        }
    }
    Ok(())
}

/// Parse classic one-line `deb`/`deb-src` entries, skipping comments.
fn parse_one_line(text: &str) -> Vec<Source> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        // A leading '#' disables the line; still surface it, marked disabled.
        let (enabled, body) = match line.strip_prefix('#') {
            Some(rest) => (false, rest.trim()),
            None => (true, line),
        };
        if body.is_empty() {
            continue;
        }
        let mut tokens = body.split_whitespace().peekable();
        let kind = match tokens.next() {
            Some(k @ ("deb" | "deb-src")) => k.to_string(),
            _ => continue,
        };
        // Skip an optional [ options ] group.
        if tokens.peek().is_some_and(|t| t.starts_with('[')) {
            for t in tokens.by_ref() {
                if t.ends_with(']') {
                    break;
                }
            }
        }
        let Some(uri) = tokens.next() else { continue };
        let Some(suite) = tokens.next() else { continue };
        let components = tokens.collect::<Vec<_>>().join(" ");
        out.push(Source {
            kind,
            uri: uri.to_string(),
            suite: suite.to_string(),
            components,
            enabled,
        });
    }
    out
}

/// Parse deb822 `.sources` stanzas into one `Source` per Types×Suites pair.
fn parse_deb822(text: &str) -> Vec<Source> {
    let mut out = Vec::new();
    for stanza in text.split("\n\n") {
        let mut types = Vec::new();
        let mut uris = Vec::new();
        let mut suites = Vec::new();
        let mut components = String::new();
        let mut enabled = true;
        for line in stanza.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "types" => types = value.split_whitespace().map(str::to_string).collect(),
                "uris" => uris = value.split_whitespace().map(str::to_string).collect(),
                "suites" => suites = value.split_whitespace().map(str::to_string).collect(),
                "components" => components = value.to_string(),
                "enabled" => {
                    enabled = !matches!(value.to_ascii_lowercase().as_str(), "no" | "false")
                }
                _ => {}
            }
        }
        if types.is_empty() || uris.is_empty() || suites.is_empty() {
            continue;
        }
        for kind in &types {
            for suite in &suites {
                out.push(Source {
                    kind: kind.clone(),
                    uri: uris[0].clone(),
                    suite: suite.clone(),
                    components: components.clone(),
                    enabled,
                });
            }
        }
    }
    out
}

/// Add (`remove == false`) or remove a repository via add-apt-repository.
fn modify(repo: &str, remove: bool, yes: bool) -> Result<()> {
    apt::ensure_root()?;

    let (verb, prompt) = if remove {
        ("Removing", format!("Remove source '{repo}'?"))
    } else {
        ("Adding", format!("Add source '{repo}'?"))
    };
    if !yes && !ui::confirm(&prompt, true) {
        ui::warn("Aborted.");
        return Ok(());
    }

    ui::header(&format!("{verb} {repo}..."));
    let mut cmd = Command::new("add-apt-repository");
    cmd.arg("-y");
    if remove {
        cmd.arg("--remove");
    }
    cmd.arg(repo);

    let status = cmd.status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!(
                "add-apt-repository not found — install it with `wrapt install software-properties-common`"
            )
        } else {
            anyhow::Error::new(e).context("failed to run add-apt-repository")
        }
    })?;
    if !status.success() {
        bail!("add-apt-repository failed");
    }

    ui::success(&format!(
        "{} {repo}.",
        if remove { "Removed" } else { "Added" }
    ));
    if !remove {
        println!(
            "   Run {} to fetch packages from the new source.",
            "wrapt update".cyan()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_line_sources() {
        let text = "\
# a comment\n\
deb http://archive.ubuntu.com/ubuntu noble main universe\n\
deb-src [arch=amd64] http://archive.ubuntu.com/ubuntu noble main\n\
# deb http://disabled.example/ubuntu noble main\n";
        let s = parse_one_line(text);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].kind, "deb");
        assert_eq!(s[0].uri, "http://archive.ubuntu.com/ubuntu");
        assert_eq!(s[0].suite, "noble");
        assert_eq!(s[0].components, "main universe");
        assert!(s[0].enabled);
        // The [arch=amd64] option group is skipped, not mistaken for the URI.
        assert_eq!(s[1].uri, "http://archive.ubuntu.com/ubuntu");
        // The commented `deb` line is surfaced but marked disabled.
        assert!(!s[2].enabled);
    }

    #[test]
    fn parses_deb822_sources() {
        let text = "\
Types: deb\n\
URIs: http://archive.ubuntu.com/ubuntu\n\
Suites: noble noble-updates\n\
Components: main universe\n\
Enabled: yes\n";
        let s = parse_deb822(text);
        assert_eq!(s.len(), 2); // one per suite
        assert_eq!(s[0].suite, "noble");
        assert_eq!(s[1].suite, "noble-updates");
        assert_eq!(s[0].components, "main universe");
        assert!(s[0].enabled);
    }

    #[test]
    fn deb822_enabled_no_is_disabled() {
        let text = "Types: deb\nURIs: http://x/y\nSuites: noble\nEnabled: no\n";
        let s = parse_deb822(text);
        assert_eq!(s.len(), 1);
        assert!(!s[0].enabled);
    }
}
