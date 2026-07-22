use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone};

use crate::apt::{Change, Transaction};

/// One recorded transaction, stored as a line of JSON in the history file.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub id: u64,
    /// Unix timestamp (seconds).
    pub time: i64,
    /// The apt-level command that ran, e.g. ["install", "htop"]. Always kept
    /// executable, since `redo` replays it verbatim.
    pub command: Vec<String>,
    /// How the transaction came about, when its command doesn't say it legibly
    /// (e.g. "undo #5", whose command is a list of pinned versions). Absent in
    /// history written by older versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub install: Vec<Change>,
    pub remove: Vec<Change>,
}

impl Entry {
    pub fn date(&self) -> String {
        Local
            .timestamp_opt(self.time, 0)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".to_string())
    }

    /// The human-facing description of what this transaction did.
    pub fn what(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.command.join(" "))
    }

    /// e.g. "install htop  (+2 ~1 -0)"
    pub fn summary(&self) -> String {
        let new = self.install.iter().filter(|c| c.old.is_none()).count();
        let upgraded = self.install.len() - new;
        let mut counts = Vec::new();
        if new > 0 {
            counts.push(format!("+{new}"));
        }
        if upgraded > 0 {
            counts.push(format!("~{upgraded}"));
        }
        if !self.remove.is_empty() {
            counts.push(format!("-{}", self.remove.len()));
        }
        format!("{}  ({})", self.what(), counts.join(" "))
    }

    pub fn to_transaction(&self) -> Transaction {
        Transaction {
            install: self.install.clone(),
            remove: self.remove.clone(),
        }
    }

    /// Build the apt-get arguments that revert this transaction, using apt's
    /// combined syntax: `pkg-` removes, `pkg=version` installs/downgrades.
    pub fn undo_args(&self) -> Vec<String> {
        let mut args = vec!["install".to_string(), "--allow-downgrades".to_string()];
        for c in &self.install {
            match &c.old {
                // Was newly installed → remove it.
                None => args.push(format!("{}-", c.name)),
                // Was upgraded → go back to the old version.
                Some(old) => args.push(format!("{}={old}", c.name)),
            }
        }
        for c in &self.remove {
            match &c.old {
                Some(old) => args.push(format!("{}={old}", c.name)),
                None => args.push(c.name.clone()),
            }
        }
        args
    }
}

/// Build the apt-get arguments that roll back the combined effect of `entries`
/// (which must be in ascending id order), restoring the state that existed
/// before the earliest of them. For each package, the target version is the
/// `old` value from the *first* entry that touched it (its pre-change state).
pub fn rollback_args(entries: &[Entry]) -> Vec<String> {
    use std::collections::BTreeMap;
    // name → pre-change version (None means "was not installed").
    let mut restore: BTreeMap<&str, &Option<String>> = BTreeMap::new();
    for entry in entries {
        for c in entry.install.iter().chain(entry.remove.iter()) {
            restore.entry(&c.name).or_insert(&c.old);
        }
    }

    let mut args = vec!["install".to_string(), "--allow-downgrades".to_string()];
    for (name, old) in restore {
        match old {
            Some(version) => args.push(format!("{name}={version}")),
            None => args.push(format!("{name}-")),
        }
    }
    args
}

/// All recorded transactions with id strictly greater than `id`, ascending.
pub fn after(id: u64) -> Vec<Entry> {
    let mut entries = load();
    entries.retain(|e| e.id > id);
    entries
}

/// WRAPT_STATE_DIR overrides the history location (useful for testing).
fn history_path() -> PathBuf {
    let dir = std::env::var_os("WRAPT_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/wrapt"));
    dir.join("history.jsonl")
}

pub fn load() -> Vec<Entry> {
    let Ok(content) = std::fs::read_to_string(history_path()) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn find(id: Option<u64>) -> Result<Entry> {
    let mut entries = load();
    match id {
        Some(id) => entries
            .into_iter()
            .find(|e| e.id == id)
            .with_context(|| format!("no transaction {id} in history")),
        None => match entries.pop() {
            Some(e) => Ok(e),
            None => bail!("the transaction history is empty"),
        },
    }
}

/// The id to give the next transaction: one past the highest seen, *not* one
/// past the last line's. `load` skips lines it can't parse, so a truncated tail
/// (a crash or full disk mid-write) would otherwise restart the numbering and
/// hand out a duplicate id — which `find` resolves to the older entry, making
/// `undo <id>` revert the wrong transaction.
fn next_id(entries: &[Entry]) -> u64 {
    entries.iter().map(|e| e.id).max().map_or(1, |max| max + 1)
}

/// How many of the oldest entries to drop to make room for one more. A `limit`
/// of zero is treated as one: config validation rejects it, but a caller
/// passing it shouldn't be able to discard the entry it just recorded.
fn overflow(len: usize, limit: usize) -> usize {
    (len + 1).saturating_sub(limit.max(1))
}

/// Append a transaction, keeping at most `limit` of them. The log would
/// otherwise grow for the life of the machine, and every write re-parses all of
/// it — so the bound protects write cost as much as disk.
pub fn record(
    command: &[String],
    label: Option<String>,
    tx: &Transaction,
    limit: usize,
) -> Result<()> {
    let path = history_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut entries = load();
    let entry = Entry {
        // Still one past the highest id of what we keep, so pruning the oldest
        // entries can't hand out an id twice.
        id: next_id(&entries),
        time: Local::now().timestamp(),
        command: command.to_vec(),
        label,
        install: tx.install.clone(),
        remove: tx.remove.clone(),
    };

    let drop_count = overflow(entries.len(), limit);
    if drop_count == 0 {
        // The ordinary path: one more line on the end.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        return Ok(());
    }
    entries.drain(..drop_count);
    entries.push(entry);
    rewrite(&path, &entries)
}

/// Replace the history with `entries`, atomically. Written to a sibling
/// temporary file and renamed into place, so an interrupted prune leaves the
/// previous history intact rather than a half-written one — this is the file
/// `undo` depends on.
fn rewrite(path: &std::path::Path, entries: &[Entry]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = path.with_extension("jsonl.tmp");
    let write = || -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)
            .with_context(|| format!("cannot open {}", tmp.display()))?;
        for entry in entries {
            writeln!(file, "{}", serde_json::to_string(entry)?)?;
        }
        // Flush to disk before the rename, so a crash can't leave the new name
        // pointing at an empty or partial file.
        file.sync_all()?;
        Ok(())
    };
    if let Err(e) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u64, command: &[&str], label: Option<&str>) -> Entry {
        Entry {
            id,
            time: 0,
            command: command.iter().map(|s| s.to_string()).collect(),
            label: label.map(Into::into),
            install: vec![],
            remove: vec![],
        }
    }

    #[test]
    fn next_id_is_one_past_the_highest() {
        assert_eq!(next_id(&[]), 1);
        assert_eq!(next_id(&[entry(1, &["install"], None)]), 2);
        // A dropped/corrupt tail line leaves a lower id last; the next id must
        // still clear every id on file rather than collide with #3.
        let recovered = [
            entry(1, &["install"], None),
            entry(3, &["upgrade"], None),
            entry(2, &["remove"], None),
        ];
        assert_eq!(next_id(&recovered), 4);
    }

    #[test]
    fn overflow_drops_only_once_the_cap_is_reached() {
        // Below the cap, nothing is dropped and the append path is used.
        assert_eq!(overflow(0, 3), 0);
        assert_eq!(overflow(1, 3), 0);
        // At len == limit - 1 the new entry exactly fills the cap.
        assert_eq!(overflow(2, 3), 0);
        // At the cap, one must go to make room.
        assert_eq!(overflow(3, 3), 1);
        // A file already over the cap (an older wrapt, or a limit lowered in
        // config) is brought back down rather than merely held steady.
        assert_eq!(overflow(10, 3), 8);
    }

    #[test]
    fn a_limit_of_one_keeps_exactly_the_new_entry() {
        // The smallest the config allows: every write replaces the log.
        assert_eq!(overflow(0, 1), 0);
        assert_eq!(overflow(1, 1), 1);
        assert_eq!(overflow(9, 1), 9);
        // Zero is rejected by config validation, but must not discard the
        // entry being recorded if it ever reaches here.
        assert_eq!(overflow(0, 0), 0);
        assert_eq!(overflow(5, 0), 5);
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_reuses_an_id() {
        let entries: Vec<Entry> = (1..=5).map(|i| entry(i, &["install"], None)).collect();
        let next = next_id(&entries);
        assert_eq!(next, 6);

        // Simulate what `record` does at a cap of 3.
        let mut kept = entries;
        let drop_count = overflow(kept.len(), 3);
        kept.drain(..drop_count);
        kept.push(entry(next, &["install"], None));

        let ids: Vec<u64> = kept.iter().map(|e| e.id).collect();
        assert_eq!(ids, [4, 5, 6], "oldest go first, newest survive");
        // The id after pruning still clears everything kept.
        assert_eq!(next_id(&kept), 7);
    }

    #[test]
    fn rewrite_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("wrapt-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        std::fs::write(&path, "{\"corrupt\n").unwrap();

        let entries = vec![
            entry(7, &["install", "htop"], None),
            entry(8, &["upgrade"], None),
        ];
        rewrite(&path, &entries).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\"id\":7") && text.contains("\"id\":8"));
        // The temporary file must not be left behind.
        assert!(!path.with_extension("jsonl.tmp").exists());

        // And what was written parses back to what went in.
        let back: Vec<Entry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(back.iter().map(|e| e.id).collect::<Vec<_>>(), [7, 8]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn summary_prefers_the_label_over_raw_args() {
        // Undo records a pile of pinned versions; the label is what's legible.
        let e = entry(
            7,
            &["install", "--allow-downgrades", "htop=1"],
            Some("undo #6"),
        );
        assert!(e.summary().starts_with("undo #6"));
        // Without a label it still falls back to the command (old history files).
        let e = entry(7, &["install", "htop"], None);
        assert!(e.summary().starts_with("install htop"));
    }

    #[test]
    fn entries_without_a_label_still_parse() {
        // History written before `label` existed must keep loading.
        let old = r#"{"id":1,"time":0,"command":["install","htop"],"install":[],"remove":[]}"#;
        let e: Entry = serde_json::from_str(old).unwrap();
        assert_eq!(e.label, None);
        assert_eq!(e.what(), "install htop");
    }

    fn change(name: &str, old: Option<&str>, new: Option<&str>) -> Change {
        Change {
            name: name.into(),
            old: old.map(Into::into),
            new: new.map(Into::into),
            security: false,
        }
    }

    #[test]
    fn rollback_restores_earliest_pre_state() {
        // #1 installed htop (new); #2 upgraded htop 1→2 and removed vlc.
        let entries = vec![
            Entry {
                id: 1,
                time: 0,
                command: vec!["install".into(), "htop".into()],
                label: None,
                install: vec![change("htop", None, Some("1"))],
                remove: vec![],
            },
            Entry {
                id: 2,
                time: 0,
                command: vec!["upgrade".into()],
                label: None,
                install: vec![change("htop", Some("1"), Some("2"))],
                remove: vec![change("vlc", Some("3"), None)],
            },
        ];
        // Rolling back both should remove htop (wasn't installed before #1)
        // and reinstall vlc at version 3.
        let args = rollback_args(&entries);
        assert!(args.contains(&"htop-".to_string()));
        assert!(args.contains(&"vlc=3".to_string()));
        // htop's target is its earliest pre-state (None), not the #2 old ("1").
        assert!(!args.iter().any(|a| a == "htop=1"));
    }
}
