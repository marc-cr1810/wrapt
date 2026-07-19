#!/usr/bin/env bash
#
# Build a .deb of wrapt itself, so it can be installed and removed the normal
# way (`sudo apt install ./wrapt_<ver>_<arch>.deb`). Uses only dpkg-deb, which
# is present on any Debian-based system — no cargo-deb required.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$SCRIPT_DIR"

VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"
ARCH="$(dpkg --print-architecture)"
MAINTAINER="${DEB_MAINTAINER:-wrapt maintainers <wrapt@localhost>}"
PKGDIR="target/deb/wrapt_${VERSION}_${ARCH}"
DEB="target/deb/wrapt_${VERSION}_${ARCH}.deb"

echo ":: Building release binary"
cargo build --release
BIN="target/release/wrapt"

echo ":: Staging package tree at $PKGDIR"
rm -rf "$PKGDIR"
install -Dm0755 "$BIN" "$PKGDIR/usr/bin/wrapt"

# Completions and man page, generated straight from the binary.
"$BIN" completions bash | install -Dm0644 /dev/stdin "$PKGDIR/usr/share/bash-completion/completions/wrapt"
"$BIN" completions zsh  | install -Dm0644 /dev/stdin "$PKGDIR/usr/share/zsh/vendor-completions/_wrapt"
"$BIN" completions fish | install -Dm0644 /dev/stdin "$PKGDIR/usr/share/fish/vendor_completions.d/wrapt.fish"
"$BIN" man | gzip -9n | install -Dm0644 /dev/stdin "$PKGDIR/usr/share/man/man1/wrapt.1.gz"

# Control metadata.
INSTALLED_KB="$(du -ks "$PKGDIR/usr" | cut -f1)"
install -d "$PKGDIR/DEBIAN"
cat > "$PKGDIR/DEBIAN/control" <<EOF
Package: wrapt
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Depends: apt
Installed-Size: $INSTALLED_KB
Maintainer: $MAINTAINER
Description: A faster, prettier front-end for apt
 wrapt wraps apt and dpkg to add parallel downloads, clean output,
 transaction history with undo/redo/rollback, dependency explanations,
 security-update highlighting, and a system health check — without
 bypassing apt's package database.
EOF

echo ":: Building $DEB"
dpkg-deb --root-owner-group --build "$PKGDIR" "$DEB" >/dev/null
echo ":: Done"
dpkg-deb --info "$DEB" | sed 's/^/    /'
echo
echo "Install with:  sudo apt install ./$DEB"
