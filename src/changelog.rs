//! `wrapt changelog`: a package's changelog, with security fixes highlighted.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::{apt, ui};

pub fn run(package: &str) -> Result<()> {
    let text = apt::changelog(package)?;
    let text = text.trim_end();
    if text.trim().is_empty() {
        ui::warn(&format!("No changelog available for {package}."));
        return Ok(());
    }

    ui::header(&format!("Changelog for {package}"));
    for line in text.lines() {
        if is_acquire_noise(line) {
            continue;
        }
        println!("{}", render_line(line));
    }
    Ok(())
}

/// apt streams its download progress onto stdout ahead of the changelog itself
/// ("Get:1 …", "Hit:1 …", "Fetched 165 kB …"); drop those lines.
fn is_acquire_noise(line: &str) -> bool {
    ["Get:", "Hit:", "Ign:", "Err:", "Fetched ", "Reading "]
        .iter()
        .any(|p| line.starts_with(p))
}

/// Colour a single changelog line: entry headers in cyan, security-relevant
/// lines in yellow, everything else plain.
fn render_line(line: &str) -> String {
    // Entry header, e.g. "htop (3.4.1-1) noble; urgency=medium".
    if !line.starts_with(char::is_whitespace)
        && !line.is_empty()
        && line.contains('(')
        && line.contains(')')
    {
        return line.cyan().bold().to_string();
    }

    let lower = line.to_lowercase();
    if lower.contains("cve-")
        || lower.contains("security")
        || lower.contains("urgency=high")
        || lower.contains("urgency=critical")
    {
        return line.yellow().to_string();
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::render_line;

    #[test]
    fn highlights_entry_headers_and_cves() {
        // Header line is recognised (contains parenthesised version).
        assert_ne!(
            render_line("htop (3.4.1-1) noble; urgency=medium"),
            "htop (3.4.1-1) noble; urgency=medium"
        );
        // A plain body line is returned unchanged.
        assert_eq!(
            render_line("  * Some ordinary change"),
            "  * Some ordinary change"
        );
        // A CVE mention is decorated (not returned verbatim).
        assert_ne!(
            render_line("  * Fix CVE-2024-1234"),
            "  * Fix CVE-2024-1234"
        );
    }
}
