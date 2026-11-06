#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Renders the images of the README and the project page into
# docs/pages/assets/shots/ from real runs of antiq in the demo world.
#
# Usage: scripts/render.sh        (or: just shots)
#
# It builds antiq in release mode, builds the world with scripts/world.sh,
# runs the tapes of docs/pages/capture/tapes in their numbered order with vhs
# (each one leaves the world in the state the next one expects), builds a fresh
# world for the demo video and runs its tape, cuts the GIF of the README from
# the video, compresses the PNGs without loss and removes the world, also when
# a step fails.
# The world lives in a fixed folder because its path shows in the images; see
# scripts/world.sh.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
capture="$repo/docs/pages/capture"
shots="$repo/docs/pages/assets/shots"

# The number of columns common.tape is sized for. Tables switch to their wide
# form from 100 columns, so an image rendered with fewer shows another layout.
expected_columns=105

# =========================================================
# Prerequisites
# =========================================================

missing=""
for tool in vhs ttyd ffmpeg oxipng openssl cargo; do
    command -v "$tool" > /dev/null || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "render.sh: missing tools:$missing" >&2
    exit 1
fi

# antiq refuses a push with an rsync older than 3.2.4, and a tape that shows
# an error block in place of the table of a push is worth nothing.
rsync_ok=""
for candidate in "$(command -v rsync || true)" /opt/homebrew/bin/rsync /usr/local/bin/rsync; do
    [ -x "$candidate" ] || continue
    version=$("$candidate" --version | sed -n '1s/.*version \([0-9.]*\).*/\1/p')
    if [ "$(printf '%s\n3.2.4\n' "$version" | sort -V | head -1)" = 3.2.4 ]; then
        rsync_ok=$candidate
        break
    fi
done
if [ -z "$rsync_ok" ]; then
    echo "render.sh: no rsync of version 3.2.4 or newer found; push cannot be shown" >&2
    exit 1
fi

# The full world holds about 330 MB, and the archives about as much again.
free_kb=$(df -Pk "$(dirname "${ANTIQ_DEMO_ROOT:-/tmp}")" | awk 'NR == 2 { print $4 }')
if [ "$free_kb" -lt 1500000 ]; then
    echo "render.sh: less than 1.5 GB free for the demo world" >&2
    exit 1
fi

# =========================================================
# The world
# =========================================================

case $(uname -s) in
    Darwin) ANTIQ_DEMO_ROOT=/private/tmp/antiq-demo ;;
    *) ANTIQ_DEMO_ROOT=/tmp/antiq-demo ;;
esac
export ANTIQ_DEMO_ROOT

# The world holds nothing worth keeping, and it is the only folder this script
# deletes. world.sh refuses to delete a folder that does not carry its marker.
cleanup() {
    rm -rf "$capture/out"
    if [ -e "$ANTIQ_DEMO_ROOT/.antiq-demo-world" ]; then
        rm -rf "$ANTIQ_DEMO_ROOT"
    fi
}
trap cleanup EXIT

cargo build --release --manifest-path "$repo/Cargo.toml"
export ANTIQ_BIN="$repo/target/release/antiq"
PATH="$repo/target/release:$PATH"
export PATH

"$here/world.sh"

# =========================================================
# The tapes
# =========================================================

# vhs exits with 0 after a tape that did not parse or whose Wait ran out, so
# the run is judged by what vhs prints: a failed step has a line starting with
# "parser:" or "Error".
run_tape() {
    output=$(vhs "$1" 2>&1) || true
    if printf '%s\n' "$output" | grep -qE '^(parser:|Error|.*[Tt]imed out)'; then
        printf '%s\n' "$output" >&2
        echo "render.sh: vhs failed on $1" >&2
        exit 1
    fi
}

mkdir -p "$shots" "$capture/out"
cd "$capture"

run_tape tapes/00-columns.tape
columns=$(tr -dc '0-9' < out/columns.txt)
if [ "$columns" != "$expected_columns" ]; then
    echo "render.sh: the terminal has $columns columns, common.tape is sized for $expected_columns" >&2
    exit 1
fi

for tape in tapes/0[1-9]-*.tape; do
    echo "rendering $tape"
    run_tape "$tape"
done

# A Screenshot that vhs could not take is no error for vhs, so every image
# that the README and the page use has to be there.
for image in init status keep dry-run archive-live archive-done find fork versions push error-damaged rebuild; do
    if [ ! -s "$shots/$image.png" ]; then
        echo "render.sh: the tapes wrote no $image.png" >&2
        exit 1
    fi
done

# The demo video starts from a collection that does not exist yet, which the
# numbered tapes have used up, so it gets a fresh world.
"$here/world.sh"
echo "rendering tapes/10-demo.tape"
run_tape tapes/10-demo.tape
for video in demo.webm demo.mp4; do
    if [ ! -s "$capture/../assets/$video" ]; then
        echo "render.sh: the demo tape wrote no $video" >&2
        exit 1
    fi
done

# GitHub plays no video from a repository, so the README shows a GIF of the
# demo. The palette is built from the frames themselves; 10 frames a second
# and 1000 pixels of width keep it small enough for a README.
ffmpeg -v error -y -i "$capture/../assets/demo.mp4" \
    -vf "fps=10,scale=1000:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=64[p];[b][p]paletteuse=dither=none" \
    "$shots/demo.gif"
if [ ! -s "$shots/demo.gif" ]; then
    echo "render.sh: ffmpeg wrote no demo.gif" >&2
    exit 1
fi

# =========================================================
# Compression
# =========================================================

# oxipng is lossless: the pixels, and so the colours, stay as vhs wrote them.
oxipng --quiet --opt 4 --strip safe "$shots"/*.png

ls -l "$shots"
