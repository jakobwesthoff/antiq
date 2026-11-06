# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Archive the due pieces with the live cards. The two stills are taken while
# the pieces are packing and after the last line. Waiting for the text keeps
# the stills independent of the speed of the machine.
# Expects: keep-me is pinned, nothing archived. Leaves: scanner, scan-old and
# recipe-api are archived and gone from the folder.

Output out/discard.txt
# The cards while packing need the most: the command, three cards of two lines
# and the cursor line, 8 rows of 38.46 pixels. With the padding of 80 that makes
# 388. The finished picture needs 7 rows and shares the height of the tape.
Set Height 388
Source common.tape

Type "antiq archive"
Enter
Wait+Screen@120s /packing/
Sleep 1s
Screenshot ../assets/shots/archive-live.png
Wait+Screen@120s /took [0-9]+:[0-9][0-9]/
Sleep 2s
Screenshot ../assets/shots/archive-done.png
Sleep 1s
