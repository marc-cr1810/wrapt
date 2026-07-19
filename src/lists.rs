//! Fast package search by parsing apt's list files directly, instead of
//! shelling out to apt-cache.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::apt::{self, SearchResult};

const LISTS_DIR: &str = "/var/lib/apt/lists";

/// Search every `*_Packages` index for `query` in the name or description.
/// Fails if no readable indexes exist (caller falls back to apt-cache).
pub fn search(query: &str) -> Result<Vec<SearchResult>> {
    let query = query.to_lowercase();
    let mut found_index = false;
    // name → (version, description)
    let mut best: HashMap<String, (String, String)> = HashMap::new();

    for entry in std::fs::read_dir(Path::new(LISTS_DIR))? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if !name.to_string_lossy().ends_with("_Packages") {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        found_index = true;
        scan_index(&String::from_utf8_lossy(&bytes), &query, &mut best);
    }
    if !found_index {
        bail!("no package indexes found in {LISTS_DIR}");
    }

    let installed = apt::installed_set();
    let mut results: Vec<SearchResult> = best
        .into_iter()
        .map(|(name, (version, description))| SearchResult {
            installed: installed.contains(&name),
            name,
            version: Some(version),
            description,
        })
        .collect();
    results.sort_by_key(|r| (r.name != query, !r.name.contains(&query), r.name.clone()));
    Ok(results)
}

/// Walk the stanzas of one Packages index, collecting matches into `best`
/// (keeping the highest version when a package appears in several indexes).
fn scan_index(content: &str, query: &str, best: &mut HashMap<String, (String, String)>) {
    let mut name: Option<&str> = None;
    let mut version: Option<&str> = None;
    let mut description: Option<&str> = None;

    for line in content.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let (Some(name), Some(version)) = (name, version) {
                let description = description.unwrap_or("");
                if name.contains(query) || description.to_lowercase().contains(query) {
                    match best.get_mut(name) {
                        Some((existing, _)) => {
                            if deb_version_cmp(version, existing) == Ordering::Greater {
                                best.insert(
                                    name.to_string(),
                                    (version.to_string(), description.to_string()),
                                );
                            }
                        }
                        None => {
                            best.insert(
                                name.to_string(),
                                (version.to_string(), description.to_string()),
                            );
                        }
                    }
                }
            }
            (name, version, description) = (None, None, None);
        } else if let Some(v) = line.strip_prefix("Package: ") {
            name = Some(v);
        } else if let Some(v) = line.strip_prefix("Version: ") {
            version = Some(v);
        } else if let Some(v) = line
            .strip_prefix("Description: ")
            .or_else(|| line.strip_prefix("Description-en: "))
        {
            description = Some(v);
        }
    }
}

/// Compare two Debian version strings per deb-version(7):
/// `[epoch:]upstream[-revision]`, with dpkg's verrevcmp for each part.
pub fn deb_version_cmp(a: &str, b: &str) -> Ordering {
    let (a_epoch, a_rest) = split_epoch(a);
    let (b_epoch, b_rest) = split_epoch(b);
    let (a_upstream, a_rev) = split_revision(a_rest);
    let (b_upstream, b_rev) = split_revision(b_rest);
    a_epoch
        .cmp(&b_epoch)
        .then_with(|| verrevcmp(a_upstream, b_upstream))
        .then_with(|| verrevcmp(a_rev, b_rev))
}

fn split_epoch(v: &str) -> (u64, &str) {
    match v.split_once(':') {
        Some((epoch, rest)) => (epoch.parse().unwrap_or(0), rest),
        None => (0, v),
    }
}

fn split_revision(v: &str) -> (&str, &str) {
    v.rsplit_once('-').unwrap_or((v, ""))
}

/// dpkg's character ordering: '~' before end-of-string, letters before
/// everything else.
fn char_order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(c) if c.is_ascii_digit() => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

fn verrevcmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        // Non-digit runs compare by character order.
        while a.get(i).is_some_and(|c| !c.is_ascii_digit())
            || b.get(j).is_some_and(|c| !c.is_ascii_digit())
        {
            let (ac, bc) = (char_order(a.get(i).copied()), char_order(b.get(j).copied()));
            if ac != bc {
                return ac.cmp(&bc);
            }
            i += 1;
            j += 1;
        }
        // Digit runs compare numerically: skip leading zeros, then longest
        // run wins, then first differing digit.
        while a.get(i) == Some(&b'0') {
            i += 1;
        }
        while b.get(j) == Some(&b'0') {
            j += 1;
        }
        let mut first_diff = Ordering::Equal;
        while a.get(i).is_some_and(u8::is_ascii_digit) && b.get(j).is_some_and(u8::is_ascii_digit) {
            if first_diff == Ordering::Equal {
                first_diff = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if a.get(i).is_some_and(u8::is_ascii_digit) {
            return Ordering::Greater;
        }
        if b.get(j).is_some_and(u8::is_ascii_digit) {
            return Ordering::Less;
        }
        if first_diff != Ordering::Equal {
            return first_diff;
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn version_ordering() {
        assert_eq!(deb_version_cmp("1.0", "1.0"), Equal);
        assert_eq!(deb_version_cmp("1.10", "1.9"), Greater);
        assert_eq!(deb_version_cmp("1.0~rc1", "1.0"), Less);
        assert_eq!(deb_version_cmp("1:0.5", "2.0"), Greater);
        assert_eq!(deb_version_cmp("1.0-2", "1.0-1"), Greater);
        assert_eq!(deb_version_cmp("1.0a", "1.0"), Greater);
        assert_eq!(deb_version_cmp("1.0+dfsg", "1.0"), Greater);
        assert_eq!(deb_version_cmp("3.0.23-1", "3.0.9-2"), Greater);
        assert_eq!(deb_version_cmp("1.0-1ubuntu1", "1.0-1"), Greater);
        assert_eq!(deb_version_cmp("2.4.1-5build2", "2.4.1-5"), Greater);
        assert_eq!(deb_version_cmp("1.0~beta", "1.0~alpha"), Greater);
        assert_eq!(deb_version_cmp("0.9", "1:0.1"), Less);
    }
}
