//! Old-kernel detection for `wrapt clean --kernels`.
//!
//! Ubuntu keeps every installed kernel forever, which slowly fills `/boot`.
//! This finds the versioned `linux-*` packages outside the newest few kernels
//! (and the running one), so they can be purged. The actual removal goes
//! through the normal transaction flow, so apt still shows the plan, resolves
//! dependencies, and asks for confirmation.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::process::Command;

use crate::apt;
use crate::lists::deb_version_cmp;

/// Prefixes of versioned kernel packages. Each is followed by a version that
/// starts with a digit (e.g. `linux-image-6.8.0-31-generic`); the unversioned
/// meta-packages (`linux-image-generic`) don't match and are never touched.
const KERNEL_PREFIXES: &[&str] = &[
    "linux-image-unsigned-",
    "linux-image-",
    "linux-headers-",
    "linux-modules-extra-",
    "linux-modules-",
    "linux-buildinfo-",
    "linux-tools-",
    "linux-cloud-tools-",
];

/// Names of installed kernel packages that are safe to purge: everything
/// outside the `keep` newest kernels, plus the running one whatever its age.
/// Returns them sorted. Empty when there's nothing old to remove.
pub fn old_kernel_packages(keep: usize) -> Vec<String> {
    let running_abi = running_release().and_then(|r| abi_of(&r));
    let installed = apt::installed_versions();

    // Every installed package that carries a kernel version, tagged with its ABI.
    let mut kernel_pkgs: Vec<(String, String, String)> = Vec::new(); // (name, abi, version)
    for (name, version) in &installed {
        if let Some(abi) = kernel_abi(name) {
            kernel_pkgs.push((name.clone(), abi, version.clone()));
        }
    }

    select_removable(&kernel_pkgs, running_abi.as_deref(), keep)
}

/// The keep policy, separated from the system calls so it can be tested.
///
/// Keeps the `keep` newest kernels and the running one. Those overlap in the
/// common case; they don't when you're booted into an older kernel, and then
/// both are kept. Ranking counts only ABIs that still have an image package,
/// so a headers-only leftover can't masquerade as one of the newest and
/// displace a kernel you could actually boot.
fn select_removable(
    kernel_pkgs: &[(String, String, String)],
    running_abi: Option<&str>,
    keep: usize,
) -> Vec<String> {
    let mut ranked: Vec<(&str, &str)> = Vec::new(); // (abi, newest version seen)
    for (name, abi, version) in kernel_pkgs {
        if !is_image(name) {
            continue;
        }
        match ranked.iter_mut().find(|(a, _)| *a == abi.as_str()) {
            Some(slot) if deb_version_cmp(version, slot.1) == Ordering::Greater => {
                slot.1 = version;
            }
            Some(_) => {}
            None => ranked.push((abi, version)),
        }
    }
    ranked.sort_by(|a, b| deb_version_cmp(b.1, a.1)); // newest first

    // At least one kernel must survive even if asked for zero.
    let mut keeping: BTreeSet<&str> = ranked
        .iter()
        .take(keep.max(1))
        .map(|(abi, _)| *abi)
        .collect();
    // Never purge what we're booted into, however old it is.
    if let Some(running) = running_abi {
        keeping.insert(running);
    }

    let mut removable: Vec<String> = kernel_pkgs
        .iter()
        .filter(|(_, abi, _)| !keeping.contains(abi.as_str()))
        .map(|(name, ..)| name.clone())
        .collect();
    removable.sort();
    removable.dedup();
    removable
}

fn running_release() -> Option<String> {
    let out = Command::new("uname").arg("-r").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let r = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!r.is_empty()).then_some(r)
}

fn is_image(name: &str) -> bool {
    name.starts_with("linux-image-")
}

/// The kernel ABI a package name encodes, or `None` if it isn't a versioned
/// kernel package. e.g. `linux-image-6.8.0-31-generic` -> `6.8.0-31`,
/// `linux-headers-6.8.0-31` -> `6.8.0-31`.
fn kernel_abi(name: &str) -> Option<String> {
    for prefix in KERNEL_PREFIXES {
        if let Some(rest) = name.strip_prefix(prefix)
            && rest.starts_with(|c: char| c.is_ascii_digit())
        {
            return abi_of(rest);
        }
    }
    None
}

/// The ABI (`major.minor.patch-build`) from a kernel version string, dropping
/// any trailing flavour. `6.8.0-31-generic` -> `6.8.0-31`; `6.8.0-31` -> same.
fn abi_of(version: &str) -> Option<String> {
    let mut parts = version.split('-');
    let base = parts.next()?; // 6.8.0
    let build = parts.next()?; // 31
    // The base must look like a dotted version and the build like a number, so
    // we don't mistake something else for a kernel ABI.
    if !base.contains('.') || !build.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("{base}-{build}"))
}

/// Order two ABIs oldest-first (used only in tests / display).
#[allow(dead_code)]
pub fn abi_cmp(a: &str, b: &str) -> Ordering {
    deb_version_cmp(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_abi_from_package_names() {
        assert_eq!(
            kernel_abi("linux-image-6.8.0-31-generic").as_deref(),
            Some("6.8.0-31")
        );
        assert_eq!(
            kernel_abi("linux-headers-6.8.0-31").as_deref(),
            Some("6.8.0-31")
        );
        assert_eq!(
            kernel_abi("linux-modules-extra-6.8.0-31-generic").as_deref(),
            Some("6.8.0-31")
        );
        // Multi-word flavour keeps just the ABI.
        assert_eq!(
            kernel_abi("linux-image-6.8.0-31-generic-64k").as_deref(),
            Some("6.8.0-31")
        );
        // Meta-packages have no version and must never match.
        assert_eq!(kernel_abi("linux-image-generic"), None);
        assert_eq!(kernel_abi("linux-headers-generic"), None);
        // Non-kernel packages are ignored.
        assert_eq!(kernel_abi("linux-libc-dev"), None);
        assert_eq!(kernel_abi("htop"), None);
    }

    #[test]
    fn abi_of_strips_flavour() {
        assert_eq!(abi_of("6.8.0-31-generic").as_deref(), Some("6.8.0-31"));
        assert_eq!(abi_of("6.8.0-31").as_deref(), Some("6.8.0-31"));
        assert_eq!(abi_of("not-a-kernel"), None);
    }

    #[test]
    fn abis_order_oldest_first() {
        assert_eq!(abi_cmp("6.8.0-31", "6.8.0-40"), Ordering::Less);
        assert_eq!(abi_cmp("6.8.0-9", "6.8.0-31"), Ordering::Less);
    }

    /// Three installed kernels, image + headers for each.
    fn three_kernels() -> Vec<(String, String, String)> {
        let mut v = Vec::new();
        for (abi, ver) in [
            ("6.8.0-31", "6.8.0-31.31"),
            ("6.8.0-40", "6.8.0-40.40"),
            ("6.8.0-45", "6.8.0-45.45"),
        ] {
            v.push((
                format!("linux-image-{abi}-generic"),
                abi.to_string(),
                ver.to_string(),
            ));
            v.push((
                format!("linux-headers-{abi}"),
                abi.to_string(),
                ver.to_string(),
            ));
        }
        v
    }

    #[test]
    fn running_the_newest_kernel_still_leaves_a_fallback() {
        // The regression this policy exists for: once you reboot into the
        // newest kernel, running == newest, and keeping only "running or
        // newest" would purge every other kernel and leave no fallback.
        let pkgs = three_kernels();
        let removable = select_removable(&pkgs, Some("6.8.0-45"), 2);
        assert_eq!(
            removable,
            ["linux-headers-6.8.0-31", "linux-image-6.8.0-31-generic"]
        );
        // 6.8.0-40 survives as the fallback.
        assert!(!removable.iter().any(|p| p.contains("6.8.0-40")));
    }

    #[test]
    fn running_an_old_kernel_keeps_it_on_top_of_the_newest() {
        // Booted into the oldest: it's kept as well as the two newest, so all
        // three survive.
        let removable = select_removable(&three_kernels(), Some("6.8.0-31"), 2);
        assert!(removable.is_empty());
    }

    #[test]
    fn keep_count_controls_how_many_survive() {
        let pkgs = three_kernels();
        // Keeping one leaves only the newest — running it, so nothing else.
        let one = select_removable(&pkgs, Some("6.8.0-45"), 1);
        assert_eq!(one.len(), 4);
        // Keeping more than exist removes nothing.
        assert!(select_removable(&pkgs, Some("6.8.0-45"), 9).is_empty());
        // Zero is clamped to one — never purge every kernel.
        assert_eq!(select_removable(&pkgs, None, 0).len(), 4);
    }

    #[test]
    fn headers_only_leftovers_cannot_displace_a_bootable_kernel() {
        // A stale headers package for a kernel with no image must not count as
        // one of the newest, or it would push a real kernel out of the keep set.
        let mut pkgs = three_kernels();
        pkgs.push((
            "linux-headers-6.8.0-99".to_string(),
            "6.8.0-99".to_string(),
            "6.8.0-99.99".to_string(),
        ));
        let removable = select_removable(&pkgs, Some("6.8.0-45"), 2);
        assert!(removable.contains(&"linux-headers-6.8.0-99".to_string()));
        // The two newest real kernels are still kept.
        assert!(!removable.iter().any(|p| p.contains("6.8.0-45")));
        assert!(!removable.iter().any(|p| p.contains("6.8.0-40")));
    }
}
