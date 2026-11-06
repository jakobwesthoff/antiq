# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Delete a listing, show the refusal of a push, rebuild the version. The
# delete is hidden here because the tutorial gives it as text.
# Expects: the target is claimed. Leaves: the listing is rebuilt.

Output out/discard.txt
Set Height 560
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

Hide
Type "clear"
Enter
Show
Type "antiq rebuild scan-old 20"
Enter
Wait+Screen@120s /(Rebuilt|took [0-9]+:[0-9][0-9])/
Sleep 1s
Screenshot ../assets/shots/rebuild.png
Sleep 1s
