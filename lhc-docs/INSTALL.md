# Install & use

Build and run **this fork** from source: Grok Build with optional
long-horizon context (LHC). Upstream install scripts and prebuilt `grok`
binaries do **not** include that path.

For *why* the fork exists, see [`README.md`](README.md). For maintainer
drills, see [`../FORK.md`](../FORK.md).

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
workspace member. **Runtime** gating controls whether capture runs.

## 4. Enable LHC

**Off by default.** With LHC disabled, behavior matches upstream for practical
purposes (a resolving tee may still be present; it no-ops when capture is off).

### Environment (common)

```bash
export GROK_LHC=1
# optional storage root (default ~/.lhc):
# export GROK_LHC_ROOT="$HOME/.lhc-dev"
```

### Config file

In Grok config (`config.toml` — same locations as upstream), you can also set:

```toml
[lhc]
enabled = true
# root = "/path/to/lhc-storage"
# inference_model = "grok-4.5"   # derivation model; default grok-4.5
```

When both are set, **env wins** for enablement.

### Compact modes (optional, advanced)

- Default compact path is conservative (**shadow** / non-replace unless you
  deliberately arm experimental replace).
- **Replace** write-back into native conversation is double-gated and
  experimental:

```bash
export GROK_LHC=1
export GROK_LHC_COMPACT=replace
export GROK_LHC_COMPACT_EXPERIMENTAL=1
```

Do not arm replace casually on a session you cannot afford to rewrite. See
`FORK.md` (Gating) and `crates/lhc/grok-lhc-host/MAPPING.md`.

## 5. Run

Launch the TUI the same way you would from a source build of Grok Build, e.g.:

```bash
cargo run -p xai-grok-pager-bin
# or run the release binary you built
```

In a session:

| Command | Purpose |
|---|---|
| `/lhc` or `/lhc status` | Capture on/off, compact mode, health snapshot |
| `/lhc health` | Deeper health |
| `/lhc on` / `/lhc off` | Per-session attach / detach |
| `/lhc repair` | Repair paths (see status text; destructive steps need confirm) |

When capture is active and open, history retrieval tools
(`get_turns` / `get_messages`) are available so the agent can refresh
low-fidelity spans from the archive.

## 6. Where data lives

Default LHC storage root: **`~/.lhc`** (override with `GROK_LHC_ROOT` /
`[lhc].root`). Per-thread SQLite plus a registry — full event history lives
here even when the model only sees a compressed view.

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
