use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::apt::Transaction;

/// Whether ANSI escapes are emitted. Decided once at startup by
/// [`crate::config::Config::apply_color`] from the config, `NO_COLOR`, and
/// whether stdout is a terminal.
static COLOR: AtomicBool = AtomicBool::new(false);

pub fn set_color(on: bool) {
    COLOR.store(on, Ordering::Relaxed);
}

fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// A value wrapped in one SGR code, rendered only when colour is enabled.
pub struct Painted<T> {
    inner: T,
    code: &'static str,
}

impl<T: fmt::Display> fmt::Display for Painted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !color_enabled() {
            return fmt::Display::fmt(&self.inner, f);
        }
        f.write_str("\x1b[")?;
        f.write_str(self.code)?;
        f.write_str("m")?;
        // Forward the formatter so width/alignment applies to the text itself
        // rather than counting the escape bytes.
        fmt::Display::fmt(&self.inner, f)?;
        f.write_str("\x1b[0m")
    }
}

/// Colouring that respects the user's colour policy. Deliberately tiny: these
/// six styles are all wrapt uses, and going through our own trait (rather than
/// a library's unconditional one) is what makes `color = "never"`, `NO_COLOR`,
/// and piped output actually come out plain.
pub trait Paint {
    fn paint(&self, code: &'static str) -> Painted<&Self> {
        Painted { inner: self, code }
    }
    fn bold(&self) -> Painted<&Self> {
        self.paint("1")
    }
    fn dimmed(&self) -> Painted<&Self> {
        self.paint("2")
    }
    fn red(&self) -> Painted<&Self> {
        self.paint("31")
    }
    fn green(&self) -> Painted<&Self> {
        self.paint("32")
    }
    fn yellow(&self) -> Painted<&Self> {
        self.paint("33")
    }
    fn cyan(&self) -> Painted<&Self> {
        self.paint("36")
    }
}

// Blanket impl, so styles chain (`"x".green().bold()`) the same way they did.
impl<T: fmt::Display + ?Sized> Paint for T {}

pub fn header(text: &str) {
    println!("{}", header_line(text));
}

/// The rendered form of [`header`], so output built into a string matches what
/// is printed directly.
pub fn header_line(text: &str) -> String {
    format!("{} {}", "::".cyan().bold(), text.bold())
}

pub fn success(text: &str) {
    println!("{} {}", "✓".green().bold(), text);
}

pub fn warn(text: &str) {
    eprintln!("{}", warn_line(text));
}

/// The rendered form of [`warn`], for output assembled into a string.
pub fn warn_line(text: &str) -> String {
    format!("{} {}", "!".yellow().bold(), text.yellow())
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
    print!("{}", render_transaction(tx, manual));
}

/// The transaction plan as text. Split from printing so the exact rendering —
/// which is most of what wrapt is for — can be asserted in tests rather than
/// only ever checked by eye.
pub fn render_transaction(tx: &Transaction, manual: &std::collections::HashSet<String>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let (upgrades, installs): (Vec<_>, Vec<_>) = tx.install.iter().partition(|c| c.old.is_some());

    let name_width = tx
        .install
        .iter()
        .chain(tx.remove.iter())
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);

    if !installs.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            header_line(&format!("Installing ({})", installs.len()))
        );
        for c in &installs {
            let name = format!("{:name_width$}", c.name);
            let _ = writeln!(
                out,
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
        let _ = writeln!(out, "{}", header_line(&title));
        for c in &upgrades {
            let name = format!("{:name_width$}", c.name);
            let badge = if c.security {
                format!(" {}", "🔒 security".yellow().bold())
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "   {}  {} {} {}{badge}",
                name.bold(),
                c.old.as_deref().unwrap_or("?").dimmed(),
                "→".cyan(),
                c.new.as_deref().unwrap_or("?").green()
            );
        }
    }
    if !tx.remove.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            header_line(&format!("Removing ({})", tx.remove.len()))
        );
        for c in &tx.remove {
            let name = format!("{:name_width$}", c.name);
            // Flag removals of packages the user installed on purpose.
            let badge = if manual.contains(&c.name) {
                format!(" {}", "(you installed this)".yellow())
            } else {
                String::new()
            };
            let _ = writeln!(
                out,
                "   {}  {}{badge}",
                name.red().bold(),
                c.old.as_deref().unwrap_or("?").dimmed()
            );
        }
    }
    out
}

/// Colour is process-wide state, and tests run in parallel, so any test that
/// asserts on rendered text must hold this lock for its duration — otherwise
/// another test toggling colour mid-render makes the assertion flaky.
#[cfg(test)]
pub(crate) mod test_color {
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Force plain output and keep it that way until the guard drops. A
    /// poisoned lock is recovered rather than propagated: a panic in an
    /// unrelated test shouldn't cascade into every rendering test.
    pub(crate) fn plain() -> MutexGuard<'static, ()> {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        super::set_color(false);
        guard
    }

    /// As [`plain`], but with colour on.
    pub(crate) fn coloured() -> MutexGuard<'static, ()> {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        super::set_color(true);
        guard
    }
}

#[cfg(test)]
mod render_tests {
    use super::test_color::{coloured, plain};
    use super::*;
    use crate::apt::Change;
    use std::collections::HashSet;

    fn change(name: &str, old: Option<&str>, new: Option<&str>, security: bool) -> Change {
        Change {
            name: name.to_string(),
            old: old.map(Into::into),
            new: new.map(Into::into),
            security,
        }
    }

    #[test]
    fn renders_installs_upgrades_and_removals_aligned() {
        let _guard = plain();
        let tx = Transaction {
            install: vec![
                change("htop", None, Some("3.4.1"), false),
                change("libssl3", Some("3.0.1"), Some("3.0.2"), false),
            ],
            remove: vec![change("nano", Some("7.2"), None, false)],
        };
        // Names pad to the widest across every section, so the columns line up.
        assert_eq!(
            render_transaction(&tx, &HashSet::new()),
            ":: Installing (1)\n\
             \x20  htop     3.4.1\n\
             :: Upgrading (1)\n\
             \x20  libssl3  3.0.1 → 3.0.2\n\
             :: Removing (1)\n\
             \x20  nano     7.2\n"
        );
    }

    #[test]
    fn security_upgrades_are_counted_and_badged() {
        let _guard = plain();
        let tx = Transaction {
            install: vec![
                change("openssl", Some("3.0.1"), Some("3.0.2"), true),
                change("curl", Some("8.1"), Some("8.2"), false),
            ],
            remove: vec![],
        };
        let out = render_transaction(&tx, &HashSet::new());
        assert!(
            out.starts_with(":: Upgrading (2, 1 security)\n"),
            "header should count security upgrades: {out}"
        );
        assert!(out.contains("openssl  3.0.1 → 3.0.2 🔒 security"));
        // The non-security upgrade carries no badge.
        assert!(out.contains("curl     8.1 → 8.2\n"));
    }

    #[test]
    fn removing_a_manually_installed_package_is_flagged() {
        let _guard = plain();
        let tx = Transaction {
            install: vec![],
            remove: vec![
                change("ripgrep", Some("14.1"), None, false),
                change("libfoo", Some("1.0"), None, false),
            ],
        };
        let manual: HashSet<String> = ["ripgrep".to_string()].into_iter().collect();
        let out = render_transaction(&tx, &manual);
        assert!(out.contains("ripgrep  14.1 (you installed this)"));
        // A dependency pulled in automatically gets no warning.
        assert!(out.contains("libfoo   1.0\n"));
    }

    #[test]
    fn an_empty_transaction_renders_nothing() {
        let _guard = plain();
        let tx = Transaction {
            install: vec![],
            remove: vec![],
        };
        assert_eq!(render_transaction(&tx, &HashSet::new()), "");
    }

    #[test]
    fn colour_is_emitted_only_when_enabled() {
        let tx = Transaction {
            install: vec![change("htop", None, Some("3.4.1"), false)],
            remove: vec![],
        };
        let with_colour = {
            let _guard = coloured();
            render_transaction(&tx, &HashSet::new())
        };
        let _guard = plain();
        let plain_out = render_transaction(&tx, &HashSet::new());
        assert!(with_colour.contains('\x1b'), "should carry escapes when on");
        assert!(!plain_out.contains('\x1b'), "must be clean when off");
        // Padding must measure the text, not the escape bytes.
        assert!(plain_out.contains("   htop  3.4.1\n"));
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
