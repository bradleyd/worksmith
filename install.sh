#!/usr/bin/env sh
# Build worksmith in release and put the binary on PATH.
#
# Installs to ~/.local/bin (already on PATH on this machine); falls back to
# `cargo install --path .` if that directory isn't writable.
#
#   ./install.sh            build + install
#   ./install.sh --debug    build + install the debug binary (faster build)
set -eu

cd "$(dirname "$0")"

mode="release"
[ "${1:-}" = "--debug" ] && mode="debug"

cargo build --${mode}
# target/ may be relocated by a global ~/.cargo/config.toml (target-dir) or
# CARGO_TARGET_DIR — ask cargo where it actually built.
target_dir=$(cargo metadata --no-deps --format-version 1 |
  sed -n 's/.*"target_directory":"\([^"\\]*\)".*/\1/p')
[ -n "${target_dir}" ] || target_dir="target"
bin="${target_dir}/${mode}/worksmith"

dest_dir="${HOME}/.local/bin"
if [ -d "${dest_dir}" ] && [ -w "${dest_dir}" ]; then
  mkdir -p "${dest_dir}"
  cp "${bin}" "${dest_dir}/worksmith"
  echo "installed ${dest_dir}/worksmith"
else
  echo "~/.local/bin not writable — falling back to cargo install (~/.cargo/bin)"
  cargo install --path . --force
fi
