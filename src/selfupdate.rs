//! `wrapt self-update`: keep wrapt itself current from its GitHub Releases.
//!
//! The release workflow (.github/workflows/release.yml) publishes a `.deb` for
//! every `v*` tag. Those aren't in any apt repository, so `apt upgrade` can't
//! see them — this command bridges that gap by asking the GitHub Releases API
//! for the latest tag, comparing it with the compiled-in version, and (unless
//! `--check`) downloading and installing the matching `.deb`.

use std::cmp::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::download::{self, DownloadItem};
use crate::ui::Paint;
use crate::{apt, exec, lists, ui};

/// Fallback repository the releases live under. Override at build time with
/// `WRAPT_REPO=owner/name`, at runtime with the `WRAPT_REPO` env var, or with
/// `repo = "owner/name"` in the config file.
pub const DEFAULT_REPO: &str = "marc-cr1810/wrapt";

/// Resolve the `owner/repo` to update from: runtime env wins, then the config
/// file value, then the compile-time default.
pub fn resolve_repo(config_repo: Option<&str>) -> String {
    if let Ok(env) = std::env::var("WRAPT_REPO")
        && !env.trim().is_empty()
    {
        return env;
    }
    config_repo
        .filter(|r| !r.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Check for (and optionally install) a newer wrapt.
pub async fn run(check: bool, jobs: usize, repo: &str) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");

    ui::header(&format!("Checking {repo} for updates..."));
    let release = fetch_latest(repo).await?;
    let latest = release.tag_name.trim_start_matches('v');

    if lists::deb_version_cmp(latest, current) != Ordering::Greater {
        ui::success(&format!("wrapt is already up to date (v{current})."));
        return Ok(());
    }

    println!(
        "   {} {} {} {}",
        "Update available:".bold(),
        format!("v{current}").dimmed(),
        "→".cyan(),
        format!("v{latest}").green().bold()
    );

    if check {
        println!("   Run {} to install it.", "sudo wrapt self-update".cyan());
        return Ok(());
    }

    // Actually installing needs root (dpkg writes to the system).
    apt::ensure_root()?;

    let arch = dpkg_arch();
    let host = host_os_tag();
    let asset = pick_asset(&release.assets, &arch, host.as_deref()).ok_or_else(|| {
        anyhow!(
            "release v{latest} has no .deb for architecture '{arch}' — \
             install it manually from {}",
            release.html_url
        )
    })?;

    let dir = std::env::temp_dir().join("wrapt-selfupdate");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let item = DownloadItem {
        url: asset.browser_download_url.clone(),
        filename: asset.name.clone(),
        size: asset.size,
        hash: None,
    };

    ui::header("Downloading update...");
    download::download_all(std::slice::from_ref(&item), &dir, jobs).await?;

    let deb = dir.join(&asset.name);
    ui::header("Installing update...");
    // Install via apt so any new dependencies are resolved; dpkg safely swaps
    // the running binary's file out from under us (rename onto a new inode).
    let deb_arg = deb.to_string_lossy().to_string();
    exec::run_with_progress(&["install".to_string(), "-y".to_string(), deb_arg], false)?;
    let _ = std::fs::remove_file(&deb);

    ui::success(&format!("Updated wrapt to v{latest}."));
    Ok(())
}

/// Best-effort "a newer version exists" notice for the end of an upgrade. Never
/// fails the caller: any network/parse error is silently ignored, and a short
/// timeout keeps it from hanging the command.
pub async fn notify_if_outdated(repo: &str) {
    let current = env!("CARGO_PKG_VERSION");
    let Ok(release) = fetch_latest_quick(repo).await else {
        return;
    };
    let latest = release.tag_name.trim_start_matches('v');
    if lists::deb_version_cmp(latest, current) == Ordering::Greater {
        println!();
        ui::warn(&format!(
            "A newer wrapt is available (v{current} → v{latest}). Run `wrapt self-update`."
        ));
    }
}

async fn fetch_latest(repo: &str) -> Result<Release> {
    fetch_with_timeout(repo, Duration::from_secs(20)).await
}

async fn fetch_latest_quick(repo: &str) -> Result<Release> {
    fetch_with_timeout(repo, Duration::from_secs(3)).await
}

async fn fetch_with_timeout(repo: &str, timeout: Duration) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .user_agent(concat!("wrapt/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("could not reach the GitHub releases API")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(
            "no published release found for '{repo}' — \
             check the repo name (override with WRAPT_REPO=owner/name) or tag a release first"
        );
    }
    let resp = resp
        .error_for_status()
        .context("the GitHub releases API returned an error")?;
    let body = resp.text().await?;
    serde_json::from_str(&body).context("could not parse the GitHub releases response")
}

/// Pick the release asset to install for this host. Releases ship one `.deb`
/// per Ubuntu version (`wrapt_<ver>_ubuntu24.04_amd64.deb`, …), so among the
/// `.deb`s for our architecture we prefer the one built for the running system.
/// Failing an exact match we take the *safest* build — an untagged general
/// `.deb`, else the lowest OS version, which carries the lowest ABI floor and
/// therefore runs on the widest range of systems.
fn pick_asset<'a>(assets: &'a [Asset], arch: &str, host_tag: Option<&str>) -> Option<&'a Asset> {
    let suffix = format!("_{arch}.deb");
    let debs: Vec<&Asset> = assets
        .iter()
        .filter(|a| a.name.ends_with(&suffix))
        .collect();
    if debs.is_empty() {
        // No arch-specific name; fall back to any .deb (e.g. an `_all.deb`).
        return assets.iter().find(|a| a.name.ends_with(".deb"));
    }

    if let Some(tag) = host_tag {
        let needle = format!("_{tag}_");
        if let Some(a) = debs.iter().copied().find(|a| a.name.contains(&needle)) {
            return Some(a);
        }
    }

    debs.iter().copied().min_by(|a, b| {
        match (asset_os_version(&a.name), asset_os_version(&b.name)) {
            (Some(x), Some(y)) => lists::deb_version_cmp(&x, &y),
            // An untagged general build has the widest compatibility.
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        }
    })
}

/// Extract the OS version from an asset name like `..._ubuntu24.04_amd64.deb`.
fn asset_os_version(name: &str) -> Option<String> {
    let after = name.split("_ubuntu").nth(1)?;
    let ver = after.split('_').next()?;
    (!ver.is_empty()).then(|| ver.to_string())
}

/// The running system's OS tag (`ubuntu24.04`), matching build-deb.sh's
/// `DEB_OS_TAG`, from `/etc/os-release`.
fn host_os_tag() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    let mut id = None;
    let mut version = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("ID=") {
            id = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            version = Some(v.trim().trim_matches('"').to_string());
        }
    }
    Some(format!("{}{}", id?, version?))
}

fn dpkg_arch() -> String {
    std::process::Command::new("dpkg")
        .arg("--print-architecture")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "amd64".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            browser_download_url: format!("https://example/{name}"),
            size: 0,
        }
    }

    #[test]
    fn picks_arch_specific_deb() {
        let assets = vec![
            asset("wrapt_0.2.0_arm64.deb"),
            asset("wrapt_0.2.0_amd64.deb"),
            asset("wrapt-0.2.0.tar.gz"),
        ];
        let got = pick_asset(&assets, "amd64", None).unwrap();
        assert_eq!(got.name, "wrapt_0.2.0_amd64.deb");
    }

    #[test]
    fn prefers_matching_host_os() {
        let assets = vec![
            asset("wrapt_0.2.0_ubuntu24.04_amd64.deb"),
            asset("wrapt_0.2.0_ubuntu25.04_amd64.deb"),
            asset("wrapt_0.2.0_ubuntu26.04_amd64.deb"),
        ];
        let got = pick_asset(&assets, "amd64", Some("ubuntu25.04")).unwrap();
        assert_eq!(got.name, "wrapt_0.2.0_ubuntu25.04_amd64.deb");
    }

    #[test]
    fn without_host_match_takes_lowest_os_version() {
        // An unknown host (e.g. Debian) should get the widest-compatibility build.
        let assets = vec![
            asset("wrapt_0.2.0_ubuntu26.04_amd64.deb"),
            asset("wrapt_0.2.0_ubuntu24.04_amd64.deb"),
            asset("wrapt_0.2.0_ubuntu25.04_amd64.deb"),
        ];
        let got = pick_asset(&assets, "amd64", Some("debian12")).unwrap();
        assert_eq!(got.name, "wrapt_0.2.0_ubuntu24.04_amd64.deb");
    }

    #[test]
    fn untagged_general_deb_wins_as_safest() {
        let assets = vec![
            asset("wrapt_0.2.0_ubuntu26.04_amd64.deb"),
            asset("wrapt_0.2.0_amd64.deb"),
        ];
        let got = pick_asset(&assets, "amd64", None).unwrap();
        assert_eq!(got.name, "wrapt_0.2.0_amd64.deb");
    }

    #[test]
    fn falls_back_to_any_deb() {
        let assets = vec![asset("wrapt_0.2.0_all.deb"), asset("notes.txt")];
        assert_eq!(
            pick_asset(&assets, "amd64", None).unwrap().name,
            "wrapt_0.2.0_all.deb"
        );
    }

    #[test]
    fn no_deb_yields_none() {
        let assets = vec![asset("wrapt-0.2.0.tar.gz")];
        assert!(pick_asset(&assets, "amd64", None).is_none());
    }

    #[test]
    fn parses_asset_os_version() {
        assert_eq!(
            asset_os_version("wrapt_0.2.0_ubuntu24.04_amd64.deb").as_deref(),
            Some("24.04")
        );
        assert_eq!(asset_os_version("wrapt_0.2.0_amd64.deb"), None);
    }

    #[test]
    fn resolve_repo_precedence() {
        // Config value is used when no env override is set.
        // (We avoid setting the process-wide env var in tests to prevent races.)
        assert_eq!(resolve_repo(Some("acme/wrapt")), "acme/wrapt");
        assert_eq!(resolve_repo(None), DEFAULT_REPO);
        assert_eq!(resolve_repo(Some("  ")), DEFAULT_REPO);
    }
}
