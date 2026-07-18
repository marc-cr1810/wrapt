//! Translates apt's terse resolver/error output into plain-English guidance.
//! apt's dependency errors ("Reached two conflicting assignments", "held broken
//! packages") are notoriously opaque; this turns them into something actionable.

use std::process::Command;

use anyhow::{Result, anyhow};

/// Wrap a failed apt error with a friendlier explanation where we recognise it.
pub fn explain(err: anyhow::Error, named: &[String]) -> anyhow::Error {
    let raw = format!("{err:#}");
    match hint(&raw, named) {
        Some(hint) => anyhow!("{}\n\n{}", raw.trim(), hint),
        None => err,
    }
}

/// Produce a human hint for a known apt failure pattern, or None.
fn hint(raw: &str, named: &[String]) -> Option<String> {
    // Unknown package name — almost always a typo. Offer close matches.
    if let Some(pkg) = capture_between(raw, "Unable to locate package ", "\n")
        .or_else(|| capture_between(raw, "Unable to locate package ", ""))
    {
        let pkg = pkg.trim();
        let suggestions = did_you_mean(pkg);
        return Some(if suggestions.is_empty() {
            format!(
                "There is no package called '{pkg}'. Try `wrapt search {pkg}`, \
                 or run `wrapt update` if your package lists are stale."
            )
        } else {
            format!("No package '{pkg}'. Did you mean: {}?", suggestions.join(", "))
        });
    }

    if raw.contains("conflicting assignments") || raw.contains("held broken packages") {
        let target = named.first().map(String::as_str).unwrap_or("that package");
        return Some(format!(
            "apt can't find a set of package versions that satisfies this request — \
             installing/removing {target} would conflict with something already installed. \
             Common fixes:\n    \
             • run `wrapt update` first, in case your lists are out of date\n    \
             • it may be an essential package other packages depend on — check `wrapt why {target}`\n    \
             • try `wrapt upgrade` to move conflicting packages forward together"
        ));
    }

    // "pkg : Depends: dep but it is not installable/going to be installed"
    if let Some(dep) = capture_between(raw, "Depends: ", " but it is not") {
        return Some(format!(
            "A required dependency ('{}') can't be installed. It might be missing from your \
             enabled repositories, or held back. Try `wrapt update`, then `wrapt show {}`.",
            dep.trim(),
            dep.trim()
        ));
    }

    if raw.contains("has no installation candidate") {
        return Some(
            "The package exists but no installable version is available from your repositories \
             (it may have been removed, or needs a repo/PPA you haven't enabled)."
                .to_string(),
        );
    }

    None
}

fn capture_between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let after = &text[text.find(start)? + start.len()..];
    if end.is_empty() {
        Some(after)
    } else {
        Some(&after[..after.find(end).unwrap_or(after.len())])
    }
}

/// Package names within edit-distance 2 of `name`, best first (max 3). Ties are
/// broken toward candidates sharing the first letter and length, so an obvious
/// typo like "gti" surfaces "git" ahead of equally-close but unrelated names.
fn did_you_mean(name: &str) -> Vec<String> {
    let Ok(out) = Command::new("apt-cache").arg("pkgnames").output() else {
        return Vec::new();
    };
    let first = name.bytes().next();
    let mut scored: Vec<(usize, bool, usize, String)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|cand| {
            // Cheap length prefilter before the O(nm) distance.
            if cand.len().abs_diff(name.len()) > 2 {
                return None;
            }
            let d = damerau(name, cand);
            (d <= 2).then(|| {
                let diff_first = cand.bytes().next() != first;
                (d, diff_first, cand.len().abs_diff(name.len()), cand.to_string())
            })
        })
        .collect();
    scored.sort();
    scored.into_iter().take(3).map(|(.., c)| c).collect()
}

/// Optimal string alignment distance: like Levenshtein but a transposition of
/// two adjacent characters counts as one edit (the common keyboard typo).
fn damerau(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            let mut best = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = best;
        }
    }
    d[n][m]
}

/// A `wrapt config-diff` helper lives here too since it's small and related to
/// the post-upgrade cleanup story.
pub fn config_diff() -> Result<()> {
    use owo_colors::OwoColorize;

    // dpkg leaves the maintainer's new version alongside yours with these
    // suffixes when it can't ask (or you chose to keep your file).
    // WRAPT_ETC_DIR overrides the search root for testing.
    let root = std::env::var("WRAPT_ETC_DIR").unwrap_or_else(|_| "/etc".to_string());
    let mut found = Vec::new();
    collect_dpkg_dist(std::path::Path::new(&root), &mut found);

    if found.is_empty() {
        crate::ui::success("No pending configuration files to review.");
        return Ok(());
    }

    crate::ui::header(&format!("{} configuration file(s) to review", found.len()));
    for (new_file, orig) in &found {
        println!();
        println!(
            "   {} {} {}",
            orig.display().to_string().bold(),
            "vs".dimmed(),
            new_file.display().to_string().yellow()
        );
        let diff = Command::new("diff")
            .args(["-u", "--color=never"])
            .arg(orig)
            .arg(new_file)
            .output();
        if let Ok(out) = diff {
            for line in String::from_utf8_lossy(&out.stdout).lines().take(20) {
                let colored = match line.chars().next() {
                    Some('+') => line.green().to_string(),
                    Some('-') => line.red().to_string(),
                    Some('@') => line.cyan().to_string(),
                    _ => line.dimmed().to_string(),
                };
                println!("     {colored}");
            }
        }
        println!(
            "   {} keep yours: {}   use theirs: {}",
            "→".cyan(),
            format!("rm {}", new_file.display()).dimmed(),
            format!("mv {} {}", new_file.display(), orig.display()).dimmed()
        );
    }
    Ok(())
}

/// Recursively find `*.dpkg-dist` / `*.dpkg-new` / `*.ucf-dist` files and pair
/// each with the config file it would replace.
fn collect_dpkg_dist(dir: &std::path::Path, out: &mut Vec<(std::path::PathBuf, std::path::PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_dpkg_dist(&path, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            for suffix in [".dpkg-dist", ".dpkg-new", ".ucf-dist"] {
                if let Some(base) = name.strip_suffix(suffix) {
                    let orig = path.with_file_name(base);
                    if orig.exists() {
                        out.push((path.clone(), orig));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damerau_distance() {
        assert_eq!(damerau("htop", "htop"), 0);
        assert_eq!(damerau("htpo", "htop"), 1); // adjacent transposition = 1
        assert_eq!(damerau("gti", "git"), 1); // gti → git is one transposition
        assert_eq!(damerau("pyton3", "python3"), 1); // one insertion
        assert_eq!(damerau("abc", "xyz"), 3);
    }

    #[test]
    fn hint_detects_unknown_package() {
        // No apt available in the test env → suggestions empty, but the
        // "no package" branch still fires with actionable text.
        let raw = "E: Unable to locate package pyton3\n";
        let h = hint(raw, &["pyton3".to_string()]).unwrap();
        assert!(h.contains("pyton3"));
    }

    #[test]
    fn hint_detects_conflict() {
        let raw = "Unable to satisfy dependencies. Reached two conflicting assignments";
        let h = hint(raw, &["libfoo".to_string()]).unwrap();
        assert!(h.contains("wrapt why libfoo"));
    }
}
