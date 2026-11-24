#!/usr/bin/env bash
# Build cvenv and install the binary to bin/cvenv. Run via `pixi run build`.
# `pixi run test` / `lint` / `fmt` pass --test / --lint / --fmt instead.
# Cargo's registry cache and target dir hold thousands of files: keep them on
# node-local scratch instead of the repo / $HOME.
set -euo pipefail
REPO="$(cd -P "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="${CVENV_BUILD_TMPDIR:-/tmp}/cvenv-${USER:-$(id -u)}"
export CARGO_HOME="${CVENV_CARGO_HOME:-$TMP/cargo-home}"
export CARGO_TARGET_DIR="${CVENV_CARGO_TARGET_DIR:-$TMP/cargo-target}"
mkdir -p "$CARGO_HOME" "$CARGO_TARGET_DIR" "$REPO/bin"
cd "$REPO"
locked=""; [ -f Cargo.lock ] && locked="--locked"
# Fully static binary (glibc from the conda sysroot): runs on any x86_64/aarch64
# Linux, independent of this build env (which lives in /tmp and may be wiped).
# (an explicit --target keeps these flags away from proc-macros/build scripts)
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static -C relocation-model=static"
unset LDFLAGS
case "${1:-}" in
    --test) exec cargo test $locked --target "$TRIPLE" ;;
    --lint)
        cargo fmt --check
        exec cargo clippy $locked --target "$TRIPLE" --all-targets -- -D warnings ;;
    --fmt) exec cargo fmt ;;
esac
cargo build --release $locked --target "$TRIPLE"
install -m 755 "$CARGO_TARGET_DIR/$TRIPLE/release/cvenv" "$REPO/bin/cvenv.new"
mv -f "$REPO/bin/cvenv.new" "$REPO/bin/cvenv"
echo "installed $REPO/bin/cvenv ($("$REPO/bin/cvenv" --version))"
