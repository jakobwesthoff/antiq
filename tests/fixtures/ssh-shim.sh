#!/bin/sh
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# A stand-in for ssh that runs commands on this machine. It answers the
# calls antiq makes (master start, -O check, -O exit, a command over the
# master or direct) and nothing else: an option it does not know is an
# error, so a changed command line shows up in the tests.
#
# Tests reach it through a symlink named ssh in a directory of their own.
# That directory is the state directory:
#   calls.log    one line per call, the arguments separated by tabs
#   master.pid   pid of the master process, while one runs
#   master.lock  held by the master process for as long as it lives
#   master.log   path of the -E file the master was started with
#   leaked       one line per -M that real ssh would have left running as
#                a plain connection (the control path was taken)
#   fault        optional: "<name> [start|command|all]"; the call prints
#                ssh-texts/<name>.txt next to this script and exits 255
#                (see the cases below for the names that do something else)
#
# The master is a perl process that listens on the control socket, so
# -O check can connect to it the way ssh does. ControlPersist expiry is
# not simulated.

state=$(cd "$(dirname "$0")" && pwd)
script=$(readlink "$0")
texts="$(dirname "$script")/ssh-texts"

printf '%s\t' "$@" >>"$state/calls.log"
printf '\n' >>"$state/calls.log"

sock=""
elog=""
op=""
master=0
tty_off=0
while [ $# -gt 0 ]; do
    case "$1" in
    -o | -p | -l)
        shift 2
        ;;
    -S)
        sock="$2"
        shift 2
        ;;
    -E)
        elog="$2"
        shift 2
        ;;
    -O)
        op="$2"
        shift 2
        ;;
    -M)
        master=1
        shift
        ;;
    -N | -f)
        shift
        ;;
    -T)
        tty_off=1
        shift
        ;;
    --)
        shift
        break
        ;;
    *)
        echo "shim: unknown option $1" >&2
        exit 255
        ;;
    esac
done
host="${1:-}"
[ $# -gt 0 ] && shift
command="$*"

fault_name=""
fault_on="all"
if [ -f "$state/fault" ]; then
    read -r fault_name fault_on <"$state/fault"
    [ -n "$fault_on" ] || fault_on="all"
fi

# fault_applies <start|command>
fault_applies() {
    [ -n "$fault_name" ] && { [ "$fault_on" = all ] || [ "$fault_on" = "$1" ]; }
}

connects() {
    perl -e 'use Socket; socket(S, PF_UNIX, SOCK_STREAM, 0) or exit 1; exit(connect(S, sockaddr_un($ARGV[0])) ? 0 : 1)' "$1"
}

# Blocks until the master process has exited and released its lock.
wait_for_master_exit() {
    perl -e 'use Fcntl qw(:flock); open(my $l, ">>", $ARGV[0]) or exit 1; flock($l, LOCK_EX) or exit 1' "$state/master.lock"
}

stop_master() {
    kill -TERM "$(cat "$state/master.pid")" 2>/dev/null
    wait_for_master_exit
}

# =========================================================
# -M: start a master
# =========================================================
if [ "$master" = 1 ]; then
    [ -n "$sock" ] || {
        echo "shim: -M without -S" >&2
        exit 255
    }
    # Like ssh -E: everything the call says goes to the log file.
    if [ -n "$elog" ]; then
        exec 2>>"$elog"
        printf '%s\n' "$elog" >"$state/master.log"
    fi
    if fault_applies start; then
        case "$fault_name" in
        no-master) exit 0 ;;
        *)
            cat "$texts/$fault_name.txt" >&2
            exit 255
            ;;
        esac
    fi
    if [ "${#sock}" -gt 86 ]; then
        echo "unix_listener: path \"$sock.UkEQw139fM4rd3rY\" too long for Unix domain socket" >&2
        exit 255
    fi
    if [ -e "$sock" ] || [ -h "$sock" ]; then
        echo "ControlSocket $sock already exists, disabling multiplexing" >&2
        echo "$sock" >>"$state/leaked"
        exit 0
    fi
    perl -e '
        use strict;
        use Socket;
        use Fcntl qw(:flock);
        my ($path, $lock, $pidfile) = @ARGV;
        open(my $lk, ">>", $lock) or die "lock: $!";
        flock($lk, LOCK_EX | LOCK_NB) or die "lock held";
        socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die "socket: $!";
        bind($s, sockaddr_un($path)) or die "bind: $!";
        listen($s, 5) or die "listen: $!";
        my $pid = fork();
        die "fork: $!" unless defined $pid;
        if ($pid) {
            open(my $pf, ">", $pidfile) or die "pidfile: $!";
            print $pf "$pid\n";
            close $pf;
            exit 0;
        }
        open(STDIN, "<", "/dev/null");
        open(STDOUT, ">", "/dev/null");
        open(STDERR, ">", "/dev/null");
        $SIG{TERM} = sub { unlink $path; exit 0 };
        while (1) {
            my $c;
            accept($c, $s);
            close $c if $c;
        }
    ' "$sock" "$state/master.lock" "$state/master.pid"
    exit $?
fi

# =========================================================
# -O: ask a master
# =========================================================
if [ -n "$op" ]; then
    [ -n "$sock" ] || {
        echo "shim: -O without -S" >&2
        exit 255
    }
    if [ ! -e "$sock" ] && [ ! -h "$sock" ]; then
        echo "Control socket connect($sock): No such file or directory" >&2
        exit 255
    fi
    if ! connects "$sock"; then
        echo "Control socket connect($sock): Connection refused" >&2
        exit 255
    fi
    case "$op" in
    check)
        echo "Master running (pid=$(cat "$state/master.pid"))" >&2
        exit 0
        ;;
    exit)
        stop_master
        echo "Exit request sent." >&2
        exit 0
        ;;
    *)
        echo "shim: unknown -O $op" >&2
        exit 255
        ;;
    esac
fi

# =========================================================
# A command
# =========================================================
[ "$tty_off" = 1 ] || {
    echo "shim: a command without -T" >&2
    exit 255
}
[ -n "$host" ] || {
    echo "shim: no host" >&2
    exit 255
}

if [ -n "$sock" ] && { [ -e "$sock" ] || [ -h "$sock" ]; }; then
    if connects "$sock"; then
        live=1
    else
        live=0
        echo "Control socket connect($sock): Connection refused" >&2
    fi
else
    live=0
fi

if fault_applies command; then
    case "$fault_name" in
    session-refused)
        cat "$texts/session-refused.txt" >&2
        ;;
    master-drop)
        # The server stopped answering: the master logs the keepalive
        # timeout and exits, the client sees status 255 and nothing else.
        if [ "$live" = 1 ]; then
            cat "$texts/keepalive-timeout.txt" >>"$(cat "$state/master.log")"
            stop_master
        fi
        exit 255
        ;;
    no-master) ;;
    die)
        # ssh killed by a signal has no exit status.
        kill -KILL $$
        ;;
    *)
        cat "$texts/$fault_name.txt" >&2
        exit 255
        ;;
    esac
fi

sh -c "$command"
exit $?
