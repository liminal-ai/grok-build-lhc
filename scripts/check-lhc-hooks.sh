#!/usr/bin/env bash
# LHC fork tripwires — run after every upstream sync and before every push.
# Three layers (FORK.md "Sync drill"): sentinel count, compile, golden smoke.
# Exit nonzero on any tripped layer. Keep this script dependency-free.
set -u
cd "$(dirname "$0")/.."
command -v cargo >/dev/null 2>&1 || . "$HOME/.cargo/env" 2>/dev/null || true
fail=0

# ── Layer 1: sentinel count ────────────────────────────────────────────
# Every core touchpoint carries a `LHC-HOOK <n>/<total>` marker. The
# expected total lives here and in FORK.md's touchpoint inventory — update
# BOTH in the same commit as any hook change.
EXPECTED_HOOKS=9
actual=$(grep -rl "LHC-HOOK" crates bin prod 2>/dev/null \
  | grep -v "^crates/lhc/" | xargs -r grep -o "LHC-HOOK [0-9]*/[0-9]*" | wc -l)
if [ "$actual" -ne "$EXPECTED_HOOKS" ]; then
  echo "TRIPWIRE sentinel: expected $EXPECTED_HOOKS LHC-HOOK markers, found $actual"
  echo "  A sync dropped or duplicated a hook. Do not push. See FORK.md recovery."
  fail=1
else
  echo "ok sentinel: $actual/$EXPECTED_HOOKS LHC-HOOK markers"
fi

# Root Cargo.toml is auto-generated/sorted — no LHC-HOOK comment allowed.
# Dedicated named assertion for the workspace-members entry (FORK.md).
if grep -q '"crates/lhc/grok-lhc-host"' Cargo.toml; then
  echo "ok workspace-member: crates/lhc/grok-lhc-host"
else
  echo "TRIPWIRE workspace-member: missing crates/lhc/grok-lhc-host in root Cargo.toml"
  echo "  Re-add in sort order (after crates/common/*, before prod/*). See FORK.md."
  fail=1
fi

# ── Layer 2: compile ───────────────────────────────────────────────────
# The adapter crate consumes the vendored port (and, from Chunk 1, the host
# crates it seams into) — drift at a seam breaks this build loudly.
if [ ! -f crates/lhc/vendor/long-horizon-context/packages/lhc-rs/Cargo.toml ]; then
  echo "TRIPWIRE compile: vendor submodule not initialized (git submodule update --init)"
  fail=1
elif cargo check -q --manifest-path crates/lhc/grok-lhc-host/Cargo.toml 2>compile.err; then
  echo "ok compile: grok-lhc-host + vendored lhc"
  rm -f compile.err
else
  echo "TRIPWIRE compile: grok-lhc-host failed — first errors:"
  head -20 compile.err; rm -f compile.err
  fail=1
fi

# ── Layer 2b: formatting gates ─────────────────────────────────────────
# Named acceptance gates. A tripwire that does not run fmt --check cannot
# report on it (same blind spot that hid a red --lib suite until --lib
# was added). Both invocations must stay here.
if cargo fmt -p xai-grok-shell --check >/dev/null 2>&1; then
  echo "ok fmt: xai-grok-shell --check"
else
  echo "TRIPWIRE fmt: cargo fmt -p xai-grok-shell --check failed"
  cargo fmt -p xai-grok-shell --check 2>&1 | head -40
  fail=1
fi
if cargo fmt --manifest-path crates/lhc/grok-lhc-host/Cargo.toml --check >/dev/null 2>&1; then
  echo "ok fmt: grok-lhc-host --check"
else
  echo "TRIPWIRE fmt: cargo fmt --manifest-path crates/lhc/grok-lhc-host --check failed"
  cargo fmt --manifest-path crates/lhc/grok-lhc-host/Cargo.toml --check 2>&1 | head -40
  fail=1
fi

# ── Layer 3: golden smoke + certification ──────────────────────────────
# Both integration binaries, with --features test-util so certification is
# not silently skipped (A4). Assert a nonzero test count for each.
run_lhc_test_bin() {
  local bin="$1"
  local label="$2"
  # bin="--lib" selects the unit-test binary; anything else is an integration
  # binary. The --lib arm exists because this script once ran only the
  # integration binaries and reported ALL TRIPWIRES GREEN over a RED unit
  # suite (Chunk 2 gate re-run) — unit tests in src/ were structurally
  # invisible to the gate. Do not remove it.
  local sel
  if [ "$bin" = "--lib" ]; then sel=(--lib); else sel=(--test "$bin"); fi
  if out=$(cargo test -q --manifest-path crates/lhc/grok-lhc-host/Cargo.toml \
      --features test-util "${sel[@]}" -- --nocapture 2>&1); then
    n=$(printf '%s\n' "$out" | sed -n 's/.*running \([0-9][0-9]*\) tests.*/\1/p' | tail -1)
    if [ -z "$n" ] || [ "$n" -le 0 ]; then
      echo "$out" | tail -20
      echo "TRIPWIRE $label: expected running N tests with N>0, got '${n:-none}'"
      return 1
    fi
    echo "ok $label: running $n tests"
    return 0
  fi
  echo "$out" | tail -20
  echo "TRIPWIRE $label: failed"
  return 1
}

if [ -d crates/lhc/grok-lhc-host/tests/goldens ]; then
  run_lhc_test_bin --lib "unit (lib)" || fail=1
  run_lhc_test_bin golden_smoke "golden smoke" || fail=1
  run_lhc_test_bin certification "certification" || fail=1
  run_lhc_test_bin harness_chunk3b "chunk3b harness" || fail=1
else
  echo "SKIP golden smoke: no goldens yet (armed by Chunk 1 certification)"
fi

# ── Submodule pin report (informational) ───────────────────────────────
echo "vendor pin: $(git -C crates/lhc/vendor/long-horizon-context rev-parse --short HEAD) (policy: certified lhc-rs-port commits only — FORK.md)"
# Pin-drift check: a pin off the shared certified line is a PENDING
# reconciliation, not a resting state — this renags every run until fixed.
git -C crates/lhc/vendor/long-horizon-context fetch origin lhc-rs-port --quiet 2>/dev/null || true
if git -C crates/lhc/vendor/long-horizon-context rev-parse --verify --quiet origin/lhc-rs-port >/dev/null; then
  if git -C crates/lhc/vendor/long-horizon-context merge-base --is-ancestor HEAD origin/lhc-rs-port; then
    behind=$(git -C crates/lhc/vendor/long-horizon-context rev-list --count HEAD..origin/lhc-rs-port)
    echo "ok pin: on certified shared line ($behind commits behind shared tip)"
  else
    echo "WARN pin: OFF the shared certified line — side-branch pin awaiting"
    echo "  reconciliation (fold into lhc-rs-port + re-pin; see FORK.md). Repeats every run."
  fi
else
  echo "SKIP pin-drift: shared branch unreachable (offline?)"
fi

[ "$fail" -eq 0 ] && echo "ALL TRIPWIRES GREEN" || echo "TRIPWIRES FAILED"
exit "$fail"
