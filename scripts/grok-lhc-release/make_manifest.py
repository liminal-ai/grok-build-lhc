#!/usr/bin/env python3
"""Release manifest + SHA256SUMS for one grok-build-lhc release.

Identity pair: `--version` is the fork release (`<base>` = fork revision 0 or
`<base>-lhc.<n>`) and `--upstream-version` is the native upstream base the
binary reports from `--version`. The pair is asserted here, against
lhc-release/VERSION, and against the built binary in the workflow; there is no
separate validator script.

Assets follow `grok-<release>-<os>-<arch>` (os linux|darwin|windows, arch
x86_64|aarch64); every such asset present in --dist is published and listed,
plus both installers (install.sh for Linux/macOS, install.ps1 for Windows).
"""
import argparse
import hashlib
import json
import re
from pathlib import Path

RELEASE_RE = re.compile(r"^(\d+\.\d+\.\d+)(?:-lhc\.(\d+))?$")
PLATFORMS = ("linux-x86_64", "linux-aarch64", "darwin-x86_64", "darwin-aarch64", "windows-x86_64", "windows-aarch64")
SOURCE_COMPATIBILITY_TARGETS = ["linux-x86_64", "windows-x86_64", "darwin-aarch64"]

parser = argparse.ArgumentParser()
parser.add_argument("--dist", type=Path, required=True)
parser.add_argument("--version", required=True, help="fork release, e.g. 1.0.16 or 1.0.16-lhc.2")
parser.add_argument("--upstream-version", required=True, help="native upstream base, e.g. 1.0.16")
parser.add_argument("--source-commit", required=True)
parser.add_argument("--source-revision", required=True)
parser.add_argument("--upstream-commit", required=True)
parser.add_argument("--lhc-sdk-commit", required=True)
parser.add_argument("--tripwire-evidence", required=True)
parser.add_argument("--run-id", required=True)
args = parser.parse_args()

ROOT = Path(__file__).resolve().parents[2]


def lhc_thread_schema() -> int:
    """The vendored Rust SDK's CURRENT_THREAD_SCHEMA_VERSION — derived, never
    hand-maintained, so the manifest (and the Daytona lifecycle check that
    reads it) follows the submodule pin. A pin bump that changes the schema
    cannot leave a stale number here."""
    storage = ROOT / "crates/lhc/vendor/long-horizon-context/packages/lhc-rs/src/shared_tech/storage.rs"
    match = re.search(r"pub const CURRENT_THREAD_SCHEMA_VERSION: i64 = (\d+);", storage.read_text(encoding="utf-8"))
    if not match:
        raise SystemExit(f"CURRENT_THREAD_SCHEMA_VERSION not found in {storage}")
    return int(match.group(1))


def check_identity_pair(release: str, upstream: str) -> int:
    match = RELEASE_RE.match(release)
    if not match:
        raise SystemExit(f"not a fork release: {release} (expected <major>.<minor>.<patch>[-lhc.<n>])")
    if match.group(1) != upstream:
        raise SystemExit(f"fork release {release} is not built on upstream {upstream}")
    embedded = (ROOT / "lhc-release/VERSION").read_text(encoding="utf-8").strip()
    if embedded != release:
        raise SystemExit(f"lhc-release/VERSION is {embedded}, not {release}")
    return int(match.group(2) or 0)


fork_revision = check_identity_pair(args.version, args.upstream_version)

assets = []
for platform in PLATFORMS:
    asset = args.dist / f"grok-{args.version}-{platform}"
    if platform.startswith("windows"):
        # `.exe` must be appended, never substituted for the dotted version.
        asset = asset.with_name(asset.name + ".exe")
    if asset.is_file():
        assets.append((asset, platform))
if not assets:
    raise SystemExit(f"no release assets grok-{args.version}-<os>-<arch> in {args.dist}")
installers = []
for name in ("install.sh", "install.ps1"):
    installer = args.dist / name
    if not installer.is_file():
        raise SystemExit(f"missing candidate installer: {installer}")
    installers.append(installer)


def entry(path, platform=None):
    item = {"path": path.name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "size": path.stat().st_size}
    if platform:
        item["platform"] = platform
    return item


artifacts = [entry(path, platform) for path, platform in assets] + [entry(path) for path in installers]
manifest = {
    "product": "grok-lhc",
    "release_version": args.version,
    "upstream_version": args.upstream_version,
    "fork_revision": fork_revision,
    "source_commit": args.source_commit,
    "source_revision": args.source_revision,
    "upstream_commit": args.upstream_commit,
    "lhc_sdk_commit": args.lhc_sdk_commit,
    "tripwire_evidence": args.tripwire_evidence,
    "lhc_thread_schema": lhc_thread_schema(),
    "candidate_run_id": args.run_id,
    "published_platforms": [platform for _, platform in assets],
    "source_compatibility_targets": SOURCE_COMPATIBILITY_TARGETS,
    "artifacts": artifacts,
}
(args.dist / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
(args.dist / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['path']}\n" for item in artifacts), encoding="utf-8")
