#!/bin/sh
# Verify the reviewed Grok sandbox gate list: unique binary/name pairs,
# and (when cargo nextest can list) that JSON identities match.
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
list_tmp=$(mktemp)
trap 'rm -f "$list_tmp"' EXIT
set +e
cargo nextest list \
  -p xai-grok-sandbox \
  -p xai-grok-shell \
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

EXPECTED_TSV="$data" EXPECTED_N="$EXPECTED" python3 - "$list_tmp" <<'PY'
import json, os, sys
path = sys.argv[1]
expected = set()
for line in os.environ["EXPECTED_TSV"].splitlines():
    line = line.strip()
    if not line:
        continue
    bid, name = line.split("\t", 1)
    expected.add((bid, name))
with open(path) as f:
    data = json.load(f)
rust = data.get("rust-binaries") or data.get("rust_binaries") or {}
got = set()
for bid, spec in rust.items():
    tests = spec.get("tests") or {}
    for tname in tests:
        got.add((bid, tname))
missing = sorted(expected - got)
extra = sorted(got - expected)
if missing:
    print("missing from nextest list:", file=sys.stderr)
    for row in missing:
        print(f"  {row[0]}\t{row[1]}", file=sys.stderr)
if extra:
    print("extra in nextest list:", file=sys.stderr)
    for row in extra:
        print(f"  {row[0]}\t{row[1]}", file=sys.stderr)
want = int(os.environ["EXPECTED_N"])
if len(expected) != want:
    print(f"tsv unique {len(expected)} != header {want}", file=sys.stderr)
    sys.exit(1)
if missing or extra:
    sys.exit(1)
print(f"ok sandbox-identities: {len(got)} match nextest list")
PY
