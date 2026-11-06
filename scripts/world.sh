#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Builds the demo world that the captures of the README and the project page
# are rendered from: a collection of experiments, a folder of agent runs, a
# folder for the push target and a folder of sources for `antiq import`.
#
# Usage:
#   scripts/world.sh [--small]       build the world from nothing
#   scripts/world.sh history         add old versions with fixed stamps
#   scripts/world.sh damage <piece>  delete the listings of a piece's versions
#
# The root is fixed because antiq prints the absolute path of the collection
# in `init`, and a path that differs between machines would put a different
# image into every render. ANTIQ_DEMO_ROOT overrides it for the tests of this
# script, and ANTIQ_BIN names the antiq that `history` runs.
#
# --small builds the same pieces with the same idle days at a hundredth of
# the size, which is what the tests use.

set -euo pipefail

case $(uname -s) in
    Darwin) default_root=/private/tmp/antiq-demo ;;
    *) default_root=/tmp/antiq-demo ;;
esac
root=${ANTIQ_DEMO_ROOT:-$default_root}
antiq=${ANTIQ_BIN:-antiq}

# A marker file tells a world of ours from any other folder, as the build
# deletes the root first.
marker=.antiq-demo-world

# =========================================================
# The root
# =========================================================

# antiq prints canonical paths. A root with a symlink in it would show up in
# the images under a name that the reader's machine does not have, so the
# parent has to be canonical before anything is created in it.
require_canonical_root() {
    parent=$(dirname "$root")
    if [ ! -d "$parent" ]; then
        echo "world.sh: the parent of the root does not exist: $parent" >&2
        exit 1
    fi
    canonical_parent=$(cd "$parent" && pwd -P)
    if [ "$canonical_parent/$(basename "$root")" != "$root" ]; then
        echo "world.sh: the root is not canonical: $root (its canonical form is $canonical_parent/$(basename "$root"))" >&2
        exit 1
    fi
}

# =========================================================
# Dates and content
# =========================================================

# Prints a touch -t stamp for <days> days ago. GNU date and BSD date spell the
# offset differently, and antiq runs on both.
stamp_days_ago() {
    if date -d "$1 days ago" +%Y%m%d%H%M 2> /dev/null; then
        return
    fi
    date -v-"$1"d +%Y%m%d%H%M
}

# Prints <bytes> bytes of a stream that depends only on <seed>: AES in
# counter mode over zeros. Random content would give every build another
# hash, and the archives another size.
stream() {
    key=$(printf '%032x' "$2")
    # head closes the pipe once it has its bytes, which kills openssl with
    # SIGPIPE; that is the end of the stream and no failure.
    openssl enc -aes-128-ctr -K "$key" -iv 00000000000000000000000000000000 \
        -in /dev/zero 2> /dev/null | head -c "$1" || true
}

# Writes the files of a piece. The stream file is the part that does not
# compress; the text file is the part that does, so that the pieces archive at
# a mixed ratio.
ballast() {
    piece=$1
    bytes=$2
    seed=$3
    mkdir -p "$root/experiments/$piece/data"
    stream $((bytes / 2)) "$seed" > "$root/experiments/$piece/data/samples.bin"
    yes 'timestamp=2026-10-02 reading=21.4 status=ok' | head -c $((bytes - bytes / 2)) \
        > "$root/experiments/$piece/data/readings.log" || true
}

# =========================================================
# The pieces of the experiments collection
# =========================================================

# One file per source file. The idle clock of a piece
# counts regular files only, so only files are backdated, below.
make_piece() {
    piece=$1
    shift
    for file in "$@"; do
        mkdir -p "$(dirname "$root/experiments/$piece/$file")"
        printf 'contents of %s\n' "$file" > "$root/experiments/$piece/$file"
    done
}

backdate_piece() {
    find "$root/experiments/$1" -type f -exec touch -t "$(stamp_days_ago "$2")" {} +
}

build() {
    scale=1
    if [ "${1:-}" = "--small" ]; then
        scale=100
    fi

    require_canonical_root
    if [ -e "$root" ] && [ ! -e "$root/$marker" ]; then
        echo "world.sh: $root exists and is not a demo world; not deleting it" >&2
        exit 1
    fi
    rm -rf "$root"
    mkdir -p "$root/experiments" "$root/agent-runs" "$root/target" "$root/staging"
    : > "$root/$marker"

    make_piece scanner src/main.rs Cargo.toml
    make_piece scan-old cli/main.rs
    make_piece recipe-api server.py requirements.txt
    make_piece ledger-cli src/lib.rs
    make_piece fresh-idea notes.md
    make_piece keep-me draft.txt

    ballast scanner $((150000000 / scale)) 1
    ballast scan-old $((60000000 / scale)) 2
    ballast recipe-api $((100000000 / scale)) 3
    ballast ledger-cli $((20000000 / scale)) 4

    backdate_piece scanner 90
    backdate_piece scan-old 200
    backdate_piece recipe-api 45
    backdate_piece ledger-cli 28
    backdate_piece fresh-idea 2
    backdate_piece keep-me 100

    # Runs of an agent: one folder per run, with the transcript and the
    # scratch output of its tools.
    for run in 2026-10-02-auth-refactor:34 2026-10-09-flaky-tests:27 2026-10-21-docs-pass:15 2026-11-03-today:2; do
        name=${run%%:*}
        days=${run##*:}
        mkdir -p "$root/agent-runs/$name/scratch"
        printf 'transcript of %s\n' "$name" > "$root/agent-runs/$name/transcript.md"
        printf 'tool output of %s\n' "$name" > "$root/agent-runs/$name/scratch/tool-output.txt"
        find "$root/agent-runs/$name" -type f -exec touch -t "$(stamp_days_ago "$days")" {} +
    done
}

# =========================================================
# History and damage
# =========================================================

# Two older versions of live pieces, made with fixed stamps. A version made
# by a real `archive` carries the time of the run, so the stamps that the
# find step shows would change with every render without these. The source of
# an import has to lie outside the collection, hence the staging folder.
history() {
    cd "$root/experiments"
    for entry in scanner:2026-08-02T093000 recipe-api:2026-09-12T181500; do
        piece=${entry%%:*}
        stamp=${entry#*:}
        source_dir="$root/staging/$piece"
        mkdir -p "$source_dir/src"
        printf 'an earlier state of %s\n' "$piece" > "$source_dir/src/main.rs"
        "$antiq" import "$source_dir" --as "$piece" --time "$stamp" > /dev/null
    done
}

# Deleting a listing is one of the ways a version becomes damaged, and the
# one that the tutorial shows: the archive survives, its listing does not.
damage() {
    find "$root/experiments/.antiq/pieces/$1" -name '*.sha256.zst' -delete
}

# =========================================================
# Commands
# =========================================================

case ${1:-build} in
    build | --small) build "${1:-}" ;;
    history) history ;;
    damage) damage "${2:?world.sh damage needs a piece}" ;;
    *)
        echo "usage: world.sh [--small] | history | damage <piece>" >&2
        exit 2
        ;;
esac
