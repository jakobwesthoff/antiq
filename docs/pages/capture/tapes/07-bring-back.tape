# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Unearth a piece, then fork a version, archive a second one from the fork
# and list both.
# Expects: three pieces archived. Leaves: recipe-api is a folder again,
# scanner has two versions and is not a folder any more.

Output out/discard.txt
# The unearth, fork and versions pictures each show the command, 2 lines of
# output and the prompt: 5 rows of 38.46 pixels plus 87 pixels, the padding
# of 80 and 7 that vhs keeps, make 280.
Set Height 280
Source common.tape

# The run of fork ends in the line "Forked ..." or, where it shows progress
# cards, in the closing line with the time.
Type "antiq unearth recipe-api"
Enter
Wait+Screen@120s /(Unearthed|took [0-9]+:[0-9][0-9])/
Sleep 1s
Screenshot ../assets/shots/unearth.png
Sleep 1s

Hide
Type "clear"
Enter
Show
Type "antiq fork scanner"
Enter
Wait+Screen@120s /(Forked|took [0-9]+:[0-9][0-9])/
Sleep 1s
Screenshot ../assets/shots/fork.png
# vhs writes a screenshot a moment later; the screen must still hold it.
Sleep 1s

Hide
Type "clear && echo '// edited' >> scanner/src/main.rs"
Enter
Show
Type "antiq archive scanner"
Enter
Wait+Screen@120s /took [0-9]+:[0-9][0-9]/
Sleep 1s
Hide
Type "clear"
Enter
Show
Type "antiq list scanner"
Enter
Sleep 1s
Screenshot ../assets/shots/versions.png
Sleep 1s
