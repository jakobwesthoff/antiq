#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Renders Archie's emblem as terminal text: 24-bit colour cells from chafa,
# one file per cell shape and glyph set, into assets/emblem/terminal/.
#
# Usage: scripts/emblem-ansi.sh        (or: just emblem-ansi)
#
# A terminal cell is narrower than half its height in many fonts (Victor
# Mono 0.387, Monaco 0.450, JetBrains Mono 0.455, Fira Code 0.500, Menlo
# and SF Mono 0.517, PT Mono 0.536, width over line height from the font
# files). Text drawn for one cell shape looks squeezed or stretched in
# another, so every shape below gets its own file. Four shapes keep any font
# in that range within about 5 % of round.

set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
svg="$repo/assets/emblem/archie-ball-vinyl-violet.svg"
out="$repo/assets/emblem/terminal"

# The height in rows of every rendering, the size asqr shows its emblem at.
rows=22

# Cell width over cell height, one rendering each.
ratios=(0.39 0.43 0.47 0.52)

# Sextants are the finest cells but need a font with the Unicode 13
# symbols for legacy computing; blocks are the fallback every font has.
symbol_sets=(sextant block)

for tool in chafa rsvg-convert; do
    if ! command -v "$tool" > /dev/null; then
        echo "emblem-ansi.sh: $tool is not installed" >&2
        exit 1
    fi
done

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# chafa reads the emblem as a raster; 880 px leave it far more pixels than
# the largest rendering has cells.
png="$scratch/emblem.png"
rsvg-convert -w 880 -h 880 -a "$svg" -o "$png"

mkdir -p "$out"
for ratio in "${ratios[@]}"; do
    # The emblem is square: as many columns as make the rows' height wide.
    columns=$(awk -v rows="$rows" -v ratio="$ratio" 'BEGIN { printf "%d", rows / ratio + 0.5 }')
    tag="r${ratio/./}"
    for symbols in "${symbol_sets[@]}"; do
        # chafa hides and shows the cursor around its output; a file that is
        # printed into other output must leave the cursor alone.
        chafa --format symbols --size "${columns}x${rows}" --font-ratio "$ratio" \
            --colors full --symbols "$symbols" --work 9 --dither none "$png" \
            | sed -e $'s/\e\\[?25[lh]//g' > "$out/archie-$tag.$symbols.ansi"
        echo "archie-$tag.$symbols.ansi: ${columns}x${rows} cells"
    done
done
