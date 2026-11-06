# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Show what the next archive would do.
# Expects: keep-me is pinned, nothing archived. Leaves: the same.

Output out/discard.txt
# The picture shows the command, 5 lines of output and the prompt: 7 rows of
# 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make
# 358.
Set Height 358
Source common.tape

Type "antiq archive -n"
Enter
Sleep 1s
Screenshot ../assets/shots/dry-run.png
Sleep 1s
