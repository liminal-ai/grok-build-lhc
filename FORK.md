# grok-build-lhc — LHC context management for Grok Build

Fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build) adding
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context): event-sourced capture of every session into a per-thread SQLite
record, with banded compaction replacing native auto-compact — full history
preserved and rebuildable at full fidelity. Working branch and default
branch: `lhc`. `main` tracks upstream, untouched.

Status: Chunk 3A (product wiring — config, `/lhc` status/repair, rollout
safety) of the 3-chunk integration
(`long-horizon-context/docs/lhc-rs-port/phase3-grok-build-integration-brief.md`).
Capture and serving are gated off by default (`GROK_LHC` / `[lhc]`). A resolving
capture tee is always installed so mid-session `/lhc on` (A5) can attach. When
a session has no worker, its persist path takes **no registry mutex** — even if
other sessions are actively capturing — via a per-session generation-cached
binding (`aa1_disabled_persist_takes_no_registry_lock_while_other_session_active`).
Steady state: one generation atomic compare; mutex only when registration
actually changes. No I/O, no spawn, no SQLite on that path. Chunk 3B (live
certification) remains.

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
| 1 | `crates/codegen/xai-grok-shell/Cargo.toml` | `LHC-HOOK 1/6` | dependency on `grok-lhc-host` | _(regen after commit)_ |
| 2 | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs` | `LHC-HOOK 2/6` | wrap persistence in the LHC capture tee (+ inference sampler) | _(regen)_ |
| 3 | `crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs` | `LHC-HOOK 3/6` | model / thinking-level change tee | _(regen)_ |
| 4 | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs` | `LHC-HOOK 4/6` | substitute LHC request context after `build_request` | _(regen)_ |
| 5 | `crates/codegen/xai-grok-shell/src/session/compaction.rs` | `LHC-HOOK 5/6` | compact bridge decision (LHC I/O at writer choke) | _(regen)_ |
| 6 | `crates/codegen/xai-grok-shell/src/session/mod.rs` | `LHC-HOOK 6/6` | `mod lhc_inference` declaration | _(regen)_ |
| 6b | `crates/codegen/xai-grok-shell/src/session/lhc_inference.rs` | new file (shell-local LHC inference transport) | _(regen)_ |
| — | root `Cargo.toml` | workspace-members entry `crates/lhc/grok-lhc-host` (no marker — auto-generated/sorted; asserted in `scripts/check-lhc-hooks.sh`) | adapter workspace membership | _(regen)_ |

### Chunk 3A authorized touchpoints (not LHC-HOOK markers)

These are Phase-3-brief “status/inspect/repair / config” surfaces. They are
**not** runtime hooks 1–6 and do **not** change `EXPECTED_HOOKS`. Sentinel
stays at 6. Include them in `patches/` regen after commit (Lee) so history-
reset recovery restores the slash/config wiring.

| # | File | Purpose |
|---|---|---|
| C3A-1 | `.../session/slash_commands.rs` | `BuiltinAction::Lhc` + `/lhc` parser + telemetry/mutating arms |
| C3A-2 | `.../session/acp_session_impl/slash_exec.rs` | `/lhc` handler (status/health/repair/on/off) |
| C3A-3 | `.../config/mod.rs` + `agent/config.rs` | `[lhc]` `LhcConfig` + `resolve_and_apply` in runtime resolution |

Rules: hooks are 1–5 line additive insertions marked
`// LHC-HOOK <n>/<total>: <purpose>`; the sentinel total in
`scripts/check-lhc-hooks.sh` and this table change in the same commit as any
hook; each hook is regenerated into `patches/` after commit (Lee). Patch
`0001` currently covers only the Chunk 1 hooks — do not claim it covers 4–6.

Carve-out (post-`cargo fmt` numstat vs `origin/main` / pre-hook tree):

| Hook file | `git diff --numstat` |
|---|---|
| `spawn.rs` (hook 2) | `+19 / -3` |
| `model_switch.rs` (hook 3) | `+9 / -0` |
| `turn.rs` (hook 4) | `+48 / -0` |
| `compaction.rs` (hook 5) | `+181 / -1` |
| `mod.rs` (hook 6) | `+2 / -0` |
| `lhc_inference.rs` (new) | `+222 / -0` |
| `rewind_cross_compaction_tests.rs` (H3 regression) | `+224 / -0` |

Hook 2 wraps persistence with `tee_chat_persistence(..., sampler)`. Hook 3
forwards previous model / thinking level. Hook 4 substitutes LHC request
context after `build_request` (gated on `any_capture_active` then
`capture_active` — no registry mutex when LHC is off) and runs
**instrumented-redundant** equivalence observation (G1; see MAPPING.md).
Hook 5 is the auto-compact / writer choke; Replace-mode write-back lands
here via `replace_conversation_for_compaction`. Hook 6 declares the
shell-local inference transport module.

### Full-conversation consumers that ride native state (do not hook)

After Replace write-back, native conversation **is** the LHC-compacted body,
so these readers are expected to see the LHC view without dedicated hooks.
Recorded so nobody rediscovers them as missing touchpoints:

| Consumer | Location | Notes |
|---|---|---|
| `/btw` recap | `.../session/acp_session_impl/recap.rs` | Reads native conversation |
| Memory flush | `.../session/acp_session_impl/memory_dream.rs` | Reads native conversation |

**Chunk 3 live certification must:**

1. Run an explicit `/btw` check and an explicit memory-flush check on a
   **compacted** session, confirming both receive the LHC body and behave
   coherently. If either misbehaves, reopen as its own decision.
2. **G2:** capture the real body the shell write-back delivers from an
   actual Replace compaction (item shapes, ordering, system prefix,
   `prompt_index` markers) and diff it against the certification fixtures
   in `grok-lhc-host` (`realistic_post_compact_ctx` / `writeback_fixture`).
   Regenerate fixtures if the live body differs.

Layer 3 of `scripts/check-lhc-hooks.sh` runs both `golden_smoke` and
`certification` under `--features test-util` and asserts a nonzero test count
for each.

### Scheduled verification (not permanent acceptance)

These are test-sensitivity / harness gaps, not closed product defects. Each
has a **named Chunk 3 live-cert checkpoint**. Do not treat them as forever
waived.

| Blind spot | What verifies it | Checkpoint |
|---|---|---|
| **Hook-2 / hook-3 session-id coupling** — today only by inspection (`session_id.0.as_ref()` at both sites). A mismatched id would silently drop model changes with no adapter-suite failure. | Live cert: drive a model/thinking change on a captured session and assert the change event lands in that session's LHC thread (proves the registered id matches the tee). Same checkpoint as write-back body capture. | **Chunk 3 live cert** (with G2) |
| **Async guards catch panicking but not silent blocks** — `chunk2_async_guard_out_of_thread_watchdog` is the corrected attempt; treat as open until CI proves a hang fails fast. | Re-run / strengthen the out-of-thread watchdog under Chunk 3 CI; hang must fail the test within the watchdog budget. | **Chunk 3 live cert / CI** |
| **G2 live write-back body harness** — fixtures today mirror the realistic post-compact *view* shape; end-to-end shell Replace write-back capture was deferred. | **Mandatory:** run an actual Replace compaction through the shell write-back path; capture item shapes, ordering, system prefix, and `prompt_index` markers; diff against `realistic_post_compact_view` / `writeback_fixture`. If the live body differs, **regenerate fixtures from the real body and re-run the gate**. | **Chunk 3 live cert** |
| **Banded LHC-ahead crash window (shell kill)** — adapter half is covered by `writeback_crash_between_lhc_compact_and_native_replace_is_transient` (deterministic cbs + tight `ViewCompactParams` + multi-turn seed → typed bands → write-back retry idempotent). Remaining: live shell Replace choke kill **before** `replace_conversation_for_compaction`. | Live cert: kill at the shell choke; reopen old native; confirm same idempotency. | **Chunk 3 live cert** (shell kill only) |

Also at that same Chunk 3 checkpoint: `/btw` + memory-flush on a compacted
session (see full-conversation consumers above), and hook-4 equivalence
informational divergence must stay zero (else first finding → Lee).

### Obstruction (Lee) — banded LHC-ahead crash window — DISCHARGED (adapter)

**Round 9 finding:** deterministic callbacks alone are **not** enough on the
production choke (`LhcSession::compact` uses `params: None` + Mock sampler +
short fixture → successful no-op, zero empty-source bands). Bands become
reachable when the test drives the **public SDK** with all three of:

1. `create_deterministic_inference_callbacks()`
2. multi-turn seed (≥12 closed turns with enough tokens)
3. tight `ViewCompactParams` (`lower_bound: Some(400.0)` + band percentages)

**Adapter discharge:** certification
`writeback_crash_between_lhc_compact_and_native_replace_is_transient` now
does that, opens capture on the banded thread with an old native body
(native replace never ran), write-back once + retry, asserts band summary
text is not double-recorded. Probe:
`n3_deterministic_callbacks_with_tight_params_can_emit_typed_bands`.

**Still Chunk 3 (shell only):** kill the live Replace choke after LHC
compact commits and **before** `replace_conversation_for_compaction`, then
reopen the real host with old native and confirm the same idempotency.
The production choke still uses `params: None`; live Replace must produce
bands under real budgets for that kill test to be meaningful.

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
  else leaves the host bit-identical (tee not installed). `[lhc] enabled =
  true` in config.toml also enables when env is unset (env wins when set).
- `GROK_LHC_ROOT` / `[lhc].root` overrides the LHC storage root (default
  `~/.lhc`) for tests.
- `GROK_LHC_COMPACT=replace` alone does **not** enable Replace. Requires also
  `GROK_LHC_COMPACT_EXPERIMENTAL=1` (or `[lhc] compact_experimental = true`).
  Without it, mode stays Shadow. When Replace is active, successful compact
  **writes back** into native state (`replace_conversation_for_compaction`) —
  ruled architecture, not a workaround (see
  `crates/lhc/grok-lhc-host/MAPPING.md`).
- `GROK_LHC_INFERENCE_MODEL` / `[lhc].inference_model` selects a dedicated
  non-main model slug for LHC ModelCall; unset → session model with refreshed
  credentials per call.
- `/lhc` slash command: status / health / repair / per-session on / off.

## Rollback runbook (Chunk 3A)

A fresh agent can disable LHC without losing the fork:

1. **Immediate (this session):** `/lhc off` — stops capture; active context
   engine becomes **native**. LHC SQLite is kept; native conversation is not
   rewritten by this step.
2. **Process-wide:** unset `GROK_LHC` (and remove `[lhc] enabled = true` from
   config.toml if present). Restart the shell. Spawn installs only a resolving
   tee that no-ops via `any_capture_active` (no worker, no SQLite) — host
   behavior matches pre-LHC.
3. **After a Replace compact:** native RAM/persisted body may already be the
   LHC-compacted conversation. Full pre-compact native body is **not**
   guaranteed in RAM; LHC thread SQLite retains full event history.
   Recoverability of older native files → confirm in Chunk 3B live cert.
4. **Verify afterward:** `/lhc` reports off + native engine; `scripts/check-
   lhc-hooks.sh` green; no `GROK_LHC` in the environment for the next
   session.
5. **Optional cleanup (explicit):** `/lhc repair` then `/lhc repair confirm
   delete-thread-db` — deletes only LHC SQLite for that session. Never
   automatic.
