use std::io::{self, Write};

use owo_colors::OwoColorize;

use crate::apt::Transaction;

pub fn header(text: &str) {
    println!("{} {}", "::".cyan().bold(), text.bold());
}

pub fn success(text: &str) {
    println!("{} {}", "✓".green().bold(), text);
}

pub fn warn(text: &str) {
    eprintln!("{} {}", "!".yellow().bold(), text.yellow());
}

pub fn error(text: &str) {
    eprintln!("{} {}", "error:".red().bold(), text);
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Ask a yes/no question on the terminal.
pub fn confirm(prompt: &str, default_yes: bool) -> bool {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!(
        "{} {} {} ",
        "::".cyan().bold(),
        prompt.bold(),
        hint.dimmed()
    );
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    match answer.trim().to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Prompt for a selection like "1 3-5 8" and return the chosen 1-based indices
/// (clamped to `count`). Empty input returns nothing.
pub fn prompt_selection(count: usize) -> Vec<usize> {
    println!();
    print!(
        "{} {} {} ",
        "::".cyan().bold(),
        "Install which? (e.g. 1 3-5, blank to skip)".bold(),
        "»".dimmed()
    );
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return Vec::new();
    }
    parse_selection(answer.trim(), count)
}

/// Parse "1 3-5 8" into sorted, de-duplicated 1-based indices within `1..=count`.
fn parse_selection(input: &str, count: usize) -> Vec<usize> {
    let mut picks = std::collections::BTreeSet::new();
    for token in input.split_whitespace() {
        match token.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    for i in a..=b {
                        if (1..=count).contains(&i) {
                            picks.insert(i);
                        }
                    }
                }
            }
            None => {
                if let Ok(i) = token.parse::<usize>()
                    && (1..=count).contains(&i)
                {
                    picks.insert(i);
                }
            }
        }
    }
    picks.into_iter().collect()
}

/// Print the pending changes of a transaction as aligned, color-coded sections.
/// `manual` names the packages apt considers manually installed, so removals of
/// packages the user chose can be flagged.
pub fn print_transaction(tx: &Transaction, manual: &std::collections::HashSet<String>) {
    let (upgrades, installs): (Vec<_>, Vec<_>) = tx.install.iter().partition(|c| c.old.is_some());

    let name_width = tx
        .install
        .iter()
        .chain(tx.remove.iter())
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);

    if !installs.is_empty() {
        header(&format!("Installing ({})", installs.len()));
        for c in &installs {
            let name = format!("{:name_width$}", c.name);
            println!(
                "   {}  {}",
                name.bold(),
                c.new.as_deref().unwrap_or("?").green()
            );
        }
    }
    if !upgrades.is_empty() {
        let security = upgrades.iter().filter(|c| c.security).count();
        let title = if security > 0 {
            format!("Upgrading ({}, {} security)", upgrades.len(), security)
        } else {
            format!("Upgrading ({})", upgrades.len())
        };
        header(&title);
        for c in &upgrades {
            let name = format!("{:name_width$}", c.name);
            let badge = if c.security {
                format!(" {}", "🔒 security".yellow().bold())
            } else {
                String::new()
            };
            println!(
                "   {}  {} {} {}{badge}",
                name.bold(),
                c.old.as_deref().unwrap_or("?").dimmed(),
                "→".cyan(),
                c.new.as_deref().unwrap_or("?").green()
            );
        }
    }
    if !tx.remove.is_empty() {
        header(&format!("Removing ({})", tx.remove.len()));
        for c in &tx.remove {
            let name = format!("{:name_width$}", c.name);
            // Flag removals of packages the user installed on purpose.
            let badge = if manual.contains(&c.name) {
                format!(" {}", "(you installed this)".yellow())
            } else {
                String::new()
            };
            println!(
                "   {}  {}{badge}",
                name.red().bold(),
                c.old.as_deref().unwrap_or("?").dimmed()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_selection;

    #[test]
    fn parses_ranges_and_singles() {
        assert_eq!(parse_selection("1 3-5 8", 10), [1, 3, 4, 5, 8]);
        // Out-of-range values are dropped; duplicates collapse.
        assert_eq!(parse_selection("2 2 9-99", 5), [2]);
        assert!(parse_selection("", 5).is_empty());
        assert!(parse_selection("abc", 5).is_empty());
    }
}
