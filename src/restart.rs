//! After a transaction, work out which running services are still using
//! outdated (deleted) libraries and offer to restart them — the job Debian's
//! `needrestart` does, which apt itself leaves to the user.

use std::collections::BTreeSet;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::Result;

use crate::ui;
use crate::ui::Paint;

/// Core session plumbing. systemd exposes no property that marks these, and
/// their names are stable across distributions, so a list is the honest tool.
/// Restarting them is survivable in theory and reboot-worthy in practice.
const CRITICAL_UNITS: &[&str] = &[
    "dbus",
    "dbus-broker",
    "systemd-logind",
    "polkit",
    "polkitd",
    "systemd",
    "init",
];

/// Display managers, used only as a backstop for when the authoritative
/// `display-manager.service` symlink is missing (a hand-rolled DM setup, or a
/// distribution that doesn't follow the convention).
const DISPLAY_MANAGERS: &[&str] = &[
    "display-manager",
    "gdm",
    "gdm3",
    "sddm",
    "lightdm",
    "lxdm",
    "xdm",
    "slim",
    "greetd",
    "ly",
];

/// What to do about services left using upgraded-out libraries.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Show them and ask (the default).
    #[default]
    Ask,
    /// Restart the safe ones without prompting.
    Auto,
    /// Only report them; never restart anything.
    Never,
}

impl Mode {
    /// Parse the config value. Anything unrecognised has already been rejected
    /// by config validation, so an unknown string falls back to the default.
    pub fn from_config(value: Option<&str>) -> Mode {
        match value {
            Some("auto") => Mode::Auto,
            Some("never") => Mode::Never,
            _ => Mode::Ask,
        }
    }
}

/// The user's restart preferences. Set once at startup, before any transaction
/// runs, mirroring how the colour policy is applied — leaf preferences aren't
/// worth threading through every transaction signature.
#[derive(Default)]
pub struct Policy {
    pub mode: Mode,
    /// Services declared off-limits, beyond those detected automatically.
    pub never_restart: Vec<String>,
}

static POLICY: OnceLock<Policy> = OnceLock::new();

/// Record the user's restart policy. Later calls are ignored, so the value is
/// fixed for the life of the process.
pub fn set_policy(policy: Policy) {
    let _ = POLICY.set(policy);
}

fn policy() -> &'static Policy {
    static DEFAULT: OnceLock<Policy> = OnceLock::new();
    POLICY
        .get()
        .unwrap_or_else(|| DEFAULT.get_or_init(Policy::default))
}

/// Which units must not be restarted *on this machine*, resolved from the live
/// system rather than guessed from names. Built once per run.
#[derive(Default)]
pub struct Guard {
    /// Units named outright: the display manager, and whatever wrapt itself is
    /// running inside.
    explicit: BTreeSet<String>,
    /// PAM service names owning live logind sessions (`gdm-password`, `sshd`).
    /// These are what tie a running session to the unit that spawned it.
    sessions: BTreeSet<String>,
}

impl Guard {
    /// Interrogate systemd and logind for units that own something live.
    pub fn detect() -> Guard {
        let mut explicit = BTreeSet::new();
        if let Some(dm) = display_manager_unit() {
            explicit.insert(dm);
        }
        if let Some(own) = own_unit() {
            explicit.insert(own);
        }
        // Whatever the user declared off-limits, in either naming form.
        for name in &policy().never_restart {
            let name = name.trim();
            explicit.insert(name.to_string());
            explicit.insert(format!("{}.service", name.trim_end_matches(".service")));
        }
        Guard {
            explicit,
            sessions: session_services(),
        }
    }

    /// True if restarting `unit` would end a live session or destabilise the
    /// system.
    pub fn is_deferred(&self, unit: &str) -> bool {
        let name = unit.strip_suffix(".service").unwrap_or(unit);

        // The per-user systemd manager owns everything in that user's session.
        if name.starts_with("user@") {
            return true;
        }
        if self.explicit.contains(unit) || self.explicit.contains(name) {
            return true;
        }
        if CRITICAL_UNITS.contains(&name) || DISPLAY_MANAGERS.contains(&name) {
            return true;
        }

        // A logind session's PAM service name is close to, but not identical
        // to, the unit that spawned it: `gdm-password` for `gdm.service`,
        // `sshd` for `ssh.service`. Prefix matching bridges both directions.
        // The length floor stops very short unit names matching by accident.
        name.len() >= 3 && self.sessions.iter().any(|svc| svc.starts_with(name))
    }
}

/// Resolve the `display-manager.service` symlink to the unit it points at —
/// the authoritative name of this machine's display manager, whatever it's
/// called. Absent when no display manager is installed.
fn display_manager_unit() -> Option<String> {
    let target = std::fs::canonicalize("/etc/systemd/system/display-manager.service").ok()?;
    Some(target.file_name()?.to_str()?.to_string())
}

/// The service unit wrapt is running inside, if any. Restarting it would kill
/// the process doing the restarting.
fn own_unit() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    service_from_cgroup(&cgroup)
}

/// The PAM service name behind every live logind session, across all users.
fn session_services() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(list) = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .env("LC_ALL", "C")
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        let Some(id) = line.split_whitespace().next() else {
            continue;
        };
        let Ok(show) = Command::new("loginctl")
            .args(["show-session", id, "-p", "Service"])
            .env("LC_ALL", "C")
            .output()
        else {
            continue;
        };
        for l in String::from_utf8_lossy(&show.stdout).lines() {
            if let Some(svc) = l.strip_prefix("Service=")
                && !svc.trim().is_empty()
            {
                out.insert(svc.trim().to_string());
            }
        }
    }
    out
}

#[derive(Default)]
pub struct Report {
    /// systemd services whose processes map deleted libraries.
    pub services: Vec<String>,
    /// Services that need restarting but are unsafe to restart live — the user
    /// must reboot or restart them deliberately.
    pub deferred: Vec<String>,
    /// Set when the running kernel differs from the newest installed one.
    pub kernel_reboot: bool,
}

impl Report {
    pub fn is_empty(&self) -> bool {
        self.services.is_empty() && self.deferred.is_empty() && !self.kernel_reboot
    }

    /// Move session-critical units out of the restart list.
    fn split_deferred(mut self, guard: &Guard) -> Self {
        let (deferred, safe): (Vec<_>, Vec<_>) = self
            .services
            .into_iter()
            .partition(|s| guard.is_deferred(s));
        self.services = safe;
        self.deferred = deferred;
        self
    }
}

/// Prefer `needrestart` if installed; otherwise scan /proc ourselves.
pub fn check() -> Report {
    let guard = Guard::detect();
    if which("needrestart")
        && let Some(report) = via_needrestart()
    {
        return report.split_deferred(&guard);
    }
    Report {
        services: scan_proc(),
        deferred: Vec::new(),
        kernel_reboot: kernel_outdated(),
    }
    .split_deferred(&guard)
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

        // An explicit `restart = "never"` outranks -y: assuming yes to package
        // prompts shouldn't be read as consent to bounce services.
        let restart_now = match policy().mode {
            Mode::Never => false,
            Mode::Auto => true,
            Mode::Ask => yes || ui::confirm("Restart them now?", true),
        };
        if restart_now {
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

    if !report.deferred.is_empty() {
        ui::warn(&format!(
            "{} service{} also updated, but wrapt will not restart {}:",
            report.deferred.len(),
            if report.deferred.len() == 1 { "" } else { "s" },
            if report.deferred.len() == 1 {
                "it"
            } else {
                "them"
            },
        ));
        for svc in &report.deferred {
            println!("   {}", svc.yellow());
        }
        println!(
            "   {}",
            "Restarting these would end your session or interrupt something you \
             rely on."
                .dimmed()
        );
        println!("   {}", "They take effect on your next reboot.".dimmed());
        println!();
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
        deferred: Vec::new(),
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

    /// A guard with nothing detected from the live system — exercises the
    /// hardcoded backstops alone.
    fn bare_guard() -> Guard {
        Guard::default()
    }

    /// A guard resembling this developer machine: gdm is the display manager
    /// and owns a live graphical session.
    fn desktop_guard() -> Guard {
        Guard {
            explicit: ["gdm.service".to_string()].into_iter().collect(),
            sessions: ["gdm-password".to_string()].into_iter().collect(),
        }
    }

    #[test]
    fn session_critical_services_are_never_restarted() {
        // The exact shape that logged a user out: a display manager reported
        // alongside ordinary services must not land in the restart list.
        let out = "\
NEEDRESTART-SVC: gdm.service
NEEDRESTART-SVC: nginx.service
NEEDRESTART-SVC: dbus.service
NEEDRESTART-SVC: cups.service
";
        let report = parse_needrestart(out).split_deferred(&desktop_guard());
        assert_eq!(report.services, ["cups.service", "nginx.service"]);
        assert_eq!(report.deferred, ["dbus.service", "gdm.service"]);
        // Still worth reporting even though nothing will be auto-restarted.
        assert!(!report.is_empty());
    }

    #[test]
    fn display_manager_symlink_defers_whatever_it_names() {
        // A display manager not on any hardcoded list is still caught, because
        // display-manager.service named it.
        let guard = Guard {
            explicit: ["exoticdm.service".to_string()].into_iter().collect(),
            sessions: BTreeSet::new(),
        };
        assert!(guard.is_deferred("exoticdm.service"));
        assert!(!bare_guard().is_deferred("exoticdm.service"));
    }

    #[test]
    fn session_owner_is_deferred_across_naming_mismatch() {
        // Ubuntu's unit is `ssh.service` while the PAM service is `sshd` —
        // restarting it would drop a live remote session.
        let guard = Guard {
            explicit: BTreeSet::new(),
            sessions: ["sshd".to_string()].into_iter().collect(),
        };
        assert!(guard.is_deferred("ssh.service"));
        assert!(guard.is_deferred("sshd.service"));
        // `gdm-password` likewise stands in for `gdm.service`.
        assert!(desktop_guard().is_deferred("gdm.service"));
    }

    #[test]
    fn hardcoded_backstops_hold_without_any_detection() {
        for unit in [
            "gdm.service",
            "gdm3",
            "sddm.service",
            "lightdm.service",
            "display-manager.service",
            "dbus.service",
            "systemd-logind.service",
            "user@1000.service",
        ] {
            assert!(bare_guard().is_deferred(unit), "{unit} should be deferred");
        }
        for unit in ["nginx.service", "cups.service", "ssh.service", "cron"] {
            assert!(
                !bare_guard().is_deferred(unit),
                "{unit} should be restartable"
            );
        }
        // Substring lookalikes must not be caught by the display-manager rules.
        assert!(!bare_guard().is_deferred("gdm-helper.service"));
        assert!(!bare_guard().is_deferred("dbus-monitor-exporter.service"));
    }

    #[test]
    fn mode_parses_from_config_and_defaults_to_ask() {
        assert_eq!(Mode::from_config(Some("auto")), Mode::Auto);
        assert_eq!(Mode::from_config(Some("never")), Mode::Never);
        assert_eq!(Mode::from_config(Some("ask")), Mode::Ask);
        assert_eq!(Mode::from_config(None), Mode::Ask);
        // Validation rejects anything else before it reaches here; falling back
        // to the safe default is better than panicking if one slips through.
        assert_eq!(Mode::from_config(Some("nonsense")), Mode::Ask);
    }

    #[test]
    fn user_configured_services_are_deferred_in_either_naming_form() {
        // `detect()` stores both forms, so config may say "docker" or
        // "docker.service" and either spelling of the unit still matches.
        let guard = Guard {
            explicit: ["docker".to_string(), "docker.service".to_string()]
                .into_iter()
                .collect(),
            sessions: BTreeSet::new(),
        };
        assert!(guard.is_deferred("docker"));
        assert!(guard.is_deferred("docker.service"));
        // Protecting docker must not protect unrelated lookalikes.
        assert!(!guard.is_deferred("docker-proxy.service"));
    }

    #[test]
    fn ordinary_daemons_stay_restartable_on_a_desktop() {
        // The feature is worthless if detection over-defers; guard against it.
        let guard = desktop_guard();
        for unit in ["nginx.service", "cups.service", "cron.service", "apache2"] {
            assert!(!guard.is_deferred(unit), "{unit} should be restartable");
        }
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
