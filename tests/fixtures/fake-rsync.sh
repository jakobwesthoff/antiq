#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# A stand-in for rsync. A test symlinks this script into a temporary
# directory, and the script reads its answers from plain files beside the
# symlink, so the test process never writes an executable.
#
#   version       printed verbatim for --version
#   version_exit  exit status of --version (default 0)
#
# Every call appends a record to calls.log: the mode ("version"), then each
# argument, all NUL-separated. env.log gets one line per RSYNC_* or
# CVSIGNORE variable the call received, and the value of LC_ALL.

set -eu

dir=$(dirname "$0")

printf '%s\0' version "$@" >>"$dir/calls.log"

{
    env | grep -E '^(RSYNC_|CVSIGNORE=)' || true
    printf 'LC_ALL=%s\n' "${LC_ALL-}"
} >>"$dir/env.log"

cat "$dir/version"
if [ -f "$dir/version_exit" ]; then
    exit "$(cat "$dir/version_exit")"
fi
exit 0
