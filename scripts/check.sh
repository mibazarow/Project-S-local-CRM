#!/usr/bin/env bash
# Local quality gate (DOCS/REFACTOR_PLAN.md, slice R-02).
#
#   scripts/check.sh                run the whole gate, stop on the first failure
#   STRICT_FMT=1 scripts/check.sh   also fail on rustfmt diffs (see step 1)
#
# Deliberately NOT wired into pre-commit: cargo test + vite build are minutes.
# Windows/Linux hosts are not part of this gate; a Windows `cargo check` is run
# manually over SSH by explicit user command (CROSPLATFORM.md §3.1).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 2
STRICT_FMT="${STRICT_FMT:-0}"

# Pre-existing clippy debt, pinned lint-by-lint so any NEW lint still fails the
# gate with -D warnings. Drop an entry once its sites are fixed (R-03 style).
CLIPPY_ALLOW=(
  -A clippy::result_large_err                  # llm/mod.rs: AppError Err-variant >= 272 bytes (5 sites)
  -A clippy::needless_borrows_for_generic_args # agent/project_tools.rs, agent/harness_tools/archive_tools.rs, config.rs (tests)
  -A clippy::type_complexity                   # scheduler/store.rs, agent/edit_tools.rs (tests)
  -A clippy::useless_format                    # agent/macos_ui_tool.rs
  -A clippy::unnecessary_fallible_conversions  # agent/harness_tools/spotlight_tools.rs
  -A clippy::redundant_closure                 # mcp/mod.rs
  -A clippy::get_first                         # agent/macos_ui_tool.rs
  -A clippy::unnecessary_map_or                # mcp/mod.rs (tests)
)

RESULTS=()
FAILED=""

step() {
  local name="$1"
  shift
  printf '\n\033[1m==> %s\033[0m\n' "$name"
  if "$@"; then
    RESULTS+=("ok    $name")
  else
    RESULTS+=("FAIL  $name")
    FAILED="$name"
    printf '\n\033[31mGATE FAILED at: %s\033[0m\n' "$name"
    finish 1
  fi
}

finish() {
  printf '\n\033[1mquality gate summary\033[0m\n'
  printf '  %s\n' "${RESULTS[@]}"
  if [ -n "$FAILED" ]; then
    printf 'result: FAILED (%s)\n' "$FAILED"
  else
    printf 'result: all checks passed\n'
  fi
  exit "$1"
}

# 1. rustfmt. The repo is hand-formatted: 126 files already differ from rustfmt
#    defaults, so a strict check would be red before any change. Reported here,
#    fatal only with STRICT_FMT=1 (until a dedicated reformat slice lands).
check_fmt() {
  local out files
  if out="$(cd src-tauri && cargo fmt --check 2>&1)"; then
    printf 'rustfmt: clean\n'
    return 0
  fi
  files="$(printf '%s\n' "$out" | grep -o '^Diff in [^:]*' | sort -u | wc -l | tr -d ' ')"
  if [ "$files" = "0" ]; then
    printf '%s\n' "$out"
    return 1
  fi
  printf 'rustfmt: %s files differ from rustfmt defaults (pre-existing)\n' "$files"
  if [ "$STRICT_FMT" = "1" ]; then
    printf '%s\n' "$out"
    return 1
  fi
  printf 'rustfmt: advisory only, set STRICT_FMT=1 to enforce\n'
  return 0
}

check_clippy() {
  (cd src-tauri && cargo clippy --all-targets -- -D warnings "${CLIPPY_ALLOW[@]}")
}

check_cargo_test() {
  (cd src-tauri && cargo test)
}

check_npm_test() {
  npm test
}

check_tsc() {
  npx tsc --noEmit
}

check_build() {
  npm run build
}

step "cargo fmt --check" check_fmt
step "cargo clippy --all-targets -D warnings" check_clippy
step "cargo test" check_cargo_test
step "npm test" check_npm_test
step "npx tsc --noEmit" check_tsc
step "npm run build" check_build

finish 0
