# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Search the paths of the files in all archives.
# Expects: three pieces archived. Leaves: the same.

Output out/discard.txt
# The picture shows the command, its output and the prompt: 8 rows of 38.46
# pixels plus 87 pixels, the padding of 80 and 7 that vhs keeps, make 395,
# rounded up to an even height.
Set Height 396
Source common.tape

Type "antiq grep 'main\.rs'"
Enter
Sleep 1s
Screenshot ../assets/shots/grep.png
Sleep 1s
