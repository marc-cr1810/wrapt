//! `wrapt command-not-found`: suggest a package for an unrecognised command,
//! plus the shell hooks that wire it into bash/zsh/fish.
//!
//! The resolver is what the shell calls when you type a command it can't find.
//! It looks the command up as an executable across all packages (via apt-file
//! when its index is present) and, failing that, checks for a same-named
//! package, then prints an install hint.

use std::process::Command;

use anyhow::{Result, bail};
use clap_complete::Shell;
use owo_colors::OwoColorize;

use crate::ui;

/// Look up `cmd` and print a suggestion. Returns `true` if the command is in
/// fact already on PATH (so the caller can exit 0 instead of 127).
pub fn resolve(cmd: &str) -> bool {
    if let Some(path) = on_path(cmd) {
        ui::success(&format!("{cmd} is already available at {}.", path.cyan()));
        return true;
    }

    let mut packages = apt_file_lookup(cmd);
    // Fall back to a same-named package when apt-file has nothing (or isn't
    // installed) — the common case where the command matches the package name.
    if packages.is_empty()
        && let Some(pkg) = same_named_package(cmd)
    {
        packages.push(pkg);
    }
    packages.sort();
    packages.dedup();

    match packages.as_slice() {
        [] => {
            eprintln!("{} command not found: {}", "!".yellow().bold(), cmd.bold());
            if !apt_file_available() {
                eprintln!(
                    "  {}",
                    "For suggestions across all packages, install apt-file:".dimmed()
                );
                eprintln!(
                    "  {}",
                    "wrapt install apt-file && sudo apt-file update".cyan()
                );
            }
        }
        [pkg] => {
            eprintln!(
                "{} the program {} is not installed. Install it with:",
                "!".yellow().bold(),
                cmd.bold()
            );
            eprintln!("  {}", format!("sudo wrapt install {pkg}").cyan());
        }
        pkgs => {
            eprintln!(
                "{} {} is provided by several packages. Install one of:",
                "!".yellow().bold(),
                cmd.bold()
            );
            for pkg in pkgs {
                eprintln!("  {}", format!("sudo wrapt install {pkg}").cyan());
            }
        }
    }
    false
}

/// Absolute path of `cmd` if it resolves on the current PATH.
fn on_path(cmd: &str) -> Option<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v -- {}", shell_quote(cmd)))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    path.starts_with('/').then_some(path)
}

fn apt_file_available() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("command -v apt-file")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Packages that ship `cmd` in a bin directory, via `apt-file`.
fn apt_file_lookup(cmd: &str) -> Vec<String> {
    if !apt_file_available() {
        return Vec::new();
    }
    // Match the command as an executable in a standard bin dir, exactly.
    let pattern = format!("/(bin|sbin|games)/{}$", regex_escape(cmd));
    let Ok(out) = Command::new("apt-file")
        .args(["--package-only", "--regexp", "search", &pattern])
        .env("LC_ALL", "C")
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// A package whose name is exactly `cmd`, if one exists in the indexes.
fn same_named_package(cmd: &str) -> Option<String> {
    let out = Command::new("apt-cache")
        .args(["show", "--", cmd])
        .env("LC_ALL", "C")
        .output()
        .ok()?;
    out.status.success().then(|| cmd.to_string())
}

/// Print the shell hook that routes unknown commands through wrapt.
pub fn print_hook(shell: Shell) -> Result<()> {
    // (hook body, rc file to add the enable line to, the enable line itself).
    let (body, rc, enable) = match shell {
        Shell::Bash => (
            "command_not_found_handle() {\n\
             \twrapt command-not-found -- \"$1\"\n\
             \treturn 127\n\
             }\n",
            "~/.bashrc",
            "eval \"$(wrapt command-not-found --init bash)\"",
        ),
        Shell::Zsh => (
            "command_not_found_handler() {\n\
             \twrapt command-not-found -- \"$1\"\n\
             \treturn 127\n\
             }\n",
            "~/.zshrc",
            "eval \"$(wrapt command-not-found --init zsh)\"",
        ),
        Shell::Fish => (
            "function fish_command_not_found\n\
             \twrapt command-not-found -- $argv[1]\n\
             end\n",
            "~/.config/fish/config.fish",
            "wrapt command-not-found --init fish | source",
        ),
        other => bail!(
            "no command-not-found hook for {other} — supported shells are bash, zsh, and fish"
        ),
    };
    // A leading label, then the hook, then a commented reminder of how to wire
    // it up — harmless when eval'd, self-documenting when read.
    print!("# wrapt command-not-found handler\n{body}");
    print!("#\n# To enable, add the following line to {rc}:\n#   {enable}\n");
    Ok(())
}

/// Escape a string for safe inclusion in an ERE (apt-file `--regexp`).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r".^$*+?()[]{}|\/".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Single-quote a string for a POSIX shell command line.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_regex_metacharacters() {
        assert_eq!(regex_escape("g++"), r"g\+\+");
        assert_eq!(regex_escape("a.b"), r"a\.b");
        assert_eq!(regex_escape("plain"), "plain");
    }

    #[test]
    fn shell_quote_handles_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote("safe"), "'safe'");
    }
}
