#!/usr/bin/env bash
# The full Linux gate, from any workstation: whitespace check here, then
# format, release build, tests, Clippy and shell syntax for every
# Linux-buildable crate, on this machine (Linux) or on a Linux build host.
#
#   tools/gate.sh
#
# The build host is site configuration, read from the environment or from
# site/gate.env (untracked; see deploy/examples/gate.env.example):
#
#   RAMPIRE_GATE_HOST  SSH destination of a Linux build host. Unset: build
#                         here, which must then be Linux.
#   RAMPIRE_GATE_DIR   Absolute directory on that host the gate owns. The
#                         source copy in its src/ is replaced on every run;
#                         Cargo output stays in its target/.
#   RAMPIRE_SSH        SSH client to use (default: ssh).
#
# The working tree is sent as it is, uncommitted and untracked (not ignored)
# files included, with Git's line endings, so the gate checks what is on disk.
set -euo pipefail

repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
if [[ -f site/gate.env ]]; then
  # shellcheck source=/dev/null
  source site/gate.env
fi
host=${RAMPIRE_GATE_HOST:-}
remote_dir=${RAMPIRE_GATE_DIR:-}
ssh=${RAMPIRE_SSH:-ssh}

# Snapshot the working tree as a Git tree object without touching the
# user's index: a temporary index takes every non-ignored file.
index=$(mktemp)
trap 'rm -f "$index"' EXIT
rm -f "$index"
GIT_INDEX_FILE=$index git -c core.safecrlf=false add -A
tree=$(GIT_INDEX_FILE=$index git write-tree)

echo "==> whitespace validation (working tree against HEAD)"
git diff --check HEAD "$tree"

if [[ -z $host ]]; then
  if [[ $(uname -s) != Linux ]]; then
    echo 'the host and xtask crates build only on Linux: set RAMPIRE_GATE_HOST' >&2
    echo '(and RAMPIRE_GATE_DIR) or create site/gate.env' >&2
    exit 1
  fi
  exec cargo xtask gate checks
fi

if [[ $remote_dir != /* || $remote_dir == / || $remote_dir =~ [[:space:]\'\"\$\`\\] ]]; then
  echo 'RAMPIRE_GATE_DIR must be an absolute, plain path other than /' >&2
  exit 1
fi

echo "==> sending tree ${tree:0:12} to $host:$remote_dir/src"
# core.autocrlf=false keeps the repository's LF endings on Windows.
git -c core.autocrlf=false archive --format=tar "$tree" |
  "$ssh" -o BatchMode=yes "$host" "set -eu
    mkdir -p '$remote_dir'
    rm -rf '$remote_dir/src.new'
    mkdir '$remote_dir/src.new'
    tar --warning=no-timestamp -x -C '$remote_dir/src.new'
    rm -rf '$remote_dir/src'
    mv '$remote_dir/src.new' '$remote_dir/src'"

echo "==> gate on $host"
"$ssh" -o BatchMode=yes "$host" "set -eu
  cd '$remote_dir/src'
  export CARGO_TARGET_DIR='$remote_dir/target'
  . \"\$HOME/.cargo/env\" 2>/dev/null || true
  cargo run --quiet --locked -p xtask -- gate checks"

# On a Windows workstation the Windows guest builds natively too. This is
# not the native gate on a Windows VM (cargo xtask windows all), which also
# runs the release build that gets deployed.
case $(uname -s) in
  MINGW* | MSYS* | CYGWIN*)
    windows=(-p rampire-core -p rampire-guest -p rampire-guest-windows)
    echo "==> Windows guest: tests and Clippy here"
    cargo test "${windows[@]}" --locked --quiet
    cargo clippy "${windows[@]}" --all-targets --locked --quiet -- -D warnings
    ;;
esac
echo "==> gate passed for tree ${tree:0:12}"
