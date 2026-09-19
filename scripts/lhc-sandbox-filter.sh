#!/bin/sh
# Emit a nextest -E filter for the reviewed Grok sandbox-path list.
set -eu
ROOT=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
LIST="$ROOT/scripts/lhc-sandbox-tests.tsv"
EXPECTED=$(awk '/^# count:/{print $3; exit}' "$LIST")
awk -F '\t' -v expected="$EXPECTED" '
  /^#/ || NF == 0 { next }
  NF != 2 { next }
  {
    if ($1 ~ /[()&|+" \t]/ || $2 ~ /[()&|+" \t]/) {
      printf "filter-unsafe identity: %s\t%s\n", $1, $2 > "/dev/stderr"
      bad=1
      next
    }
    term = "(binary_id(=" $1 ") & test(=" $2 "))"
    if (out == "") out = term
    else out = out " + " term
    n++
  }
  END {
    if (bad) exit 1
    if (n != expected + 0) {
      printf "expected %s sandbox tests, found %d\n", expected, n > "/dev/stderr"
      exit 1
    }
    print out
  }
' "$LIST"
