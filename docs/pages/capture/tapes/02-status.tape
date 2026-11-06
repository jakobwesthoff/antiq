# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Show what is idle.
# Expects: a collection, nothing archived. Leaves: the same.

Output out/discard.txt
Set Height 500
Source common.tape

Type "antiq status"
Enter
Sleep 1s
Screenshot ../assets/shots/status.png
Sleep 1s
