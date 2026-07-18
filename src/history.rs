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
    /// The apt-level command that ran, e.g. ["install", "htop"].
    pub command: Vec<String>,
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
        format!("{}  ({})", self.command.join(" "), counts.join(" "))
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

pub fn record(command: &[String], tx: &Transaction) -> Result<()> {
    let path = history_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let entry = Entry {
        id: load().last().map_or(1, |e| e.id + 1),
        time: Local::now().timestamp(),
        command: command.to_vec(),
        install: tx.install.clone(),
        remove: tx.remove.clone(),
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(&entry)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                install: vec![change("htop", None, Some("1"))],
                remove: vec![],
            },
            Entry {
                id: 2,
                time: 0,
                command: vec!["upgrade".into()],
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
