# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Claim the target and mirror the collection. The target block is the one the
# tutorial shows; the demo world made the folder it names.
# Expects: pieces archived, an empty target folder. Leaves: the target holds
# the collection.

Output out/discard.txt
# The picture shows the command, 12 lines of output and the prompt: 14 rows
# of 38.46 pixels and the padding of 80 make 620.
Set Height 620
Source common.tape

Hide
Type `printf '[target]\nurl = "/tmp/antiq-demo/target"\n' >> .antiq/config.toml && clear`
Enter
Show
Type "antiq push --init"
Enter
Wait+Screen@120s /took [0-9]+:[0-9][0-9]/
Sleep 1s
Screenshot ../assets/shots/push.png
Sleep 1s
