# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Make the demo folder a collection.
# Expects: the folders of the world, no collection yet. Leaves: a collection.

Output out/discard.txt
# The picture shows the command, 4 lines of output and the prompt: 6 rows of
# 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make
# 318.
Set Height 318
Source common.tape

Type "antiq init --pattern '*'"
Enter
Sleep 1s
Screenshot ../assets/shots/init.png
Sleep 1s
