# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Make the demo folder a collection.
# Expects: the folders of the world, no collection yet. Leaves: a collection.

Output out/discard.txt
Set Height 330
Source common.tape

Type "antiq init --pattern '*'"
Enter
Sleep 1s
Screenshot ../assets/shots/init.png
Sleep 1s
