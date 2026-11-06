# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Delete a listing and show the refusal of a push. The delete is hidden here
# because the tutorial gives it as text.
# Expects: the target is claimed. Leaves: the listing of scan-old is gone.

Output out/discard.txt
# The picture shows the command, 11 lines of output and the prompt: 13 rows
# of 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps,
# make 588.
Set Height 588
Source common.tape

Hide
Type "rm .antiq/pieces/scan-old/*.sha256.zst && clear"
Enter
Show
Type "antiq push"
Enter
Sleep 2s
Screenshot ../assets/shots/error-damaged.png
# vhs writes a screenshot a moment later; the screen must still hold it.
Sleep 1s
