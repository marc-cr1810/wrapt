//! `wrapt doctor` — a quick health check for common apt/dpkg problems, in the
//! spirit of `brew doctor`. Each check reports ok / warning / problem and, where
//! possible, the command that fixes it.

use std::collections::BTreeSet;
use std::process::Command;

use anyhow::Result;
use owo_colors::OwoColorize;

enum Status {
    Ok,
    Warn,
    Problem,
}

struct Check {
    title: String,
    status: Status,
    detail: Vec<String>,
    fix: Option<String>,
}

pub fn run(json: bool) -> Result<()> {
    let checks = vec![
        broken_packages(),
        unmet_dependencies(),
        held_packages(),
        orphans(),
        boot_space(),
        duplicate_sources(),
    ];

    if json {
        let arr: Vec<_> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "status": match c.status {
                        Status::Ok => "ok",
                        Status::Warn => "warning",
                        Status::Problem => "problem",
                    },
                    "title": c.title,
                    "detail": c.detail,
                    "fix": c.fix,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    let mut problems = 0;
    let mut warnings = 0;
    for c in &checks {
        let (mark, colored) = match c.status {
            Status::Ok => ("✓".green().bold().to_string(), c.title.clone()),
            Status::Warn => {
                warnings += 1;
                (
                    "!".yellow().bold().to_string(),
                    c.title.yellow().to_string(),
                )
            }
            Status::Problem => {
                problems += 1;
                ("✗".red().bold().to_string(), c.title.red().to_string())
            }
        };
        println!("  {mark} {colored}");
        for line in &c.detail {
            println!("      {}", line.dimmed());
        }
        if let Some(fix) = &c.fix {
            println!("      {} {}", "fix:".dimmed(), fix.cyan());
        }
    }

    println!();
    if problems == 0 && warnings == 0 {
        crate::ui::success("Your system looks healthy.");
    } else {
        crate::ui::warn(&format!(
            "{problems} problem(s), {warnings} warning(s) found."
        ));
    }
    Ok(())
}

/// `dpkg -C` lists packages that aren't fully installed / configured.
fn broken_packages() -> Check {
    let out = Command::new("dpkg").arg("-C").env("LC_ALL", "C").output();
    match out {
        Ok(o) => {
            let names: Vec<String> = String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.starts_with(' '))
                .map(|l| l.trim().to_string())
                .collect();
            if names.is_empty() {
                ok("No half-installed or broken packages")
            } else {
                Check {
                    title: format!("{} package(s) not fully installed", names.len()),
                    status: Status::Problem,
                    detail: names,
                    fix: Some("sudo apt-get install -f".into()),
                }
            }
        }
        Err(_) => ok("dpkg unavailable — skipped broken-package check"),
    }
}

/// `apt-get check` verifies the dependency tree is satisfiable.
fn unmet_dependencies() -> Check {
    let out = Command::new("apt-get")
        .arg("check")
        .env("LC_ALL", "C")
        .output();
    match out {
        Ok(o) if o.status.success() => ok("All dependencies are satisfied"),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Without root, apt can't take its lock — that's not a dep problem.
            if stderr.contains("lock") || stderr.contains("are you root") {
                return Check {
                    title: "Dependency check skipped (needs root)".into(),
                    status: Status::Warn,
                    detail: vec!["run `sudo wrapt doctor` for the full check".into()],
                    fix: None,
                };
            }
            Check {
                title: "Unmet dependencies detected".into(),
                status: Status::Problem,
                detail: stderr
                    .lines()
                    .filter_map(|l| l.strip_prefix("E: ").map(str::to_string))
                    .collect(),
                fix: Some("sudo apt-get install -f".into()),
            }
        }
        Err(_) => ok("apt-get unavailable — skipped dependency check"),
    }
}

fn held_packages() -> Check {
    let held = run_lines(Command::new("apt-mark").arg("showhold"));
    if held.is_empty() {
        ok("No packages are held back")
    } else {
        Check {
            title: format!("{} package(s) held at their current version", held.len()),
            status: Status::Warn,
            detail: held.clone(),
            fix: Some(format!("wrapt install {} (to unhold)", held.join(" "))),
        }
    }
}

fn orphans() -> Check {
    let out = Command::new("apt-get")
        .args(["-s", "autoremove"])
        .env("LC_ALL", "C")
        .output();
    let names: Vec<String> = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix("Remv "))
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };
    if names.is_empty() {
        ok("No orphaned packages")
    } else {
        Check {
            title: format!("{} unused package(s) can be removed", names.len()),
            status: Status::Warn,
            detail: preview(&names),
            fix: Some("wrapt autoremove".into()),
        }
    }
}

fn boot_space() -> Check {
    let kernels = std::fs::read_dir("/boot")
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("vmlinuz-"))
                .count()
        })
        .unwrap_or(0);

    match free_bytes("/boot") {
        Some(free) if free < 200 * 1024 * 1024 => Check {
            title: format!(
                "/boot is low on space ({} free)",
                crate::ui::format_size(free)
            ),
            status: Status::Warn,
            detail: vec![format!("{kernels} kernel(s) installed")],
            fix: Some("wrapt autoremove".into()),
        },
        _ => ok(&format!("/boot has adequate space ({kernels} kernels)")),
    }
}

/// Warn on duplicate `deb` entries across the apt sources files.
fn duplicate_sources() -> Check {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut dupes: BTreeSet<String> = BTreeSet::new();
    let mut files = vec![std::path::PathBuf::from("/etc/apt/sources.list")];
    if let Ok(entries) = std::fs::read_dir("/etc/apt/sources.list.d") {
        files.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "list")),
        );
    }
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in content.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if let Some(rest) = line.strip_prefix("deb ") {
                let key = rest.split_whitespace().collect::<Vec<_>>().join(" ");
                if !seen.insert(key.clone()) {
                    dupes.insert(key);
                }
            }
        }
    }
    if dupes.is_empty() {
        ok("No duplicate apt sources")
    } else {
        Check {
            title: format!("{} duplicate apt source line(s)", dupes.len()),
            status: Status::Warn,
            detail: dupes.into_iter().collect(),
            fix: Some("review /etc/apt/sources.list.d/".into()),
        }
    }
}

fn ok(title: &str) -> Check {
    Check {
        title: title.to_string(),
        status: Status::Ok,
        detail: Vec::new(),
        fix: None,
    }
}

fn run_lines(cmd: &mut Command) -> Vec<String> {
    cmd.output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn preview(items: &[String]) -> Vec<String> {
    const MAX: usize = 8;
    if items.len() <= MAX {
        return items.to_vec();
    }
    let mut out: Vec<String> = items[..MAX].to_vec();
    out.push(format!("… and {} more", items.len() - MAX));
    out
}

/// Free bytes on the filesystem containing `path`, via statvfs.
fn free_bytes(path: &str) -> Option<u64> {
    use std::ffi::CString;
    let c = CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}
