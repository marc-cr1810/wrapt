mod apt;
mod cli;
mod config;
mod doctor;
mod download;
mod exec;
mod history;
mod lists;
mod provides;
mod resolve;
mod restart;
mod selfupdate;
mod ui;
mod why;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use owo_colors::OwoColorize;

use cli::Command;

const APT_ARCHIVES: &str = "/var/cache/apt/archives";

/// Where downloaded .debs go. WRAPT_CACHE_DIR overrides apt's archive cache
/// (useful for testing without root).
fn archives_dir() -> (PathBuf, bool) {
    match std::env::var_os("WRAPT_CACHE_DIR") {
        Some(dir) => (PathBuf::from(dir), true),
        None => (PathBuf::from(APT_ARCHIVES), false),
    }
}

#[tokio::main]
async fn main() {
    // Rust ignores SIGPIPE by default, which makes `wrapt search … | head`
    // panic on the closed pipe. Restore the default so we exit quietly instead.
    restore_sigpipe();

    let cli = cli::Cli::parse();
    if let Err(e) = run(cli).await {
        ui::error(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn restore_sigpipe() {
    // SAFETY: setting a signal disposition to the default handler is sound and
    // is the standard fix for CLI tools that write to pipes.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

async fn run(cli: cli::Cli) -> Result<()> {
    let cfg = config::Config::load()?;
    cfg.apply_color();

    // CLI flags win; otherwise fall back to config, then built-in defaults.
    let jobs = cli.parallel.or(cfg.parallel).unwrap_or(5).max(1);
    let verbose = cli.verbose || cfg.verbose.unwrap_or(false);
    let assume_yes = cfg.assume_yes.unwrap_or(false);
    let repo = selfupdate::resolve_repo(cfg.repo.as_deref());
    let notify_updates = cfg.notify_updates.unwrap_or(false);
    let opts = TxOpts {
        yes: assume_yes,
        jobs,
        verbose,
    };
    match cli.command {
        Command::Update => cmd_update(),
        Command::Upgrade {
            yes,
            full,
            security_only,
        } => {
            let res = cmd_upgrade(
                full,
                security_only,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
            )
            .await;
            // Best-effort heads-up that wrapt itself has a newer release.
            if res.is_ok() && notify_updates {
                selfupdate::notify_if_outdated(&repo).await;
            }
            res
        }
        Command::Install { packages, yes } => {
            let mut args = vec!["install".to_string()];
            args.extend(packages.clone());
            transaction(
                args,
                &packages,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
                "Installing packages...",
            )
            .await
        }
        Command::Remove {
            packages,
            yes,
            purge,
        } => {
            let op = if purge { "purge" } else { "remove" };
            let mut args = vec![op.to_string()];
            args.extend(packages.clone());
            transaction(
                args,
                &packages,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
                "Removing packages...",
            )
            .await
        }
        Command::Autoremove { yes } => {
            transaction(
                vec!["autoremove".to_string()],
                &[],
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
                "Removing unused packages...",
            )
            .await
        }
        Command::History { id } => cmd_history(id, cli.json),
        Command::Undo { id, yes } => {
            cmd_undo(
                id,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
            )
            .await
        }
        Command::Redo { id, yes } => {
            cmd_redo(
                id,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
            )
            .await
        }
        Command::Rollback { id, yes } => {
            cmd_rollback(
                id,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
            )
            .await
        }
        Command::Search { query } => cmd_search(&query, opts, cli.json).await,
        Command::Show { package } => cmd_show(&package),
        Command::Why { package, all } => cmd_why(&package, all, cli.json),
        Command::Doctor => doctor::run(cli.json),
        Command::Provides { pattern } => provides::run(&pattern),
        Command::SelfUpdate { check } => selfupdate::run(check, jobs, &repo).await,
        Command::Hold { packages } => cmd_hold(true, &packages),
        Command::Unhold { packages } => cmd_hold(false, &packages),
        Command::Held => cmd_held(),
        Command::ConfigDiff => resolve::config_diff(),
        Command::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(
                shell,
                &mut cli::Cli::command(),
                "wrapt",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Command::Man => {
            use clap::CommandFactory;
            clap_mangen::Man::new(cli::Cli::command())
                .render(&mut std::io::stdout())
                .context("failed to render man page")?;
            Ok(())
        }
    }
}

fn cmd_hold(hold: bool, packages: &[String]) -> Result<()> {
    apt::ensure_root()?;
    if packages.is_empty() {
        ui::warn("No packages given.");
        return Ok(());
    }
    let changed = apt::set_hold(hold, packages)?;
    let verb = if hold { "Held" } else { "Unheld" };
    if changed.is_empty() {
        ui::success(&format!(
            "Nothing changed ({} already in that state).",
            packages.join(", ")
        ));
    } else {
        ui::success(&format!("{verb} {} package(s).", packages.len()));
    }
    Ok(())
}

fn cmd_held() -> Result<()> {
    let held = apt::held();
    if held.is_empty() {
        ui::success("No packages are held.");
    } else {
        ui::header(&format!("Held packages ({})", held.len()));
        for pkg in &held {
            println!("   {}", pkg.bold());
        }
    }
    Ok(())
}

async fn cmd_redo(id: u64, opts: TxOpts) -> Result<()> {
    let entry = history::find(Some(id))?;
    ui::header(&format!(
        "Re-applying transaction {} ({})",
        entry.id,
        entry.command.join(" ")
    ));
    transaction(entry.command.clone(), &[], opts, "Re-applying...").await
}

async fn cmd_rollback(id: u64, opts: TxOpts) -> Result<()> {
    // Ensure the target exists (or is 0 = "before everything").
    if id != 0 && !history::load().iter().any(|e| e.id == id) {
        anyhow::bail!("no transaction {id} in history");
    }
    let entries = history::after(id);
    if entries.is_empty() {
        ui::success(&format!(
            "Already at transaction {id} — nothing to roll back."
        ));
        return Ok(());
    }
    ui::header(&format!(
        "Rolling back {} transaction(s) to the state after #{id}",
        entries.len()
    ));
    transaction(
        history::rollback_args(&entries),
        &[],
        opts,
        "Rolling back...",
    )
    .await
}

/// Options shared by every state-changing command.
#[derive(Clone, Copy)]
struct TxOpts {
    yes: bool,
    jobs: usize,
    verbose: bool,
}

async fn cmd_upgrade(full: bool, security_only: bool, opts: TxOpts) -> Result<()> {
    let op = if full { "dist-upgrade" } else { "upgrade" };
    if !security_only {
        return transaction(vec![op.to_string()], &[], opts, "Upgrading packages...").await;
    }

    // Security-only: simulate the full upgrade, then upgrade just the packages
    // whose new version comes from a security pocket.
    apt::ensure_root()?;
    let tx = apt::simulate(&[op.to_string()])?;
    let security: Vec<String> = tx
        .install
        .iter()
        .filter(|c| c.security)
        .map(|c| c.name.clone())
        .collect();
    if security.is_empty() {
        ui::success("No security updates pending.");
        return Ok(());
    }
    let mut args = vec!["install".to_string()];
    args.extend(security.clone());
    transaction(args, &security, opts, "Applying security updates...").await
}

/// Shared flow for every state-changing command: simulate, show the plan,
/// confirm, download in parallel, then let apt-get do the real work. `named`
/// is the packages the user explicitly listed (for collateral-removal checks).
async fn transaction(
    args: Vec<String>,
    named: &[String],
    opts: TxOpts,
    action: &str,
) -> Result<()> {
    apt::ensure_root()?;

    let command = args.clone();
    let tx = apt::simulate(&args).map_err(|e| resolve::explain(e, named))?;
    if tx.is_empty() {
        ui::success("Nothing to do — everything is up to date.");
        return Ok(());
    }

    let manual = apt::manual_set();
    ui::print_transaction(&tx, &manual);
    println!();

    // Safe removal: warn when packages the user installed on purpose would be
    // removed as collateral (i.e. not ones they named on this command line).
    let named_set: std::collections::HashSet<&str> = named.iter().map(String::as_str).collect();
    let collateral: Vec<&str> = tx
        .remove
        .iter()
        .filter(|c| manual.contains(&c.name) && !named_set.contains(c.name.as_str()))
        .map(|c| c.name.as_str())
        .collect();
    if !collateral.is_empty() {
        ui::warn(&format!(
            "This will also remove {} package{} you installed manually:",
            collateral.len(),
            if collateral.len() == 1 { "" } else { "s" }
        ));
        eprintln!("    {}", collateral.join(", ").yellow());
        println!();
    }

    let items = apt::print_uris(&args)?;
    let download_size: u64 = items.iter().map(|i| i.size).sum();
    if !items.is_empty() {
        println!(
            "   {} {}",
            "Total download size:".bold(),
            ui::format_size(download_size).cyan()
        );
    }
    if let Some(disk) = apt::disk_usage(&tx) {
        if disk.installed > 0 {
            println!(
                "   {} {}",
                "Total installed size:".bold(),
                ui::format_size(disk.installed).cyan()
            );
        }
        if disk.net_change != disk.installed as i64 {
            let (sign, magnitude) = if disk.net_change >= 0 {
                ("+", disk.net_change as u64)
            } else {
                ("-", disk.net_change.unsigned_abs())
            };
            println!(
                "   {} {sign}{}",
                "Net disk change:".bold(),
                ui::format_size(magnitude).cyan()
            );
        }
    }
    println!();

    // Default the prompt to "no" when manually-installed packages would be
    // removed as collateral — safer for a destructive surprise.
    let default_yes = collateral.is_empty();
    if !opts.yes && !ui::confirm("Proceed?", default_yes) {
        ui::warn("Aborted.");
        return Ok(());
    }

    let (cache_dir, custom_cache) = archives_dir();
    if !items.is_empty() {
        ui::header(&format!(
            "Downloading {} package{}...",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        ));
        download::download_all(&items, &cache_dir, opts.jobs).await?;
    }

    ui::header(action);
    let mut run_args = args;
    if custom_cache {
        run_args.splice(
            0..0,
            [
                "-o".to_string(),
                format!("Dir::Cache::archives={}", cache_dir.display()),
            ],
        );
    }
    exec::run_with_progress(&run_args, opts.verbose)?;
    if let Err(e) = history::record(&command, &tx) {
        ui::warn(&format!("could not record history: {e:#}"));
    }
    ui::success("Done.");

    // Offer to restart services still using upgraded-out libraries.
    let report = restart::check();
    if !report.is_empty() {
        println!();
        restart::offer(&report, opts.yes)?;
    }
    Ok(())
}

fn cmd_history(id: Option<u64>, json: bool) -> Result<()> {
    if json {
        let entries = history::load();
        let arr: Vec<_> = entries
            .iter()
            .filter(|e| id.is_none_or(|want| e.id == want))
            .map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "date": e.date(),
                    "command": e.command,
                    "installed": e.install.iter().map(|c| &c.name).collect::<Vec<_>>(),
                    "removed": e.remove.iter().map(|c| &c.name).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    match id {
        Some(_) => {
            let entry = history::find(id)?;
            ui::header(&format!(
                "Transaction {} — {} — wrapt {}",
                entry.id,
                entry.date(),
                entry.command.join(" ")
            ));
            ui::print_transaction(&entry.to_transaction(), &apt::manual_set());
        }
        None => {
            let entries = history::load();
            if entries.is_empty() {
                ui::warn("No transactions recorded yet.");
                return Ok(());
            }
            println!(
                "  {:>4}  {:16}  {}",
                "ID".bold(),
                "Date".bold(),
                "Action".bold()
            );
            for e in &entries {
                println!(
                    "  {:>4}  {:16}  {}",
                    e.id.to_string().cyan(),
                    e.date().dimmed(),
                    e.summary()
                );
            }
        }
    }
    Ok(())
}

async fn cmd_undo(id: Option<u64>, opts: TxOpts) -> Result<()> {
    let entry = history::find(id)?;
    ui::header(&format!(
        "Undoing transaction {} ({} — {})",
        entry.id,
        entry.command.join(" "),
        entry.date()
    ));
    transaction(entry.undo_args(), &[], opts, "Reverting...").await
}

fn cmd_update() -> Result<()> {
    apt::ensure_root()?;
    ui::header("Refreshing package lists...");
    apt::update_pretty()?;

    let upgradable = apt::simulate(&["upgrade".to_string()])
        .map(|tx| tx.install.len())
        .unwrap_or(0);
    if upgradable == 0 {
        ui::success("All packages are up to date.");
    } else {
        ui::success(&format!(
            "{} package{} can be upgraded (run {}).",
            upgradable.to_string().bold(),
            if upgradable == 1 { "" } else { "s" },
            "wrapt upgrade".cyan()
        ));
    }
    Ok(())
}

async fn cmd_search(query: &str, opts: TxOpts, json: bool) -> Result<()> {
    use std::io::IsTerminal;

    // Parse apt's package indexes directly (fast); fall back to apt-cache.
    let results = lists::search(query).or_else(|_| apt::search(query))?;

    if json {
        let arr: Vec<_> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "version": r.version,
                    "description": r.description,
                    "installed": r.installed,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }

    if results.is_empty() {
        ui::warn(&format!("No packages found matching '{query}'."));
        return Ok(());
    }

    // Number the rows so an interactive user can pick some to install.
    let interactive = std::io::stdin().is_terminal();
    let width = results.len().to_string().len();
    for (i, r) in results.iter().enumerate() {
        let version = match &r.version {
            Some(v) => format!(" {}", v.cyan()),
            None => String::new(),
        };
        let installed = if r.installed {
            format!(" {}", "[installed]".green().bold())
        } else {
            String::new()
        };
        let index = if interactive {
            format!("{:>width$}. ", (i + 1).to_string().cyan())
        } else {
            String::new()
        };
        println!("{index}{}{version}{installed}", r.name.bold());
        println!(
            "{:indent$}{}",
            "",
            r.description.dimmed(),
            indent = if interactive { width + 2 } else { 4 }
        );
    }

    if !interactive {
        return Ok(());
    }
    let picks = ui::prompt_selection(results.len());
    let chosen: Vec<String> = picks
        .into_iter()
        .filter_map(|i| results.get(i - 1))
        .filter(|r| !r.installed)
        .map(|r| r.name.clone())
        .collect();
    if chosen.is_empty() {
        return Ok(());
    }

    let mut args = vec!["install".to_string()];
    args.extend(chosen.clone());
    transaction(args, &chosen, opts, "Installing packages...").await
}

fn cmd_show(package: &str) -> Result<()> {
    let record = apt::show(package)?;
    for line in record.lines() {
        match line.split_once(": ") {
            Some((field, value)) if !line.starts_with(' ') => {
                // Render Installed-Size in human units.
                let value = if field == "Installed-Size" {
                    value
                        .trim()
                        .parse::<u64>()
                        .map(|kib| ui::format_size(kib * 1024))
                        .unwrap_or_else(|_| value.to_string())
                } else {
                    value.to_string()
                };
                println!("{}{} {value}", field.cyan().bold(), ":".cyan().bold());
            }
            _ => println!("{line}"),
        }
    }

    // Append install status and why-it's-here, which apt-cache show omits.
    if let Ok(graph) = why::Graph::build() {
        let e = graph.explain(package, false);
        if e.installed {
            println!();
            let state = if e.manual {
                "installed (manual)"
            } else {
                "installed (automatic)"
            };
            println!("{} {}", "Status:".cyan().bold(), state.green());
            if !e.manual && e.chain.len() >= 2 {
                println!(
                    "{} {} pulls it in",
                    "Reason:".cyan().bold(),
                    e.chain[0].green()
                );
            }
            if !e.required_by.is_empty() {
                println!(
                    "{} {} package(s): {}",
                    "Required by:".cyan().bold(),
                    e.required_by.len(),
                    summarize(&e.required_by)
                );
            }
        }
    }
    Ok(())
}

fn cmd_why(package: &str, all: bool, json: bool) -> Result<()> {
    let e = why::Graph::build()?.explain(package, all);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "package": e.package,
                "installed": e.installed,
                "manual": e.manual,
                "required_by": e.required_by,
                "chain": e.chain,
                "roots": e.roots,
            }))?
        );
        return Ok(());
    }

    if !e.installed {
        ui::warn(&format!("{} is not installed.", e.package));
        return Ok(());
    }

    ui::header(&e.package);
    if e.manual {
        println!(
            "   {}",
            "Installed manually — you asked for this package directly.".green()
        );
        if !e.required_by.is_empty() {
            println!(
                "   {} {}",
                "Other installed packages also depend on it:".dimmed(),
                summarize(&e.required_by).dimmed()
            );
        }
        return Ok(());
    }

    println!(
        "   {}",
        "Installed automatically, as a dependency.".yellow()
    );

    if all {
        println!();
        if e.roots.is_empty() {
            println!(
                "   {}",
                "Couldn't trace it to any manually-installed package.".yellow()
            );
        } else {
            println!(
                "   {} ({}):",
                "Pulled in by these packages you installed manually".bold(),
                e.roots.len()
            );
            for root in &e.roots {
                println!("     {}", root.green());
            }
        }
    } else if e.chain.len() >= 2 {
        // chain is [root, ..., target]; show it as root → ... → target.
        let rendered: Vec<String> = e
            .chain
            .iter()
            .enumerate()
            .map(|(i, p)| {
                if i == 0 {
                    p.green().bold().to_string()
                } else if i == e.chain.len() - 1 {
                    p.bold().to_string()
                } else {
                    p.to_string()
                }
            })
            .collect();
        println!();
        println!(
            "   {} installed it manually, which pulls in:",
            e.chain[0].green().bold()
        );
        println!("     {}", rendered.join(&format!(" {} ", "→".cyan())));
    } else if e.required_by.is_empty() {
        println!();
        println!(
            "   {}",
            "Nothing installed depends on it — it looks orphaned.".yellow()
        );
        println!(
            "   {} {}",
            "Remove unused packages with:".dimmed(),
            "wrapt autoremove".cyan()
        );
    }

    if !e.required_by.is_empty() {
        const MAX: usize = 10;
        println!();
        println!(
            "   {} ({}):",
            "Directly required by".bold(),
            e.required_by.len()
        );
        for pkg in e.required_by.iter().take(MAX) {
            println!("     {pkg}");
        }
        if e.required_by.len() > MAX {
            println!(
                "     {}",
                format!("… and {} more", e.required_by.len() - MAX).dimmed()
            );
        }
    }
    Ok(())
}

/// Join a list for inline display, truncating long ones.
fn summarize(items: &[String]) -> String {
    const MAX: usize = 6;
    if items.len() <= MAX {
        items.join(", ")
    } else {
        format!(
            "{}, … (+{} more)",
            items[..MAX].join(", "),
            items.len() - MAX
        )
    }
}
