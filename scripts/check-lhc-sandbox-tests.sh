#!/bin/sh
# Vanilla comparison: prove TSV identities against nextest rust-suites JSON.
# Parser matches Codex C: rust-suites / binary-id / testcases / filter-match=matches.
# This 3-name set is xai-grok-sandbox only (no shell --lib compile).
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
list_tmp=$(mktemp)
trap 'rm -f "$list_tmp"' EXIT
set +e
cargo nextest list \
  -p xai-grok-sandbox \
  --message-format json \
  -E "$filter" >"$list_tmp"
list_status=$?
set -e
if [ -n "$json_out" ]; then
  cp "$list_tmp" "$json_out"
fi
if [ "$list_status" != "0" ]; then
  echo "cargo nextest list failed with status $list_status" >&2
  exit "$list_status"
fi

EXPECTED_TSV="$data" python3 - "$list_tmp" "$EXPECTED" <<'PY'
import json
import os
import sys

list_path = sys.argv[1]
expected_n = int(sys.argv[2])
payload = json.load(open(list_path, encoding="utf-8"))
suites = payload.get("rust-suites") or {}
got = set()
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
