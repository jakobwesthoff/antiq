# SSH from Rust

Researched 2026-11-03 on macOS (arm64) with OpenSSH_10.3p1 (LibreSSL
3.3.6), rsync 3.5.1 (Homebrew) and openrsync (protocol 29,
`/usr/bin/rsync`). Crate figures come from the crates.io API on that day.

Marks: **measured** (run on this machine), **read** (documentation or
source, named), **reported** (issue tracker or blog, linked).

## Requirements

1. Run commands on the NAS with exit status and stderr: probe tools,
   exclusive create with `set -C`, rename, delete, list, read small
   files, SHA-256 on the NAS, `ln`.
2. One long-lived session; a sync without changes costs at most two SSH
   connections.
3. rsync uses the same connection, or at least the user's SSH
   configuration.
4. BatchMode, ConnectTimeout, ServerAlive keepalives, HostKeyAlias.
5. The user's `~/.ssh/config`, ssh-agent, key types and known_hosts are
   honoured.
6. Reachability as three states: reachable, away (no route, timeout),
   broken (answers but refuses: authentication, changed host key,
   missing tool).
7. Maturity, licence compatible with an MPL-2.0 binary, RustSec.
8. File names with spaces and quotes reach the remote shell safely.
9. A killed antiq leaves nothing behind that breaks the next run.
10. Cost of a new connection compared with a multiplexed channel.

## Option A: the `ssh` executable with ControlMaster

- Configuration, agent, keys and known_hosts are those of the user's own
  `ssh`. `ssh -G` resolves the effective options without connecting;
  `-o BatchMode=yes -o ConnectTimeout=5 -o HostKeyAlias=nas
  -o ControlMaster=auto -o ControlPath=... -o ControlPersist=60` all
  resolved to the given values (**measured**, `ssh -G -F /dev/null`).
  Options given with `-o` take precedence over `~/.ssh/config`.
- Multiplexing: `ssh -M -N -f -S <sock> -o ControlPersist=<t> host`
  starts a master; `ssh -S <sock> host cmd` runs a command as a channel
  on it; `ssh -S <sock> -O check|exit host` queries or stops it
  (**read**, `man ssh`, `man ssh_config`).
- `ControlPersist=yes` or `0` keeps the master until it is killed or
  stopped with `-O exit`; a time value makes it exit after being idle
  that long without clients (**read**, `man ssh_config`, OpenSSH 10.3). A
  finite value therefore cleans up after a killed antiq without a
  cleanup pass. Not tested against a live server.
- A master that died without removing its socket makes `ssh -S` fail to
  connect. Not reproduced. The usual handling is `-O check`, then
  removing the dead socket and starting a new master.
- Unix socket paths are limited by `sun_path[104]` on macOS (**read**,
  SDK `sys/un.h`), so the ControlPath needs a short directory.
- Errors: exit status 255 means `ssh` itself failed; any other status is
  the remote command's. The kind of failure is only in the stderr text
  ("Could not resolve", "Network is unreachable", "Connection refused",
  "Operation timed out", "REMOTE HOST IDENTIFICATION HAS CHANGED"),
  which depends on version and locale. Under BatchMode a changed host
  key ends the connection with exit 255 (**measured**).
- Quoting: `ssh` joins the remote arguments with spaces and hands the
  string to the remote login shell, so quoting is the caller's job:
  POSIX single quotes (`'` becomes `'\''`), or a fixed script on stdin
  (`ssh host sh -s`) with values passed as positional arguments.
- Connection cost: each new `ssh` process does TCP setup, key exchange
  and authentication; a multiplexed command only opens a channel
  (**read**, `man ssh`, ControlMaster). Not timed: no SSH server was
  reachable from this machine.
- rsync: `rsync -e "ssh -S <sock> -o ControlMaster=no -o BatchMode=yes"`
  makes rsync's ssh child use the existing master. rsync splits the `-e`
  string itself, removing quotes, and runs the program with plain argv
  `[options] <host> <rsync-path> --server <flags> . <dest>`; rsync 3.5.1
  and openrsync behaved the same (**measured** with a logging stand-in
  for `ssh`).
- openrsync sends a different server flag set (`-g -l -o -p -D -r -t
  --dirs`) than rsync 3.5.1 (`-logDtpre.iLsfxCIvu`) (**measured**), so
  the rsync binary has to be probed.
- Licence: OpenSSH is called as an executable; no linking.

## Option B: the `openssh` crate

- 0.11.6, released 2025-12-03; first release 2020-04-11; 5,565,706
  downloads, 485,799 in the last 90 days; 42 reverse dependencies;
  MIT OR Apache-2.0; no RustSec entry (**read**/**measured**, crates.io
  API, rustsec.org 404).
- It runs `ssh -M -f -N -S <tmp>/master -o ControlPersist=...
  -o BatchMode=yes ...` and then either one `ssh -S` per command
  (`process-mux`) or speaks the mux protocol to the socket directly
  (`native-mux`) (**read**, `src/builder.rs`, `src/process_impl/session.rs`).
- Configuration, agent and known_hosts come from the real `ssh`. The
  builder offers `connect_timeout` and `server_alive_interval`, sets
  BatchMode always, and has no HostKeyAlias and no free `-o` option; a
  generated config file passed with `-F` is the only way to set other
  options, and it replaces `~/.ssh/config` unless it includes it
  (**read**, `src/builder.rs`).
- ControlPersist defaults to `Forever` (`ControlPersist=yes`);
  `IdleFor(d)` exists (**read**, `src/builder.rs`). Cleanup runs in
  `Drop`, which a kill skips; control directories `.ssh-connection*` stay
  behind. `clean_history_control_directory(true)` removes all of them in
  the control directory, including those of another running process
  (**read**, `src/builder.rs`).
- `Session::control_socket()` returns the socket path, so rsync can join
  the master the same way as in option A (**read**, `src/session.rs`).
- `Command::arg()` escapes bash-style; `raw_arg()` passes the argument
  unescaped (**read**, `src/command.rs`, `src/escape.rs`).
- Error kinds come from parsing ssh's stderr; the source calls this
  "error-prone, but the best we can do" (**read**, `src/error.rs`).
- Async only, on tokio.

## Option C: `russh` (pure Rust)

- 0.65.0, released 2026-10-25; first release 2022-03-13; 9,640,933
  downloads, 4,191,283 recent; 264 reverse dependencies; Apache-2.0;
  rust-version 1.89 (**read**, crates.io API).
- RUSTSEC-2026-0154 (HIGH): unbounded allocation in agent frame
  parsing, fixed in 0.60.3 and later (**read**,
  <https://rustsec.org/advisories/RUSTSEC-2026-0154.html>).
- Key types ed25519, ECDSA P-256/384/521, rsa-sha2-256/512; crypto
  backend aws-lc-rs by default (C code), ring optional (**read**,
  README). Agent client via `SSH_AUTH_SOCK`; known_hosts checks take a
  host string and port (**read**, `russh/src/keys/`).
- `~/.ssh/config` is not read by `russh`. The companion `russh-config`
  0.58.0 understands only Host, Hostname, User, Port, IdentityFile,
  ProxyCommand, ProxyJump, AddKeysToAgent, UserKnownHostsFile and
  StrictHostKeyChecking; every other key, including HostKeyAlias,
  Include and Match, is ignored (**read**, `russh-config/src/lib.rs`).
- Many channels over one connection, each with stdout, stderr and exit
  status (**read**, `client/mod.rs`, `channels/mod.rs`). Async only.
- rsync cannot use the connection (see "Bridging rsync" below).

## Option D: `ssh2` (libssh2)

- 0.9.6, released 2026-06-30; first release 2014-11-11; 8,562,829
  downloads, 1,298,964 recent; 183 reverse dependencies; MIT OR
  Apache-2.0; wraps libssh2 (BSD-3-Clause) and needs OpenSSL, vendored
  on request; no RustSec entry for `ssh2` or `libssh2-sys` (**read**,
  crates.io; **measured**, rustsec.org 404).
- Blocking API with agent authentication, known_hosts, several channels
  on one session, timeouts and keepalives (**read**, `src/session.rs`).
- Does not read `~/.ssh/config`; the separate `ssh2-config` 0.9.0 parses
  it, key coverage not examined.
- rsync cannot use the connection.

## Option E: `libssh-rs` (libssh)

- 0.3.8, released 2026-07-05; 326,223 downloads; 2 reverse
  dependencies; the binding is MIT, libssh is LGPL-2.1 (**read**,
  crates.io; libssh `COPYING` on its GitHub mirror). Linking libssh
  statically into an MPL-2.0 binary brings LGPL relinking obligations.
- `options_parse_config` lets libssh read `~/.ssh/config`; coverage of
  Include, Match and HostKeyAlias not checked. No keepalive option found
  in the binding (**read**, `libssh-rs/src/lib.rs`).
- rsync cannot use the connection.

## Others

- `thrussh` 0.49.0, ancestor of russh: 7,794 recent downloads.
- `async-ssh2-tokio` 0.13.0: a layer over russh; crates.io lists its
  licence as non-standard.
- `openssh-sftp-client`: SFTP over an `openssh` session; antiq uses
  shell commands for metadata and rsync for payloads.
- `ssh_config` 0.1.0 (2020): no reverse dependencies, unmaintained.

## Bridging rsync to a library connection

The library clients (C, D, E) cannot hand their connection to a child
process, and rsync needs a program that behaves like `ssh host command`
on stdin and stdout. Two ways remain:

1. rsync opens a second connection with the real `ssh`. Two separately
   configured SSH stacks then exist side by side.
2. antiq passes itself as the rsync shell (`-e "<antiq> --rsh-shim"`);
   the shim forwards stdin, stdout, stderr and the exit status to an
   exec channel of the library session. rsync 3.5.1 and openrsync both
   run an arbitrary `-e` program with plain argv (**measured**). No crate
   doing this was found; half-close, stderr, exit status and timeouts
   would be antiq's own code.

Options A and B need neither: rsync's ssh child joins the master socket.

## Comparison

| Requirement | A: ssh + ControlMaster | B: openssh | C: russh | D: ssh2 | E: libssh-rs |
|---|---|---|---|---|---|
| Full `~/.ssh/config` | yes | yes (via ssh) | 10 keys | no | not checked |
| ssh-agent | yes | yes | yes | yes | not checked |
| Commands on one connection | master socket | master socket | channels | channels, blocking | channels |
| rsync on the same connection | yes | yes | shim needed | shim needed | shim needed |
| BatchMode, ConnectTimeout, keepalive | `-o` | partly, fixed | own code | own code | timeout only |
| HostKeyAlias | `-o` | only via `-F` file | host argument | host argument | not checked |
| Reachability from errors | stderr + exit 255 | parsed stderr | typed errors | typed errors | typed errors |
| Quoting | own | `arg()` escapes | own | own | own |
| After a kill | finite ControlPersist exits | master stays (default), dirs left | nothing left | nothing left | nothing left |
| Version, date | n/a | 0.11.6, 2025-12-03 | 0.65.0, 2026-10-25 | 0.9.6, 2026-06-30 | 0.3.8, 2026-07-05 |
| Downloads, recent | n/a | 486k | 4.19M | 1.30M | 45k |
| Reverse dependencies | n/a | 42 | 264 | 183 | 2 |
| RustSec | n/a | none | 2026-0154, fixed | none | none |
| Licence | executable | MIT/Apache | Apache | MIT/Apache + BSD C | MIT + LGPL C |
| Runtime | std::process | tokio | tokio | blocking | blocking |

## Assessment

- rsync has to run over the user's SSH setup, sharing the session with
  the metadata commands. Only options A and B provide that without extra
  code.
- The NAS access is described by `~/.ssh/config`. The executable reads
  all of it; `russh-config` reads ten keys and `ssh2` none. A silent
  difference from what `ssh nas` does is a worse failure than a missing
  feature.
- Between A and B: B brings a session type and `native-mux`, but no
  HostKeyAlias, no free `-o`, tokio, a default of a master that never
  exits, and leftover directories after a kill. A small module in antiq
  (start the master, `-O check`, `-O exit`, run a command via `-S`, a
  quoting helper, a stderr classifier) covers what B offers.
- Recommendation: option A. Finite ControlPersist (for example 60 s),
  ControlPath in a short per-user directory, `BatchMode`,
  `ConnectTimeout`, `ServerAliveInterval`, `ServerAliveCountMax` and an
  optional `HostKeyAlias` always given with `-o`.
- With a master, a sync without changes uses one connection: commands
  and rsync are channels on it.
- russh becomes worth a look only where no OpenSSH client exists, and
  would still need the rsync shim.

## Not verified

- No SSH server was reachable, so multiplexing, a killed master, a stale
  socket and connection cost were not tested; points 9 and 10 rest on
  documentation.
- The NAS's sshd: version, algorithms, login shell and `MaxSessions`
  (OpenSSH's default is 10 sessions per connection, **read**,
  `man sshd_config`) are untested.
- `-O check` on the OpenSSH versions Linux distributions ship.
- Notable dependents of `russh` and `openssh`; `ssh2-config` and libssh
  config coverage; russh keepalive settings.

## Related findings

- `ssh -G host` prints the resolved configuration without connecting
  (**measured**); antiq can use it to show the effective host, user and
  ControlPath in `status` and error messages.
