# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Show what the next archive would do.
# Expects: keep-me is pinned, nothing archived. Leaves: the same.

Output out/discard.txt
Set Height 400
Source common.tape

Type "antiq archive -n"
Enter
Sleep 1s
Screenshot ../assets/shots/dry-run.png
Sleep 1s
