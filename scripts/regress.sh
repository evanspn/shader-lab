#!/bin/bash
# Run every regression check (needs a GPU): golden pictures, orientation, text, coverage, temporal and perf.
#   scripts/regress.sh            everything
#   scripts/regress.sh --fast     the quick subset (what the pre-push hook runs)
#   scripts/regress.sh --update   re-record goldens and the perf baseline (review the diff, then commit it)
# Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --quiet
BIN=target/release/shaderlab
status=0
"$BIN" regress examples/shaders "$@" || status=$?
echo
echo "cargo test (unit, pty and GPU tests; GPU ones skip with a message when there is no adapter):"
if [[ " $* " == *" --update "* ]]; then
  echo "(skipped while updating)"
else
  cargo test --release --quiet 2>&1 | grep -E "test result|FAILED|SKIPPED|panicked" || true
  cargo test --release --quiet >/dev/null 2>&1 || status=1
fi
exit $status
