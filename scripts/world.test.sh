#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Tests scripts/world.sh, the builder of the demo world that the captures of
# the README and the project page are rendered from. Needs a built antiq
# (target/release/antiq, or ANTIQ_BIN), jq and shasum.
#
# Usage: scripts/world.test.sh [test-name]
#
# Each test builds the small world (the same pieces and idle days as the full
# one, at a hundredth of the size) in a scratch root of its own, so a test
# that fails does not depend on another one. Without a name every test runs.

set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
export ANTIQ_BIN=${ANTIQ_BIN:-$repo/target/release/antiq}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
# The root has to be canonical, like the one the script defaults to, or the
# script refuses it; macOS puts its temporary directory behind a symlink.
scratch=$(cd "$scratch" && pwd -P)

failures=0

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# Builds the small world under $scratch/<name> and prints its root.
build() {
    root="$scratch/$1"
    ANTIQ_DEMO_ROOT="$root" "$here/world.sh" --small > /dev/null || fail "world.sh failed for $1"
    echo "$root"
}

# The pieces of the world with the idle days that the recipe gives them.
recipe_idle_days="scanner:90 scan-old:200 recipe-api:45 ledger-cli:28 fresh-idea:2 keep-me:100"

# status --json lists only the pieces that have a state, so the fresh piece
# is not among them; the count of folders covers it.
test_pieces() {
    root=$(build pieces) || exit 1
    cd "$root/experiments" || exit 1
    "$ANTIQ_BIN" init --pattern '*' > /dev/null
    status=$("$ANTIQ_BIN" status --json)
    folders=$(echo "$status" | jq -r '.folders')
    listed=$(echo "$status" | jq -r '[.pieces[].piece] | sort | join(" ")')
    [ "$folders" = 6 ] || fail "pieces: expected 6 folders, got $folders"
    [ "$listed" = "keep-me ledger-cli recipe-api scan-old scanner" ] \
        || fail "pieces: listed pieces are '$listed'"
    [ -d "$root/experiments/fresh-idea" ] || fail "pieces: fresh-idea is missing"
}

test_idle_days() {
    root=$(build idle) || exit 1
    cd "$root/experiments" || exit 1
    "$ANTIQ_BIN" init --pattern '*' > /dev/null
    status=$("$ANTIQ_BIN" status --json)
    for entry in $recipe_idle_days; do
        piece=${entry%%:*}
        days=${entry##*:}
        [ "$piece" = fresh-idea ] && continue
        got=$(echo "$status" | jq -r --arg p "$piece" '.pieces[] | select(.piece == $p) | .idle_for_days')
        [ "$got" = "$days" ] || fail "idle days: $piece is idle for '$got' days, expected $days"
    done
}

# Hash of every path and every file content, with times left out: two builds
# of the world must agree, or a render would show different data.
tree_hash() {
    (cd "$1" && find . -type f | LC_ALL=C sort | while read -r file; do
        printf '%s ' "$file"
        shasum -a 256 < "$file"
    done | shasum -a 256)
}

test_content_identical() {
    first_root=$(build first) || exit 1
    first=$(tree_hash "$first_root")
    second_root=$(build second) || exit 1
    second=$(tree_hash "$second_root")
    [ "$first" = "$second" ] || fail "content: two builds differ"
}

# Sizes are measured with the filesystem, as status --json has no byte count
# per piece.
test_sizes() {
    root=$(build sizes) || exit 1
    for entry in scanner:1500000 scan-old:600000 recipe-api:1000000 ledger-cli:200000; do
        piece=${entry%%:*}
        expected=${entry##*:}
        got=$(find "$root/experiments/$piece" -type f -exec wc -c {} + | awk 'END { print $1 }')
        [ "$got" -ge "$expected" ] && [ "$got" -lt $((expected + 1000)) ] \
            || fail "sizes: $piece holds $got bytes, expected $expected plus under 1000"
    done
}

test_root_is_canonical() {
    mkdir "$scratch/real"
    ln -s "$scratch/real" "$scratch/link"
    # The message is checked, so a script that is missing or broken does not
    # pass for one that refuses the root.
    if message=$(ANTIQ_DEMO_ROOT="$scratch/link/demo" "$here/world.sh" --small 2>&1); then
        fail "root: a root behind a symlink was accepted"
    fi
    case $message in
        *canonical*) ;;
        *) fail "root: the refusal does not say the root is not canonical: $message" ;;
    esac
}

# The history is two old versions that the tutorial's find step shows with
# stamps that do not depend on the day of the render.
test_history() {
    root=$(build history) || exit 1
    cd "$root/experiments" || exit 1
    "$ANTIQ_BIN" init --pattern '*' > /dev/null
    ANTIQ_DEMO_ROOT="$root" "$here/world.sh" history > /dev/null || fail "history: world.sh history failed"
    listing=$("$ANTIQ_BIN" find --json)
    stamps=$(echo "$listing" | jq -r '[.. | strings | select(test("^2026-0[89]-[0-9]{2}T[0-9]{6}$"))] | sort | join(" ")')
    [ "$stamps" = "2026-08-02T093000 2026-09-12T181500" ] \
        || fail "history: the stamps of the versions are '$stamps'"
}

test_damage() {
    root=$(build damage) || exit 1
    cd "$root/experiments" || exit 1
    "$ANTIQ_BIN" init --pattern '*' > /dev/null
    ANTIQ_DEMO_ROOT="$root" "$here/world.sh" history > /dev/null || fail "damage: world.sh history failed"
    ANTIQ_DEMO_ROOT="$root" "$here/world.sh" damage scanner > /dev/null || fail "damage: world.sh damage failed"
    damaged=$("$ANTIQ_BIN" status --json | jq -r '.damaged_versions | length')
    [ "$damaged" -ge 1 ] || fail "damage: status reports no damaged version"
}

tests="test_pieces test_idle_days test_content_identical test_sizes test_root_is_canonical test_history test_damage"
if [ "$#" -gt 0 ]; then
    tests="test_$1"
fi

for name in $tests; do
    echo "running $name"
    ( "$name" ) || failures=$((failures + 1))
done

if [ "$failures" -gt 0 ]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "all tests passed"
