use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use owo_colors::OwoColorize;

use crate::download::{DownloadItem, ExpectedHash};
use crate::ui;

/// One package changed by a transaction. `old` is the currently installed
/// version (upgrade/removal), `new` the version being installed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Change {
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
    /// The new version comes from a security pocket (`-security`).
    #[serde(default)]
    pub security: bool,
}

#[derive(Debug, Default)]
pub struct Transaction {
    pub install: Vec<Change>,
    pub remove: Vec<Change>,
}

/// Disk usage impact of a transaction, in bytes.
pub struct DiskUsage {
    /// Total installed size of everything being installed/upgraded.
    pub installed: u64,
    /// Net change on disk once old versions/removals are accounted for.
    pub net_change: i64,
}

impl Transaction {
    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.remove.is_empty()
    }
}

pub struct SearchResult {
    pub name: String,
    pub version: Option<String>,
    pub description: String,
    pub installed: bool,
}

pub fn ensure_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("this operation requires root privileges — try: sudo wrapt ...");
    }
    Ok(())
}

fn apt_get() -> Command {
    let mut cmd = Command::new("apt-get");
    cmd.env("LC_ALL", "C");
    cmd
}

/// Extract apt's "E: ..." lines into a single error message.
fn apt_error(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let errors: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("E: "))
        .collect();
    if errors.is_empty() {
        stderr.into_owned()
    } else {
        errors.join("\n")
    }
}

/// Run `apt-get -s <args>` and parse the resulting transaction.
pub fn simulate(args: &[String]) -> Result<Transaction> {
    let out = apt_get()
        .arg("-s")
        .arg("-y")
        .args(args)
        .output()
        .context("failed to run apt-get — is this a Debian-based system?")?;
    if !out.status.success() {
        bail!("{}", apt_error(&out.stderr));
    }

    let mut tx = Transaction::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut tokens = line.split_whitespace();
        match tokens.next() {
            Some("Inst") => {
                // Inst <name> [<old>] (<new> <origin…> [<arch>])
                let name = tokens.next().unwrap_or_default().to_string();
                let rest: Vec<&str> = tokens.collect();
                let mut old = None;
                let mut idx = 0;
                if let Some(v) = rest.first().and_then(|t| t.strip_prefix('[')) {
                    old = Some(v.trim_end_matches(']').to_string());
                    idx = 1;
                }
                let new = rest
                    .get(idx)
                    .and_then(|t| t.strip_prefix('('))
                    .map(str::to_string);
                // The origin token(s) name the archive/suite, e.g.
                // "Ubuntu:26.04/resolute-security"; scan them for "-security".
                let security = rest[idx..].iter().any(|t| t.contains("-security"));
                tx.install.push(Change {
                    name,
                    old,
                    new,
                    security,
                });
            }
            Some("Remv") => {
                let name = tokens.next().unwrap_or_default().to_string();
                let old = tokens
                    .next()
                    .map(|v| v.trim_start_matches('[').trim_end_matches(']').to_string());
                tx.remove.push(Change {
                    name,
                    old,
                    new: None,
                    security: false,
                });
            }
            _ => {}
        }
    }
    Ok(tx)
}

/// Compute the disk usage impact of a transaction from package metadata
/// (apt 3.x no longer prints "After this operation..." in simulations).
pub fn disk_usage(tx: &Transaction) -> Option<DiskUsage> {
    // Installed-Size of every version being installed, via one apt-cache call.
    let specs: Vec<String> = tx
        .install
        .iter()
        .filter_map(|c| Some(format!("{}={}", c.name, c.new.as_deref()?)))
        .collect();
    let mut installed = 0u64;
    if !specs.is_empty() {
        let out = Command::new("apt-cache")
            .arg("show")
            .arg("--")
            .args(&specs)
            .env("LC_ALL", "C")
            .output()
            .ok()?;
        for record in String::from_utf8_lossy(&out.stdout).split("\n\n") {
            for line in record.lines() {
                if let Some(kib) = line.strip_prefix("Installed-Size: ") {
                    installed += kib.trim().parse::<u64>().unwrap_or(0) * 1024;
                    break;
                }
            }
        }
    }

    // Installed-Size of the versions being replaced or removed.
    let old_names: Vec<&str> = tx
        .install
        .iter()
        .filter(|c| c.old.is_some())
        .chain(tx.remove.iter())
        .map(|c| c.name.as_str())
        .collect();
    let mut old_size = 0u64;
    if !old_names.is_empty()
        && let Ok(out) = Command::new("dpkg-query")
            .args(["-W", "-f", "${Installed-Size}\n"])
            .args(&old_names)
            .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            old_size += line.trim().parse::<u64>().unwrap_or(0) * 1024;
        }
    }

    Some(DiskUsage {
        installed,
        net_change: installed as i64 - old_size as i64,
    })
}

/// Run `apt-get --print-uris <args>` and parse the packages apt would download.
pub fn print_uris(args: &[String]) -> Result<Vec<DownloadItem>> {
    let out = apt_get()
        .args(["-qq", "-y", "--print-uris"])
        .args(args)
        .output()
        .context("failed to run apt-get")?;
    if !out.status.success() {
        bail!("{}", apt_error(&out.stderr));
    }

    let mut items = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut tokens = line.split_whitespace();
        let (Some(url), Some(filename), Some(size)) = (tokens.next(), tokens.next(), tokens.next())
        else {
            continue;
        };
        if !url.starts_with('\'') {
            continue;
        }
        let hash = tokens.next().and_then(|tok| {
            let (kind, hex) = tok.split_once(':')?;
            match kind {
                "SHA256" => Some(ExpectedHash::Sha256(hex.to_lowercase())),
                "MD5Sum" => Some(ExpectedHash::Md5(hex.to_lowercase())),
                _ => None,
            }
        });
        items.push(DownloadItem {
            url: url.trim_matches('\'').to_string(),
            filename: filename.to_string(),
            size: size.parse().unwrap_or(0),
            hash,
        });
    }
    Ok(items)
}

/// Run `apt-get update`, restyling its progress lines as they stream in.
pub fn update_pretty() -> Result<()> {
    let mut child = apt_get()
        .arg("update")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to run apt-get")?;

    let stderr = child.stderr.take().unwrap();
    let stderr_thread = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            lines.push(line);
        }
        lines
    });

    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        let Some((tag, rest)) = line.split_once(' ') else {
            continue;
        };
        match tag.split(':').next() {
            Some("Hit") => println!("  {} {}", "✓".green().bold(), rest.dimmed()),
            Some("Get") => println!("  {} {}", "↓".cyan().bold(), rest),
            Some("Ign") => println!("  {} {}", "–".yellow().bold(), rest.dimmed()),
            Some("Err") => println!("  {} {}", "✗".red().bold(), rest),
            _ => {}
        }
    }

    let status = child.wait()?;
    let stderr_lines = stderr_thread.join().unwrap_or_default();
    for line in &stderr_lines {
        if let Some(msg) = line.strip_prefix("W: ") {
            ui::warn(msg);
        }
    }
    if !status.success() {
        let errors: Vec<&str> = stderr_lines
            .iter()
            .filter_map(|l| l.strip_prefix("E: "))
            .collect();
        bail!("{}", errors.join("\n"));
    }
    Ok(())
}

/// Names of all currently installed packages (both plain and name:arch forms).
pub fn installed_set() -> HashSet<String> {
    let Ok(out) = Command::new("dpkg-query")
        .args(["-W", "-f", "${db:Status-Status} ${binary:Package}\n"])
        .output()
    else {
        return HashSet::new();
    };
    let mut set = HashSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(pkg) = line.strip_prefix("installed ") {
            set.insert(pkg.to_string());
            if let Some((name, _arch)) = pkg.split_once(':') {
                set.insert(name.to_string());
            }
        }
    }
    set
}

/// Set or release a hold on packages via apt-mark (`hold`/`unhold`).
pub fn set_hold(hold: bool, packages: &[String]) -> Result<Vec<String>> {
    let action = if hold { "hold" } else { "unhold" };
    let out = Command::new("apt-mark")
        .arg(action)
        .args(packages)
        .env("LC_ALL", "C")
        .output()
        .context("failed to run apt-mark")?;
    if !out.status.success() {
        bail!("{}", apt_error(&out.stderr));
    }
    // apt-mark echoes a line per package it changed.
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// Packages currently held (`apt-mark showhold`).
pub fn held() -> Vec<String> {
    Command::new("apt-mark")
        .arg("showhold")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Names apt considers manually installed (`apt-mark showmanual`).
pub fn manual_set() -> HashSet<String> {
    let Ok(out) = Command::new("apt-mark").arg("showmanual").output() else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.split(':').next().unwrap_or(l).trim().to_string())
        .collect()
}

pub fn search(query: &str) -> Result<Vec<SearchResult>> {
    let out = Command::new("apt-cache")
        .args(["search", "--", query])
        .env("LC_ALL", "C")
        .output()
        .context("failed to run apt-cache")?;
    if !out.status.success() {
        bail!("{}", apt_error(&out.stderr));
    }

    let installed = installed_set();
    let mut results: Vec<SearchResult> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (name, desc) = line.split_once(" - ")?;
            Some(SearchResult {
                installed: installed.contains(name),
                name: name.to_string(),
                version: None,
                description: desc.to_string(),
            })
        })
        .collect();

    // Exact matches first, then name matches, then everything else.
    results.sort_by_key(|r| (r.name != query, !r.name.contains(query), r.name.clone()));
    Ok(results)
}

/// The first record of `apt-cache show <package>`.
pub fn show(package: &str) -> Result<String> {
    let out = Command::new("apt-cache")
        .args(["show", "--", package])
        .env("LC_ALL", "C")
        .output()
        .context("failed to run apt-cache")?;
    if !out.status.success() {
        bail!("{}", apt_error(&out.stderr));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let record = stdout.split("\n\n").next().unwrap_or_default();
    Ok(record.to_string())
}
