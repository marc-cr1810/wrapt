mod apt;
mod changelog;
mod cli;
mod cnf;
mod config;
mod doctor;
mod download;
mod exec;
mod fetch;
mod history;
mod kernels;
mod listpkgs;
mod lists;
mod provides;
mod repo;
mod resolve;
mod restart;
mod selfupdate;
mod ui;
mod why;
mod whynot;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use ui::Paint;

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

/// Commands that actually emit JSON. Anything else is told so plainly rather
/// than accepting `--json` and printing its normal output.
fn supports_json(command: &Command) -> bool {
    matches!(
        command,
        Command::Search { .. }
            | Command::List { .. }
            | Command::Why { .. }
            | Command::History { .. }
            | Command::Doctor
            | Command::Held
            | Command::Provides { .. }
    )
}

async fn run(cli: cli::Cli) -> Result<()> {
    let cfg = config::Config::load()?;
    cfg.apply_color();
    restart::set_policy(restart::Policy {
        mode: restart::Mode::from_config(cfg.restart.as_deref()),
        never_restart: cfg.never_restart.clone().unwrap_or_default(),
    });

    if cli.json && !supports_json(&cli.command) {
        anyhow::bail!(
            "--json is not supported by this command — it works with \
             search, list, why, history, doctor, held, and provides"
        );
    }

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
        dry_run: cli.dry_run,
        history_limit: cfg.history_limit.unwrap_or(1000),
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
            cmd_install(
                packages,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
            )
            .await
        }
        Command::Reinstall { packages, yes } => {
            let mut args = vec!["install".to_string(), "--reinstall".to_string()];
            args.extend(packages.clone());
            transaction(
                args,
                &packages,
                TxOpts {
                    yes: yes || assume_yes,
                    ..opts
                },
                "Reinstalling packages...",
            )
            .await
        }
        Command::Download { packages } => cmd_download(&packages, opts.jobs).await,
        Command::List {
            upgradable,
            manual,
            pattern,
        } => listpkgs::run(upgradable, manual, pattern.as_deref(), cli.json),
        Command::Plan { packages } => cmd_plan(&packages),
        Command::Clean { all, kernels } => {
            if kernels {
                cmd_clean_kernels(opts, cfg.keep_kernels.unwrap_or(2)).await
            } else {
                cmd_clean(all)
            }
        }
        Command::Fetch {
            apply,
            count,
            country,
        } => fetch::run(apply, count, country.or(cfg.mirror_country.clone())).await,
        Command::CommandNotFound { command, init } => {
            if let Some(shell) = init {
                cnf::print_hook(shell)
            } else {
                let cmd = command.ok_or_else(|| {
                    anyhow::anyhow!("a command name (or --init <shell>) is required")
                })?;
                // The command genuinely wasn't found unless it's on PATH; mirror
                // the shell's own 127 exit so callers behave correctly.
                if !cnf::resolve(&cmd) {
                    std::process::exit(127);
                }
                Ok(())
            }
        }
        Command::WhyNot { package } => whynot::run(&package),
        Command::Changelog { package } => changelog::run(&package),
        Command::Repo { action } => repo::run(action),
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
        Command::Provides { pattern } => provides::run(&pattern, cli.json),
        Command::SelfUpdate { check } => selfupdate::run(check, jobs, &repo).await,
        Command::Hold { packages } => cmd_hold(true, &packages),
        Command::Unhold { packages } => cmd_hold(false, &packages),
        Command::Held => cmd_held(cli.json),
        Command::Config { init, path } => cmd_config(init, path),
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
        // Report what apt-mark actually changed, not what was asked for —
        // packages already in the target state aren't touched.
        ui::success(&format!(
            "{verb} {} package{}.",
            changed.len(),
            if changed.len() == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

fn cmd_held(json: bool) -> Result<()> {
    let held = apt::held();
    if json {
        println!("{}", serde_json::to_string_pretty(&held)?);
        return Ok(());
    }
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
        entry.what()
    ));
    transaction_as(
        entry.command.clone(),
        &[],
        opts,
        "Re-applying...",
        Some(format!("redo #{}", entry.id)),
    )
    .await
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
    transaction_as(
        history::rollback_args(&entries),
        &[],
        opts,
        "Rolling back...",
        Some(format!("rollback to #{id}")),
    )
    .await
}

/// Options shared by every state-changing command.
#[derive(Clone, Copy)]
struct TxOpts {
    yes: bool,
    jobs: usize,
    verbose: bool,
    /// Show the plan and stop, without downloading or changing anything.
    dry_run: bool,
    /// How many transactions the history keeps.
    history_limit: usize,
}

async fn cmd_upgrade(full: bool, security_only: bool, opts: TxOpts) -> Result<()> {
    let op = if full { "dist-upgrade" } else { "upgrade" };
    if !security_only {
        return transaction(vec![op.to_string()], &[], opts, "Upgrading packages...").await;
    }

    // Security-only: simulate the full upgrade, then upgrade just the packages
    // whose new version comes from a security pocket. Like every other dry run,
    // the preview needs no privileges — only the real transaction does.
    if !opts.dry_run {
        apt::ensure_root()?;
    }
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
    transaction_as(args, named, opts, action, None).await
}

/// As [`transaction`], but records the run in history under `label` instead of
/// its raw apt arguments — used by undo/redo/rollback, whose arguments are a
/// long list of pinned versions that says nothing about where they came from.
async fn transaction_as(
    args: Vec<String>,
    named: &[String],
    opts: TxOpts,
    action: &str,
    label: Option<String>,
) -> Result<()> {
    // A dry run only previews; it needs no root and touches nothing.
    if !opts.dry_run {
        apt::ensure_root()?;
    }

    let command = args.clone();
    let tx = apt::simulate(&args).map_err(|e| resolve::explain(e, named))?;
    if tx.is_empty() {
        ui::success("Nothing to do — everything is up to date.");
        return Ok(());
    }

    let (default_yes, items) = print_plan(&tx, &args, named)?;

    if opts.dry_run {
        ui::success("Dry run — nothing was changed.");
        return Ok(());
    }

    // Default the prompt to "no" when manually-installed packages would be
    // removed as collateral — safer for a destructive surprise.
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
    if let Err(e) = history::record(&command, label, &tx, opts.history_limit) {
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

/// `wrapt config`: show the effective settings and where each came from, print
/// the paths, or write a starter file.
fn cmd_config(init: bool, path_only: bool) -> Result<()> {
    let user_path = config::user_config_path();
    let system_path = config::machine_config_path();

    if init {
        let Some(target) = user_path else {
            anyhow::bail!("cannot work out where your config should go (no HOME)");
        };
        config::write_template(&target)?;
        ui::success(&format!("Wrote a starter config to {}", target.display()));
        println!(
            "   {}",
            "Every setting is commented out — uncomment what you want to change.".dimmed()
        );
        return Ok(());
    }

    if path_only {
        println!("system  {}", system_path.display());
        match &user_path {
            Some(p) => println!("user    {}", p.display()),
            None => println!("user    (no HOME — none read)"),
        }
        return Ok(());
    }

    let (system, user) = config::Config::layers()?;
    let merged = config::Config::load()?;

    ui::header("Configuration files:");
    println!(
        "   {:7} {} {}",
        "system",
        system_path.display(),
        exists_note(&system_path)
    );
    match &user_path {
        Some(p) => println!("   {:7} {} {}", "user", p.display(), exists_note(p)),
        None => println!("   {:7} {}", "user", "(no HOME — none read)".dimmed()),
    }
    println!();

    ui::header("Effective settings:");
    for (name, value, source) in config::describe(&merged, &system, &user) {
        println!(
            "   {:<15} {:<28} {}",
            name,
            value,
            format!("({})", source.label()).dimmed()
        );
    }
    Ok(())
}

fn exists_note(path: &std::path::Path) -> String {
    if path.exists() {
        String::new()
    } else {
        "(not present)".dimmed().to_string()
    }
}

/// Print a transaction's plan: the package changes, any collateral-removal
/// warning, and the download/disk sizes. Returns `(safe_default_yes, items)`
/// where `safe_default_yes` is false when manually-installed packages would be
/// removed as collateral, and `items` is what apt would download.
fn print_plan(
    tx: &apt::Transaction,
    args: &[String],
    named: &[String],
) -> Result<(bool, Vec<download::DownloadItem>)> {
    let manual = apt::manual_set();
    ui::print_transaction(tx, &manual);
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

    let items = apt::print_uris(args)?;
    let download_size: u64 = items.iter().map(|i| i.size).sum();
    if !items.is_empty() {
        println!(
            "   {} {}",
            "Total download size:".bold(),
            ui::format_size(download_size).cyan()
        );
    }
    if let Some(disk) = apt::disk_usage(tx) {
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

    Ok((collateral.is_empty(), items))
}

/// `wrapt install`, extended so a spec may be a plain package name, a path to a
/// local `.deb`, or an `http(s)://…deb` URL (fetched first, then installed).
async fn cmd_install(packages: Vec<String>, opts: TxOpts) -> Result<()> {
    let mut args = vec!["install".to_string()];
    let mut named: Vec<String> = Vec::new();
    let mut url_items: Vec<download::DownloadItem> = Vec::new();

    // Created up front (once) when any spec is a URL, so the download target is
    // a directory we know we own.
    let is_url = |s: &str| s.starts_with("http://") || s.starts_with("https://");
    let tmp: Option<PathBuf> = packages
        .iter()
        .any(|s| is_url(s))
        .then(private_temp_dir)
        .transpose()?;

    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for spec in &packages {
        if is_url(spec) {
            let dir = tmp
                .as_ref()
                .expect("temp dir created when a URL is present");
            let filename = unique_name(url_filename(spec), &mut used);
            args.push(dir.join(&filename).to_string_lossy().into_owned());
            url_items.push(download::DownloadItem {
                url: spec.clone(),
                filename,
                size: 0,
                hash: None,
            });
        } else if is_local_deb(spec) {
            // apt needs a path it recognises as a file (absolute is unambiguous).
            let abs = std::fs::canonicalize(spec).with_context(|| format!("cannot read {spec}"))?;
            args.push(abs.to_string_lossy().into_owned());
        } else {
            args.push(spec.clone());
            named.push(spec.clone());
        }
    }

    // Fetch any remote .debs before handing off to apt.
    if let Some(dir) = &tmp {
        ui::header(&format!(
            "Fetching {} remote package{}...",
            url_items.len(),
            if url_items.len() == 1 { "" } else { "s" }
        ));
        download::download_all(&url_items, dir, opts.jobs).await?;
    }

    let result = transaction(args, &named, opts, "Installing packages...").await;

    if let Some(dir) = &tmp {
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

/// True if `spec` points at an existing local `.deb` file.
fn is_local_deb(spec: &str) -> bool {
    spec.ends_with(".deb") && std::path::Path::new(spec).is_file()
}

/// A private directory for remote downloads: mode 0700 and created
/// exclusively, so it can never land on a path someone else pre-created.
/// `wrapt install` runs as root, so following an unprivileged user's symlink
/// here would hand them control of where root writes.
fn private_temp_dir() -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let base = std::env::temp_dir();
    for attempt in 0..8u32 {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos())
            ^ attempt;
        let dir = base.join(format!("wrapt-install-{}-{nonce:08x}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            // Someone got there first — try a different name rather than reuse it.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => anyhow::bail!("cannot create {}: {e}", dir.display()),
        }
    }
    anyhow::bail!(
        "could not create a private temporary directory in {}",
        base.display()
    )
}

/// The file name to save a package URL under: its last path segment, with any
/// query or fragment dropped. Anything that isn't a plain file name falls back
/// to a fixed name, so a crafted URL can't steer the path we write to.
fn url_filename(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("");
    if name.is_empty() || name == "." || name == ".." || name.contains('\0') {
        return "package.deb".to_string();
    }
    name.to_string()
}

/// Keep file names distinct, so two URLs ending in the same name don't collapse
/// onto one file (which would silently install the same .deb twice).
fn unique_name(name: String, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(name.clone()) {
        return name;
    }
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name.as_str(), ""));
    for n in 1.. {
        let candidate = if ext.is_empty() {
            format!("{stem}-{n}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!()
}

/// `wrapt plan`: show what installing `packages` would do, then stop.
fn cmd_plan(packages: &[String]) -> Result<()> {
    let mut args = vec!["install".to_string()];
    args.extend(packages.iter().cloned());
    let tx = apt::simulate(&args).map_err(|e| resolve::explain(e, packages))?;
    if tx.is_empty() {
        ui::success("Nothing to do — those packages are already installed and up to date.");
        return Ok(());
    }
    print_plan(&tx, &args, packages)?;
    ui::success("Preview only — run `wrapt install …` to apply.");
    Ok(())
}

/// `wrapt download`: fetch the named packages' .debs into the current directory.
async fn cmd_download(packages: &[String], jobs: usize) -> Result<()> {
    let items = apt::download_uris(packages)?;
    let total: u64 = items.iter().map(|i| i.size).sum();
    let dest = std::env::current_dir().context("cannot determine the current directory")?;

    ui::header(&format!(
        "Downloading {} package{}...",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    ));
    download::download_all(&items, &dest, jobs).await?;
    // download_all creates a `partial/` working dir; remove it if now empty.
    let _ = std::fs::remove_dir(dest.join("partial"));

    ui::success(&format!(
        "Downloaded {} file{} ({}) to {}.",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        ui::format_size(total),
        dest.display()
    ));
    Ok(())
}

/// `wrapt clean`: clear apt's package cache and report the space reclaimed.
fn cmd_clean(all: bool) -> Result<()> {
    apt::ensure_root()?;
    let (dir, custom) = archives_dir();
    let before = dir_size(&dir);
    apt::clean(all, custom.then_some(dir.as_path()))?;
    let freed = before.saturating_sub(dir_size(&dir));
    if freed == 0 {
        ui::success("Nothing to clean — the package cache is already empty.");
    } else {
        ui::success(&format!("Freed {}.", ui::format_size(freed).cyan()));
    }
    Ok(())
}

/// `wrapt clean --kernels`: purge old kernels, keeping the running one and the
/// newest installed. The removal runs through the normal transaction flow, so
/// apt shows the plan and asks before anything is deleted.
async fn cmd_clean_kernels(opts: TxOpts, keep: usize) -> Result<()> {
    let old = kernels::old_kernel_packages(keep);
    if old.is_empty() {
        ui::success(&format!(
            "No old kernels to remove — the newest {keep} (and the running one) are all that's installed."
        ));
        return Ok(());
    }
    let mut args = vec!["purge".to_string()];
    args.extend(old.iter().cloned());
    transaction(args, &old, opts, "Removing old kernels...").await
}

/// Total size in bytes of the files under `dir` (one level of subdirs deep,
/// which covers apt's `archives/` and `archives/partial/`).
fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if let Ok(sub) = std::fs::read_dir(entry.path()) {
                total += sub
                    .flatten()
                    .filter_map(|e| e.metadata().ok())
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .sum::<u64>();
            }
        } else {
            total += meta.len();
        }
    }
    total
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
                    "label": e.label,
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
                entry.what()
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
        entry.what(),
        entry.date()
    ));
    transaction_as(
        entry.undo_args(),
        &[],
        opts,
        "Reverting...",
        Some(format!("undo #{}", entry.id)),
    )
    .await
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

    // Number the rows so an interactive user can pick some to install. Both
    // ends must be a terminal: with stdout piped the numbers are noise and the
    // prompt would disappear into the pipe.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn url_filename_takes_a_plain_basename() {
        assert_eq!(
            url_filename("https://x.dev/htop_3.4.1_amd64.deb"),
            "htop_3.4.1_amd64.deb"
        );
        // Query strings and fragments are not part of the name.
        assert_eq!(
            url_filename("https://x.dev/a/htop.deb?token=abc"),
            "htop.deb"
        );
        assert_eq!(url_filename("https://x.dev/htop.deb#sig"), "htop.deb");
        // Anything that isn't a plain file name falls back.
        assert_eq!(url_filename("https://x.dev/pkgs/"), "package.deb");
        assert_eq!(url_filename("https://x.dev/a/.."), "package.deb");
        assert_eq!(url_filename("https://x.dev/a/."), "package.deb");
    }

    #[test]
    fn unique_name_disambiguates_collisions() {
        let mut used = HashSet::new();
        assert_eq!(unique_name("htop.deb".into(), &mut used), "htop.deb");
        assert_eq!(unique_name("htop.deb".into(), &mut used), "htop-1.deb");
        assert_eq!(unique_name("htop.deb".into(), &mut used), "htop-2.deb");
        // Extensionless names still get a suffix.
        assert_eq!(unique_name("pkg".into(), &mut used), "pkg");
        assert_eq!(unique_name("pkg".into(), &mut used), "pkg-1");
    }

    #[test]
    fn private_temp_dir_is_exclusive_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let a = private_temp_dir().unwrap();
        let b = private_temp_dir().unwrap();
        assert_ne!(a, b, "each call must get its own directory");
        let mode = std::fs::metadata(&a).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "must not be group/world accessible");
        std::fs::remove_dir(&a).unwrap();
        std::fs::remove_dir(&b).unwrap();
    }
}
