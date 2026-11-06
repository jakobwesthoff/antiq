# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Writes the number of columns that the terminal of the captures has to
# columns.txt. render.sh compares it with the number common.tape promises.
# Expects: any world. Leaves: nothing.

Output out/columns.txt
Set Height 300
Source common.tape

Hide
Type "tput cols"
Enter
Sleep 1s
Show
