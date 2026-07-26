# LHC hook patch series

Every core-file touchpoint (an `LHC-HOOK` marked insertion outside
`crates/lhc/`) is maintained BOTH as normal commits on the `lhc` branch AND
as a `git format-patch` file here, regenerated whenever a hook changes.

On a normal upstream sync these files are redundant. They exist for the day
upstream resets history (this repo is a daily monorepo squash-sync; ancestry
is not guaranteed): fresh clone of new upstream -> re-add `crates/lhc/` ->
`git am patches/*.patch` -> `scripts/check-lhc-hooks.sh`. The full drill is
in /FORK.md.

Series (Chunk 1 — generated, and the drill is **rehearsed**):
- `0001-fork-lhc-Chunk-1-*.patch` — all four core touchpoints in one patch:
  root `Cargo.toml` workspace-members entry, shell `Cargo.toml` dependency
  (`LHC-HOOK 1/3`), persistence tee in `spawn.rs` (`2/3`), model /
  thinking-level tee in `model_switch.rs` (`3/3`).

One patch rather than three: the four edits land in a single commit, and
`git am` of one file is a shorter recovery than three that must be ordered.
Split it only if a future chunk changes hooks independently.

**Deliberately excluded** — do not add them:
- `crates/lhc/**` — fork-owned; the drill re-adds that directory wholesale
  (submodule + adapter), so patching it would be redundant and enormous.
- `Cargo.lock` — regenerate with `cargo check` after applying.
- `FORK.md`, `patches/`, `scripts/check-lhc-hooks.sh` — fork-owned, copied.

## Regenerating

    git format-patch <first-chunk-commit>~1..HEAD \
      --output-directory patches --suffix=.patch \
      -- $(git diff --name-only origin/main -- crates/codegen/ Cargo.toml | tr '\n' ' ')

**The path list is DERIVED, never hand-maintained.** That `$( )` is the whole
point: the invariant this file used to merely assert — "the list must equal
`git diff --name-only origin/main -- crates/codegen/ Cargo.toml`" — is now
structurally guaranteed instead of checked.

It broke silently twice while hand-maintained:

1. The documented command still listed only Chunk 1's four paths, so running it
   after Chunk 2 would drop five touchpoints — including a core-tree regression
   test — and a history-reset recovery would restore an incomplete fork with
   nothing to signal it.
2. It used `-1 <commit>`, a single commit, so regenerating after Chunk 2 dropped
   root `Cargo.toml`'s workspace-members entry, which lives in the Chunk 1
   commit. Deleting the old patch removed its only copy.

Regenerate the WHOLE series (`<first>~1..HEAD`), not one commit. Current series:
five patches, seventeen paths. Verify by rehearsing the drill — `git am --3way`
onto a fresh clone must apply clean.


