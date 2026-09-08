# Install & use

Install or build **this fork**: Grok Build with long-horizon context (LHC).
Official `grok` installers and xAI's prebuilt binaries do **not** include it.

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

The release is built from the public upstream **source** at that base, not
from xAI's separately published binaries, so `grok --version` names the
source it was built from while the fork release identifies the artifact.

One installer per platform owns download, checksums, the managed store,
receipts, and activation. Neither touches `~/.grok` (`%USERPROFILE%\.grok`),
so a stock `grok` can stay installed side by side: stock keeps its own command
and updater, the fork gets its own command (default **`grok-lhc`**).

Linux and macOS:

```bash
curl -fsSL https://github.com/liminal-ai/grok-build-lhc/releases/latest/download/install.sh -o install.sh
sh install.sh --download                 # latest release, host platform
# sh install.sh --download --version 1.0.16-lhc.2
```

Windows (PowerShell):

```powershell
irm https://github.com/liminal-ai/grok-build-lhc/releases/latest/download/install.ps1 -OutFile install.ps1
powershell -ExecutionPolicy Bypass -File install.ps1 -Download
# powershell -ExecutionPolicy Bypass -File install.ps1 -Download -Version 1.0.16-lhc.2
```

Defaults on Linux/macOS: command `~/.local/bin/grok-lhc`, managed store
`~/.local/share/grok-lhc` (`versions/<release>/bin/grok`, `current` symlink,
receipts `installed-name`, `installed-version`, `installed-prefix`). On
Windows: launcher `%LOCALAPPDATA%\grok-lhc\bin\grok-lhc.cmd` (forwards all
arguments and the exit status), store `%LOCALAPPDATA%\grok-lhc`
(`versions\<release>\bin\grok.exe`, `current` directory junction, the same
receipts). The Windows installer does not edit `PATH`; add
`%LOCALAPPDATA%\grok-lhc\bin` yourself or call the launcher by path. Choose
otherwise explicitly: `--name grok-memory`, `--prefix /opt/grok-lhc`,
`--install-root DIR` (`-Name`, `-Prefix`, `-InstallRoot` on Windows).
Re-running the installer against an existing store keeps its recorded name and
prefix. `--uninstall` / `-Uninstall` removes the command and the store and
preserves configuration and LHC archives. A downloaded candidate directory
works offline with `--asset-dir DIR` / `-AssetDir DIR` (release lane).

| Platform | Asset | Installer | Qualification of the published bytes |
|---|---|---|---|
| Linux x86_64 | `grok-<release>-linux-x86_64` | `install.sh` | Daytona sandbox: install, identity, default-on LHC persistence with a mock model, uninstall, data preservation |
| macOS Apple Silicon | `grok-<release>-darwin-aarch64` | `install.sh` | build runner: architecture, identity, shell installer lifecycle with the built asset |
| Windows x86_64 | `grok-<release>-windows-x86_64.exe` | `install.ps1` | build runner: architecture, identity, isolated installer lifecycle, native `grok update` against local release assets |

Hosted checks are the release gate. Interactive use on the maintainers'
own machines happens after publication (burn-in), not before. Executables are
not code-signed or notarized: a browser-downloaded binary may be blocked by
macOS Gatekeeper (`xattr -d com.apple.quarantine <file>`) or warned about by
Windows SmartScreen; the installers download directly and are not affected.
Intel macOS and Windows ARM64 are not built. Reinstalling the release that is
currently running on Windows fails because the executable is locked: close
`grok` first (a different release installs beside it).

This is the **liminal-ai LHC fork**, not official xAI. Do **not** update with
`curl … https://x.ai/cli/install.sh` or `irm … x.ai/cli/install.ps1` — that
replaces the fork with stock Grok.

### Updating

A managed install updates itself from this repo's GitHub Releases (no `gh`
needed): `grok update` (explicit, always available) or `grok update --check`.
Background/automatic updates are **opt-in**: set `[cli] auto_update = true`
in `~/.grok/config.toml`; unset or `false` means off. The updater fetches the
selected release's own installer (`install.sh`, or `install.ps1` on Windows)
and `SHA256SUMS`, verifies the installer against them, and runs it against its
own store; it reads and writes only that store (`<store>/version.json` cache,
receipts, versions) and relaunches `<store>/current/bin/grok`. It never writes
the shared `[cli].installer` key, `~/.grok/bin`, `~/.grok/downloads`, or
`~/.grok/version.json`, so a stock install's own updater is unaffected. A
build that is not running from a managed store (source build, copied file)
prints these install instructions instead. In T3 Code, the Update button of a
Grok instance runs that instance's configured binary's `grok update`, so a
stock instance and an LHC instance each update their own install.

**Existing 0.3.1 installs** (`installed-name = grok`, no prefix receipt)
cannot update themselves to an aligned release, and running `grok update` on
the 0.3.1 binary would install into `~/.grok/bin` instead of the store. Do not
run its updater; run the new installer once against the existing store with
its real prefix. The command name and links are kept and the prefix is
recorded:

```bash
sh install.sh --download --install-root ~/.local/share/grok-lhc --prefix ~/.local
```

If a stock Grok shares that machine, its `~/.grok/config.toml` `[cli]
installer` key may still say `gh-release` from an earlier fork build. The fork
never writes that key; restore the value stock recorded (`internal` for an
official-installer install) by hand when stock should update itself again.

Maintainers cut releases through three manual stages: **Grok LHC candidate**
builds one immutable three-platform bundle with every platform verified on its
own runner, **Grok LHC Linux smoke** qualifies the exact Linux bytes in
Daytona, and **Promote Grok LHC release** republishes the same files without
rebuilding through the `production` environment. A source tag alone does not
publish anything.

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
  build, not liminal-ai’s LHC integration. `grok --version` reports the
  upstream *source* base this fork was built from; the same number on an
  xAI-published binary is a different build.
- **Long-session behavior (1.0.16):** LHC's own compacted history is no longer
  re-captured into the transcript after a compact, and long tool-driven turns
  are segmented LHC-side (about every 18k tokens of an open turn, at a
  complete tool exchange) so compaction can cut inside a running task.
  Limits: a segment holding only tool traffic compresses to an empty
  detailed-band entry; a cancel right after a segment end lands on an empty
  open turn; older huge turns are not split retrospectively. Details and the
  live evidence are in `../FORK.md` (Known limitations).
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
