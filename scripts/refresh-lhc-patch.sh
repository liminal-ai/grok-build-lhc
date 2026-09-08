#!/usr/bin/env bash
# Regenerate patches/0001-lhc-touchpoints.patch: ONE state diff from the
# recorded upstream base (patches/BASE) to the committed HEAD, over the
# patch's fixed scope (crates/codegen/ and the root Cargo.toml).
#
# Order: commit the source change first, run this, commit the patch.
# The diff is commit-to-commit, so uncommitted edits never enter it.
#
# This never writes patches/BASE and never touches the index. BASE names
# the upstream commit the fork is currently based on; it changes only in
# an upstream sync (FORK.md "Sync drill" step 6), by hand, in that commit.
#
# Exclusions are by construction (scope), not by list: crates/lhc/**
# (fork-owned, copied whole in recovery), Cargo.lock, FORK.md, patches/,
# scripts/, lhc-docs/, .github/, README.md banner — see patches/README.md.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

base=$(tr -d '[:space:]' < patches/BASE)
git rev-parse --verify --quiet "${base}^{commit}" > /dev/null \
  || { echo "refresh-lhc-patch: patches/BASE ($base) is not a commit in this repo (fetch upstream first)" >&2; exit 1; }

if [ -n "$(git status --porcelain -- crates/codegen/ Cargo.toml)" ]; then
  echo "refresh-lhc-patch: note: uncommitted changes under crates/codegen/ or Cargo.toml are NOT in the patch (commit first)" >&2
fi

git diff "$base" HEAD -- crates/codegen/ Cargo.toml > patches/0001-lhc-touchpoints.patch

echo "refresh-lhc-patch: BASE=$base HEAD=$(git rev-parse --short HEAD) files=$(grep -c '^diff --git' patches/0001-lhc-touchpoints.patch || true)"
