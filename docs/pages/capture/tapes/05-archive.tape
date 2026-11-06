# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Archive the due pieces with the live cards. The GIF holds the whole run;
# the two stills are taken while the pieces are packing and after the last
# line. Waiting for the text keeps the stills independent of the speed of the
# machine.
# Expects: keep-me is pinned, nothing archived. Leaves: scanner, scan-old and
# recipe-api are archived and gone from the folder.

Output ../assets/shots/archive-live.gif
Set Height 640
Source common.tape
Set Framerate 12

Type "antiq archive"
Enter
Wait+Screen@120s /packing/
Sleep 1s
Screenshot ../assets/shots/archive-live.png
Wait+Screen@120s /took [0-9]+:[0-9][0-9]/
Sleep 2s
Screenshot ../assets/shots/archive-done.png
Sleep 1s
