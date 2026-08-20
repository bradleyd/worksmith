#!/usr/bin/env sh
# Build worksmith in release and put the binary on PATH.
#
# Portable across macOS and Linux. It prefers a fast path (cargo build + copy
# into a directory already on PATH) and falls back to `cargo install`, which
# always lands in ~/.cargo/bin (on PATH wherever cargo is installed).
#
#   ./install.sh            build + install (release)
#   ./install.sh --debug    build + install the debug binary (faster build)
#   ./install.sh --cargo    force `cargo install` (canonical, isolated build)
#
# Once published, `cargo install worksmith` is equivalent to `--cargo`.
set -eu

cd "$(dirname "$0")"

mode="release"
force_cargo=0
for arg in "$@"; do
  case "$arg" in
    --debug) mode="debug" ;;
    --cargo) force_cargo=1 ;;
  esac
done

# True if $1 is a directory already on PATH.
on_path() {
  case ":${PATH}:" in
    *":$1:"*) return 0 ;;
    *) return 1 ;;
  esac
}

# target/ may be relocated by a global ~/.cargo/config.toml (target-dir) or
# CARGO_TARGET_DIR — ask cargo where it actually built.
target_dir_of() {
  cargo metadata --no-deps --format-version 1 |
    sed -n 's/.*"target_directory":"\([^"\\]*\)".*/\1/p'
}

if [ "$force_cargo" -eq 1 ]; then
  cargo install --path . --force
  echo "installed ~/.cargo/bin/worksmith (cargo install)"
  exit 0
fi

# Fast path: pick a writable directory that is already on PATH. Prefer
# ~/.local/bin (keeps it out of cargo's dir), then ~/.cargo/bin (always on PATH
# where cargo is). If neither is usable, fall through to cargo install.
dest_dir=""
for d in "${HOME}/.local/bin" "${HOME}/.cargo/bin"; do
  if [ -d "$d" ] && [ -w "$d" ] && on_path "$d"; then
    dest_dir="$d"
    break
  fi
done

if [ -z "$dest_dir" ]; then
  echo "no writable PATH dir found — falling back to cargo install (~/.cargo/bin)"
  cargo install --path . --force
  exit 0
fi

cargo build --${mode}
target_dir=$(target_dir_of)
[ -n "${target_dir}" ] || target_dir="target"
bin="${target_dir}/${mode}/worksmith"
[ -f "$bin" ] || { echo "error: built binary not found at $bin" >&2; exit 1; }

mkdir -p "$dest_dir"
cp "$bin" "${dest_dir}/worksmith"
echo "installed ${dest_dir}/worksmith"
