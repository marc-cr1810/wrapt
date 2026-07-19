# wrapt

A faster, prettier front-end for `apt`.

wrapt wraps `apt`/`dpkg` rather than reimplementing them, so it can never corrupt
your package database — but it fixes the things apt does poorly: slow sequential
downloads, noisy output, no undo, and cryptic errors. Think of it as apt with
pacman's speed and a friendlier face.

```
:: Installing (1)
   htop  3.4.1-5build2

   Total download size:  177.0 KiB
   Total installed size: 440.0 KiB

:: Downloading 1 package...
  htop      177.0 KiB   4.2 MiB/s [━━━━━━━━━━━━━━] 100%
:: Installing packages...
  ✓ htop
✓ Done.
```

## Features

- **Parallel downloads** — fetches packages several at a time (pacman-style) with
  live per-package and total progress bars, verifying checksums as they stream.
- **Clean output** — color-coded transaction plans; apt's and dpkg's chatter is
  hidden behind a single progress bar (the raw log is shown only on failure).
- **Transaction history** — every change is recorded; `undo`, `redo`, and
  `rollback` to any past point.
- **Fast search** — parses apt's package indexes directly (~2× faster than
  `apt-cache search`), with an interactive pick-to-install prompt.
- **`why` / `why-not`** — explains *why* a package is on your system (tracing the
  dependency chain back to something you installed on purpose), or *why not* — the
  plain-English reason a package can't be installed.
- **Preview anything** — `--dry-run` on any command, or `wrapt plan <pkgs>`, shows
  the full transaction (sizes and all) without touching the system.
- **Local & remote `.debs`** — `wrapt install ./foo.deb` or an `https://…deb` URL,
  with dependencies resolved by apt as usual.
- **Source management** — `wrapt repo` lists, adds, and removes apt sources / PPAs.
- **Fastest mirrors** — `wrapt fetch` benchmarks the Ubuntu mirrors near you and
  (with `--apply`) switches your archive sources to the fastest, à la `nala fetch`.
- **Missing-command hints** — an optional shell hook turns "command not found"
  into "the program 'foo' is not installed — `sudo wrapt install foo`".
- **Kernel cleanup** — `wrapt clean --kernels` purges old kernels (keeping the
  running one and the newest) to free up `/boot`.
- **Safe removal** — warns (and defaults to "no") when a removal would take
  manually-installed packages with it.
- **Security-aware** — highlights which upgrades are security fixes;
  `upgrade --security-only` applies just those.
- **Restart detection** — after upgrades, finds services still using outdated
  libraries and offers to restart them.
- **`doctor`** — a health check for broken packages, held packages, orphans,
  low `/boot` space, and duplicate sources.
- **Helpful errors** — decodes apt's cryptic resolver failures into plain
  English, with did-you-mean suggestions for typos.
- **Scriptable** — `--json` output on query commands, plus shell completions.

## Requirements

- A Debian/Ubuntu-based system (`apt`, `dpkg`, `apt-cache`, `apt-mark`)
- Rust toolchain to build (`cargo`)

## Installation

```bash
git clone <your-repo-url> wrapt
cd wrapt
./install.sh          # or: make install
```

This builds a `.deb` and installs it with `apt`, so wrapt becomes a normal
dpkg-managed package at `/usr/bin/wrapt` — the same path `wrapt self-update`
uses. Keeping a single, canonical copy means updates never leave an older binary
behind to shadow the new one. Completions (bash, zsh, fish) and the man page come
from the package; open a new shell for completions to take effect. If an earlier
copy-method install left a `wrapt` in `/usr/local/bin`, the installer removes it
so it can't shadow the packaged copy.

For a rootless install (or a system without dpkg/apt), install by copying files
instead:

```bash
./install.sh --copy          # copy into /usr/local
PREFIX=~/.local ./install.sh # copy somewhere else, no root needed
```

Remove everything — package or copied files — with `./install.sh --uninstall`.

### Prebuilt `.deb` packages

Each [release](https://github.com/marc-cr1810/wrapt/releases) ships a `.deb`
built for every supported Ubuntu version:

| File | Built for |
| --- | --- |
| `wrapt_<ver>_ubuntu24.04_amd64.deb` | Ubuntu 24.04 (and newer) |
| `wrapt_<ver>_ubuntu25.04_amd64.deb` | Ubuntu 25.04 |
| `wrapt_<ver>_ubuntu26.04_amd64.deb` | Ubuntu 26.04 |

```bash
sudo apt install ./wrapt_<ver>_ubuntu26.04_amd64.deb
```

Each package records its real library floor (`libc6`, `libgcc-s1`) in its
dependencies, so `apt` refuses a package built for a newer system rather than
letting it crash at runtime. If you're unsure which to pick, the `ubuntu24.04`
build has the widest compatibility. Better still, once wrapt is installed, let
it keep itself current — see `wrapt self-update` below, which automatically
downloads the build matching your system.

## Usage

Commands that change the system (`install`, `remove`, `upgrade`, …) require root:

```bash
sudo wrapt install htop
sudo wrapt upgrade --security-only
```

### Managing packages

| Command | Description |
| --- | --- |
| `wrapt update` | Refresh package lists |
| `wrapt upgrade [--full] [--security-only]` | Upgrade installed packages |
| `wrapt install <pkgs\|./file.deb\|url…>` | Install packages, local `.deb`s, or remote `.deb` URLs |
| `wrapt reinstall <pkgs…>` | Reinstall packages, fetching them again |
| `wrapt remove <pkgs…> [--purge]` | Remove packages |
| `wrapt autoremove` | Remove packages that are no longer needed |
| `wrapt download <pkgs…>` | Download `.deb`s to the current directory (no install) |
| `wrapt clean [--all] [--kernels]` | Clear the download cache, or purge old kernels (`--kernels`) |
| `wrapt hold <pkgs…>` / `unhold` / `held` | Pin packages at their current version |

### History

| Command | Description |
| --- | --- |
| `wrapt history [id]` | List transactions, or show one in detail |
| `wrapt undo [id]` | Undo a transaction (most recent by default) |
| `wrapt redo <id>` | Re-apply a past transaction |
| `wrapt rollback <id>` | Undo everything after transaction `id` |

### Discovery

| Command | Description |
| --- | --- |
| `wrapt search <query>` | Search for packages (interactive install) |
| `wrapt list [--upgradable\|--manual] [pattern]` | List installed / upgradable / manual packages |
| `wrapt show <pkg>` | Detailed info, including why it's installed |
| `wrapt why <pkg> [--all]` | Explain why a package is installed |
| `wrapt why-not <pkg>` | Explain why a package can't be installed |
| `wrapt plan <pkgs…>` | Preview what installing packages would do |
| `wrapt changelog <pkg>` | Show a changelog, highlighting security fixes |
| `wrapt provides <file>` | Find which package provides a file/command |

### Maintenance

| Command | Description |
| --- | --- |
| `wrapt doctor` | Check the system for common package problems |
| `wrapt fetch [--apply] [--country CC]` | Benchmark mirrors; `--apply` switches to the fastest |
| `wrapt repo list` / `add <repo>` / `remove <repo>` | Manage apt sources and PPAs |
| `wrapt config-diff` | Review config files left by upgrades (`*.dpkg-dist`) |
| `wrapt completions <shell>` | Print a shell completion script |
| `wrapt self-update` | Update wrapt itself to the latest release (`--check` to only look) |

### Suggesting packages for unknown commands

wrapt can hook your shell so that typing a command you don't have prints a hint
on how to install it. Add the hook for your shell:

```bash
# bash — in ~/.bashrc
eval "$(wrapt command-not-found --init bash)"
# zsh — in ~/.zshrc
eval "$(wrapt command-not-found --init zsh)"
# fish — in ~/.config/fish/config.fish
wrapt command-not-found --init fish | source
```

Then a missing command suggests a package:

```
$ cowsay
! the program cowsay is not installed. Install it with:
  sudo wrapt install cowsay
```

Suggestions across *all* packages (not just same-named ones) need `apt-file`:
`wrapt install apt-file && sudo apt-file update`.

### Keeping wrapt up to date

wrapt is distributed as a `.deb` on [GitHub Releases](https://github.com/marc-cr1810/wrapt/releases),
not through an apt repository, so `apt upgrade` won't see new versions. Instead:

```sh
wrapt self-update --check   # report whether a newer release exists (no root needed)
sudo wrapt self-update      # download and install the latest .deb
```

It queries the GitHub Releases API, compares the latest tag with the running
version, and installs the `.deb` matching your architecture. The repository it
pulls from can be overridden with the `WRAPT_REPO=owner/name` environment
variable or a `repo = "owner/name"` line in the config file.

### Global flags

- `-j, --parallel <N>` — number of parallel downloads (default 5)
- `-v, --verbose` — show apt's raw output instead of the clean display
- `-n, --dry-run` — show what a command would do, then stop without changing anything
- `--json` — machine-readable output (`search`, `list`, `why`, `history`, `doctor`)

## How it works

For any state-changing command, wrapt:

1. Simulates the transaction with `apt-get -s` and shows a clean plan.
2. Gets the download URLs with `apt-get --print-uris` and fetches the `.deb`s
   itself, in parallel, into apt's archive cache.
3. Hands off to `apt-get`, which finds everything pre-downloaded and installs it.

Because the real work is still done by apt and dpkg, the package database stays
consistent and nothing about your system's package management is bypassed.

## License

TBD.
