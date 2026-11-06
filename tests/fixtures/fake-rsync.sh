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
#   pace          seconds a transfer waits after each line of its stdout
#   stderr_fill   bytes a transfer writes to stderr before anything else
#
# A run with several transfers answers each differently: the Nth transfer
# (counted from 1, versions not counted) reads stdout.N, stderr.N, exit.N
# and signal.N, and falls back to the plain file of that name when the
# numbered one is absent. The counter lives in the file count.
#
# Every call appends a record to calls.log: the mode ("version" or
# "transfer"), then each argument, each followed by a NUL, and the record
# ends with the byte 0x1e so that the log splits into calls. env.log gets
# one line per RSYNC_* or CVSIGNORE variable the call received, and the
# value of LC_ALL.
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
printf '\036' >>"$dir/calls.log"

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

call=0
if [ -f "$dir/count" ]; then
    call=$(cat "$dir/count")
fi
call=$((call + 1))
printf '%s' "$call" >"$dir/count"

# answer NAME prints the file of the Nth call, else the plain one.
answer() {
    if [ -f "$dir/$1.$call" ]; then
        echo "$dir/$1.$call"
    elif [ -f "$dir/$1" ]; then
        echo "$dir/$1"
    fi
}

if [ -n "$(answer signal)" ]; then
    kill -KILL $$
fi
file=$(answer stderr_fill)
if [ -n "$file" ]; then
    head -c "$(cat "$file")" /dev/zero | tr '\0' 'x' >&2
fi
file=$(answer stdout)
pace=$(answer pace)
if [ -n "$file" ]; then
    if [ -n "$pace" ]; then
        while IFS= read -r line; do
            printf '%s\n' "$line"
            sleep "$(cat "$pace")"
        done <"$file"
    else
        cat "$file"
    fi
fi
file=$(answer stderr)
if [ -n "$file" ]; then
    cat "$file" >&2
fi
file=$(answer exit)
if [ -n "$file" ]; then
    exit "$(cat "$file")"
fi
exit 0
