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
# which names the version.
Hide
Type `d="$ANTIQ_DEMO_ROOT/old-disk/pixel-clock" && mkdir -p "$d/src" && echo 'fn main() {}' > "$d/src/main.rs" && echo '# pixel clock' > "$d/README.md" && touch -t 202503141200 "$d/src/main.rs" "$d/README.md" && export HOME="$ANTIQ_DEMO_ROOT" && clear`
Enter
Sleep 500ms
Show

Type "antiq import ~/old-disk/pixel-clock"
Enter
Wait+Screen@60s /took [0-9]+:[0-9][0-9]/
Sleep 500ms
Type "antiq list pixel-clock"
Enter
Sleep 1s
Screenshot ../assets/shots/import.png
Sleep 1s
