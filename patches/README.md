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

    git format-patch <first-chunk-commit>~1..<head> \
      --output-directory patches --suffix=.patch \
      -- Cargo.toml \
         crates/codegen/xai-grok-shell/Cargo.toml \
         crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs \
         crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs \
         crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs \
         crates/codegen/xai-grok-shell/src/session/acp_session_tests/rewind_cross_compaction_tests.rs \
         crates/codegen/xai-grok-shell/src/session/compaction.rs \
         crates/codegen/xai-grok-shell/src/session/lhc_inference.rs \
         crates/codegen/xai-grok-shell/src/session/mod.rs

**Regenerate the WHOLE series, not one commit.** This example previously used
`format-patch -1 <commit>` with only Chunk 1's four paths. Two traps in that:

1. Running it after Chunk 2 would silently drop five touchpoints
   (`turn.rs`, `compaction.rs`, `lhc_inference.rs`, `mod.rs`,
   `rewind_cross_compaction_tests.rs`) — the recovery drill would restore an
   incomplete fork and nothing would say so.
2. `-1 <head>` only captures the newest commit. Root `Cargo.toml`'s
   workspace-members entry lands in the CHUNK 1 commit, so a single-commit
   regeneration drops it entirely. Hit for real on 2026-07-25.

**When a hook is added, add its file to this list in the same commit.** The
list must equal `git diff --name-only origin/main -- crates/codegen/ Cargo.toml`.

Delete the old `*.patch` first — a stale patch that still applies is worse
than no patch at all.

## Rehearsal record

2026-07-25, Chunk 1 (fork commit `9ea06ea`). Rehearsed against the **raw
upstream tip** `6e38642`, which contains no `crates/lhc/` at all — the real
history-reset shape, not a convenient one:

    git worktree add --detach /tmp/lhc-recovery-test 6e38642
    cd /tmp/lhc-recovery-test
    git am /srv/work/grok-build/patches/0001-*.patch

Applied cleanly (exit 0); all three `LHC-HOOK` markers and the
workspace-members entry restored. Re-rehearse whenever a hook changes.
