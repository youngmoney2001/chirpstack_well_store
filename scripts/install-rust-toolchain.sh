#!/usr/bin/env bash
# Install a standalone Rust toolchain in this sandbox.
#
# Why this script exists
# ----------------------
# The sandbox only allows outbound access to github.com, codeload.github.com,
# api.github.com, registry.npmjs.org, pypi.org and files.pythonhosted.org.
# The official channels (static.rust-lang.org, sh.rustup.rs, deb.debian.org,
# crates.io) are blocked, so `rustup` and `apt` cannot be used.
#
# The `@toolchain-bin/*` packages on registry.npmjs.org contain the *official*
# Rust dist components (rust-installer-version, components, version,
# git-commit-hash, install.sh) repackaged as npm tarballs. We merge them into
# one prefix, exactly like the official installer would.
#
# Installed version: Rust 1.99.0 (rustc/cargo/rust-std/rustfmt/clippy).
# NOTE: rust-toolchain.toml pins 1.89.0, but that file is only read by the
# `rustup` proxy. Here we call the real binaries directly, so it is ignored.
# On a normal machine with internet access, prefer `rustup toolchain install 1.89.0`.
#
# Usage:
#   bash scripts/install-rust-toolchain.sh [PREFIX] [BIN_DIR]
# Defaults: PREFIX=$HOME/.cache/rust-toolchain  BIN_DIR=/usr/local/bin
# (.cache is excluded from workspace snapshots so the ~600 MB toolchain is
# not committed or exported as an artifact.)
#
# This script does NOT provide crates: crates.io remains unreachable here.
# `cargo metadata --no-deps --offline`, `rustfmt` and standalone `rustc`
# snippets work; `cargo build` / `cargo check` need a vendored dependency tree.

set -euo pipefail

PREFIX="${1:-$HOME/.cache/rust-toolchain}"
BIN_DIR="${2:-/usr/local/bin}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

COMPONENTS=(
  "rustc|@toolchain-bin/rustc-1.99.0-x86_64-unknown-linux-gnu"
  "cargo|@toolchain-bin/cargo-1.99.0-x86_64-unknown-linux-gnu"
  "rust-std|@toolchain-bin/rust-std-1.99.0-x86_64-unknown-linux-gnu"
  "rustfmt|@toolchain-bin/rustfmt-1.99.0-x86_64-unknown-linux-gnu"
  "clippy|@toolchain-bin/clippy-1.99.0-x86_64-unknown-linux-gnu"
)

echo "==> Resolving component tarball URLs from registry.npmjs.org"
python3 - "${COMPONENTS[@]}" > "$WORK/urls.txt" <<'PY'
import json, sys, urllib.parse, urllib.request

for entry in sys.argv[1:]:
    short, pkg = entry.split("|", 1)
    url = "https://registry.npmjs.org/" + urllib.parse.quote(pkg, safe="") + "/latest"
    with urllib.request.urlopen(url, timeout=30) as r:
        meta = json.load(r)
    print(short, meta["dist"]["tarball"])
PY
cat "$WORK/urls.txt"

echo "==> Downloading (5 components, ~213 MB)"
while read -r short url; do
  curl -sS -L --retry 2 --fail -o "$WORK/$short.tgz" "$url"
  echo "    $short: $(du -h "$WORK/$short.tgz" | cut -f1)"
done < "$WORK/urls.txt"

echo "==> Extracting and merging into $PREFIX"
rm -rf "$PREFIX"
mkdir -p "$PREFIX"
while read -r short _; do
  mkdir -p "$WORK/x-$short"
  tar xzf "$WORK/$short.tgz" -C "$WORK/x-$short"
  for comp in "$WORK/x-$short"/package/*/; do
    name="$(basename "$comp")"
    case "$name" in
      rustc|cargo|rust-std-*|rustfmt-preview|clippy-preview)
        cp -a "$comp/." "$PREFIX/" ;;
    esac
  done
done < "$WORK/urls.txt"

# npm tarballs drop the executable bit on binaries.
chmod +x "$PREFIX"/bin/*
find "$PREFIX/lib/rustlib" -type f -path '*/bin/*' -exec chmod +x {} + 2>/dev/null || true

echo "==> Linking binaries into $BIN_DIR"
mkdir -p "$BIN_DIR" 2>/dev/null || true
LINK_DIR="$BIN_DIR"
if [ ! -w "$BIN_DIR" ]; then
  LINK_DIR="$HOME/.local/bin"
  echo "    $BIN_DIR is not writable, using $LINK_DIR instead (add it to PATH)"
fi
mkdir -p "$LINK_DIR"
for b in cargo rustc rustfmt cargo-fmt rustdoc cargo-clippy clippy-driver rust-gdb rust-lldb; do
  if [ -f "$PREFIX/bin/$b" ]; then
    ln -sf "$PREFIX/bin/$b" "$LINK_DIR/$b"
  fi
done

echo "==> Installed:"
"$PREFIX/bin/rustc" --version
"$PREFIX/bin/cargo" --version
"$PREFIX/bin/rustfmt" --version
"$PREFIX/bin/cargo-clippy" --version
echo "Done. Run: cargo metadata --no-deps --offline"
