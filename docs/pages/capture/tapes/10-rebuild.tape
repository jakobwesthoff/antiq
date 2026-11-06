# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Rebuild the version whose listing 09-damage.tape deleted.
# Expects: the listing of scan-old is gone. Leaves: the listing is rebuilt.

Output out/discard.txt
# The picture shows the command, 6 lines of output and the prompt: 8 rows of
# 38.46 pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make
# 396.
Set Height 396
Source common.tape

Type "antiq rebuild scan-old 20"
Enter
Wait+Screen@120s /(Rebuilt|took [0-9]+:[0-9][0-9])/
Sleep 1s
Screenshot ../assets/shots/rebuild.png
Sleep 1s
