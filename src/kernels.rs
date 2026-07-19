//! Old-kernel detection for `wrapt clean --kernels`.
//!
//! Ubuntu keeps every installed kernel forever, which slowly fills `/boot`.
//! This finds the versioned `linux-*` packages that belong to neither the
//! running kernel nor the newest installed one, so they can be purged. The
//! actual removal goes through the normal transaction flow, so apt still shows
//! the plan, resolves dependencies, and asks for confirmation.

use std::cmp::Ordering;
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

/// Names of installed kernel packages that are safe to purge: everything whose
/// ABI is neither the running kernel's nor the newest installed image's.
/// Returns them sorted. Empty when there's nothing old to remove.
pub fn old_kernel_packages() -> Vec<String> {
    let running_abi = running_release().and_then(|r| abi_of(&r));
    let installed = apt::installed_versions();

    // Every installed package that carries a kernel version, tagged with its ABI.
    let mut kernel_pkgs: Vec<(String, String, String)> = Vec::new(); // (name, abi, version)
    for (name, version) in &installed {
        if let Some(abi) = kernel_abi(name) {
            kernel_pkgs.push((name.clone(), abi, version.clone()));
        }
    }

    // The newest ABI, decided by the version of the actual image packages (so a
    // stale headers-only leftover can't masquerade as "newest").
    let newest_abi = kernel_pkgs
        .iter()
        .filter(|(name, ..)| is_image(name))
        .max_by(|a, b| deb_version_cmp(&a.2, &b.2))
        .map(|(_, abi, _)| abi.clone());

    let keep =
        |abi: &str| running_abi.as_deref() == Some(abi) || newest_abi.as_deref() == Some(abi);

    let mut removable: Vec<String> = kernel_pkgs
        .into_iter()
        .filter(|(_, abi, _)| !keep(abi))
        .map(|(name, ..)| name)
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
}
