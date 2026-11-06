#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# A stand-in for ssh that runs the command on this machine, so that rsync
# can use it as its remote shell (`-e`) against a directory of the test.
#
# A test reaches it through a symlink named ssh in a directory of its own,
# the state directory:
#   calls.log   one line per call, the arguments separated by tabs
#   fault       optional: a name; the call prints ssh-texts/<name>.txt next
#               to this script and exits 255 without running anything
#
# It understands the options rsync passes (-o, -l and -p with a value) and
# a few without one, and fails on any other so that a changed command line
# shows up in the tests.

state=$(cd "$(dirname "$0")" && pwd)
script=$(readlink "$0")
texts="$(dirname "$script")/ssh-texts"

printf '%s\t' "$@" >>"$state/calls.log"
printf '\n' >>"$state/calls.log"

while [ $# -gt 0 ]; do
    case "$1" in
    -o | -l | -p)
        shift 2
        ;;
    -T | -4 | -6 | -q | -x)
        shift
        ;;
    --)
        shift
        break
        ;;
    -*)
        echo "shim: unknown option $1" >&2
        exit 255
        ;;
    *)
        # The first word that is no option is the host. rsync puts no
        # `--` before it.
        break
        ;;
    esac
done
[ $# -gt 0 ] && shift
command="$*"

if [ -f "$state/fault" ]; then
    read -r fault <"$state/fault"
    cat "$texts/$fault.txt" >&2
    exit 255
fi

exec sh -c "$command"
