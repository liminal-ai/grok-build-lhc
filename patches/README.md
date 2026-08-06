# LHC hook patch series

Every core-file touchpoint (an `LHC-HOOK` marked insertion outside
`crates/lhc/`) is maintained BOTH as normal commits on the `lhc` branch AND
as a re-appliable patch here, regenerated after any hook change and after
every upstream sync.

On a normal upstream sync these files are redundant. They exist for the day
upstream resets history (this repo is a daily monorepo squash-sync; ancestry
is not guaranteed): fresh clone of new upstream -> re-add `crates/lhc/` ->
apply the patch -> `scripts/check-lhc-hooks.sh`. The full drill is in
/FORK.md.

## Model: ONE state diff from ONE recorded base

- `BASE` — the upstream commit the diff was generated against (always the
  `main` == `upstream/main` of the last sync).
- `0001-lhc-touchpoints.patch` — `git diff` of every fork-owned core-file
  delta, path list derived (see below).

Apply with:

    git apply --3way patches/0001-lhc-touchpoints.patch

This replaced the original `git format-patch` per-chunk series on
2026-08-06, at the first real upstream sync, because commit-anchored
patches rot: the hook commits carry old upstream context, so `git am
--3way` re-hits every conflict the sync itself already resolved (rehearsal
failed at patch 2 of 7 on `compaction.rs` + `mod.rs`). A state diff from
the recorded BASE applies clean by construction, because it is derived
from exactly that tree. The codex-lhc sister fork hit the same failure
class through its Chunk 2 and made the same ruling (`patches/lhc/BASE`
there; "regenerating against whatever HEAD happens to be is what broke
this series").

The cost: per-chunk history shape is no longer in `patches/` — it lives in
the `lhc` branch itself and in the fork tags/records. Recovery restores
the touchpoints as one commit.

## Regenerating (after any hook change; after every sync)

    git rev-parse main > patches/BASE
    git diff main..HEAD \
      -- $(git diff --name-only main -- crates/codegen/ Cargo.toml | tr '\n' ' ') \
      > patches/0001-lhc-touchpoints.patch

**`main` must already be fast-forwarded to `upstream/main`** (sync drill
step 4) so BASE records the actual current base.

**The path list is DERIVED, never hand-maintained.** That `$( )` is the
whole point: the invariant this file used to merely assert — "the list must
equal `git diff --name-only main -- crates/codegen/ Cargo.toml`" — is
structurally guaranteed instead of checked. It broke silently twice while
hand-maintained (dropped five touchpoints after Chunk 2; dropped the root
`Cargo.toml` workspace entry by regenerating a single commit).

**Deliberately excluded** — do not add them:
- `crates/lhc/**` — fork-owned; the drill re-adds that directory wholesale
  (submodule + adapter), so patching it would be redundant and enormous.
- `Cargo.lock` — regenerate with `cargo check` after applying.
- `FORK.md`, `patches/`, `scripts/check-lhc-hooks.sh` — fork-owned, copied.

## Verifying

Rehearse the drill: worktree (or fresh clone) at raw `upstream/main`,
`git apply --3way` the patch, then assert the sentinel count (10/10
`LHC-HOOK` markers) and the root `Cargo.toml` workspace entry. Rehearsed
green 2026-08-06 against `a5589e9`.
