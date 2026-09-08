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

- `BASE` — the upstream commit the diff was generated against: the
  `upstream/main` tip taken in by the last sync. It is a recorded fact of
  the fork, not a branch; no local branch layout is consulted. (`origin/main`
  and `origin/lhc` both carry the product tree — neither is the base.)
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

Commit the source change first, then:

    scripts/refresh-lhc-patch.sh   # git diff $(cat patches/BASE) HEAD -- crates/codegen/ Cargo.toml
    git add patches/0001-lhc-touchpoints.patch && git commit

The script reads `BASE`, diffs it against the **committed** HEAD over the
fixed scope, and writes the one patch file. It never writes `BASE` and never
stages anything. An empty diff is an empty file — ordinary `git diff`
output, nothing special-cased.

**`BASE` moves only in an upstream sync** (FORK.md "Sync drill" step 6):
write the merged `upstream/main` commit into `patches/BASE` by hand in the
sync commit, then refresh. Never rewrite `BASE` as part of a routine
refresh, and never derive it from a local branch — that is how the recorded
base drifted to "whatever a developer's `main` was".

**The path list is DERIVED, never hand-maintained.** The scope is the two
pathspecs, so the file list is whatever differs under them — the invariant
this file used to merely assert ("the list must equal
`git diff --name-only BASE -- crates/codegen/ Cargo.toml`") is structurally
guaranteed instead of checked. It broke silently twice while
hand-maintained (dropped five touchpoints after Chunk 2; dropped the root
`Cargo.toml` workspace entry by regenerating a single commit).

**Deliberately excluded** — do not add them:
- `crates/lhc/**` — fork-owned; the drill re-adds that directory wholesale
  (submodule + adapter), so patching it would be redundant and enormous.
- `Cargo.lock` — regenerate with `cargo check` after applying.
- `FORK.md`, `patches/`, `scripts/**`, `lhc-docs/**`, `lhc-release/**`,
  `.github/workflows/**`, `.gitignore`, `.gitmodules` — fork-owned, copied
  whole in recovery. (`crates/codegen/xai-grok-update/src/lhc_release.rs` is a
  fork-owned *new file inside an upstream crate*, so it rides in the patch.)
- Root `README.md` — only the fork banner differs; re-asserted by hand at
  every sync (FORK.md "Sync drill" step 3), not patched.

**Known fork delta outside the scope, deliberately not covered (slice 2
finding, 2026-09-08):** `crates/build/xai-proto-build/src/lib.rs` (+132/-35
vs BASE) — Windows-safe protoc dependency handling from the 2026-08 sync
line (`a4650096`, `18277e97`, `33385ce5`). It carries no `LHC-HOOK` marker
and is not an LHC touchpoint; the recovery drill would not restore it.
Left as-is for a later slice to decide (upstream it, or widen scope
deliberately); do not add it to the pathspecs silently.

## Upstream files added by slice 1A (2026-09-08)

Three files outside the original hook set entered the patch and stay there
until upstream carries the change: `crates/codegen/xai-grok-shell/src/
extensions/notification.rs` (the `lhc_source_tip: Option<u64>` field on
`CompactionCheckpointFile`, serde default) and two struct-literal test sites
that must name it — `session/helpers/replay.rs` (test mod) and
`session/storage/jsonl/copy_tests.rs`. On the next upstream sync expect
conflicts only if upstream touches that struct or those literals.

## Verifying

Rehearse the drill: disposable worktree at the commit named in
`patches/BASE`, `git apply --3way` the patch, then assert the covered paths
match the candidate (`git diff <candidate> -- crates/codegen/ Cargo.toml`
empty in that worktree), the sentinel count (10/10 `LHC-HOOK` markers), and
the root `Cargo.toml` workspace entry. Rehearsed green 2026-08-06 against
`a5589e9`; 2026-09-08 (slice 2) against `72a61251` from `d3bd799c`.
