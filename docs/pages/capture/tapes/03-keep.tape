# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Pin a folder so that it stays.
# Expects: a collection, nothing archived. Leaves: keep-me is pinned.

Output out/discard.txt
Set Height 780
Source common.tape

Type "antiq pin keep-me"
Enter
Sleep 1s
Type "antiq status"
Enter
Sleep 1s
Screenshot ../assets/shots/keep.png
Sleep 1s
