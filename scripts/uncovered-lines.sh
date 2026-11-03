#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Lists the lines under src/ that changed since <base> and that the lcov
# report counts as never executed. Run
#   cargo llvm-cov --all-targets --lcov --output-path target/llvm-cov/lcov.info
# first. Prints one path:line per uncovered line and exits 1 when there
# are any.
#
# Usage: scripts/uncovered-lines.sh <base> [lcov-file]

set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <base> [lcov-file]" >&2
    exit 2
fi

base="$1"
root="$(git rev-parse --show-toplevel)"
lcov="${2:-$root/target/llvm-cov/lcov.info}"

if [ ! -f "$lcov" ]; then
    echo "no lcov file at $lcov; run cargo llvm-cov first" >&2
    exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Added lines per file, from the zero-context diff hunks: "@@ -a,b +c,d @@"
# adds d lines starting at c, and a missing d means one line.
git -C "$root" diff -U0 "$base" -- src |
    awk '
        /^\+\+\+ b\// { file = substr($0, 7); next }
        /^@@ / {
            plus = $3
            sub(/^\+/, "", plus)
            n = split(plus, parts, ",")
            start = parts[1] + 0
            count = (n == 2) ? parts[2] + 0 : 1
            for (i = 0; i < count; i++) print file ":" (start + i)
        }
    ' | sort -u >"$work/changed"

# Lines the report counts as executed zero times, as repo-relative paths.
awk -v root="$root/" '
    /^SF:/ { file = substr($0, 4); sub("^" root, "", file); next }
    /^DA:/ {
        split(substr($0, 4), parts, ",")
        if (parts[2] + 0 == 0) print file ":" parts[1]
    }
' "$lcov" | sort -u >"$work/uncovered"

comm -12 "$work/changed" "$work/uncovered" >"$work/result"
cat "$work/result"
[ ! -s "$work/result" ]
