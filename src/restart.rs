//! After a transaction, work out which running services are still using
//! outdated (deleted) libraries and offer to restart them — the job Debian's
//! `needrestart` does, which apt itself leaves to the user.

use std::collections::BTreeSet;
use std::process::Command;

use anyhow::Result;

use crate::ui;
use crate::ui::Paint;

#[derive(Default)]
pub struct Report {
    /// systemd services whose processes map deleted libraries.
    pub services: Vec<String>,
    /// Set when the running kernel differs from the newest installed one.
    pub kernel_reboot: bool,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.services.is_empty() && !self.kernel_reboot
    }
}

/// Prefer `needrestart` if installed; otherwise scan /proc ourselves.
pub fn check() -> Report {
    if which("needrestart")
        && let Some(report) = via_needrestart()
    {
        return report;
    }
    Report {
        services: scan_proc(),
        kernel_reboot: kernel_outdated(),
    }
}

/// Show the report and, with the user's consent, restart the services.
pub fn offer(report: &Report, yes: bool) -> Result<()> {
    if report.is_empty() {
        return Ok(());
    }

    if !report.services.is_empty() {
        ui::header(&format!(
            "{} service{} need restarting (using outdated libraries):",
            report.services.len(),
            if report.services.len() == 1 { "" } else { "s" }
        ));
        for svc in &report.services {
            println!("   {}", svc.yellow());
        }
        println!();

        if yes || ui::confirm("Restart them now?", true) {
            for svc in &report.services {
                print!("   restarting {svc} ... ");
                let ok = Command::new("systemctl")
                    .args(["restart", svc])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                println!(
                    "{}",
                    if ok {
                        "ok".green().to_string()
                    } else {
                        "failed".red().to_string()
                    }
                );
            }
        } else {
            println!(
                "   {} {}",
                "skip — restart later with:".dimmed(),
                format!("systemctl restart {}", report.services.join(" ")).cyan()
            );
        }
    }

    if report.kernel_reboot {
        ui::warn("The running kernel is outdated — a reboot is recommended.");
    }
    Ok(())
}

fn which(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse `needrestart -b` (batch/machine-readable) output.
fn via_needrestart() -> Option<Report> {
    let out = Command::new("needrestart")
        .arg("-b")
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    Some(parse_needrestart(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_needrestart(text: &str) -> Report {
    let mut services = Vec::new();
    let mut kcur = None;
    let mut kexp = None;
    for line in text.lines() {
        if let Some(svc) = line.strip_prefix("NEEDRESTART-SVC: ") {
            services.push(svc.trim().to_string());
        } else if let Some(v) = line.strip_prefix("NEEDRESTART-KCUR: ") {
            kcur = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("NEEDRESTART-KEXP: ") {
            kexp = Some(v.trim().to_string());
        }
    }
    services.sort();
    services.dedup();
    Report {
        services,
        kernel_reboot: matches!((kcur, kexp), (Some(c), Some(e)) if c != e),
    }
}

/// Fallback: find systemd services whose processes still map a deleted `.so`.
fn scan_proc() -> Vec<String> {
    let mut services = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        if !maps_deleted_lib(pid) {
            continue;
        }
        if let Some(unit) = pid_service(pid) {
            services.insert(unit);
        }
    }
    services.into_iter().collect()
}

/// True if the process maps a deleted shared library (an upgraded-out lib).
fn maps_deleted_lib(pid: &str) -> bool {
    let Ok(maps) = std::fs::read_to_string(format!("/proc/{pid}/maps")) else {
        return false;
    };
    maps.lines().any(|line| {
        line.ends_with("(deleted)")
            && line.contains(".so")
            // Ignore deleted files in memory/tmp that aren't real library upgrades.
            && (line.contains("/usr/") || line.contains("/lib"))
    })
}

/// Resolve a PID to its systemd service via the cgroup-v2 path.
fn pid_service(pid: &str) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    service_from_cgroup(&cgroup)
}

/// Extract a system service unit from a cgroup file's contents, ignoring
/// user-session scopes (which shouldn't be restarted from under the user).
fn service_from_cgroup(cgroup: &str) -> Option<String> {
    if !cgroup.contains("system.slice") {
        return None;
    }
    cgroup
        .trim()
        .split('/')
        .rev()
        .find(|seg| seg.ends_with(".service"))
        .map(|s| s.trim().to_string())
}

/// True if a strictly newer kernel is installed than the one running — so
/// leftover older kernels in /boot don't trigger a spurious reboot notice.
fn kernel_outdated() -> bool {
    use std::cmp::Ordering;

    let Ok(running) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else {
        return false;
    };
    let running = running.trim();
    let Ok(entries) = std::fs::read_dir("/boot") else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .and_then(|n| n.strip_prefix("vmlinuz-"))
            .map(|s| s.to_string())
            .is_some_and(|installed| {
                crate::lists::deb_version_cmp(&installed, running) == Ordering::Greater
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_needrestart_batch() {
        let out = "\
NEEDRESTART-VER: 3.6
NEEDRESTART-KCUR: 6.8.0-40-generic
NEEDRESTART-KEXP: 6.8.0-41-generic
NEEDRESTART-KSTA: 3
NEEDRESTART-SVC: dbus.service
NEEDRESTART-SVC: systemd-logind.service
NEEDRESTART-SVC: dbus.service
";
        let report = parse_needrestart(out);
        assert_eq!(report.services, ["dbus.service", "systemd-logind.service"]);
        assert!(report.kernel_reboot);
    }

    #[test]
    fn no_reboot_when_kernel_matches() {
        let out = "\
NEEDRESTART-KCUR: 6.8.0-41-generic
NEEDRESTART-KEXP: 6.8.0-41-generic
";
        assert!(!parse_needrestart(out).kernel_reboot);
    }

    #[test]
    fn service_from_cgroup_v2() {
        assert_eq!(
            service_from_cgroup("0::/system.slice/nginx.service\n").as_deref(),
            Some("nginx.service")
        );
        // User-session scopes are excluded.
        assert_eq!(
            service_from_cgroup("0::/user.slice/user-1000.slice/session-2.scope"),
            None
        );
        // No service component.
        assert_eq!(service_from_cgroup("0::/system.slice/foo.mount"), None);
    }
}
