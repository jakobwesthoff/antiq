# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Find versions by piece name and by the files they hold.
# Expects: three pieces archived. Leaves: the same.

Output out/discard.txt
Set Height 520
Source common.tape

Type "antiq list scan"
Enter
Sleep 1s
Type "antiq grep 'main\.rs'"
Enter
Sleep 1s
Screenshot ../assets/shots/find.png
Sleep 1s
