# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The demo video of the project page: what is idle, archive it, find a file of
# an archived piece, bring the piece back. The README shows a GIF that
# scripts/render.sh cuts from the mp4, as GitHub plays no video from a
# repository.
# Expects: a fresh world without a collection (render.sh builds one for this
# tape). Leaves: recipe-api is a folder again, scan-old and scanner are
# archived.

Output ../assets/demo.webm
Output ../assets/demo.mp4
Set Height 700
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
Sleep 4s
