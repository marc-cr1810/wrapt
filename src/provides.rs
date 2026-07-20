//! `wrapt provides <file-or-command>` — find which package owns a file or
//! would provide a command. Installed files are resolved with `dpkg -S`; for
//! not-yet-installed files it uses `apt-file` when that index is available.

use std::process::Command;

use anyhow::Result;

use crate::apt;
use crate::ui;
use crate::ui::Paint;

pub fn run(pattern: &str, json: bool) -> Result<()> {
    // 1. Is it a command already on PATH? Point at its owning package.
    if !pattern.contains('/')
        && let Some(path) = which(pattern)
        && let Some(pkg) = dpkg_owner(&path)
    {
        if json {
            return print_json(&[(pkg, path)], &apt::installed_set());
        }
        ui::header(pattern);
        println!(
            "   {} is provided by {} {}",
            path.cyan(),
            pkg.bold().green(),
            "[installed]".green()
        );
        return Ok(());
    }

    // 2. Search installed packages' file lists (fast, always available).
    let installed = dpkg_search(pattern);
    // 3. Search all packages via apt-file, if its index is present.
    let available = apt_file_search(pattern);

    if installed.is_empty() && available.is_empty() {
        if json {
            return print_json(&[], &apt::installed_set());
        }
        ui::warn(&format!("No package found providing '{pattern}'."));
        if !apt_file_available() {
            println!(
                "   {}",
                "Only installed files were searched. For all packages, install apt-file:".dimmed()
            );
            println!(
                "     {}",
                "wrapt install apt-file && sudo apt-file update".cyan()
            );
        }
        return Ok(());
    }

    let installed_set = apt::installed_set();
    let mut shown = std::collections::BTreeSet::new();
    let mut rows: Vec<(String, String)> = Vec::new();
    for (pkg, file) in installed.into_iter().chain(available) {
        if shown.insert((pkg.clone(), file.clone())) {
            rows.push((pkg, file));
        }
    }
    // dpkg/apt-file match on substrings, so a loose pattern can return many
    // files; prefer exact basename matches and cap the rest.
    rows.sort_by(|(_, a), (_, b)| {
        let base = |f: &str| f.rsplit('/').next().unwrap_or(f) != pattern;
        (base(a), a).cmp(&(base(b), b))
    });

    if json {
        return print_json(&rows, &installed_set);
    }

    ui::header(&format!("Packages providing '{pattern}'"));
    const MAX: usize = 15;
    let total = rows.len();
    for (pkg, file) in rows.iter().take(MAX) {
        let tag = if installed_set.contains(pkg) {
            format!(" {}", "[installed]".green())
        } else {
            String::new()
        };
        println!("   {}{tag}", pkg.bold());
        println!("     {}", file.dimmed());
    }
    if total > MAX {
        println!(
            "   {}",
            format!("… and {} more match(es)", total - MAX).dimmed()
        );
    }
    Ok(())
}

/// Emit the matches as JSON. Unlike the pretty output this is uncapped — a
/// consumer can do its own filtering.
fn print_json(
    rows: &[(String, String)],
    installed: &std::collections::HashSet<String>,
) -> Result<()> {
    let arr: Vec<_> = rows
        .iter()
        .map(|(pkg, file)| {
            serde_json::json!({
                "package": pkg,
                "file": file,
                "installed": installed.contains(pkg),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr)?);
    Ok(())
}

fn which(cmd: &str) -> Option<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    path.starts_with('/').then_some(path)
}

/// Owning package of an existing file path (`dpkg -S`).
fn dpkg_owner(path: &str) -> Option<String> {
    let out = Command::new("dpkg")
        .args(["-S", path])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|l| l.split(':').next())
        .map(str::to_string)
}

/// `(package, file)` pairs from installed packages whose files match `pattern`.
fn dpkg_search(pattern: &str) -> Vec<(String, String)> {
    let Ok(out) = Command::new("dpkg")
        .args(["-S", pattern])
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    parse_pairs(&String::from_utf8_lossy(&out.stdout))
}

fn apt_file_available() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("command -v apt-file")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn apt_file_search(pattern: &str) -> Vec<(String, String)> {
    if !apt_file_available() {
        return Vec::new();
    }
    let Ok(out) = Command::new("apt-file")
        .args(["search", "--", pattern])
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    parse_pairs(&String::from_utf8_lossy(&out.stdout))
}

/// Both `dpkg -S` and `apt-file search` emit `package: /path/to/file` lines.
fn parse_pairs(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (pkgs, file) = l.split_once(": ")?;
            // dpkg may list several packages comma-separated for one file.
            let pkg = pkgs.split(',').next()?.split(':').next()?.trim();
            Some((pkg.to_string(), file.trim().to_string()))
        })
        .collect()
}
