//! `wrapt fetch`: benchmark apt mirrors and optionally switch to the fastest.
//!
//! This is the feature people reach for `nala fetch` to get. It pulls the
//! geolocated Ubuntu mirror list, times a small download from each in parallel,
//! ranks them, and (with `--apply`) rewrites apt's sources to point the archive
//! at the fastest one — leaving the security pocket on the official host.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::time::Instant;

use crate::ui;
use crate::ui::Paint;

/// How many mirrors to probe, and how many to race at once.
const MAX_MIRRORS: usize = 60;
const CONCURRENCY: usize = 12;

struct Distro {
    id: String,
    codename: String,
}

/// A probed mirror and its measured throughput (bytes/sec), `None` on failure.
struct Probe {
    url: String,
    speed: Option<f64>,
}

pub async fn run(apply: bool, count: usize, country: Option<String>) -> Result<()> {
    let distro = detect_distro()?;
    if distro.id != "ubuntu" {
        bail!(
            "wrapt fetch currently supports Ubuntu only (detected '{}'). \
             Manage sources manually with `wrapt repo`.",
            distro.id
        );
    }

    ui::header("Fetching the mirror list...");
    let mut mirrors = mirror_list(country.as_deref()).await?;
    if mirrors.is_empty() {
        bail!("the mirror list was empty — try again, or pass --country <CC>");
    }

    // Always benchmark the mirror we're already on, so a switch can only ever
    // move to something faster — never silently downgrade a good local mirror.
    let current = current_archive_mirror();
    if let Some(c) = &current
        && !mirrors.iter().any(|m| same_mirror(m, c))
    {
        mirrors.push(c.clone());
    }

    ui::header(&format!(
        "Benchmarking {} mirror{}...",
        mirrors.len(),
        if mirrors.len() == 1 { "" } else { "s" }
    ));
    let mut probes = benchmark(&mirrors, &distro.codename).await;

    // Fastest first; failures (None) sink to the bottom.
    probes.sort_by(|a, b| {
        b.speed
            .partial_cmp(&a.speed)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let ranked: Vec<&Probe> = probes.iter().filter(|p| p.speed.is_some()).collect();
    if ranked.is_empty() {
        bail!("no mirror responded — check your network connection");
    }

    println!();
    for (i, p) in ranked.iter().take(count).enumerate() {
        let speed = p.speed.unwrap_or(0.0);
        let marker = if i == 0 {
            "★".yellow().bold().to_string()
        } else {
            format!("{:>2}", i + 1).dimmed().to_string()
        };
        let tag = if current.as_deref().is_some_and(|c| same_mirror(&p.url, c)) {
            format!(" {}", "(current)".dimmed())
        } else {
            String::new()
        };
        println!(
            "  {marker}  {:>11}/s  {}{tag}",
            ui::format_size(speed as u64).green(),
            p.url.cyan()
        );
    }
    println!();

    // A geolocated list with a single entry is just the fallback archive; a
    // country list gives something real to choose from.
    if ranked.len() < 2 && country.is_none() {
        ui::warn("Only one mirror was found for your location.");
        println!(
            "  {} {}",
            "Try a country list, e.g.".dimmed(),
            "wrapt fetch --country AU".cyan()
        );
    }

    let fastest = ranked[0];
    if !apply {
        ui::success(&format!("Fastest mirror: {}", fastest.url.cyan()));
        println!(
            "  {} {}",
            "Apply it with:".dimmed(),
            "sudo wrapt fetch --apply".cyan()
        );
        return Ok(());
    }

    decide_and_apply(fastest, &ranked, current.as_deref())
}

/// Apply the fastest mirror, but only when it's a genuine, meaningful win over
/// what's already configured — never on a benchmark of one, never a downgrade.
fn decide_and_apply(fastest: &Probe, ranked: &[&Probe], current: Option<&str>) -> Result<()> {
    // Nothing to compare against — refuse rather than clobber the current mirror.
    if ranked.len() < 2 {
        bail!(
            "only one mirror responded, so there's nothing to benchmark against — \
             run `wrapt fetch --country <CC>` (e.g. AU) for a real mirror list"
        );
    }

    // Already on the fastest mirror: leave it alone.
    if let Some(c) = current
        && same_mirror(&fastest.url, c)
    {
        ui::success(&format!(
            "Your current mirror is already the fastest: {}",
            c.cyan()
        ));
        return Ok(());
    }

    // If the current mirror responded, only switch for a clear (>10%) speedup —
    // the measurements are latency-noisy and not worth churning sources over.
    let current_speed = current.and_then(|c| {
        ranked
            .iter()
            .find(|p| same_mirror(&p.url, c))
            .and_then(|p| p.speed)
    });
    if let Some(cur) = current_speed
        && fastest.speed.unwrap_or(0.0) < cur * 1.10
    {
        ui::success("Your current mirror is within 10% of the fastest — keeping it.");
        return Ok(());
    }

    apply_mirror(&fastest.url)
}

/// The archive mirror currently configured in apt's sources (the first
/// `archive.ubuntu.com` URI, which excludes the security pocket), if any.
fn current_archive_mirror() -> Option<String> {
    let root = apt_dir();
    for path in [
        root.join("sources.list.d").join("ubuntu.sources"),
        root.join("sources.list"),
    ] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            for tok in line.split_whitespace() {
                if (tok.starts_with("http://") || tok.starts_with("https://"))
                    && tok.contains("archive.ubuntu.com")
                {
                    // Normalise to the trailing-slash form the mirror list uses.
                    let base = tok.trim_end_matches('/');
                    return Some(format!("{base}/"));
                }
            }
        }
    }
    None
}

/// Two mirror URLs naming the same location (ignoring a trailing slash).
fn same_mirror(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Read `/etc/os-release` (overridable via `WRAPT_OS_RELEASE`) for the distro
/// id and release codename.
fn detect_distro() -> Result<Distro> {
    let path = std::env::var_os("WRAPT_OS_RELEASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/os-release"));
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let mut id = String::new();
    let mut codename = String::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "ID" => id = value,
            // VERSION_CODENAME is preferred; UBUNTU_CODENAME is a fallback.
            "VERSION_CODENAME" => codename = value,
            "UBUNTU_CODENAME" if codename.is_empty() => codename = value,
            _ => {}
        }
    }
    if codename.is_empty() {
        bail!(
            "could not determine the release codename from {}",
            path.display()
        );
    }
    Ok(Distro { id, codename })
}

/// Pull the list of mirror base URLs from mirrors.ubuntu.com (geolocated, or a
/// specific country when `country` is given).
async fn mirror_list(country: Option<&str>) -> Result<Vec<String>> {
    let url = match country {
        Some(cc) => format!("http://mirrors.ubuntu.com/{}.txt", cc.to_uppercase()),
        None => "http://mirrors.ubuntu.com/mirrors.txt".to_string(),
    };
    let client = reqwest::Client::builder()
        .user_agent(concat!("wrapt/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;
    let body = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to fetch {url}"))?
        .error_for_status()
        .context("the mirror list request failed (bad country code?)")?
        .text()
        .await?;

    let mut mirrors: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(|l| {
            if l.ends_with('/') {
                l.to_string()
            } else {
                format!("{l}/")
            }
        })
        .collect();
    mirrors.truncate(MAX_MIRRORS);
    Ok(mirrors)
}

/// Time a small download (`dists/<codename>/Release`) from each mirror.
async fn benchmark(mirrors: &[String], codename: &str) -> Vec<Probe> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("wrapt/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .build()
        .expect("reqwest client");

    let bar = ProgressBar::new(mirrors.len() as u64);
    bar.set_style(
        ProgressStyle::with_template("  [{bar:30.cyan/black}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("━╸ "),
    );

    let probes: Vec<Probe> = stream::iter(mirrors)
        .map(|url| {
            let client = client.clone();
            let bar = bar.clone();
            let probe_url = format!("{url}dists/{codename}/Release");
            async move {
                let speed = probe_speed(&client, &probe_url).await;
                bar.inc(1);
                Probe {
                    url: url.clone(),
                    speed,
                }
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;

    bar.finish_and_clear();
    probes
}

/// Download `url` fully and return bytes/second, or `None` on any failure.
async fn probe_speed(client: &reqwest::Client, url: &str) -> Option<f64> {
    let start = Instant::now();
    let resp = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let bytes = resp.bytes().await.ok()?;
    let elapsed = start.elapsed().as_secs_f64();
    if bytes.is_empty() || elapsed <= 0.0 {
        return None;
    }
    Some(bytes.len() as f64 / elapsed)
}

/// Where apt's sources live (`WRAPT_APT_DIR` overrides for testing).
fn apt_dir() -> PathBuf {
    std::env::var_os("WRAPT_APT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/apt"))
}

/// Point the archive sources at `mirror`, backing each edited file up first and
/// leaving the security pocket untouched.
fn apply_mirror(mirror: &str) -> Result<()> {
    crate::apt::ensure_root()?;
    // deb822 URIs usually carry no trailing slash; match that style.
    let mirror = mirror.trim_end_matches('/');

    let root = apt_dir();
    let candidates = [
        root.join("sources.list.d").join("ubuntu.sources"),
        root.join("sources.list"),
    ];

    let mut changed_files = 0usize;
    let mut changed_uris = 0usize;
    for path in &candidates {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let (rewritten, n) = rewrite_archive_uris(&text, mirror);
        if n == 0 {
            continue;
        }
        let backup = path.with_extension(format!(
            "{}wrapt-bak",
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| format!("{e}."))
                .unwrap_or_default()
        ));
        std::fs::write(&backup, &text)
            .with_context(|| format!("cannot write backup {}", backup.display()))?;
        std::fs::write(path, &rewritten)
            .with_context(|| format!("cannot write {}", path.display()))?;
        ui::success(&format!(
            "Updated {} ({n} source{}) — backup at {}",
            path.display(),
            if n == 1 { "" } else { "s" },
            backup.display()
        ));
        changed_files += 1;
        changed_uris += n;
    }

    if changed_files == 0 {
        ui::warn("Couldn't find the default archive sources to update automatically.");
        println!("  Point your archive source at this mirror by hand:");
        println!("    {}", mirror.cyan());
        return Ok(());
    }

    ui::success(&format!(
        "Switched {changed_uris} archive source(s) to {}.",
        mirror.cyan()
    ));
    println!("  {} {}", "Refresh with:".dimmed(), "wrapt update".cyan());
    Ok(())
}

/// Replace archive.ubuntu.com URIs with `mirror`, skipping comments and the
/// security pocket (which never contains "archive.ubuntu.com"). Returns the new
/// text and the number of URIs changed.
fn rewrite_archive_uris(text: &str, mirror: &str) -> (String, usize) {
    let mut changed = 0;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        // Leave comments alone.
        if body.trim_start().starts_with('#') {
            out.push_str(line);
            continue;
        }
        if !body.contains("archive.ubuntu.com") {
            out.push_str(line);
            continue;
        }
        // Rewrite each whitespace token that names an archive mirror. This
        // covers both classic `deb <uri> …` lines and deb822 `URIs: <uri>` lines.
        let rebuilt: Vec<String> = body
            .split_whitespace()
            .map(|tok| {
                if tok.contains("archive.ubuntu.com") {
                    changed += 1;
                    mirror.to_string()
                } else {
                    tok.to_string()
                }
            })
            .collect();
        // Preserve the leading indentation of the original line.
        let indent: String = body.chars().take_while(|c| c.is_whitespace()).collect();
        out.push_str(&indent);
        out.push_str(&rebuilt.join(" "));
        out.push_str(newline);
    }
    (out, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIRROR: &str = "http://mirror.example/ubuntu";

    #[test]
    fn rewrites_deb822_archive_but_not_security() {
        let text = "\
Types: deb\n\
URIs: http://archive.ubuntu.com/ubuntu\n\
Suites: noble noble-updates\n\
Components: main universe\n\
\n\
Types: deb\n\
URIs: http://security.ubuntu.com/ubuntu\n\
Suites: noble-security\n";
        let (out, n) = rewrite_archive_uris(text, MIRROR);
        assert_eq!(n, 1);
        assert!(out.contains("URIs: http://mirror.example/ubuntu"));
        // Security pocket is left exactly as it was.
        assert!(out.contains("URIs: http://security.ubuntu.com/ubuntu"));
    }

    #[test]
    fn rewrites_classic_and_country_mirror() {
        let text = "\
deb http://us.archive.ubuntu.com/ubuntu noble main\n\
# deb http://archive.ubuntu.com/ubuntu noble main\n\
deb http://security.ubuntu.com/ubuntu noble-security main\n";
        let (out, n) = rewrite_archive_uris(text, MIRROR);
        assert_eq!(n, 1); // only the live archive line
        assert!(out.contains("deb http://mirror.example/ubuntu noble main"));
        // Comment preserved verbatim, security untouched.
        assert!(out.contains("# deb http://archive.ubuntu.com/ubuntu noble main"));
        assert!(out.contains("deb http://security.ubuntu.com/ubuntu noble-security main"));
    }

    #[test]
    fn no_archive_uris_changes_nothing() {
        let text = "deb http://security.ubuntu.com/ubuntu noble-security main\n";
        let (out, n) = rewrite_archive_uris(text, MIRROR);
        assert_eq!(n, 0);
        assert_eq!(out, text);
    }

    #[test]
    fn same_mirror_ignores_trailing_slash() {
        assert!(same_mirror(
            "http://au.archive.ubuntu.com/ubuntu",
            "http://au.archive.ubuntu.com/ubuntu/"
        ));
        assert!(!same_mirror(
            "http://au.archive.ubuntu.com/ubuntu",
            "http://archive.ubuntu.com/ubuntu"
        ));
    }
}
