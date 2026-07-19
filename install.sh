#!/usr/bin/env bash
#
# Installer for wrapt.
#
# By default this builds a .deb and installs it with apt, so wrapt becomes a
# normal dpkg-managed package at /usr/bin/wrapt — the very path that
# `wrapt self-update` installs to. That means there is exactly one copy of
# wrapt, and updates never leave an older, shadowing binary behind.
#
# For a rootless install (or a system without dpkg/apt), pass --copy or set a
# custom PREFIX to install by copying files instead.
#
# Usage:
#   ./install.sh              # build + install the .deb (system-wide)
#   ./install.sh --uninstall  # remove wrapt (package or copied files)
#   ./install.sh --copy       # copy files instead of building a .deb
#   PREFIX=~/.local ./install.sh   # rootless copy install (implies --copy)
#
# Overridable via environment (copy method only):
#   PREFIX                 install prefix for the binary   (default /usr/local)
#   BASH_COMPLETION_DIR    bash completion dir  (default /usr/share/bash-completion/completions)
#   ZSH_COMPLETION_DIR     zsh  completion dir  (default /usr/local/share/zsh/site-functions)
#   FISH_COMPLETION_DIR    fish completion dir  (default /usr/share/fish/vendor_completions.d)

set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
BIN_DIR="$PREFIX/bin"
BASH_COMPLETION_DIR="${BASH_COMPLETION_DIR:-/usr/share/bash-completion/completions}"
ZSH_COMPLETION_DIR="${ZSH_COMPLETION_DIR:-/usr/local/share/zsh/site-functions}"
FISH_COMPLETION_DIR="${FISH_COMPLETION_DIR:-/usr/share/fish/vendor_completions.d}"
MAN_DIR="${MAN_DIR:-$PREFIX/share/man/man1}"

BIN_PATH="$BIN_DIR/wrapt"
BASH_COMP="$BASH_COMPLETION_DIR/wrapt"
ZSH_COMP="$ZSH_COMPLETION_DIR/_wrapt"
FISH_COMP="$FISH_COMPLETION_DIR/wrapt.fish"
MAN_PATH="$MAN_DIR/wrapt.1.gz"

# Files a previous copy-method install to /usr/local could have left behind.
# The binary is the important one: /usr/local/bin is ahead of /usr/bin on the
# default PATH, so a leftover there shadows the packaged copy.
STALE_PATHS=(
    /usr/local/bin/wrapt
    /usr/local/share/zsh/site-functions/_wrapt
    /usr/local/share/man/man1/wrapt.1.gz
)

# --- argument parsing --------------------------------------------------------
UNINSTALL=0
FORCE_COPY=0
for arg in "$@"; do
    case "$arg" in
        --uninstall) UNINSTALL=1 ;;
        --copy)      FORCE_COPY=1 ;;
        *) printf 'unknown option: %s\n' "$arg" >&2; exit 1 ;;
    esac
done

# --- pretty output -----------------------------------------------------------
if [ -t 1 ]; then
    BOLD=$'\033[1m'; GREEN=$'\033[32m'; CYAN=$'\033[36m'; YELLOW=$'\033[33m'; DIM=$'\033[2m'; RESET=$'\033[0m'
else
    BOLD=''; GREEN=''; CYAN=''; YELLOW=''; DIM=''; RESET=''
fi
step() { printf '%s::%s %s\n' "$CYAN$BOLD" "$RESET$BOLD" "$1$RESET"; }
ok()   { printf '  %s✓%s %s\n' "$GREEN$BOLD" "$RESET" "$1"; }
warn() { printf '  %s!%s %s\n' "$YELLOW$BOLD" "$RESET" "$1"; }
err()  { printf '%serror:%s %s\n' "$YELLOW$BOLD" "$RESET" "$1" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# --- privilege escalation only where the target isn't writable ---------------
# Returns the command prefix ("sudo" or "") needed to write to a given dir.
sudo_for() {
    local dir="$1"
    while [ ! -e "$dir" ]; do dir="$(dirname "$dir")"; done
    if [ -w "$dir" ]; then
        printf ''
    elif command -v sudo >/dev/null 2>&1; then
        printf 'sudo'
    else
        printf ''
    fi
}

install_file() {  # install_file <src> <dest> <mode>
    local src="$1" dest="$2" mode="$3"
    local dir; dir="$(dirname "$dest")"
    local SUDO; SUDO="$(sudo_for "$dir")"
    $SUDO install -d "$dir"
    $SUDO install -m "$mode" "$src" "$dest"
}

remove_file() {  # remove_file <path>
    local path="$1"
    [ -e "$path" ] || return 0
    local SUDO; SUDO="$(sudo_for "$(dirname "$path")")"
    $SUDO rm -f "$path"
    ok "removed $path"
}

# Remove leftovers from an old copy-method install that would shadow the
# packaged binary. Safe to call whenever we install/uninstall the package.
clear_stale_copies() {
    local p
    for p in "${STALE_PATHS[@]}"; do
        remove_file "$p"
    done
}

# Decide which method to use. A custom PREFIX, an explicit --copy, or a system
# without dpkg/apt all mean "copy files"; otherwise build and install the .deb.
METHOD="deb"
if [ "$FORCE_COPY" = 1 ] || [ "$PREFIX" != "/usr/local" ] \
    || ! command -v dpkg-deb >/dev/null 2>&1 \
    || ! command -v apt-get >/dev/null 2>&1; then
    METHOD="copy"
fi

# --- uninstall ---------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
    step "Uninstalling wrapt"
    if command -v dpkg-query >/dev/null 2>&1 && dpkg-query -W -f='${Status}' wrapt 2>/dev/null | grep -q "install ok installed"; then
        SUDO="$(sudo_for /usr/bin)"
        $SUDO apt-get remove -y wrapt
        ok "removed the wrapt package"
    fi
    # Also clean up anything a copy-method install may have placed.
    remove_file "$BIN_PATH"
    remove_file "$BASH_COMP"
    remove_file "$ZSH_COMP"
    remove_file "$FISH_COMP"
    remove_file "$MAN_PATH"
    clear_stale_copies
    printf '\n%s✓ wrapt removed.%s\n' "$GREEN$BOLD" "$RESET"
    exit 0
fi

# --- build prerequisites -----------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    err "cargo not found. Install Rust from https://rustup.rs first."
    exit 1
fi

# --- .deb method (default) ---------------------------------------------------
if [ "$METHOD" = "deb" ]; then
    step "Building wrapt .deb"
    scripts/build-deb.sh >/dev/null
    VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
    ARCH="$(dpkg --print-architecture)"
    DEB="$SCRIPT_DIR/target/deb/wrapt_${VERSION}_${ARCH}.deb"
    if [ ! -f "$DEB" ]; then
        err "expected package not found at $DEB"
        exit 1
    fi
    ok "built $DEB"

    step "Installing with apt"
    if [ -n "$(sudo_for /usr/bin)" ]; then
        warn "installing system-wide — you may be prompted for your password"
    fi
    SUDO="$(sudo_for /usr/bin)"
    # A leading ./ (absolute path here) tells apt to treat this as a local file
    # and resolve its dependencies, rather than a package name to look up.
    $SUDO apt-get install -y "$DEB"
    ok "installed wrapt $VERSION to /usr/bin/wrapt"

    # Retire any older copy-method install so it can't shadow the package.
    clear_stale_copies

    printf '\n%s✓ wrapt installed.%s\n' "$GREEN$BOLD" "$RESET"
    RESOLVED="$(command -v wrapt || true)"
    if [ "$RESOLVED" = "/usr/bin/wrapt" ] || [ "$RESOLVED" = "/bin/wrapt" ]; then
        ok "wrapt is on your PATH ($RESOLVED)"
    elif [ -n "$RESOLVED" ]; then
        warn "PATH resolves wrapt to $RESOLVED, not the packaged /usr/bin/wrapt — remove that copy"
    fi
    printf '  %sKeep it current with:%s %swrapt self-update%s   %s(open a new shell for completions)%s\n' \
        "$BOLD" "$RESET" "$CYAN" "$RESET" "$DIM" "$RESET"
    exit 0
fi

# --- copy method (rootless / custom PREFIX / no dpkg) -------------------------
step "Building wrapt (release)"
cargo build --release
BUILT="$SCRIPT_DIR/target/release/wrapt"
ok "built $BUILT"

step "Installing binary"
if [ -n "$(sudo_for "$BIN_DIR")" ]; then
    warn "$BIN_DIR needs root — you may be prompted for your password"
fi
install_file "$BUILT" "$BIN_PATH" 0755
ok "installed $BIN_PATH"

# Completions, generated straight from the freshly built binary so they never
# drift from the actual CLI. Each shell's file goes where that shell looks.
step "Installing shell completions"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

"$BUILT" completions bash > "$TMP/wrapt.bash"
install_file "$TMP/wrapt.bash" "$BASH_COMP" 0644
ok "bash  → $BASH_COMP"

"$BUILT" completions zsh > "$TMP/_wrapt"
install_file "$TMP/_wrapt" "$ZSH_COMP" 0644
ok "zsh   → $ZSH_COMP ${DIM}(on the default fpath — no .zshrc edit needed)${RESET}"

"$BUILT" completions fish > "$TMP/wrapt.fish"
install_file "$TMP/wrapt.fish" "$FISH_COMP" 0644
ok "fish  → $FISH_COMP"

step "Installing man page"
"$BUILT" man | gzip -9n > "$TMP/wrapt.1.gz"
install_file "$TMP/wrapt.1.gz" "$MAN_PATH" 0644
ok "man   → $MAN_PATH"

printf '\n%s✓ wrapt installed.%s\n' "$GREEN$BOLD" "$RESET"
if command -v wrapt >/dev/null 2>&1 && [ "$(command -v wrapt)" = "$BIN_PATH" ]; then
    ok "wrapt is on your PATH"
else
    warn "$BIN_DIR is not on your PATH — add it, or restart your shell"
fi
printf '  %sTry:%s %ssudo wrapt upgrade%s   %s(open a new shell for completions)%s\n' \
    "$BOLD" "$RESET" "$CYAN" "$RESET" "$DIM" "$RESET"
