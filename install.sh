#!/usr/bin/env bash
#
# Installer for wrapt. Builds the release binary, installs it to a system path
# that sudo can see, and drops shell completions into standard locations so
# they work with no shell-config editing.
#
# Usage:
#   ./install.sh              # build + install to /usr/local
#   ./install.sh --uninstall  # remove everything this script installs
#   PREFIX=~/.local ./install.sh   # install somewhere else (no sudo needed)
#
# Overridable via environment:
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

BIN_PATH="$BIN_DIR/wrapt"
BASH_COMP="$BASH_COMPLETION_DIR/wrapt"
ZSH_COMP="$ZSH_COMPLETION_DIR/_wrapt"
FISH_COMP="$FISH_COMPLETION_DIR/wrapt.fish"

# --- pretty output -----------------------------------------------------------
if [ -t 1 ]; then
    BOLD=$'\033[1m'; GREEN=$'\033[32m'; CYAN=$'\033[36m'; YELLOW=$'\033[33m'; DIM=$'\033[2m'; RESET=$'\033[0m'
else
    BOLD=''; GREEN=''; CYAN=''; YELLOW=''; DIM=''; RESET=''
fi
step() { printf '%s::%s %s\n' "$CYAN$BOLD" "$RESET$BOLD" "$1$RESET"; }
ok()   { printf '  %s✓%s %s\n' "$GREEN$BOLD" "$RESET" "$1"; }
warn() { printf '  %s!%s %s\n' "$YELLOW$BOLD" "$RESET" "$1"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# --- privilege escalation only where the target isn't writable ---------------
# Returns the command prefix ("sudo" or "") needed to write to a given dir.
sudo_for() {
    local dir="$1"
    # Walk up to the nearest existing ancestor and test writability there.
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

# --- uninstall ---------------------------------------------------------------
if [ "${1:-}" = "--uninstall" ]; then
    step "Uninstalling wrapt"
    remove_file "$BIN_PATH"
    remove_file "$BASH_COMP"
    remove_file "$ZSH_COMP"
    remove_file "$FISH_COMP"
    printf '\n%s✓ wrapt removed.%s\n' "$GREEN$BOLD" "$RESET"
    exit 0
fi

# --- build -------------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    printf '%serror:%s cargo not found. Install Rust from https://rustup.rs first.\n' "$YELLOW$BOLD" "$RESET" >&2
    exit 1
fi

step "Building wrapt (release)"
cargo build --release
BUILT="$SCRIPT_DIR/target/release/wrapt"
ok "built $BUILT"

# --- install binary ----------------------------------------------------------
step "Installing binary"
if [ -n "$(sudo_for "$BIN_DIR")" ]; then
    warn "$BIN_DIR needs root — you may be prompted for your password"
fi
install_file "$BUILT" "$BIN_PATH" 0755
ok "installed $BIN_PATH"

# --- install completions -----------------------------------------------------
# Generated straight from the freshly built binary, so they never drift from
# the actual CLI. Each shell's file goes where that shell already looks.
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

# --- done --------------------------------------------------------------------
printf '\n%s✓ wrapt installed.%s\n' "$GREEN$BOLD" "$RESET"
if command -v wrapt >/dev/null 2>&1 && [ "$(command -v wrapt)" = "$BIN_PATH" ]; then
    ok "wrapt is on your PATH"
else
    warn "$BIN_DIR is not on your PATH — add it, or restart your shell"
fi
printf '  %sTry:%s %ssudo wrapt upgrade%s   %s(open a new shell for completions)%s\n' \
    "$BOLD" "$RESET" "$CYAN" "$RESET" "$DIM" "$RESET"
