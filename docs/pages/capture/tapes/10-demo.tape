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
# The font of the chapter images has no sextants, which the emblem is drawn
# with, so boxes would stand in for them. Cascadia Mono has them; the file is
# docs/pages/capture/fonts/CascadiaMono.ttf and has to be installed. The font
# is set before common.tape is sourced, as vhs ignores it afterwards. vhs gives
# antiq no pixel size, so antiq draws for a cell 0.52 times as wide as it is
# high. The cell of this font at this size measures 0.505, which leaves Archie
# 3 percent narrow. A letter spacing of 2 pixels makes the cell 0.535 and him
# 2 percent too wide.
Set FontFamily "Cascadia Mono"
# The version screen needs 24 rows: 22 for the emblem, the command and the prompt.
Set Height 900
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

# The last output stays for 20 seconds. Then comes the version screen. antiq
# draws sextants only for a terminal that it knows to draw them, and the shell
# of vhs is in a tmux whose client antiq cannot ask, which gives blocks. So the
# hidden setup makes the shell look like Ghostty's: no tmux, and its name.
Sleep 20s
Hide
Type "unset TMUX && export TERM_PROGRAM=ghostty && clear"
Enter
Show
Sleep 500ms
Type "antiq --version"
Enter
Sleep 5s
