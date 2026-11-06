# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Show what is in the archive: every piece with its versions, then the
# pieces a search word fits.
# Expects: three pieces archived. Leaves: the same.

Output out/discard.txt
# The picture shows both commands, their output and the prompt: 10 rows of
# 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make
# 472.
Set Height 472
Source common.tape

Type "antiq list"
Enter
Sleep 1s
Type "antiq list scan"
Enter
Sleep 1s
Screenshot ../assets/shots/list.png
Sleep 1s
