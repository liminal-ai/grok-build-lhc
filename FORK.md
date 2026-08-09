# grok-build-lhc — LHC context management for Grok Build

**Visitors / evaluators:** start at
[`lhc-docs/README.md`](lhc-docs/README.md) (what this fork is) and
[`lhc-docs/INSTALL.md`](lhc-docs/INSTALL.md) (build and enable). The root
[`README.md`](README.md) opens with a short fork banner, then upstream’s
README. This file is the **maintenance contract** — touchpoints, sync,
recovery, gating detail.

Fork of [`xai-org/grok-build`](https://github.com/xai-org/grok-build) adding
[LHC](https://github.com/liminal-ai/long-horizon-context) (Long Horizon
Context): event-sourced capture of every session into a per-thread SQLite
record, with banded compaction replacing native auto-compact — full history
preserved and rebuildable at full fidelity. Working branch and default
branch: `lhc`. `main` tracks upstream, untouched.

Status: Chunk 3B harness track in progress (3A accepted); product wiring — config, `/lhc` status/repair, rollout
safety) of the 3-chunk integration
(`long-horizon-context/docs/lhc-rs-port/phase3-grok-build-integration-brief.md`).
Wave B retrieval tools (`get_turns` / `get_messages`) and identity/signature
work are on `lhc`. Capture and serving are gated off by default (`GROK_LHC` /
`[lhc]`). A resolving capture tee is always installed so mid-session `/lhc on`
(A5) can attach. When a session has no worker, its persist path takes **no
registry mutex** — even if other sessions are actively capturing — via a
per-session generation-cached binding
(`aa1_disabled_persist_takes_no_registry_lock_while_other_session_active`).
Steady state: one generation atomic compare; mutex only when registration
actually changes. No I/O, no spawn, no SQLite on that path. Chunk 3B (live
certification) remains.

## Layout

- `crates/lhc/grok-lhc-host/` — the adapter crate (capture mapping, compact
  bridge, ModelCall). Fork-only; workspace member from Chunk 1.
- `crates/lhc/vendor/long-horizon-context/` — submodule, pinned to certified
  commits of the SDK repo's `main` only (Phase 2 acceptance `358c8d1` or
  later; the historical `lhc-rs-port` working branch was retired into `main`
  2026-08-08). Never copy the port in; bump the pin and record it here.
  Current pin: `dd251ec` (certified Wave B code tip — R1–R6 retrieval +
  thinking-signature/provenance schema + clean-tail windows; gate 584).
  Later LHC `main` commit `7493f36` is bookkeeping only and is **not** the
  pin. Wave B Slice 1 advances the pin and host schema compatibility only;
  retrieval tools and host identity/provenance wiring are later slices.
- `patches/` — every core touchpoint as a re-appliable `format-patch` file
  (see `patches/README.md`). The history-reset recovery path.
- `scripts/check-lhc-hooks.sh` — the three-layer tripwire (sentinel count,
  compile, golden smoke). Run after every sync, before every push.

## Touchpoint inventory (every owned core line)

| # | File | Lines | Purpose | Patch |
|---|------|-------|---------|-------|
| 1 | `crates/codegen/xai-grok-shell/Cargo.toml` | `LHC-HOOK 1/10` | dependency on `grok-lhc-host` | _(regen after commit)_ |
| 2 | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs` | `LHC-HOOK 2/10` | wrap persistence in the LHC capture tee (+ inference sampler) | _(regen)_ |
| 3 | `crates/codegen/xai-grok-shell/src/agent/handlers/model_switch.rs` | `LHC-HOOK 3/10` | model / thinking-level change tee | _(regen)_ |
| 4 | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs` | `LHC-HOOK 4/10` | substitute LHC request context after `build_request` (+ live identity for signature gate) | _(regen)_ |
| 5 | `crates/codegen/xai-grok-shell/src/session/compaction.rs` | `LHC-HOOK 5/10` | compact bridge decision (LHC I/O at writer choke) | _(regen)_ |
| 6 | `crates/codegen/xai-grok-shell/src/session/mod.rs` | `LHC-HOOK 6/10` | `mod lhc_inference` declaration | _(regen)_ |
| 6b | `crates/codegen/xai-grok-shell/src/session/lhc_inference.rs` | new file (shell-local LHC inference transport) | _(regen)_ |
| 7 | `crates/codegen/xai-chat-state/src/actor/state.rs` | `LHC-HOOK 7/10` | pending model-call `TokenUsage` + assistant identity for LHC capture (schema v5 / D3 + Wave B) | _(regen)_ |
| 8 | `crates/codegen/xai-chat-state/src/actor/mutations.rs` | `LHC-HOOK 8/10` | stash usage + identity; share identity with Reasoning; consume both on Assistant | _(regen)_ |
| 9 | `crates/codegen/xai-chat-state/src/persistence.rs` | `LHC-HOOK 9/10` | `persist_message_with_provider_usage` side-channel (usage + identity) | _(regen)_ |
| 10 | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs` | `LHC-HOOK 10/10` | shell turn-outcome → LHC `turn_end` facts (schema v5 / G2) | _(regen)_ |
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
`0001` currently covers only the Chunk 1 hooks — do not claim it covers 4–9.

Schema v5 G1 carve-out (hooks 7–9): `xai-chat-state` stashes the last model
call's `TokenUsage` and passes it through a defaulted trait method so the LHC
tee can attach `providerUsage` on the next `assistant_text`.

Wave B identity (hooks 7–9 + hook 4, same markers): host-observed
`provider`/`model`/`api` for one model response. Stamped
**usage-independently** before response items are pushed (`record_response_identity`);
cloned onto preceding `Reasoning` persists; taken once on trailing `Assistant`.
Provider is `xai`; model is resolved `AssistantItem.model_id` when present
(never invented). **API is the frozen sampler-attempt backend** from
`prepare_sampler_for_turn` (the config enqueued via FIFO `UpdateConfig` before
that attempt's `Submit`) — never a later sample of live `sampling_config`
(which can change under concurrent `SetSessionModel`). Normalized to
`chat_completions` / `responses` / `messages`. Each auth-retry /
compact-resubmit freezes its own attempt. Bootstrap/replace re-map never
invents identity. Hook 4 passes that same frozen attempt identity into serve
so opaque `encrypted_content` is re-emitted only when stored identity is
complete and exactly matches the attempt that will submit.

Schema v5 G2 (hook 10): shell `turn.rs` after-turn fan-out delivers
`TurnEndFacts` (outcome fold, `outcomeReason`, ISO `startedAt`/`endedAt`) to
the capture worker. Item-mapped `turn_end` (Assistant-without-tools) is
deferred until facts arrive so a facts-bearing close never becomes a second
event with a different key.

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

**Chunk 3 live certification** is the runbook at
`crates/lhc/grok-lhc-host/LIVE_RUNBOOK.md` (setup, commands, expected
output, failure criteria, evidence, stop/rollback). Do not improvise from
scattered prose — execute that file.

**G2 fixture ruling (do not “regen on diff”):** when a real compacted
Replace body differs from `realistic_post_compact_ctx` /
`writeback_fixture`, **classify first** — (1) expected compaction/profile
variance, (2) different input coverage, or (3) genuine calibration error —
and only then decide. Never regenerate fixtures merely because the
fingerprint differs (MAPPING.md retains the richer H2 fixtures pending
diagnosis). The harness scenario cannot exercise tool-cycle or
typed-runtime discrimination at all (its only tool cycle sits at turn 1
and is compacted away; it never seeds a runtime note or model change), so
regenerating from any harness body would destroy that coverage even if
bands matched.

Layer 3 of `scripts/check-lhc-hooks.sh` runs `golden_smoke`,
`certification`, and `harness_chunk3b` under `--features test-util` and
asserts a nonzero test count for each.

### Scheduled verification (not permanent acceptance)

These are test-sensitivity / harness gaps, not closed product defects. Each
has a **named Chunk 3 live-cert checkpoint**. Do not treat them as forever
waived.

| Blind spot | What verifies it | Checkpoint |
|---|---|---|
| **Hook-2 / hook-3 session-id coupling** — today only by inspection (`session_id.0.as_ref()` at both sites). A mismatched id would silently drop model changes with no adapter-suite failure. | **Harness discharged:** `b2_hook2_hook3_session_id_coupling_no_cross_leak` (spawn / resume / fork, no cross-leak). Live: confirm on real ACP session ids. | **Chunk 3B harness** + live confirm |
| **Async guards catch panicking but not silent blocks** — `chunk2_async_guard_out_of_thread_watchdog` is the corrected attempt; treat as open until CI proves a hang fails fast. | **Harness narrowed:** out-of-thread hangs detectable via `recv_timeout`; in-task async silent awaits still need an external controller. | **Chunk 3B harness** (narrowed) + live/CI |
| **G2 live write-back body harness** — fixtures today mirror the realistic post-compact *view* shape; end-to-end shell Replace write-back capture was deferred. | **Harness discharged** (deterministic). **Real-inference G2:** `l3_g2_real_inference_writeback_body_vs_fixture` (bands>0 under production params). Live: [`LIVE_RUNBOOK.md`](crates/lhc/grok-lhc-host/LIVE_RUNBOOK.md) L1. **Do not regen fixtures on diff** — classify first. | **Chunk 3B** + live runbook |
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

## Sync record

### 2026-08-09 — Wave B Slice 3 validation R2: final-output fidelity + open readiness

Correction on top of Slice 3 retrieval tools (still pin `dd251ec`; `main`
stays `8a14c91`). No new LHC-HOOK markers (still 10). No SDK/vendor edits.

- **Final tool-result fidelity:** `handle_bridge_tool_success` exempts exactly
  `get_turns` / `get_messages` from base64/PDF extraction and worktree
  `PathRewriter`. SDK historical envelopes (including `data:image` /
  `data:application/pdf` and plain + URL-encoded real cwd strings) stay
  byte-for-byte; no deferred live image follow-ups. Unrelated tools still
  extract/rewrite.
- **Archive-open readiness:** capture still registers pre-open (turn buffer
  preserved), but a bounded `CaptureOpenState` rendezvous (`Pending` /
  `Ready` / `Failed`) gates retrieval tool publication and `/lhc on` success.
  Spawn, `/lhc on`, and agent rebuild wait for open success (not mere
  `capture_active`). Retrieval calls fail explicitly while open is pending
  (`NotReady`) rather than queue forever. `/lhc off` always unregisters both
  retrieval definitions (stale cleanup after refused/provisional open).
  Registration is all-or-none (rollback on partial failure).

### 2026-08-09 — Wave B Slice 3: retrieval tools (`get_turns` / `get_messages`)

Smallest native Grok integration for history pull (still pin `dd251ec`;
`main` stays `8a14c91`). No new LHC-HOOK markers (still 10). No SDK/vendor
edits.

- Host (`grok-lhc-host`): strict arg validation, order-preserving dedupe before
  32-id cap, SDK `format::*` assembly, capture-worker commands
  (`GetTurns` / `GetMessages` / `ListImpressions`) so retrieval shares the
  per-session SDK owner with capture/compaction (no independent thread-DB open).
- Shell: direct `xai_tool_runtime::Tool` tools registered on ToolBridge only
  while capture is active (spawn if tee opened, `/lhc on`, agent rebuild);
  unregistered on `/lhc off`. Session id bound at registration; inactive /
  cross-session resolve fails explicitly with zero cross-access.
- **Output-cap audit:** tool result → `finalize_output` → `prompt_text` →
  `ConversationItem::tool_result` has **no** universal host truncation for
  plain Text tools. `byte_budget` is **not** passed (token budget 8000 only).
  MCP/`use_tool`/bash caps do not apply to this path.

### 2026-08-09 — Wave B Slice 2: host-observed identity + safe signature replay

Identity/signature slice only (still pin `dd251ec`; `main` stays `8a14c91`).
No retrieval tools; no SDK/vendor source changes. No new LHC-HOOK markers —
extends hooks 7–9 (side channel) and hook 4 (live identity on serve).

- Live capture: provider `xai`, resolved `AssistantItem.model_id`, **frozen
  attempt** `ApiBackend` label → same identity on `assistant_thinking` +
  `assistant_text` for one response (usage-independent; no cross-response leak).
- Exact-attempt freeze: `prepare_sampler_for_turn` returns identity from the
  config it pushes into the sampler; hook-4 gate and response stamp both use
  it so a mid-flight `SetSessionModel` cannot mislabel provenance or admit
  encrypted reasoning for the wrong backend.
- Serve/write-back: re-emit `encrypted_content` only when stored identity is
  complete and matches frozen attempt identity; otherwise visible reasoning
  without ciphertext. Restore stored model onto `AssistantItem.model_id`.
- Bootstrap/replace: never invent identity from current config.
- Maintenance: pin-drift check in `scripts/check-lhc-hooks.sh` tracks
  `origin/main` (not retired `origin/lhc-rs-port`) so `dd251ec` is clean.

### 2026-08-09 — Wave B Slice 1: vendor pin `c136899` → `dd251ec`

Submodule-only certified code tip advance (not a main/upstream merge).
`main` remains at `8a14c91`. Host compatibility for the new SDK surface:

- `assistant_thinking.signature` ← host `Reasoning.encrypted_content` (R2)
- session-view `thinkingSignature` → `encrypted_content` on serve
- struct literals for new optional view fields (`thinking_signature`,
  assistant `provider`/`model`/`api`) — values left `None` until Slice 2
- golden payload decode: `AssistantThinkingPayload` (no longer plain
  `TextPayload`)

**Not in Slice 1:** retrieval tool wiring, host model-identity/provenance
capture for signature replay, any new LHC-HOOK markers.

### 2026-08-09 — `a5589e9..8a14c91` (3 squash commits, ~37k insertions)

Wave B slice 0. Ancestry intact (no history reset). `git merge upstream/main`
completed with **zero conflicts** — ort auto-merged every fork-touched file
(including root `Cargo.toml`, which retained `crates/lhc/grok-lhc-host` in
sort order after the common members). No hand resolution required; no LHC
behavior weakened or deleted. All 10 `LHC-HOOK` markers and Chunk 3A
surfaces (`slash_commands` / `slash_exec` / `[lhc]` config) verified present.

Auto-merged upstream structural noise on shared files (no fork logic change):
`mcp_strategy` → `Cell`, `delivery_tools` / `attach_non_interactive` fields,
`managed_mcp_expires_at` removal, `workflow_max_concurrent_agents`, shell
version `0.2.120` → `1.0.0`.

Vendor pin **unchanged** at `c136899` (Wave B slice 0: do not bump).

Tripwire: ALL GREEN (sentinel 10/10, compile, fmt ×2, unit 164, golden 5,
certification 97, chunk3b 10; pin-policy WARN expected for side-branch pin).
`patches/BASE` advanced to `8a14c91`; state-diff regenerated.

### 2026-08-06 — `6e38642..a5589e9` (11 squash commits, ~119k insertions)

First real upstream sync on `lhc`. Ancestry intact (no history reset). Four
conflicts, all in fork-touched files, resolved per contract:

- `session/mod.rs` — took upstream's `pub(crate)` on `inference_metrics`,
  kept hook 6.
- `session/compaction.rs` — hook 5 writer choke stays first; upstream's new
  compaction cancel scope (`self.compaction.cancel.enter()`) enters after it,
  on the native path only.
- `session/slash_commands.rs` — kept `LhcSlashOp` block (upstream side empty).
- `config/tests.rs` — kept both our `[lhc]` config tests and upstream's new
  `from_remote_gated_requires_xai_auth_for_writeback`.

Sync repairs (in the merge commit):

- `SamplingError::Auth` became a struct variant → `{ .. }` match in
  `lhc_inference.rs`; visibility tightened to `pub(crate)` for upstream's
  `unreachable_pub` lint.
- Upstream `TokenUsage` gained `cache_creation_prompt_tokens` — production
  mapping serializes verbatim (field flows through automatically); adapter
  test initializer + assertion extended.
- Upstream's own `tool_layer_images_bridge_tests.rs` (squash `ed6d543`)
  landed without `use base64::Engine` in scope — does not compile as
  published; added the function-local import (upstream bug, not fork drift).

Vendor pin advanced `614543a` → `a3deafd` in the same sync. Tripwire: ALL
GREEN (sentinel 10/10, compile, fmt ×2, unit 164, golden 5, certification 97,
chunk3b 10, pin policy). Patch series regenerated (first regen to include the
G1/G2 hook commits).

## Upstream model (read before your first sync)

Upstream publishes via daily monorepo squash-syncs — every commit is
"Synced from monorepo" (6.7k–57k line diffs), external PRs are not accepted,
and **history may be rewritten or reset without notice**. Ancestry is a
convenience here, not a guarantee: the patch series + this file are the
durable representation of the fork.

## Sync drill (weekly or on-need, not per upstream commit)

1. `git fetch upstream && git checkout lhc && git merge upstream/main`
2. Expected recurring conflict: root `Cargo.toml` is auto-generated and
   sorted; our `crates/lhc/grok-lhc-host` members entry will collide.
   Resolution rule: take upstream's list, re-add our single entry in sort
   order. Never hand-resolve anything else in that file.
3. **Re-assert the root `README.md` fork banner.** Upstream rewrites the
   README often. The blockquote at the top of `README.md` (starts with
   `This is a maintained fork of`, links `lhc-docs/` and
   `liminal-ai/long-horizon-context`, ends with `Everything below is
   upstream's README`) is fork-owned. After a merge, if the banner is
   missing or mangled, restore it from the previous `lhc` tip or from
   `lhc-docs/` intent — do not leave default-branch `README.md` as pure
   upstream. `lhc-docs/**` and `FORK.md` are fork-only; they should not
   conflict with upstream.
4. `scripts/check-lhc-hooks.sh` — all layers green.
5. Fast-forward `main` to `upstream/main`.
6. **Advance the patch base.** `patches/BASE` names the upstream commit the
   state diff is generated from; a merge moves the tree past it. Rewrite
   `BASE` with the new `main` and regenerate `0001-lhc-touchpoints.patch`
   (`patches/README.md`). This step is **part of the sync**, not cleanup
   after it — the codex-lhc fork learned this the hard way (its `patch-repro`
   gate failed at the first real sync for exactly this omission).
7. Push both branches.
8. Sync commit body records: upstream range, tripwire results, smoke verdict,
   and whether the README banner was re-applied. Append an entry to the Sync
   record section above.

## History-reset recovery drill

If `git merge upstream/main` reports unrelated histories or the diff is
implausibly large, upstream reset. Do not merge. Instead:

1. Fresh clone of new upstream; branch `lhc` from its tip.
2. Copy `crates/lhc/` (or re-add the submodule + adapter), `patches/`,
   `scripts/check-lhc-hooks.sh`, `FORK.md` from the old tree — these are
   fork-owned, upstream never touches them.
3. `git apply --3way patches/0001-lhc-touchpoints.patch` — one state diff
   from `patches/BASE` (model changed 2026-08-06; see patches/README.md).
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
- `GROK_LHC_INFERENCE_MODEL` / `[lhc].inference_model` overrides the derivation
  inference model. **Default is `grok-4.5`** (never the session chat model).
  Thinking is fixed at **low** (`ReasoningEffort::Low`). **Scoped ruling:**
  real inference applies to lanes that call inference — today
  `WorkKind::PromptSmoothing → smoothed_prompt` via
  `ShellLhcInferenceSampler`. `WorkKind::ToolResultSummary →
  tool_result_summary` keeps the vendored truncate-fallback
  (**DERIV-12**: inference at intake rate clogged the queue; classifier path
  dormant pending a high-speed lane —
  `FORCE_TOOL_RESULT_SUMMARY_FALLBACK = true` at
  `…/lhc-rs/src/messages/internal/handlers.rs:35`; call site `opts: None` at
  `:537`). Not unresolved.
  Details: `crates/lhc/grok-lhc-host/MAPPING.md` (Derivation lanes).
- SDK mode is **Background** unconditionally (pi-lhc / t3code shape). Host
  drain surface: capped `drainSettled` at session close only. Compact never
  waits on derivation.
- `/lhc` slash command: status / health / repair / per-session on / off.
- Live cert checklist: `crates/lhc/grok-lhc-host/LIVE_RUNBOOK.md`.

## Rollback runbook (Chunk 3A)

A fresh agent can disable LHC without losing the fork:

1. **Immediate (this session):** `/lhc off` — stops capture (unregisters
   immediately; short background settle at worker close); active context
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
