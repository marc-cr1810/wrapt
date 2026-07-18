//! `wrapt why <pkg>` — explains *why* a package is on the system: either you
//! installed it directly, or it was pulled in as a dependency, in which case we
//! trace the chain back to a package you did install on purpose.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Reverse-dependency graph over currently-installed packages.
pub struct Graph {
    installed: HashSet<String>,
    /// Packages apt marked as automatically installed (i.e. dependencies).
    auto: HashSet<String>,
    /// pkg → the installed packages that depend on it.
    rdeps: HashMap<String, BTreeSet<String>>,
}

pub struct Explanation {
    pub package: String,
    pub installed: bool,
    pub manual: bool,
    /// Installed packages that directly depend on the target.
    pub required_by: Vec<String>,
    /// A representative chain from a manually-installed root down to the
    /// target: `[root, ..., target]`. Empty if the target is a manual root.
    pub chain: Vec<String>,
    /// Every manually-installed package that (transitively) pulls in the
    /// target. Only populated in `--all` mode.
    pub roots: Vec<String>,
}

impl Graph {
    pub fn build() -> Result<Graph> {
        // One dpkg-query call for status + relationship fields.
        let out = Command::new("dpkg-query")
            .args([
                "-W",
                "-f",
                "${db:Status-Status}\t${Package}\t${Depends}\t${Pre-Depends}\t${Recommends}\t${Provides}\n",
            ])
            .output()
            .context("failed to run dpkg-query")?;
        if !out.status.success() {
            bail!("dpkg-query failed");
        }
        let text = String::from_utf8_lossy(&out.stdout);

        let mut installed = HashSet::new();
        // Records kept until after `installed` is complete, so we can resolve
        // which dependency targets actually exist on the system.
        let mut records: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        // virtual/real name → packages providing it.
        let mut provided_by: HashMap<String, Vec<String>> = HashMap::new();

        for line in text.lines() {
            let mut f = line.split('\t');
            let (status, pkg, depends, predepends, recommends, provides) =
                match (f.next(), f.next(), f.next(), f.next(), f.next(), f.next()) {
                    (Some(a), Some(b), Some(c), Some(d), Some(e), Some(g)) => (a, b, c, d, e, g),
                    _ => continue,
                };
            if status != "installed" {
                continue;
            }
            let pkg = strip_arch(pkg);
            installed.insert(pkg.clone());

            let mut deps = parse_names(depends);
            deps.extend(parse_names(predepends));
            deps.extend(parse_names(recommends));

            let provides = parse_names(provides);
            for v in &provides {
                provided_by.entry(v.clone()).or_default().push(pkg.clone());
            }
            records.push((pkg, deps, provides));
        }

        // Build reverse edges: for each installed P depending on name D, add an
        // edge D → P (and edges from any installed provider of a virtual D).
        let mut rdeps: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (pkg, deps, _) in &records {
            for dep in deps {
                if installed.contains(dep) {
                    rdeps.entry(dep.clone()).or_default().insert(pkg.clone());
                }
                if let Some(providers) = provided_by.get(dep) {
                    for provider in providers {
                        if provider != pkg {
                            rdeps.entry(provider.clone()).or_default().insert(pkg.clone());
                        }
                    }
                }
            }
        }

        Ok(Graph { installed, auto: auto_set(), rdeps })
    }

    pub fn explain(&self, package: &str, all: bool) -> Explanation {
        let package = strip_arch(package);
        if !self.installed.contains(&package) {
            return Explanation {
                package,
                installed: false,
                manual: false,
                required_by: Vec::new(),
                chain: Vec::new(),
                roots: Vec::new(),
            };
        }
        let manual = !self.auto.contains(&package);
        let required_by = self
            .rdeps
            .get(&package)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let (chain, roots) = match (manual, all) {
            (true, _) => (Vec::new(), Vec::new()),
            (false, false) => (self.chain_to_root(&package), Vec::new()),
            (false, true) => (Vec::new(), self.manual_roots(&package)),
        };

        Explanation { package, installed: true, manual, required_by, chain, roots }
    }

    /// Breadth-first walk up the reverse-dependency edges to the nearest
    /// manually-installed package; returns `[root, ..., target]`.
    fn chain_to_root(&self, target: &str) -> Vec<String> {
        let mut came_from: HashMap<&str, &str> = HashMap::new();
        let mut seen: HashSet<&str> = HashSet::from([target]);
        let mut queue: VecDeque<&str> = VecDeque::from([target]);

        while let Some(node) = queue.pop_front() {
            // A manual package other than the target is a root cause.
            if node != target && !self.auto.contains(node) {
                let mut chain = vec![node.to_string()];
                let mut cur = node;
                while let Some(&next) = came_from.get(cur) {
                    chain.push(next.to_string());
                    cur = next;
                }
                // chain is root → ... → target already (we walked down-to-up).
                return chain;
            }
            if let Some(parents) = self.rdeps.get(node) {
                for parent in parents {
                    if seen.insert(parent) {
                        came_from.insert(parent, node);
                        queue.push_back(parent);
                    }
                }
            }
        }
        Vec::new()
    }

    /// Every manually-installed package that transitively pulls in `target`.
    /// Traversal stops at each manual package: it's already a sufficient reason
    /// the target is installed, so its own dependents don't add new reasons.
    fn manual_roots(&self, target: &str) -> Vec<String> {
        let mut roots: BTreeSet<String> = BTreeSet::new();
        let mut seen: HashSet<&str> = HashSet::from([target]);
        let mut queue: VecDeque<&str> = VecDeque::from([target]);

        while let Some(node) = queue.pop_front() {
            if node != target && !self.auto.contains(node) {
                roots.insert(node.to_string());
                continue;
            }
            if let Some(parents) = self.rdeps.get(node) {
                for parent in parents {
                    if seen.insert(parent) {
                        queue.push_back(parent);
                    }
                }
            }
        }
        roots.into_iter().collect()
    }
}

/// Names apt marked auto-installed (`apt-mark showauto`).
fn auto_set() -> HashSet<String> {
    let Ok(out) = Command::new("apt-mark").arg("showauto").output() else {
        return HashSet::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| strip_arch(l.trim()))
        .collect()
}

fn strip_arch(name: &str) -> String {
    name.split(':').next().unwrap_or(name).to_string()
}

/// Parse a dpkg relationship field ("a (>= 1), b | c, d") into bare package
/// names, expanding alternatives and dropping version constraints.
fn parse_names(field: &str) -> Vec<String> {
    let mut names = Vec::new();
    for clause in field.split(',') {
        for alt in clause.split('|') {
            let name = alt
                .trim()
                .split(|c: char| c.is_whitespace() || c == '(' || c == ':')
                .next()
                .unwrap_or("")
                .trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_relationship_fields() {
        assert_eq!(
            parse_names("libc6 (>= 2.38), libtinfo6 (>= 6)"),
            ["libc6", "libtinfo6"]
        );
        // Alternatives both expand.
        assert_eq!(
            parse_names("default-mta | mail-transport-agent"),
            ["default-mta", "mail-transport-agent"]
        );
        // Arch qualifiers dropped; empty field yields nothing.
        assert_eq!(parse_names("libfoo:amd64 (= 1.2)"), ["libfoo"]);
        assert!(parse_names("").is_empty());
    }

    #[test]
    fn chain_walks_to_manual_root() {
        // vlc (manual) → libvlc5 → libvlccore9 (target, auto)
        let installed = ["vlc", "libvlc5", "libvlccore9"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let auto = ["libvlc5", "libvlccore9"].iter().map(|s| s.to_string()).collect();
        let mut rdeps: HashMap<String, BTreeSet<String>> = HashMap::new();
        rdeps.entry("libvlccore9".into()).or_default().insert("libvlc5".into());
        rdeps.entry("libvlc5".into()).or_default().insert("vlc".into());
        let g = Graph { installed, auto, rdeps };

        let e = g.explain("libvlccore9", false);
        assert!(e.installed && !e.manual);
        assert_eq!(e.required_by, ["libvlc5"]);
        assert_eq!(e.chain, ["vlc", "libvlc5", "libvlccore9"]);

        assert!(g.explain("vlc", false).manual);
        assert!(!g.explain("nonexistent", false).installed);
    }

    #[test]
    fn all_mode_lists_every_manual_root() {
        // Two manual roots (vlc, mpv) both pull in libavcodec60 (target, auto)
        // via an intermediate auto lib; ffmpeg-lib is auto and not a root.
        let installed = ["vlc", "mpv", "ffmpeg-lib", "libavcodec60"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let auto = ["ffmpeg-lib", "libavcodec60"].iter().map(|s| s.to_string()).collect();
        let mut rdeps: HashMap<String, BTreeSet<String>> = HashMap::new();
        rdeps.entry("libavcodec60".into()).or_default().insert("ffmpeg-lib".into());
        rdeps.entry("ffmpeg-lib".into()).or_default().extend(["vlc".to_string(), "mpv".to_string()]);
        let g = Graph { installed, auto, rdeps };

        let e = g.explain("libavcodec60", true);
        assert_eq!(e.roots, ["mpv", "vlc"]);
        assert!(e.chain.is_empty());
    }
}
