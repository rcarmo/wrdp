#!/usr/bin/env bash
# Set real Actions subprocess paths; no host /workspace tooling dependency.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/project-tmp.sh"
root="$(project_tmp_resolve wrdp)"
project_tmp_init "$root"
run_id="${WRDP_RUN_ID:?set a CI run id}"
case "$run_id" in ''|*[!A-Za-z0-9._-]*|.*|-*) echo 'invalid run id' >&2; exit 1;; esac
run_dir="$root/runs/ci/$run_id"
mkdir -p "$run_dir/tmp" "$root/cache/cargo" "$root/cache/python/pycache" "$root/cache/xdg" "$root/build/wrdp"
{
    printf '%s\n' "PROJECT_TMP_ROOT=$root" "WRDP_TMP_ROOT=$root" \
        "WRDP_RUN_DIR=$run_dir" "WRDP_TMPDIR=$run_dir/tmp" \
        "TMPDIR=$run_dir/tmp" "TMP=$run_dir/tmp" "TEMP=$run_dir/tmp" \
        "CARGO_HOME=$root/cache/cargo" "CARGO_TARGET_DIR=$root/build/wrdp/cargo-target" \
        "COMPOSITOR_BUILD_DIR=$root/build/wrdp/compositor" \
        "PYTHONPYCACHEPREFIX=$root/cache/python/pycache" "XDG_CACHE_HOME=$root/cache/xdg"
} >> "${GITHUB_ENV:?Actions environment file required}"
