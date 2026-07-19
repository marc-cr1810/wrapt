//! Runs the real `apt-get` behind a clean progress display driven entirely by
//! apt's machine-readable status protocol (`APT::Status-Fd`) on fd 3.
//!
//! apt's and dpkg's own chatter (`Unpacking...`, `Setting up...`, `Processing
//! triggers...`) is fully suppressed: stdout is captured into a hidden log and
//! never shown, unless the operation fails — then the log is dumped so nothing
//! is lost when it matters. `Dpkg::Use-Pty=0` keeps dpkg from writing straight
//! to the terminal behind our back.

use std::collections::HashSet;
use std::fs::File;
use std::io::{ErrorKind, IsTerminal};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;

use crate::ui;

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Status,
    Stdout,
    Stderr,
}

struct Source {
    fd: RawFd,
    kind: Kind,
    buf: Vec<u8>,
    eof: bool,
}

/// Run apt-get, showing a single clean progress bar. When `verbose`, skip the
/// bar entirely and stream apt's native (fancy) output instead.
pub fn run_with_progress(args: &[String], verbose: bool) -> Result<()> {
    if verbose {
        return run_verbose(args);
    }

    // With a real terminal we can let apt/dpkg prompt (e.g. changed config
    // files); otherwise stay fully non-interactive and report afterwards.
    let interactive = std::io::stdin().is_terminal();

    let mut pipe = [0i32; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        bail!("pipe2 failed: {}", std::io::Error::last_os_error());
    }
    let (status_r, status_w) = (pipe[0], pipe[1]);

    let mut cmd = Command::new("apt-get");
    cmd.env("LC_ALL", "C")
        // readline is line-oriented, so a prompt cooperates with our display;
        // the fullscreen dialog frontend would fight the progress bar.
        .env(
            "DEBIAN_FRONTEND",
            if interactive {
                "readline"
            } else {
                "noninteractive"
            },
        )
        .arg("-y")
        .args(["-o", "APT::Status-Fd=3"])
        // Don't let dpkg allocate a terminal and write past our capture.
        .args(["-o", "Dpkg::Use-Pty=0"])
        .args(args)
        // Inherit the terminal when interactive so the user can answer prompts.
        .stdin(if interactive {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        // Hand the child apt's status pipe as fd 3 (dup2 clears CLOEXEC).
        cmd.pre_exec(move || {
            if libc::dup2(status_w, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("failed to run apt-get")?;
    unsafe { libc::close(status_w) };

    let status_file = unsafe { File::from_raw_fd(status_r) };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let bar = ProgressBar::new(100);
    bar.set_style(
        ProgressStyle::with_template(
            "  {spinner:.cyan} {msg:<40!} [{bar:28.cyan/black}] {percent:>3}%",
        )
        .unwrap()
        .progress_chars("━╸ ")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(90));
    bar.set_message("Preparing...");

    let mut sources = [
        Source {
            fd: status_file.as_raw_fd(),
            kind: Kind::Status,
            buf: Vec::new(),
            eof: false,
        },
        Source {
            fd: stdout.as_raw_fd(),
            kind: Kind::Stdout,
            buf: Vec::new(),
            eof: false,
        },
        Source {
            fd: stderr.as_raw_fd(),
            kind: Kind::Stderr,
            buf: Vec::new(),
            eof: false,
        },
    ];
    for s in &sources {
        set_nonblocking(s.fd)?;
    }

    // Hidden log of apt/dpkg output, dumped only if the operation fails.
    let mut log: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // Packages we've already printed a ✓/- line for (apt repeats "Installed").
    let mut done: HashSet<String> = HashSet::new();
    // Config files apt reported as changed during this run.
    let mut conffiles: Vec<String> = Vec::new();
    // Once a prompt appears (interactive), stop hiding and let it through.
    let mut revealing = false;
    // True once apt's last status was a package completion ("Installed"/"Removed").
    // apt then goes silent during dpkg trigger processing without sending more
    // status, so we relabel the (frozen) bar instead of leaving a stale line.
    let mut last_completion = false;
    loop {
        let mut pfds: Vec<libc::pollfd> = sources
            .iter()
            .filter(|s| !s.eof)
            .map(|s| libc::pollfd {
                fd: s.fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if pfds.is_empty() {
            break;
        }
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as _, 200) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            bail!("poll failed: {err}");
        }

        for pfd in pfds.iter().filter(|p| p.revents != 0) {
            let src = sources.iter_mut().find(|s| s.fd == pfd.fd).unwrap();
            read_available(src);
        }
        // A config-file prompt just started — surface apt's real prompt so the
        // user can answer it, instead of hiding it and silently defaulting.
        let mut new_conffiles = Vec::new();
        for src in &mut sources {
            for line in take_lines(&mut src.buf) {
                match src.kind {
                    Kind::Status => handle_status(
                        &line,
                        &bar,
                        &mut done,
                        &mut new_conffiles,
                        &mut last_completion,
                    ),
                    Kind::Stdout => {
                        if revealing {
                            bar.suspend(|| println!("{line}"));
                        } else if !line.is_empty() {
                            // apt's status-fd goes silent after the last package's
                            // "Installed" (80%), but dpkg still writes "Processing
                            // triggers for ..." here while it works. Reflect that
                            // so the parked bar reads as active, not stalled.
                            if last_completion && line.starts_with("Processing triggers") {
                                bar.set_message("Finishing up...");
                            }
                            log.push(line);
                        }
                    }
                    Kind::Stderr => {
                        if let Some(msg) = line.strip_prefix("E: ") {
                            errors.push(msg.to_string());
                            log.push(format!("E: {msg}"));
                        } else if let Some(msg) = line.strip_prefix("W: ") {
                            bar.suspend(|| ui::warn(msg));
                            log.push(format!("W: {msg}"));
                        } else if revealing {
                            bar.suspend(|| eprintln!("{line}"));
                        } else if !line.is_empty() {
                            log.push(line);
                        }
                    }
                }
            }
        }

        if !new_conffiles.is_empty() {
            conffiles.append(&mut new_conffiles);
            if interactive && !revealing {
                enter_reveal(&bar);
                revealing = true;
            }
        }

        // apt reports a package as "Installed" at 80% and then works silently
        // through dpkg's trigger processing, sending no further status. Relabel
        // the parked bar so that tail reads as active work, not a stale line.
        if rc == 0 && !revealing && last_completion {
            bar.set_message("Finishing up...");
        }

        // Interactive and apt has stalled mid-line on something that looks like
        // a question (not just its normal "Building dependency tree..." chatter,
        // which also sits unterminated while apt works). Reveal it so the user
        // can answer. Genuine config-file prompts also arrive via pmstatus above.
        if rc == 0 && interactive && !revealing {
            let stdout_src = sources.iter_mut().find(|s| s.kind == Kind::Stdout).unwrap();
            let pending = String::from_utf8_lossy(&stdout_src.buf);
            if looks_like_prompt(&pending) {
                let pending = pending.into_owned();
                stdout_src.buf.clear();
                enter_reveal(&bar);
                revealing = true;
                print!("{pending}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        }
    }

    let status = child.wait()?;
    // apt never sends a final 100% (it stops at the last package's 80%), so set
    // it ourselves on success — otherwise a captured/non-tty log ends at 80%.
    if status.success() {
        bar.set_position(100);
    }
    bar.finish_and_clear();

    if !status.success() {
        // Reveal what we were hiding so failures are never opaque.
        if !log.is_empty() {
            ui::warn("apt output:");
            for line in &log {
                eprintln!("    {}", line.dimmed());
            }
        }
        if errors.is_empty() {
            bail!("apt-get exited with {status}");
        }
        bail!("{}", errors.join("\n"));
    }

    // In non-interactive runs we couldn't ask, so apt kept the existing config
    // files. Say so plainly rather than leaving it silent.
    if !conffiles.is_empty() && !interactive {
        conffiles.sort();
        conffiles.dedup();
        ui::warn(&format!(
            "{} package{} shipped changed config files; kept your existing versions \
             (new ones saved alongside as *.dpkg-dist):",
            conffiles.len(),
            if conffiles.len() == 1 { "" } else { "s" }
        ));
        for pkg in &conffiles {
            eprintln!("    {}", pkg.dimmed());
        }
    }
    Ok(())
}

/// Verbose mode: run apt-get with its own fancy progress, inheriting our stdio.
fn run_verbose(args: &[String]) -> Result<()> {
    let status = Command::new("apt-get")
        .env("LC_ALL", "C")
        .arg("-y")
        .args(["-o", "Dpkg::Progress-Fancy=1"])
        .args(args)
        .status()
        .context("failed to run apt-get")?;
    if !status.success() {
        bail!("apt-get exited with {status}");
    }
    Ok(())
}

/// Status-fd lines look like `pmstatus:<pkg>:<percent>:<message>`.
fn handle_status(
    line: &str,
    bar: &ProgressBar,
    done: &mut HashSet<String>,
    conffiles: &mut Vec<String>,
    last_completion: &mut bool,
) {
    let mut parts = line.splitn(4, ':');
    let kind = parts.next().unwrap_or("");
    let pkg = parts.next().unwrap_or("");
    let percent: f64 = parts.next().unwrap_or("").parse().unwrap_or(0.0);
    let msg = parts.next().unwrap_or("");
    match kind {
        "pmstatus" => {
            bar.set_position(percent.round() as u64);
            bar.set_message(strip_arch(msg));
            // Track whether this milestone completed a package, so a following
            // silent stretch can be labelled as trigger processing.
            *last_completion = msg.starts_with("Installed ") || msg.starts_with("Removed ");
            // apt repeats "Installed <pkg>" — print the tick only once.
            if let Some(name) = msg.strip_prefix("Installed ") {
                if done.insert(pkg.to_string()) {
                    let name = strip_arch(name);
                    bar.suspend(|| println!("  {} {}", "✓".green().bold(), name));
                }
            } else if let Some(name) = msg.strip_prefix("Removed ")
                && done.insert(pkg.to_string())
            {
                let name = strip_arch(name);
                bar.suspend(|| println!("  {} {}", "-".red().bold(), name));
            }
        }
        "pmerror" => {
            bar.suspend(|| println!("  {} {}", "✗".red().bold(), msg.red()));
        }
        "pmconffile" => {
            conffiles.push(pkg.to_string());
        }
        _ => {}
    }
}

/// Clear the progress bar so revealed prompt/output has the terminal to itself.
fn enter_reveal(bar: &ProgressBar) {
    bar.set_draw_target(ProgressDrawTarget::hidden());
    ui::warn("apt needs your input:");
}

/// Does a stalled, unterminated stdout buffer look like an interactive prompt
/// rather than apt's normal in-progress chatter? Kept deliberately strict so
/// lines like "Building dependency tree..." or "The following NEW packages:"
/// never trigger a reveal.
fn looks_like_prompt(buf: &str) -> bool {
    // Consider only the final unterminated line.
    let last = buf.rsplit(['\n', '\r']).next().unwrap_or(buf);
    let t = last.trim_end_matches(' ');
    if t.is_empty() {
        return false;
    }
    // Explicit yes/no or default-answer markers.
    if t.contains("[Y/n]")
        || t.contains("[y/N]")
        || t.contains("[yes/no]")
        || t.contains("[default=")
    {
        return true;
    }
    // A trailing question mark ("...continue? ", conffile "... ? ").
    if t.ends_with('?') {
        return true;
    }
    // A bracketed default like "... [1]:" or "... [2] " (debconf select).
    if (t.ends_with("]:") || t.ends_with(']')) && t.contains('[') {
        return true;
    }
    false
}

/// "htop (amd64) 3.4.1" → "htop 3.4.1"; leaves other text untouched.
fn strip_arch(s: &str) -> String {
    let mut out = s.to_string();
    for arch in [" (amd64)", " (i386)", " (all)", " (arm64)", " (armhf)"] {
        out = out.replace(arch, "");
    }
    out
}

fn set_nonblocking(fd: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        bail!("fcntl failed: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn read_available(src: &mut Source) {
    let mut tmp = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(src.fd, tmp.as_mut_ptr().cast(), tmp.len()) };
        match n {
            0 => {
                src.eof = true;
                break;
            }
            n if n > 0 => src.buf.extend_from_slice(&tmp[..n as usize]),
            _ => {
                let err = std::io::Error::last_os_error();
                if err.kind() != ErrorKind::WouldBlock && err.kind() != ErrorKind::Interrupted {
                    src.eof = true;
                }
                if err.kind() != ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

fn take_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let raw: Vec<u8> = buf.drain(..=pos).collect();
        lines.push(
            String::from_utf8_lossy(&raw[..raw.len() - 1])
                .trim_end_matches('\r')
                .to_string(),
        );
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{handle_status, looks_like_prompt};

    #[test]
    fn tracks_package_completion_for_trigger_phase() {
        use indicatif::ProgressBar;
        use std::collections::HashSet;

        let bar = ProgressBar::hidden();
        let mut done = HashSet::new();
        let mut conf = Vec::new();
        let mut last = false;

        // Mid-package milestones are not completions.
        handle_status(
            "pmstatus:htop:60.0000:Configuring htop (amd64)",
            &bar,
            &mut done,
            &mut conf,
            &mut last,
        );
        assert!(!last, "'Configuring' should not count as a completion");

        // "Installed" (the last line apt sends) is what precedes the silent
        // trigger-processing tail.
        handle_status(
            "pmstatus:htop:80.0000:Installed htop (amd64)",
            &bar,
            &mut done,
            &mut conf,
            &mut last,
        );
        assert!(last, "'Installed' should mark a completion");

        // A following package's preparation clears it again (multi-package runs).
        handle_status(
            "pmstatus:tree:0.0000:Preparing tree (amd64)",
            &bar,
            &mut done,
            &mut conf,
            &mut last,
        );
        assert!(!last, "next package's 'Preparing' should clear the flag");
    }

    #[test]
    fn apt_progress_chatter_is_not_a_prompt() {
        // The exact lines that used to trigger a spurious reveal.
        for line in [
            "Building dependency tree...",
            "Reading state information...",
            "Solving dependencies...",
            "The following NEW packages will be installed:",
            "(Reading database ... 45%",
            "Preparing to unpack .../gcc-16_16_amd64.deb ...",
            "",
        ] {
            assert!(!looks_like_prompt(line), "false positive on: {line:?}");
        }
    }

    #[test]
    fn real_prompts_are_detected() {
        for line in [
            "Do you want to continue? [Y/n] ",
            "Remove obsolete package? [y/N]",
            "*** file.conf (Y/I/N/O/D/Z) [default=N] ? ",
            "  What would you like to do about it ? ",
            "(Enter the item number) [1]: ",
        ] {
            assert!(looks_like_prompt(line), "missed prompt: {line:?}");
        }
    }

    #[test]
    fn only_the_last_line_matters() {
        // Completed lines before the stalled one shouldn't matter.
        let buf = "Unpacking gcc-16 ...\nSetting up gcc-16 ...\nContinue? [Y/n] ";
        assert!(looks_like_prompt(buf));
        let buf = "Continue? [Y/n] yes\nBuilding dependency tree...";
        assert!(!looks_like_prompt(buf));
    }
}
