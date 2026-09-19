# D sync 2026-09-19 — grok public source 1.0.35

BASE=`a28ee2b2063426e8816e380ccea528b9de95e5da` (1.0.35; Reed-selected. Upstream stable advertised 1.0.34; no public 1.0.34 snapshot; source jumped 1.0.32 → 1.0.35).
Accepted product=`fc7853d2921f8b0b3c84461440d82aa76aa35e83`.
LHC pin=`aa9caa16`. `lhc-release/VERSION` remains `1.0.16`. No tag / version cut / cutover.

## Hosted sandbox — 3 exact tests (no ignores / no exemption / no product fix)

Candidate `fc7853d2` run **35433358472**: 34 selected, **31 pass / 3 fail**, retries 0, `SANDBOX_E2E_REQUIRE_ENFORCEMENT=1`.
Vanilla harness-only `0622242b261f354b0e56d19539f3167b31da0d77` run **35434361418**: 3 selected, **0 pass / 3 fail**. Identical EACCES text.

| identity | cause |
|---|---|
| `xai-grok-sandbox::integration_test` `test_profile_capability_set_construction` | ReadOnly CapabilitySet: `/run/podman/podman.sock` Permission denied (os error 13) |
| `xai-grok-sandbox::deny_paths_e2e` `read_deny_empty_set_verifies_inside_bwrap` | same resolver EACCES → `bwrap_reexec_for_profile` returned None |
| `xai-grok-sandbox::deny_paths_e2e` `devbox_genuine_reexec_applies_enforcement` | `bwrap_reexec_for_profile` returned None outside bwrap |

Cause: upstream resolver treats an unreadable `/run/podman/podman.sock` as fatal; GitHub ubuntu-24.04 ships Podman with a root-owned socket. Same three failures on vanilla, so not fork-caused. Future harness handling only; no product change.

## Candidate runtime weighted + local gates

Runtime (isolated grok `34c61b6b1521`, serving **grok-4.5**): compact **view_id=v60 compact_point=48 tail_tokens=42843 receipt_total=47105**; `/lhc status` **tail_tokens=42882**. Persistence stays raw o200k; budget reads weigh 1.05×. Recall AMBER-QUARRY-77 PASS.

Local at `3de477bf` (product-identical to `fc7853d2` except harness YAML): tripwire GREEN 10/10 (lib 188, golden 5, cert 121, chunk3b 11, serving_model_budget 2, pin 0 behind); patch `--3way` to BASE covered tree identical; targeted E2 2, auth 11, rewind 9, rate 7, headless 9.
