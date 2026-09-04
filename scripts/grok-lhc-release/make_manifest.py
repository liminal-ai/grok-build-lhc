#!/usr/bin/env python3
import argparse
import hashlib
import json
import re
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--dist", type=Path, required=True)
parser.add_argument("--version", required=True)
parser.add_argument("--source-commit", required=True)
parser.add_argument("--source-revision", required=True)
parser.add_argument("--upstream-commit", required=True)
parser.add_argument("--lhc-sdk-commit", required=True)
parser.add_argument("--tripwire-evidence", required=True)
parser.add_argument("--run-id", required=True)
args = parser.parse_args()

def lhc_thread_schema() -> int:
    """The vendored Rust SDK's CURRENT_THREAD_SCHEMA_VERSION — derived, never
    hand-maintained, so the manifest (and the Daytona lifecycle check that
    reads it) follows the submodule pin. A pin bump that changes the schema
    cannot leave a stale number here."""
    storage = (
        Path(__file__).resolve().parents[2]
        / "crates/lhc/vendor/long-horizon-context/packages/lhc-rs/src/shared_tech/storage.rs"
    )
    match = re.search(r"pub const CURRENT_THREAD_SCHEMA_VERSION: i64 = (\d+);", storage.read_text(encoding="utf-8"))
    if not match:
        raise SystemExit(f"CURRENT_THREAD_SCHEMA_VERSION not found in {storage}")
    return int(match.group(1))


asset = args.dist / f"grok-{args.version}-linux-x86_64"
if not asset.is_file():
    raise SystemExit(f"missing Linux candidate: {asset}")
installer = args.dist / "install.sh"
if not installer.is_file():
    raise SystemExit(f"missing candidate installer: {installer}")

def entry(path, platform=None):
    item = {"path": path.name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "size": path.stat().st_size}
    if platform:
        item["platform"] = platform
    return item

artifacts = [entry(asset, "linux-x86_64"), entry(installer)]
manifest = {
    "product": "grok-lhc",
    "release_version": args.version,
    "source_commit": args.source_commit,
    "source_revision": args.source_revision,
    "upstream_commit": args.upstream_commit,
    "lhc_sdk_commit": args.lhc_sdk_commit,
    "tripwire_evidence": args.tripwire_evidence,
    "lhc_thread_schema": lhc_thread_schema(),
    "candidate_run_id": args.run_id,
    "published_platforms": ["linux-x86_64"],
    "source_compatibility_targets": ["linux-x86_64", "windows-x86_64", "macos-aarch64"],
    "artifacts": artifacts,
}
(args.dist / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
(args.dist / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['path']}\n" for item in artifacts), encoding="utf-8")
