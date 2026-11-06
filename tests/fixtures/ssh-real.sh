#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# The real ssh with a configuration file of its own, so that the user's
# ~/.ssh/config stays out of a test. Tests reach it through a symlink named
# ssh in a directory that also holds the file ssh_config.

dir=$(cd "$(dirname "$0")" && pwd)
exec /usr/bin/ssh -F "$dir/ssh_config" "$@"
