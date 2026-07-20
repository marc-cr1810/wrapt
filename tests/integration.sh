#!/usr/bin/env bash
#
# Integration tests for wrapt: exercise the real code paths as root against a
# real apt, installing and removing a tiny throwaway package. Designed to run in
# a disposable container (see .github/workflows/ci.yml) — it DOES modify the
# system's package state, so don't run it on a machine you care about.
#
#   WRAPT=./target/release/wrapt bash tests/integration.sh

set -uo pipefail

WRAPT="${WRAPT:-./target/release/wrapt}"
PKG="${TEST_PKG:-hello}"   # tiny, no prompts, minimal deps
FAILS=0

pass() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAILS=$((FAILS + 1)); }

# assert_has <output> <substring> <name>
assert_has() {
    if printf '%s' "$1" | grep -qF -- "$2"; then pass "$3"; else
        fail "$3 — expected to find: $2"
        printf '%s\n' "$1" | tail -20 | sed 's/^/      | /'
    fi
}
# assert_missing <output> <substring> <name>
assert_missing() {
    if printf '%s' "$1" | grep -qF -- "$2"; then
        fail "$3 — should not contain: $2"
        printf '%s\n' "$1" | tail -20 | sed 's/^/      | /'
    else pass "$3"; fi
}

if [ "$(id -u)" -ne 0 ]; then
    echo "integration tests must run as root (they install/remove packages)" >&2
    exit 1
fi
if [ ! -x "$WRAPT" ]; then
    echo "wrapt binary not found at $WRAPT (build it first)" >&2
    exit 1
fi

echo ":: preparing environment"
apt-get update -qq
apt-get remove -y -qq "$PKG" >/dev/null 2>&1 || true

echo ":: search (read-only, no root needed)"
out=$("$WRAPT" search "$PKG" 2>&1)
assert_has "$out" "$PKG" "search finds $PKG"

echo ":: --json search is valid JSON"
if "$WRAPT" --json search "$PKG" 2>/dev/null | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    pass "search --json parses"
else
    fail "search --json is not valid JSON"
fi

echo ":: colour policy"
# Piped output must be plain; --json consumers and greps depend on it.
out=$("$WRAPT" held 2>&1)
assert_missing "$out" $'\033[' "piped output carries no ANSI escapes"
out=$(NO_COLOR=1 "$WRAPT" held 2>&1)
assert_missing "$out" $'\033[' "NO_COLOR output carries no ANSI escapes"

echo ":: --json is accepted or refused, never ignored"
out=$("$WRAPT" --json held 2>&1)
if printf '%s' "$out" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
    pass "held --json parses"
else
    fail "held --json is not valid JSON"
fi
out=$("$WRAPT" --json show "$PKG" 2>&1)
assert_has "$out" "not supported" "show --json is refused, not silently ignored"

echo ":: dry runs need no privileges"
# Run as an unprivileged user to prove the preview paths don't demand root.
# Confirm the drop actually happened first: if setpriv fails, the "requires
# root" string is absent for the wrong reason and the check would pass vacuously.
if id -u nobody >/dev/null 2>&1 && command -v setpriv >/dev/null 2>&1; then
    drop() { setpriv --reuid=nobody --regid=nogroup --clear-groups "$@" 2>&1; }
    if [ "$(drop id -u)" != "0" ]; then
        out=$(drop "$WRAPT" upgrade --security-only --dry-run)
        assert_missing "$out" "requires root" "upgrade --security-only --dry-run works unprivileged"
        out=$(drop "$WRAPT" plan "$PKG")
        assert_missing "$out" "requires root" "plan works unprivileged"
    else
        fail "could not drop privileges — unprivileged dry-run checks did not run"
    fi
fi

echo ":: install under a PTY (interactive path — reveal-bug regression)"
# `script` gives wrapt a real terminal, so the interactive reveal logic is live.
# A fresh install runs dpkg unpack/configure, so any leaked apt chatter shows up.
out=$(script -qec "$WRAPT install -y $PKG" /dev/null 2>&1)
out=$(printf '%s' "$out" | tr -d '\r')
assert_has "$out" "Done" "install completes"
if command -v "$PKG" >/dev/null 2>&1; then pass "$PKG is installed"; else fail "$PKG not on PATH after install"; fi
# Sentinels that appear ONLY in raw dpkg/apt output, never in wrapt's own
# status-driven progress bar (whose phase labels are "Preparing to configure",
# "Configuring", "Unpacking <pkg>", etc.). "Preparing to unpack" and "Selecting
# previously unselected" are dpkg-only phrasings, so they cleanly detect a leak.
assert_missing "$out" "Building dependency tree" "no dependency-tree chatter leaked"
assert_missing "$out" "Preparing to unpack" "no dpkg unpack chatter leaked"
assert_missing "$out" "Selecting previously unselected" "no dpkg selection chatter leaked"
assert_missing "$out" "Setting up " "no dpkg setup chatter leaked"
assert_missing "$out" "Processing triggers" "no trigger chatter leaked"
assert_missing "$out" "apt needs your input" "no spurious prompt reveal"

echo ":: why (installed manually)"
out=$("$WRAPT" why "$PKG" 2>&1)
assert_has "$out" "manually" "why reports manual install"

echo ":: show includes install status"
out=$("$WRAPT" show "$PKG" 2>&1)
assert_has "$out" "Status:" "show adds Status line"

echo ":: history recorded the install"
out=$("$WRAPT" history 2>&1)
assert_has "$out" "install" "history lists the install"

echo ":: provides resolves an installed command"
out=$("$WRAPT" provides ls 2>&1)
assert_has "$out" "provided by" "provides finds owner of ls"

echo ":: doctor runs"
out=$("$WRAPT" doctor 2>&1)
# "dependencies" appears whether the check passes or finds problems; robust to
# whatever incidental state the container is in.
assert_has "$out" "dependencies" "doctor runs its dependency check"

echo ":: remove the package"
out=$("$WRAPT" remove -y "$PKG" 2>&1)
assert_has "$out" "Done" "remove completes"
if command -v "$PKG" >/dev/null 2>&1; then fail "$PKG still present after remove"; else pass "$PKG removed"; fi

echo
if [ "$FAILS" -eq 0 ]; then
    printf '\033[32mAll integration tests passed.\033[0m\n'
    exit 0
else
    printf '\033[31m%d integration test(s) failed.\033[0m\n' "$FAILS"
    exit 1
fi
