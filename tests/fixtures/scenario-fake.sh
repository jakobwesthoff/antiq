#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# A stand-in for the antiq binary, for the tests of the scenario harness.
# It takes `-C <dir>` and `--json` like antiq and knows a few commands that
# do one observable thing each, so a harness test can tell what the harness
# ran, in which mode and with which environment.
#
#   put <dir> <name> [modedep]  writes the file <dir>/<name>, with <dir>
#                               relative to the -C directory unless it is
#                               absolute. Its content is "same", or with
#                               modedep "mode-json" or "mode-text".
#   fail                        exits 1 with the error kind "other"
#   badjson                     prints text even under --json
#   tz                          prints $TZ
#   tty                         prints "tty" or "pipe" for its stdout
#   flood                       prints 20000 lines of "line", more than a
#                               terminal buffer holds
#   hang                        sleeps for 30 seconds
#   touchdir <dir>              creates a file in <dir> (relative to -C),
#                               which changes the directory's mtime
#
# Under --json a command answers {"format":1,"command":"<c>","<key>":"<v>"}.

set -eu

root=.
json=0
while [ $# -gt 0 ]; do
    case $1 in
    -C)
        root=$2
        shift 2
        ;;
    --json)
        json=1
        shift
        ;;
    *) break ;;
    esac
done

command=${1:-}
[ $# -gt 0 ] && shift

# answer <key> <value>: the one line a command prints in either mode.
answer() {
    if [ "$json" = 1 ]; then
        printf '{"format":1,"command":"%s","%s":"%s"}\n' "$command" "$1" "$2"
    else
        printf '%s\n' "$2"
    fi
}

case $command in
put)
    dir=$1
    name=$2
    case $dir in
    /*) ;;
    *) dir=$root/$dir ;;
    esac
    content=same
    if [ "${3:-}" = modedep ]; then
        if [ "$json" = 1 ]; then content=mode-json; else content=mode-text; fi
    fi
    mkdir -p "$dir"
    printf '%s' "$content" >"$dir/$name"
    answer path "$name"
    ;;
fail)
    if [ "$json" = 1 ]; then
        printf '{"format":1,"command":"fail","error":{"kind":"other","message":"it failed"}}\n'
    else
        echo "error: it failed" >&2
    fi
    exit 1
    ;;
badjson)
    echo "this is not json"
    ;;
tz)
    answer tz "${TZ:-unset}"
    ;;
tty)
    if [ -t 1 ]; then answer stdout tty; else answer stdout pipe; fi
    ;;
flood)
    i=0
    while [ "$i" -lt 20000 ]; do
        echo line
        i=$((i + 1))
    done
    ;;
hang)
    exec sleep 30
    ;;
touchdir)
    dir=$1
    case $dir in
    /*) ;;
    *) dir=$root/$dir ;;
    esac
    : >"$dir/.touched"
    answer touched "$1"
    ;;
*)
    echo "scenario-fake: unknown command '$command'" >&2
    exit 64
    ;;
esac
