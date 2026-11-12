# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Import a folder from outside the collection and list its version.
# Expects: the collection of the earlier tapes. Leaves: pixel-clock is an
# archived piece, and its source folder stays in old-disk.

Output out/discard.txt
# The picture shows both commands, their output and the prompt: 8 rows of
# 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make
# 395.
Set Height 395
Source common.tape

# The source lies beside the collection and stands for an old disk. It is
# written by its full path, since a ~ on the line that sets HOME can still
# mean the real home, and reached as ~/old-disk afterwards, once HOME points
# at the demo world in this shell. Its files carry a time in March 2025,
# which names the version. A glyph table that compresses well and an asset
# that does not, both written the same on every render, make the archive
# about 20 % of the content, as for a real project: two one-line files alone
# make an archive several times larger than its content, as the tar headers
# outweigh them.
Hide
Type `d="$ANTIQ_DEMO_ROOT/old-disk/pixel-clock" && mkdir -p "$d/src" "$d/assets" && echo 'fn main() {}' > "$d/src/main.rs" && echo '# pixel clock' > "$d/README.md" && seq 0 4095 | awk '{ printf "    Glyph { code: %d, rows: [%d, %d, %d, %d, %d, %d, %d, %d] },\n", $1, ($1*7)%256, ($1*13)%256, ($1*31)%256, ($1*61)%256, ($1*97)%256, ($1*127)%256, ($1*151)%256, ($1*199)%256 }' > "$d/src/font.rs" && LC_ALL=C awk 'BEGIN { srand(7); for (i = 0; i < 56000; i++) printf "%c", int(rand() * 255) + 1 }' > "$d/assets/splash.bin" && touch -t 202503141200 "$d/src/main.rs" "$d/src/font.rs" "$d/README.md" "$d/assets/splash.bin" && export HOME="$ANTIQ_DEMO_ROOT" && clear`
Enter
Sleep 500ms
Show

Type "antiq import ~/old-disk/pixel-clock"
Enter
Wait+Screen@60s /took [0-9]+:[0-9][0-9]/
Sleep 500ms
Type "antiq find pixel-clock"
Enter
Sleep 1s
Screenshot ../assets/shots/import.png
Sleep 1s
