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
- **`why`** — explains *why* a package is on your system, tracing the dependency
  chain back to something you installed on purpose.
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

This builds the release binary, installs it to `/usr/local/bin` (so `sudo wrapt`
works), and installs shell completions for bash, zsh, and fish into their system
directories — no shell-config editing required. Open a new shell afterwards for
completions to take effect.

To install elsewhere without root, set a prefix:

```bash
PREFIX=~/.local ./install.sh
```

Remove everything with `./install.sh --uninstall`.

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
| `wrapt install <pkgs…>` | Install packages |
| `wrapt remove <pkgs…> [--purge]` | Remove packages |
| `wrapt autoremove` | Remove packages that are no longer needed |
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
| `wrapt show <pkg>` | Detailed info, including why it's installed |
| `wrapt why <pkg> [--all]` | Explain why a package is installed |
| `wrapt provides <file>` | Find which package provides a file/command |

### Maintenance

| Command | Description |
| --- | --- |
| `wrapt doctor` | Check the system for common package problems |
| `wrapt config-diff` | Review config files left by upgrades (`*.dpkg-dist`) |
| `wrapt completions <shell>` | Print a shell completion script |
| `wrapt self-update` | Update wrapt itself to the latest release (`--check` to only look) |

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
- `--json` — machine-readable output (`search`, `why`, `history`, `doctor`)

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
