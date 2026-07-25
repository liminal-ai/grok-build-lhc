# grok-build-lhc — LHC context management for Grok Build

Fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build) adding
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context): event-sourced capture of every session into a per-thread SQLite
record, with banded compaction replacing native auto-compact — full history
preserved and rebuildable at full fidelity. Working branch and default
branch: `lhc`. `main` tracks upstream, untouched.

Status: Chunk 1 (packaging, session identity, event capture) of the 3-chunk
integration
(`long-horizon-context/docs/lhc-rs-port/phase3-grok-build-integration-brief.md`).
Capture is gated off by default (`GROK_LHC`); host behavior is unchanged until
enabled. Chunks 2–3 (inference adapter / compact bridge / product wiring)
remain.

## Layout

- `crates/lhc/grok-lhc-host/` — the adapter crate (capture mapping, compact
  bridge, ModelCall). Fork-only; workspace member from Chunk 1.
- `crates/lhc/vendor/long-horizon-context/` — submodule, pinned to certified
  commits of the `lhc-rs-port` branch only (Phase 2 acceptance `358c8d1` or
  later). Never copy the port in; bump the pin and record it here.
  Current pin: `e582465`.
- `patches/` — every core touchpoint as a re-appliable `format-patch` file
  (see `patches/README.md`). The history-reset recovery path.
- `scripts/check-lhc-hooks.sh` — the three-layer tripwire (sentinel count,
  compile, golden smoke). Run after every sync, before every push.

## Touchpoint inventory (every owned core line)

| # | File | Lines | Purpose | Patch |
|---|------|-------|---------|-------|
| 1 | `crates/codegen/xai-grok-shell/Cargo.toml` | `LHC-HOOK 1/3` | dependency on `grok-lhc-host` | 0001 |
| 2 | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs` | `LHC-HOOK 2/3` | wrap persistence in the LHC capture tee | 0001 |
| 3 | `crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs` | `LHC-HOOK 3/3` | model / thinking-level change tee | 0001 |
| — | root `Cargo.toml` | workspace-members entry `crates/lhc/grok-lhc-host` (no marker — auto-generated/sorted; asserted in `scripts/check-lhc-hooks.sh`) | adapter workspace membership | 0001 |

Rules: hooks are 1–5 line additive insertions marked
`// LHC-HOOK <n>/<total>: <purpose>`; the sentinel total in
`scripts/check-lhc-hooks.sh` and this table change in the same commit as any
hook; each hook is regenerated into `patches/` in that same commit.

Carve-out (Chunk 1, post-`cargo fmt` numstat vs pre-hook tree):

| Hook file | `git diff --numstat` |
|---|---|
| `spawn.rs` (hook 2) | `+10 / -3` |
| `model_switch.rs` (hook 3) | `+9 / -0` |

Hook 2 hoists `ChannelChatPersistence::new(...)` to a `let` (preserving that
expression) and wraps with a formatted multi-line `tee_chat_persistence(...)`.
Hook 3 adds one capture line for `previous_reasoning_effort` plus a formatted
`capture_model_or_thinking_change(...)` call so the host's previous model /
thinking level are forwarded (no fabricated `"unknown"`). Both exceed the
nominal 1–5-line additive budget once rustfmt expands the call sites; the
counts above are the durable, CI-canonical figures.

Layer 3 of `scripts/check-lhc-hooks.sh` runs both `golden_smoke` and
`certification` under `--features test-util` and asserts a nonzero test count
for each.

### Accepted limitations (Chunk 1 acceptance, orchestrator ruling)

Two verifier findings were adjudicated as accepted rather than fixed. Both are
test-sensitivity gaps, not product defects; both are recorded so a later
maintainer does not mistake them for oversights.

1. **The hook-2 / hook-3 session-id coupling is verified by inspection, not by
   a test.** If `model_switch.rs` ever passed a different id than `spawn.rs`
   registered, every model change would be silently discarded and no test in
   `grok-lhc-host` would fail. The coupling is a property of two call sites in
   `xai-grok-shell`, so the adapter's own suite structurally cannot observe
   it; a test would have to live in the core crate, i.e. a fourth core
   touchpoint. Both ids are `session_id.0.as_ref()` / `session_info.id.0.as_ref()`
   — the same ACP id — confirmed independently by both verifiers. **If you
   ever change either hook's arguments, re-check this by hand.**
2. **The async-convention guards catch panicking blocks, not silent ones.**
   `tee_from_async_context_does_not_block_or_panic` and its `Drop` counterpart
   wrap a `tokio::time::timeout`, but the awaited body contains no suspension
   point — so a *synchronous* block (`JoinHandle::join()`,
   `std::sync::mpsc::recv()`) hangs the test instead of failing it. The guards
   still detect such a regression, just as a hang rather than a fast failure.
   The defect that actually shipped (`blocking_recv` on the production path)
   panics and is caught. Closing this properly needs an out-of-runtime
   watchdog; carried into Chunk 2.

## Upstream model (read before your first sync)

Upstream publishes via daily monorepo squash-syncs — every commit is
"Synced from monorepo" (6.7k–57k line diffs), external PRs are not accepted,
and **history may be rewritten or reset without notice**. Ancestry is a
convenience here, not a guarantee: the patch series + this file are the
durable representation of the fork.

## Sync drill (weekly or on-need, not per upstream commit)

1. `git fetch upstream && git checkout lhc && git merge upstream/main`
2. Expected recurring conflict: root `Cargo.toml` is auto-generated and
   sorted; from Chunk 1 on, our `crates/lhc/grok-lhc-host` members entry
   (patch 0001) will collide. Resolution rule: take upstream's list, re-add
   our single entry in sort order. Never hand-resolve anything else in that
   file.
3. `scripts/check-lhc-hooks.sh` — all three layers green (or golden layer
   SKIP before Chunk 1 lands).
4. Fast-forward `main` to `upstream/main`, push both branches.
5. Sync commit body records: upstream range, tripwire results, smoke verdict.

## History-reset recovery drill

If `git merge upstream/main` reports unrelated histories or the diff is
implausibly large, upstream reset. Do not merge. Instead:

1. Fresh clone of new upstream; branch `lhc` from its tip.
2. Copy `crates/lhc/` (or re-add the submodule + adapter), `patches/`,
   `scripts/check-lhc-hooks.sh`, `FORK.md` from the old tree — these are
   fork-owned, upstream never touches them.
3. `git am patches/*.patch` for every core touchpoint.
4. `scripts/check-lhc-hooks.sh` — green means the fork is whole.
5. Force-update `origin/lhc`; keep the old tree until green.

**Rehearsed 2026-07-25 at Chunk 1** (fork `9ea06ea`), against the raw upstream
tip `6e38642` — a tree with no `crates/lhc/` at all. `git am` applied cleanly;
all three sentinels and the workspace entry were restored. Commands and the
full record are in `patches/README.md`. Re-rehearse whenever a hook changes.
(The Chunk 0 brief required this once before Chunk 3 sign-off; doing it at
Chunk 1 means the first real upstream sync already has a proven fallback.)

## Never run

- `grok upgrade` / any self-update: this is a source checkout; self-update
  would clobber the tree.
- `git merge` after a suspected history reset (see drill above).

## Host obligations toward LHC (from the port's acceptance record)

- Timestamps passed into LHC public APIs must be canonical
  `YYYY-MM-DDTHH:MM:SS(.mmm)Z` (Amendment D ceiling).
- Do not set `SdkConfig.clock` in production (cross-port provenance parity).

## Gating

- `GROK_LHC=1` or `true` enables capture at session spawn; unset / anything
  else leaves the host bit-identical (tee not installed).
- `GROK_LHC_ROOT` overrides the LHC storage root (default `~/.lhc`) for tests.
