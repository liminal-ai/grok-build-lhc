# Install & use

Build and run **this fork** from source: Grok Build with long-horizon context
(LHC). Official `grok` installers and prebuilt binaries do **not** include it.

For *what* the fork is, see [`README.md`](README.md). For maintainer drills,
see [`../FORK.md`](../FORK.md).

---

## 1. Clone with the LHC submodule

```bash
git clone --recurse-submodules https://github.com/liminal-ai/grok-build-lhc.git
cd grok-build-lhc
git checkout lhc   # default branch; product lives here
```

If you already cloned without submodules:

```bash
git submodule update --init --recursive
```

The SDK lives at `crates/lhc/vendor/long-horizon-context` and is **pinned**.
Do not casually retarget it; pin policy is in `FORK.md`.

## 2. Toolchain

Same requirements as upstream Grok Build (see root README):

- Rust via [`rust-toolchain.toml`](../rust-toolchain.toml) (`rustup` picks it up)
- [DotSlash](https://dotslash-cli.com) on `PATH` (for hermetic `bin/protoc`, etc.)

```bash
cargo install dotslash   # if needed
rustup show              # confirm toolchain
```

## 3. Build

From the repo root:

```bash
cargo build -p xai-grok-pager-bin --release
# binary: target/release/xai-grok-pager  (name may match upstream packaging)
```

Faster check:

```bash
cargo check -p xai-grok-pager-bin
cargo check -p grok-lhc-host
```

There is no separate cargo feature flag for LHC — the adapter is a normal
workspace member.

## 4. Run

Launch the TUI the same way you would from a source build of Grok Build:

```bash
cargo run -p xai-grok-pager-bin
# or run the release binary you built
```

**LHC is on by default** in this fork. You do not need `GROK_LHC=1` for
normal use. Storage defaults to `~/.lhc` (override with `GROK_LHC_ROOT` or
`[lhc].root`).

In a session:

| Command | Purpose |
|---|---|
| `/lhc` or `/lhc status` | Capture on/off, compact mode, health snapshot |
| `/lhc health` | Deeper health |
| `/lhc on` / `/lhc off` | Per-session attach / detach |
| `/lhc repair` | Repair paths (see status text; destructive steps need confirm) |

History retrieval tools (`get_turns` / `get_messages`) are available while
capture is active so the agent can refresh low-fidelity spans from the archive.

## 5. Disable (troubleshooting only)

If you need to rule LHC out of a bug:

```bash
export GROK_LHC=0
# or in config.toml:
# [lhc]
# enabled = false
```

Then restart the process. For a clean side-by-side against stock Grok, use
**upstream** binaries/builds—not this fork with the gate flipped.

## 6. Optional config

```toml
[lhc]
# enabled = false              # only to disable
# root = "/path/to/lhc-storage"
# inference_model = "grok-4.5" # derivation model; default grok-4.5
# compact = "shadow"           # default; "replace" needs experimental gate
# compact_experimental = false
```

Env wins when set (`GROK_LHC`, `GROK_LHC_ROOT`, `GROK_LHC_COMPACT`, …).

### Compact replace (advanced / experimental)

```bash
export GROK_LHC_COMPACT=replace
export GROK_LHC_COMPACT_EXPERIMENTAL=1
```

Do not arm replace casually on a session you cannot afford to rewrite. See
`FORK.md` (Gating) and `crates/lhc/grok-lhc-host/MAPPING.md`.

## 7. Verify the fork is intact

```bash
./scripts/check-lhc-hooks.sh
```

Tripwire layers include hook sentinels, compile, formatting, and LHC test
bins. Green means the integration markers and adapter still hang together
after a sync or local edit.

## Important cautions

- **Never run `grok upgrade` / self-update on this checkout.** It is a
  git-tracked source tree; self-update can clobber the fork.
- **Upstream binaries ≠ this fork.** Official installers install xAI’s
  build, not liminal-ai’s LHC integration.
- **Derivation** uses a dedicated inference path (default model
  `grok-4.5`, low reasoning). Session chat model and derivation model are
  not the same thing by design.
- **Tool-result summarization** currently uses a deterministic truncate
  fallback in the SDK (not full inference at intake rate). That is intentional
  interim behavior, not a broken install.

## Next

- Concepts and product story: [`README.md`](README.md)  
- LHC design docs: [long-horizon-context onboard](https://github.com/liminal-ai/long-horizon-context/tree/main/docs/onboard)  
- Live certification (maintainers): `crates/lhc/grok-lhc-host/LIVE_RUNBOOK.md`  
