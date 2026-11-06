#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Renders the social preview image docs/pages/assets/og.png from
# assets/social/og.html.
#
# Usage: scripts/og.sh        (or: just og)
#
# Chrome screenshots the page at exactly 1280 x 640 and oxipng compresses the
# PNG without loss. The page loads its fonts from assets/social/fonts, so the
# render needs no network.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
source_page="$repo/assets/social/og.html"
target="$repo/docs/pages/assets/og.png"

# =========================================================
# Prerequisites
# =========================================================

chrome="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
if [ ! -x "$chrome" ]; then
    echo "og.sh: Google Chrome not found at '$chrome'; set CHROME to its binary" >&2
    exit 1
fi
if ! command -v oxipng > /dev/null; then
    echo "og.sh: missing tool: oxipng" >&2
    exit 1
fi

# =========================================================
# Render
# =========================================================

# Chrome writes into a scratch folder first so that a failed render never
# leaves a broken image at the target. A private --user-data-dir keeps the run
# apart from any Chrome window that is already open.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

"$chrome" --headless=new --disable-gpu --hide-scrollbars \
    --user-data-dir="$work/profile" \
    --window-size=1280,640 \
    --force-device-scale-factor=1 \
    --virtual-time-budget=5000 \
    --screenshot="$work/og.png" \
    "file://$source_page" > /dev/null 2>&1

if [ ! -s "$work/og.png" ]; then
    echo "og.sh: Chrome produced no screenshot" >&2
    exit 1
fi

# A headless window can come out a few rows short of --window-size, which
# would shift the image GitHub crops to its middle band.
size=$(file "$work/og.png" | grep -o '[0-9]* x [0-9]*' | head -n 1)
if [ "$size" != "1280 x 640" ]; then
    echo "og.sh: screenshot is $size, expected 1280 x 640" >&2
    exit 1
fi

oxipng --quiet --strip safe "$work/og.png"
mv "$work/og.png" "$target"
echo "og.sh: wrote $target"
