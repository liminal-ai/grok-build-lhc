# Chunk 3B — live certification runbook

**Product of Chunk 3B for Lee.** Run from the **repo root**
(`/srv/work/grok-build` or your clone). Do not improvise from scattered
FORK/MAPPING prose. Each item is runnable end-to-end.

**Standing rules (read before L1)**

- **Scoped inference ruling:** real inference (`grok-4.5` @
  `ReasoningEffort::Low`, native `ShellLhcInferenceSampler`) applies to
  derivation lanes that call inference. **Today that is PromptSmoothing
  only.** ToolResultSummary uses the vendored **truncate-fallback** and will
  show **no sampler op** — that is correct, not a live failure. (Port:
  `FORCE_TOOL_RESULT_SUMMARY_FALLBACK = true` at
  `crates/lhc/vendor/long-horizon-context/packages/lhc-rs/src/messages/internal/handlers.rs:35`;
  call site `opts: None` at `:537`. Details in MAPPING.md.)
- When credentials are missing for a real-inference step: **stop and report
  BLOCKED** — do not substitute a shim.
- Production compact uses `params: None` (continuation profile:
  `lower_bound=120000`, full share 30% ⇒ **full budget 36000 tokens**). Do not
  shrink `ViewCompactParams` to force bands in live cert.
- **G2 fixture ruling:** on a body/fixture difference, **classify first**
  (expected compaction/profile variance | different input coverage | genuine
  calibration error). Never regenerate fixtures merely because the fingerprint
  differs. Input-coverage gaps on the harness/G2 seed are accepted (not a
  regen trigger).

**How Replace is armed (needed by L1–L5)**

```bash
export GROK_LHC=1
export GROK_LHC_COMPACT=replace
export GROK_LHC_COMPACT_EXPERIMENTAL=1   # required; replace alone stays Shadow
# optional: export GROK_LHC_ROOT=/tmp/lhc-live-$USER
# leave GROK_LHC_INFERENCE_MODEL unset → default grok-4.5
```

Then start the shell the way you normally do for this fork. Confirm with
`/lhc status` (capture active, compact=replace, inference model shown).

**Global stop / rollback**

1. Immediate: `/lhc off` in the session (unregisters immediately; worker
   settles in-flight background work under a short `drainSettled` cap —
   not a multi-minute catch-up bill; status becomes capture stopped /
   engine native).
2. Process-wide: unset `GROK_LHC` (and `[lhc] enabled` in config); restart.
3. Flag-off is the rollback at every point — do not leave Replace armed after a
   failed live item unless the failure criteria say to keep evidence.

---

## L1 — Real-model Replace body vs fixtures (G2)

| Field | Value |
|---|---|
| **Setup** | Repo root. Readable `~/.grok/auth.json` (live bearer). Env as in “How Replace is armed”. No `GROK_LHC_INFERENCE_MODEL` override. |
| **Commands** | `cargo test -p xai-grok-shell --lib l3_g2_real_inference_writeback_body_vs_fixture -- --ignored --nocapture` |
| **Cadence** | Before each Phase-3 sign-off; after any derivation-lane / sampler / compact-drain change. **Not** in the tripwire (credentials + minutes of network). |
| **Scenario** | Automated seed ≥ continuation full budget (**6×5000 words/msg**, ≈60k tokens) loaded as **one history batch** at `spawn_capture` — not paced turns with Background gaps. Test calls production `replace_compact_for_writeback` (selection-only; compact never drains). |
| **Expected** | Probe: PromptSmoothing = production lane; ToolResultSummary line labeled **direct-call capability only** (**DERIV-12** — not a production drain lane). Compact **fast** (≪ 30s; typically fractions of a second). **`bands > 0`**. Degraded rungs **reportable** (print rung + `receipt.degraded` / gaps) — not a hard fail. Five hard gates + B8.3. Fingerprint vs fixture: match or classified FINDING. |
| **Failure** | Missing auth / probe fail → **BLOCKED** (no shim). `bands == 0` → **M1 FAIL**. Compact wall-time still in hundreds of seconds → drain wrongly back on compact (architecture regression). Do **not** fail on `[degraded:…]` body markers when the receipt records the rung. Do **not** fail because ToolResultSummary produced no production sampler op (DERIV-12). |
| **Evidence** | `--nocapture` log: seed tokens, compact wall time, degraded report + receipt, body + fixture fingerprints, bands, gate PASS lines for `credentialed-real-inference-body`, B8.3 calibration line. |
| **Stop** | On BLOCKED/M1 FAIL/architecture regression: stop G2; do not regenerate fixtures; file numbers. |
| **L1 does not prove** | Between-turn Background settling. The G2 seed is one batch; any queue-settle waits elsewhere are harness artifacts. What *does* prove paced settle: adapter cert `drain_architecture_background_mode_cert_measurements` (deterministic) and a **real interactive session** (watch `/lhc status` ready≈total between turns — see Performance notes). |

**Budget probe (no credentials, adapter only):**  
`cargo test -p grok-lhc-host --features test-util --test certification m1_production_params_none_requires_seed_above_full_budget -- --nocapture`

**Background-drain counting proof (no credentials):**  
`cargo test -p grok-lhc-host --features test-util --test certification n1_background_drain_prompt_smoothing -- --nocapture`  
Expect SmoothPrompt ops **before** compact; ToolResultSummary absent (**DERIV-12**); compact wall ≪ 5s.

**Drain-architecture cert (no credentials):**  
`cargo test -p grok-lhc-host --features test-util --test certification drain_architecture_background_mode_cert -- --nocapture`

**Turn-abort proofs (no credentials):**  
`cargo test -p grok-lhc-host --features test-util --test certification r3_abort_installs_no_compact_snapshot -- --nocapture`  
— no snapshot install; derivation may continue.  
`cargo test -p grok-lhc-host --features test-util --test certification r1_cancel_at_snapshot_write -- --nocapture`  
— CompactWrite abort → no LHC snapshot (`signal` live atomic).

### Performance notes (expected cost, not bugs)

- **Background drain (repair 2026-07-26):** derivation cost is paid between
  turns, not at compact. Healthy evidence: ready≈total at threshold; compact
  wall-time fractions of a second. The old ~400 s compact-time drain was the
  defect. Degraded bands from the fallback ladder are correct when material
  is not yet ready.
- **Natural queue settling (carryable — record only):** harnesses that call
  an explicit `wait_queue_settle` / `drain_settled` prove *that the wait
  works*, not that a live session settles unaided between turns. Only a real
  interactive session can show natural settle — look for `/lhc status`
  `ready≈total` and `queue.queued==0` / `claimed==0` between user turns
  without an artificial wait. Measurement harness
  `lhc_derivation_quality_timing` uses an explicit wait and must not be
  cited as proof of natural settle.
- **Prefix-cache cliff:** derivation sets `prompt_cache_key: None`. Replace
  write-back rewrites the conversation, so a compact **invalidates ~100% of
  the session prefix cache** (Codex measured). Expect a cost cliff on the next
  turn after compact — that is expected, not a regression.
- **Derivation request shape (pinned):** purpose-built ~250-char system prompt,
  empty tools — not the session agent prompt. Pinning test:
  `derivation_request_excludes_session_tools_and_agent_prompt`.
- **Inference reachability:** pi-lhc validates lanes at startup
  (`validateReachable`); this fork discovers unreachable lanes only when a
  derivation fails. Belongs on the live runbook / follow-up — not this repair.

---

## L2 — Shell choke kill with LHC ahead

| Field | Value |
|---|---|
| **Setup** | Live shell with Replace armed (env above). Grow the session until production compact would emit **bands > 0** (past ~36000 estimated tokens — same budget as L1). Have a way to kill the process at hook 5 (`session/compaction.rs` writer choke). |
| **Commands** | Drive conversation past full budget → trigger auto-compact. Kill **after** LHC compact commits and **before** `replace_conversation_for_compaction` returns. Restart shell; reopen the **same** session id / thread files. |
| **Scenario** | LHC SQLite has the compacted view; native conversation is still the pre-replace body (LHC ahead of native). |
| **Expected** | Re-enter Replace write-back once (or retry). Band summary text is **not** double-recorded. Native ends as the single compacted body. Same spirit as adapter cert `writeback_crash_between_lhc_compact_and_native_replace_is_transient`. |
| **Failure** | Double band text in event log / native body; stuck LHC-ahead with no recovery; silent data loss. |
| **Evidence** | Pre-kill native length; post-reopen event keys; post-retry native fingerprint; `/lhc status` before/after. |
| **Stop** | On double-record or unrecoverable ahead state: `/lhc off`, preserve thread SQLite + native session dir, escalate. |

---

## L3 — `/btw` on a compacted session

| Field | Value |
|---|---|
| **Setup** | Live session after a successful Replace write-back (`bands > 0` in the native body; `/lhc status` can show engine LHC after a substituted serve). |
| **Commands** | In the session prompt: `/btw`. Also `/lhc status`. |
| **Scenario** | `/btw` reads native conversation (`crates/codegen/xai-grok-shell/src/session/acp_session_impl/recap.rs`). After write-back that native body **is** the compacted LHC body. |
| **Expected** | Recap runs on the compacted body (context band prose such as `[context` visible in what was summarized). Coherent output; no crash; no request rebuilt from the entire pre-compact history. |
| **Failure** | Recap sees pre-compact full history; empty/error; engine label wrong. |
| **Evidence** | Recap transcript; `/lhc status`; note that native body contained `[context`. |
| **Stop** | Misbehaviour → reopen as its own decision (FORK.md); do not add a `/btw` hook without a ruling. |

---

## L4 — Memory flush on a compacted session

| Field | Value |
|---|---|
| **Setup** | Same compacted session as L3; memory enabled for the session (`/memory` / product defaults — if memory is off, enable it first or stop and note BLOCKED). |
| **Commands** | Trigger the product flush path: `/flush` and/or the pre-compaction memory-dream flush (`acp_session_impl/memory_dream.rs`). |
| **Scenario** | Flush reads native conversation; after Replace that is the LHC-compacted body. |
| **Expected** | Flush proceeds on compacted body; no panic; no re-ingest of the entire pre-compact history as if write-back never happened. |
| **Failure** | Flush skipped incorrectly; errors; reads stale pre-replace body when status says compacted. |
| **Evidence** | Flush log lines; native fingerprint at flush time; memory store artifacts if any. |
| **Stop** | Same as L3 — product decision, not a silent harness waiver. |

---

## L5 — Equivalence under real traffic

| Field | Value |
|---|---|
| **Setup** | Replace armed; equivalence left at default (armed). Multi-turn live chat with at least one successful Replace behind it. |
| **Commands** | Continue normal prompts after compaction. Adapter baseline (optional): `cargo test -p grok-lhc-host --features test-util --test certification equiv_ -- --nocapture`. Live: `/lhc status` and logs for equivalence counters when exposed. |
| **Scenario** | Hook 4 `observe_serve_equivalence` on substituted turns. |
| **Expected** | **Informational divergence stays 0** through the live window. Structural divergences only where already documented (e.g. band-collapse shape). First unexpected informational divergence → Lee. |
| **Failure** | Any new informational divergence under real traffic; silent fallback spike without diagnosis. |
| **Evidence** | Equivalence snapshot counters; turn ids; native vs served fingerprints for the divergent turn. |
| **Stop** | First informational divergence: stop feature push; preserve logs; escalate to Lee (MAPPING.md G1). |

---

## L6 — 3A carryables routed to 3B

Execute each; do not mark 3A closed over these.

### L6.1 — Unknown `/lhc` subcommands + `repair confirm` case

| Field | Value |
|---|---|
| **Setup** | Shell running ( `/lhc` is always registered). |
| **Commands** | `/lhc totally-unknown-subcommand` → expect Status or help, not a panic. `/lhc repair` then `/lhc repair confirm <id>` vs `/lhc repair CONFIRM <id>` — record actual case behaviour. |
| **Expected** | Unknown → help/status fallthrough. Confirm case policy explicit (document if asymmetric). |
| **Failure** | Crash; silent no-op with no status; undocumented case asymmetry. |
| **Evidence** | Command transcript. |
| **Stop** | Crash → ship stop. |

### L6.2 — Status early-return asymmetry

| Field | Value |
|---|---|
| **Setup** | Ability to have (a) this session capturing, (b) only another session capturing, (c) none. Two shell sessions or `/lhc off` / `/lhc on` on one. |
| **Commands** | `/lhc status` in each case. |
| **Expected** | No false “healthy LHC” / active capture for a session that has no worker. Session-local vs process-wide gates match MAPPING or a documented fix. |
| **Failure** | Status claims LHC active for a session with no capture; or hides active capture. |
| **Evidence** | Status text for each case. |
| **Stop** | Misleading engine label → treat as P0 UX. |

### L6.3 — Config refresh on `/new`

| Field | Value |
|---|---|
| **Setup** | Live multi-worker runtime; `[lhc]` present in `~/.grok/config.toml` (or env). Another session mid-turn if possible. |
| **Commands** | `/new` (path that hits `refresh_settings_and_reapply` in `agent_ops.rs`) while work is in flight. |
| **Expected** | No data race / env corruption; LHC config remains coherent. |
| **Failure** | Panic, torn config, cross-session bleed. |
| **Evidence** | Stack trace / status before-after. |
| **Stop** | Race observed → disable mid-session config refresh until fixed. |

### L6.4 — Telemetry / Replace recoverability (spot checks)

| Field | Value |
|---|---|
| **Setup** | LHC on; telemetry as in product. |
| **Commands** | Confirm LHC SQLite paths/contents are not uploaded as chat telemetry. After Replace, confirm native session files remain recoverable under FORK.md rollback. |
| **Expected** | Matches MAPPING 3A claims; no SQLite blob in upload. |
| **Failure** | Store path or contents in telemetry; native unrecoverable after Replace without documented restore. |
| **Evidence** | Telemetry payload sample (redacted); session dir listing. |
| **Stop** | Privacy failure → immediate `/lhc off` + escalate. |

---

## Adapter / harness (not a substitute for live)

Run from repo root:

| Suite | Command | Role |
|---|---|---|
| Unit | `cargo test -p grok-lhc-host --lib --features test-util` | Adapter |
| Cert | `cargo test -p grok-lhc-host --test certification --features test-util` | Adapter gates + M1 budget + N1 scoped drain proof |
| Harness 3B | `cargo test -p grok-lhc-host --test harness_chunk3b --features test-util` | Mechanism (deterministic cbs) — gates labeled `deterministic-harness-body` |
| Real G2 | L1 (`-- --ignored`) | Derivation **content** + gates labeled `credentialed-real-inference-body` |
| Tripwire | `./scripts/check-lhc-hooks.sh` | 6/6 hooks, Layer 3 suites |

Mechanism harness **must not** be described as real-inference G2.
