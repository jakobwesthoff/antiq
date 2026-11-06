# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The demo video of the project page: what is idle, archive it, find a file of
# an archived piece, bring the piece back, then the version screen with
# Archie. The README shows a GIF that scripts/render.sh cuts from the mp4, as
# GitHub plays no video from a repository.
# Expects: a fresh world without a collection (render.sh builds one for this
# tape). Leaves: recipe-api is a folder again, scan-old and scanner are
# archived.

Output ../assets/demo.webm
Output ../assets/demo.mp4
# The version screen needs 24 rows: 22 for the emblem, the command and the prompt.
# vhs gives antiq no pixel size, so antiq draws for a cell 0.52 times as wide as
# it is high, while this font's cell is 0.45, which leaves Archie a little tall.
# A letter spacing of 3 brings the cell to 0.505, but it has to be set before
# common.tape, applies to the whole video, and cuts the 105 columns to 94, so
# the tables would change their layout. It is left out.
Set Height 1000
Source common.tape
Set TypingSpeed 55ms

# The collection and the pin are the state of the story's start. keep-me is
# pinned so that three pieces are due, as in the stills of the README.
Hide
Type "antiq init --pattern '*' > /dev/null && antiq pin keep-me > /dev/null && clear"
Enter
Show

Sleep 1s
Type "antiq status"
Enter
Sleep 5s

Hide
Type "clear"
Enter
Show
Sleep 500ms

# The pieces archive in as long as this machine needs. Waiting for the closing
# line keeps the video independent of the speed of the machine.
Type "antiq archive"
Enter
Wait+Screen@120s /took [0-9]+:[0-9][0-9]/
Sleep 4s

Hide
Type "clear"
Enter
Show
Sleep 500ms

Type "antiq grep 'server\.py'"
Enter
Sleep 4s

# unearth acts only on the exact path of a piece.
Type "antiq unearth recipe-api"
Enter
Wait+Screen@120s /Unearthed/

# The last output stays for 20 seconds. Then comes the version screen. The
# shell of vhs runs in a tmux whose client antiq cannot ask, and antiq draws
# sextants for terminals it knows to draw them, which this font may lack. The
# hidden setup removes the variables that decide on the glyphs, so antiq draws
# blocks.
Sleep 20s
Hide
Type "unset TMUX TERM_PROGRAM && clear"
Enter
Show
Sleep 500ms
Type "antiq --version"
Enter
Sleep 5s
