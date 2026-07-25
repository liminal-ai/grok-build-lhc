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
EXPECTED_HOOKS=0
actual=$(grep -rl "LHC-HOOK" crates bin prod 2>/dev/null \
  | grep -v "^crates/lhc/" | xargs -r grep -o "LHC-HOOK [0-9]*/[0-9]*" | wc -l)
if [ "$actual" -ne "$EXPECTED_HOOKS" ]; then
  echo "TRIPWIRE sentinel: expected $EXPECTED_HOOKS LHC-HOOK markers, found $actual"
  echo "  A sync dropped or duplicated a hook. Do not push. See FORK.md recovery."
  fail=1
else
  echo "ok sentinel: $actual/$EXPECTED_HOOKS LHC-HOOK markers"
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

# ── Layer 3: golden smoke ──────────────────────────────────────────────
# Capture/rebuild golden transcripts are a Chunk 1 certification artifact;
# until they exist this layer reports SKIP loudly rather than passing silently.
if [ -d crates/lhc/grok-lhc-host/tests/goldens ]; then
  if cargo test -q --manifest-path crates/lhc/grok-lhc-host/Cargo.toml 2>&1 | tail -3; then
    echo "ok golden smoke"
  else
    echo "TRIPWIRE golden smoke: capture/rebuild goldens failed"
    fail=1
  fi
else
  echo "SKIP golden smoke: no goldens yet (armed by Chunk 1 certification)"
fi

# ── Submodule pin report (informational) ───────────────────────────────
echo "vendor pin: $(git -C crates/lhc/vendor/long-horizon-context rev-parse --short HEAD) (policy: certified lhc-rs-port commits only — FORK.md)"

[ "$fail" -eq 0 ] && echo "ALL TRIPWIRES GREEN" || echo "TRIPWIRES FAILED"
exit "$fail"
