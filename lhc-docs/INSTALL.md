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

## 3. Install from GitHub Releases (preferred)

Releases live at
[liminal-ai/grok-build-lhc releases](https://github.com/liminal-ai/grok-build-lhc/releases).
Each release is a fork release of an upstream Grok base: `grok --version`
prints the upstream base (for example `grok 1.0.16`), `grok --lhc-version`
prints the fork release (`1.0.16` for fork revision 0, `1.0.16-lhc.2` for a
repair of the same base).

The one installer owns download, checksums, the managed store, receipts, and
activation. It never touches `~/.grok`, so a stock `grok` can stay installed
side by side:

```bash
curl -fsSL https://github.com/liminal-ai/grok-build-lhc/releases/latest/download/install.sh -o install.sh
sh install.sh --download                 # latest release, host platform
# sh install.sh --download --version 1.0.16-lhc.2
```

Defaults: command **`grok-lhc`** at `~/.local/bin/grok-lhc`, managed store
`~/.local/share/grok-lhc` (`versions/<release>/bin/grok`, `current`, receipts
`installed-name`, `installed-version`, `installed-prefix`). Choose otherwise
explicitly: `--name grok-memory`, `--prefix /opt/grok-lhc`,
`--install-root DIR`. Re-running the installer against an existing store keeps
its recorded name and prefix. `--uninstall` removes the command and the store
and preserves configuration and LHC archives. A downloaded candidate directory
works offline with `--asset-dir DIR` (release lane).

| Platform | Asset | Status |
|---|---|---|
| Linux x86_64 | `grok-<release>-linux-x86_64` | published |
| macOS Apple Silicon | `grok-<release>-darwin-aarch64` | installer selects it; first prebuilt asset is the release slice |
| Windows x86_64 | `grok-<release>-windows-x86_64.exe` | same store contract; PowerShell installer and managed update arrive with the release slice |

This is the **liminal-ai LHC fork**, not official xAI. Do **not** update with
`curl … https://x.ai/cli/install.sh` — that replaces the fork with stock Grok.

### Updating

A managed install updates itself from this repo's GitHub Releases (no `gh`
needed): `grok update` (explicit, always available) or `grok update --check`.
Background/automatic updates are **opt-in**: set `[cli] auto_update = true`
in `~/.grok/config.toml`; unset or `false` means off. The updater reads and
writes only its own store (`<store>/version.json` cache, receipts, versions)
and relaunches `<store>/current/bin/grok`. It never writes the shared
`[cli].installer` key, `~/.grok/bin`, `~/.grok/downloads`, or
`~/.grok/version.json`, so a stock install's own updater is unaffected. A
build that is not running from a managed store (source build, copied file)
prints these install instructions instead.

**Existing 0.3.1 installs** (`installed-name = grok`, no prefix receipt)
cannot update themselves to an aligned release. Run the installer once
against the existing store with its real prefix; the command name and links
are kept and the prefix is recorded:

```bash
sh install.sh --download --install-root ~/.local/share/grok-lhc --prefix ~/.local
```

Maintainers cut releases through three manual stages: **Grok LHC candidate**
builds one immutable Linux bundle, **Grok LHC Linux smoke** qualifies those
exact bytes in Daytona, and **Promote Grok LHC release** republishes them
without rebuilding after protected Lee/CTO approval. A source tag alone does
not publish anything.

## 4. Build from source

From the repo root:

```bash
cargo build -p xai-grok-pager-bin --release
# binary: target/release/xai-grok-pager
cp target/release/xai-grok-pager ~/.local/bin/grok   # optional
```

Faster check:

```bash
cargo check -p xai-grok-pager-bin
cargo check -p grok-lhc-host
```

There is no separate cargo feature flag for LHC — the adapter is a normal
workspace member.

## 5. Run

```bash
grok
# or: cargo run -p xai-grok-pager-bin
```

**LHC is on by default** in this fork: capture, **Replace** compact, and
storage under `~/.grok-lhc` (override with `GROK_LHC_ROOT` or `[lhc].root`).

In a session:

| Command | Purpose |
|---|---|
| `/lhc` or `/lhc status` | Capture on/off, compact mode, health snapshot |
| `/lhc health` | Deeper health |
| `/lhc on` / `/lhc off` | Per-session attach / detach |
| `/lhc repair` | Repair paths (see status text; destructive steps need confirm) |

History retrieval tools (`get_turns` / `get_messages`) are available while
capture is active so the agent can refresh low-fidelity spans from the archive.

## 6. Disable (troubleshooting only)

If you need to rule LHC out of a bug:

```bash
export GROK_LHC=0
# or in config.toml:
# [lhc]
# enabled = false
```

Then restart the process. For a clean side-by-side against stock Grok, use
**upstream** binaries/builds—not this fork with the kill switch flipped.

## 7. Optional config

```toml
[lhc]
# enabled = false              # only to disable
# root = "/path/to/lhc-storage"
# inference_model = "grok-4.5" # derivation model; default grok-4.5
```

Env wins when set (`GROK_LHC`, `GROK_LHC_ROOT`, `GROK_LHC_INFERENCE_MODEL`, …).
Compact is always Replace while LHC is on — no compact mode key.

## 8. Verify the fork is intact

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
