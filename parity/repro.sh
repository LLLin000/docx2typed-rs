#!/usr/bin/env bash
# Minimal reproduction of the Phase-1 blockers named in the migration plan.
#
# Purpose: PR 1 must hand PR 2 a *reproducible* red light, not a description.
# Every command below is deterministic and reports its own exit code, so a
# fix in PR 2 can be checked by re-running this file.
#
# Usage:  bash parity/repro.sh
# Exit:   0 if every check ran (regardless of pass/fail of the underlying
#         check); the printed status is the payload, not the exit code.

set -u
cd "$(dirname "$0")/.."

echo "== repo =="
echo "target commit: $(git rev-parse HEAD)"
echo

run() {
  local label="$1"; shift
  echo "----------------------------------------------------------------"
  echo "## $label"
  echo "\$ $*"
  "$@" >/tmp/parity-repro.out 2>&1
  local code=$?
  if [ $code -eq 0 ]; then echo "RESULT: PASS (exit 0)"; else echo "RESULT: FAIL (exit $code)"; fi
  tail -n 20 /tmp/parity-repro.out
  echo
}

# --- 1. compile baseline -------------------------------------------------
run "cargo check -p docx2typed-core" \
    cargo check -p docx2typed-core --locked

# --- 2. clippy red light: prose.rs:1330 tracked_run_replacement (8/7 args)
run "cargo clippy -p docx2typed-core -- -D warnings" \
    cargo clippy -p docx2typed-core --locked -- -D warnings

# --- 3. python-oracle differential / migration parity --------------------
# Requires DOCX2TYPED_PYTHON_ORACLE pointing at the frozen reference checkout.
if [ -n "${DOCX2TYPED_PYTHON_ORACLE:-}" ]; then
  run "differential (python-oracle)" \
      cargo test --workspace --features python-oracle --test differential
  run "migrate (python-oracle)" \
      cargo test --workspace --features python-oracle --test migrate
else
  echo "----------------------------------------------------------------"
  echo "## python-oracle differential / migrate"
  echo "SKIPPED: DOCX2TYPED_PYTHON_ORACLE is not set."
  echo "  export DOCX2TYPED_PYTHON_ORACLE=/path/to/docx2typed @ 1adcc94"
  echo
fi

# --- 4. parity matrix is current -----------------------------------------
run "parity matrix regeneration" \
    python parity/generate.py
