#!/bin/sh
# Verify the reviewed Grok sandbox gate list: unique binary/name pairs,
# and (when cargo nextest can list) that JSON identities match.
# Parser matches Codex C: rust-suites / binary-id / testcases / filter-match=matches.
# Shell unit tests are listed with --lib (binary-id xai-grok-shell). Do not
# compile xai-grok-shell integration tests (they need test-support).
# Do not pass --retries to `nextest list`.
set -eu
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
LIST="$ROOT/scripts/lhc-sandbox-tests.tsv"

if [ ! -f "$LIST" ]; then
  echo "missing $LIST" >&2
  exit 1
fi

EXPECTED=$(awk '/^# count:/{print $3; exit}' "$LIST")

data=$(awk -F '\t' '
  /^#/ || NF == 0 { next }
  NF != 2 {
    printf "malformed line %d: %s\n", NR, $0 > "/dev/stderr"
    bad=1
    next
  }
  {
    key=$1 "\t" $2
    if (seen[key]++) {
      printf "duplicate: %s\n", key > "/dev/stderr"
      bad=1
    }
    print key
  }
  END { if (bad) exit 1 }
' "$LIST")

count=$(printf '%s\n' "$data" | grep -c . || true)
if [ "$count" != "$EXPECTED" ]; then
  echo "expected $EXPECTED sandbox tests, found $count in $LIST" >&2
  exit 1
fi

echo "ok sandbox-list: $EXPECTED reviewed tests"

if [ "${LHC_SANDBOX_LIST_ONLY:-}" = "1" ]; then
  exit 0
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found; list-only check passed" >&2
  exit 0
fi

filter=$("$ROOT/scripts/lhc-sandbox-filter.sh")
json_out=${LHC_SANDBOX_LIST_JSON:-}
sandbox_json=$(mktemp)
shell_json=$(mktemp)
trap 'rm -f "$sandbox_json" "$shell_json"' EXIT

set +e
cargo nextest list \
  -p xai-grok-sandbox \
  --message-format json \
  -E "$filter" >"$sandbox_json"
sandbox_status=$?
cargo nextest list \
  -p xai-grok-shell \
  --lib \
  --message-format json \
  -E "$filter" >"$shell_json"
shell_status=$?
set -e

if [ "$sandbox_status" != "0" ]; then
  echo "cargo nextest list -p xai-grok-sandbox failed with status $sandbox_status" >&2
  exit "$sandbox_status"
fi
if [ "$shell_status" != "0" ]; then
  echo "cargo nextest list -p xai-grok-shell --lib failed with status $shell_status" >&2
  exit "$shell_status"
fi

if [ -n "$json_out" ]; then
  python3 - "$sandbox_json" "$shell_json" "$json_out" <<'PY'
import json, sys
a = json.load(open(sys.argv[1], encoding="utf-8"))
b = json.load(open(sys.argv[2], encoding="utf-8"))
out = {
    "rust-suites": {},
    "test-count": (a.get("test-count") or 0) + (b.get("test-count") or 0),
}
for src in (a, b):
    out["rust-suites"].update(src.get("rust-suites") or {})
json.dump(out, open(sys.argv[3], "w", encoding="utf-8"))
PY
fi

EXPECTED_TSV="$data" python3 - "$sandbox_json" "$shell_json" "$EXPECTED" <<'PY'
import json
import os
import sys

expected_n = int(sys.argv[3])
got = set()
for path in (sys.argv[1], sys.argv[2]):
    payload = json.load(open(path, encoding="utf-8"))
    suites = payload.get("rust-suites") or {}
    for suite in suites.values():
        binary = suite.get("binary-id")
        if not binary:
            print("list JSON suite missing binary-id", file=sys.stderr)
            sys.exit(1)
        for name, meta in (suite.get("testcases") or {}).items():
            match = (meta or {}).get("filter-match") or {}
            if match.get("status") != "matches":
                continue
            got.add(f"{binary}\t{name}")

expected = {line for line in os.environ["EXPECTED_TSV"].splitlines() if line}
if len(expected) != expected_n:
    print(f"internal expected set {len(expected)} != {expected_n}", file=sys.stderr)
    sys.exit(1)
missing = sorted(expected - got)
extra = sorted(got - expected)
if missing or extra or len(got) != expected_n:
    print(
        f"sandbox JSON identity mismatch: listed {len(got)}, expected {expected_n}",
        file=sys.stderr,
    )
    if missing:
        print("missing:", file=sys.stderr)
        print("\n".join(missing), file=sys.stderr)
    if extra:
        print("extra:", file=sys.stderr)
        print("\n".join(extra), file=sys.stderr)
    sys.exit(1)
print(f"ok sandbox-filter: {expected_n} nextest binary/name identities match")
ident_path = os.environ.get("LHC_SANDBOX_IDENTITIES")
if ident_path:
    with open(ident_path, "w", encoding="utf-8") as f:
        f.write("\n".join(sorted(got)) + "\n")
PY
