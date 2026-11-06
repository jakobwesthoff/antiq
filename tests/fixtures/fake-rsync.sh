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
#   stdout        printed verbatim by a transfer
#   stderr        printed verbatim to standard error by a transfer
#   exit          exit status of a transfer (default 0)
#   signal        when present, a transfer kills itself with SIGKILL
#
# Every call appends a record to calls.log: the mode ("version" or
# "transfer"), then each argument, all NUL-separated. env.log gets one line
# per RSYNC_* or CVSIGNORE variable the call received, and the value of
# LC_ALL.
#
# A test can also hand the script to rsync as its remote shell (-e), where
# it records the arguments rsync passes and fails the connection.

set -eu

dir=$(dirname "$0")

if [ "${1-}" = "--version" ]; then
    mode=version
else
    mode=transfer
fi

printf '%s\0' "$mode" "$@" >>"$dir/calls.log"

{
    env | grep -E '^(RSYNC_|CVSIGNORE=)' || true
    printf 'LC_ALL=%s\n' "${LC_ALL-}"
} >>"$dir/env.log"

if [ "$mode" = version ]; then
    cat "$dir/version"
    if [ -f "$dir/version_exit" ]; then
        exit "$(cat "$dir/version_exit")"
    fi
    exit 0
fi

if [ -f "$dir/signal" ]; then
    kill -KILL $$
fi
if [ -f "$dir/stdout" ]; then
    cat "$dir/stdout"
fi
if [ -f "$dir/stderr" ]; then
    cat "$dir/stderr" >&2
fi
if [ -f "$dir/exit" ]; then
    exit "$(cat "$dir/exit")"
fi
exit 0
