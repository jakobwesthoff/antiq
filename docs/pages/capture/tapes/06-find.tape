# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Show what is in the collection: the live folders and every archived piece
# with its versions, then the pieces a search word fits.
# Expects: three pieces archived, three folders live. Leaves: the same.

Output out/discard.txt
# The picture shows both commands, their output and the prompt: 13 rows of
# 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make
# 587, rounded up to an even height.
Set Height 588
Source common.tape

Type "antiq find"
Enter
Sleep 1s
Type "antiq find scan"
Enter
Sleep 1s
Screenshot ../assets/shots/find.png
Sleep 1s
