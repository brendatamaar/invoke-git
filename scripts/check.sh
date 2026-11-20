#!/usr/bin/env bash
set -euo pipefail

run_step() {
    local step=$1
    shift
    "$@" || {
        local status=$?
        printf 'check: %s failed (exit %s)\n' "$step" "$status" >&2
        exit "$status"
    }
}

run_step 'enter project root' cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

if ! { exec 9>>/tmp/invoke-git-check.lock; }; then
    printf 'check: opening the lock file failed\n' >&2
    exit 1
fi
run_step 'acquire check lock' flock 9

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
if command -v sccache >/dev/null 2>&1; then
    export RUSTC_WRAPPER=sccache
elif [[ ${RUSTC_WRAPPER:-} == sccache ]]; then
    unset RUSTC_WRAPPER
fi

excluded_packages=(ui app spike-diff spike-graph)
exclude_args=()
for package in "${excluded_packages[@]}"; do
    exclude_args+=(--exclude "$package")
done

metadata=$(run_step 'cargo metadata' cargo metadata --manifest-path Cargo.toml --no-deps --format-version 1)
package_names=$(run_step 'workspace package discovery' python3 -c '
import json
import sys

workspace = json.load(sys.stdin)
members = set(workspace["workspace_members"])
excluded = set(sys.argv[1:])

for package in workspace["packages"]:
    if package["id"] in members and package["name"] not in excluded:
        print(package["name"])
' "${excluded_packages[@]}" <<< "$metadata")

if [[ -z $package_names ]]; then
    printf 'check: workspace package discovery found no non-UI packages\n' >&2
    exit 1
fi

fmt_args=()
while IFS= read -r package; do
    fmt_args+=(-p "$package")
done <<< "$package_names"

run_step 'cargo fmt' cargo fmt "${fmt_args[@]}" --check
run_step 'cargo clippy' cargo clippy --workspace "${exclude_args[@]}" --all-targets -- -D warnings
run_step 'cargo test' cargo test --workspace "${exclude_args[@]}"
