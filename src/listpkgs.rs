//! `wrapt list`: installed / upgradable / manually-installed package listings.

use std::collections::HashSet;

use anyhow::Result;

use crate::ui::Paint;
use crate::{apt, ui};

/// List packages. With no flags, every installed package; `upgradable` shows
/// only ones with a newer version; `manual` narrows to packages you asked for.
pub fn run(upgradable: bool, manual: bool, pattern: Option<&str>, json: bool) -> Result<()> {
    if upgradable {
        return list_upgradable(pattern, json);
    }
    list_installed(manual, pattern, json)
}

fn matches(name: &str, pattern: Option<&str>) -> bool {
    pattern.is_none_or(|p| name.contains(p))
}

fn held_set() -> HashSet<String> {
    apt::held().into_iter().collect()
}

fn list_installed(manual_only: bool, pattern: Option<&str>, json: bool) -> Result<()> {
    let manual = apt::manual_set();
    let held = held_set();
    let mut rows: Vec<(String, String, bool, bool)> = apt::installed_versions()
        .into_iter()
        .filter(|(name, _)| matches(name, pattern))
        .map(|(name, version)| {
            let auto = !manual.contains(&name);
            let is_held = held.contains(&name);
            (name, version, auto, is_held)
        })
        .filter(|(_, _, auto, _)| !manual_only || !*auto)
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|(name, version, auto, held)| {
                serde_json::json!({
                    "name": name,
                    "version": version,
                    "automatic": auto,
                    "held": held,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if rows.is_empty() {
        ui::warn("No matching packages.");
        return Ok(());
    }

    let title = if manual_only {
        format!("Manually installed ({})", rows.len())
    } else {
        format!("Installed ({})", rows.len())
    };
    ui::header(&title);
    let name_width = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    for (name, version, auto, held) in &rows {
        let mut badges = String::new();
        if *held {
            badges.push_str(&format!(" {}", "[held]".yellow().bold()));
        }
        if *auto {
            badges.push_str(&format!(" {}", "[auto]".dimmed()));
        }
        println!("   {:name_width$}  {}{badges}", name.bold(), version.cyan());
    }
    Ok(())
}

fn list_upgradable(pattern: Option<&str>, json: bool) -> Result<()> {
    let tx = apt::simulate(&["upgrade".to_string()])?;
    let mut rows: Vec<&apt::Change> = tx
        .install
        .iter()
        .filter(|c| c.old.is_some() && matches(&c.name, pattern))
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));

    if json {
        let arr: Vec<_> = rows
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "current": c.old,
                    "candidate": c.new,
                    "security": c.security,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if rows.is_empty() {
        ui::success("Everything is up to date.");
        return Ok(());
    }

    let security = rows.iter().filter(|c| c.security).count();
    let title = if security > 0 {
        format!("Upgradable ({}, {} security)", rows.len(), security)
    } else {
        format!("Upgradable ({})", rows.len())
    };
    ui::header(&title);
    let name_width = rows.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in &rows {
        let badge = if c.security {
            format!(" {}", "🔒 security".yellow().bold())
        } else {
            String::new()
        };
        println!(
            "   {:name_width$}  {} {} {}{badge}",
            c.name.bold(),
            c.old.as_deref().unwrap_or("?").dimmed(),
            "→".cyan(),
            c.new.as_deref().unwrap_or("?").green()
        );
    }
    Ok(())
}
