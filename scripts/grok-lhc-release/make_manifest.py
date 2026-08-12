#!/usr/bin/env python3
import argparse
import hashlib
import json
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
    "lhc_thread_schema": 6,
    "candidate_run_id": args.run_id,
    "published_platforms": ["linux-x86_64"],
    "source_compatibility_targets": ["linux-x86_64", "windows-x86_64", "macos-aarch64"],
    "artifacts": artifacts,
}
(args.dist / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
(args.dist / "SHA256SUMS").write_text("".join(f"{item['sha256']}  {item['path']}\n" for item in artifacts), encoding="utf-8")
