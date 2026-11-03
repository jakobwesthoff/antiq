# Delta transfer from Rust: rsync

Researched 2026-11-03 on macOS arm64 with Homebrew rsync 3.5.1
(protocol 33), `/usr/bin/rsync` (openrsync, "protocol version 29, rsync
version 2.6.9 compatible"), Rust 1.97.0, `fast_rsync` 0.2.0 and
`librsync` 0.2.6. Crate figures come from the crates.io API on that day.
No SSH server was reachable, so the transport was a wrapper script given
to `-e` that runs the remote command locally: the rsync client and
server protocol ran between two processes, without SSH.

Marks: **measured** (run on this machine), **read** (documentation or
source, named), **reported** (third party, linked).

## Requirements

| # | Requirement |
|---|---|
| R1 | Refresh a package of tens of MB to GBs by sending only changed parts; the previous package on the NAS is the basis |
| R2 | Works against a stock Synology NAS over SSH without installing anything |
| R3 | The path remote is a plain copy |
| R4 | Transfer stats, literal and matched bytes |
| R5 | Timeouts; exit code 24 tolerated |
| R6 | Refuse a local rsync below 3.1; openrsync is the macOS default |
| R7 | Licence compatible with an MPL-2.0 binary |
| R8 | Works on tar and zstd rsyncable packages |
| R9 | On-the-wire compression |
| R10 | Machine-readable stats |

## Option A: the `rsync` executable

### Versions

- `rsync --version` starts with `rsync  version X.Y.Z  protocol version
  N` for rsync and with `openrsync: protocol version 29` for openrsync
  (**measured**), so a probe that requires the first form and 3.1 or
  later refuses openrsync and rsync 2.x.
- NAS: DSM 7 users report rsync 3.1.1 to 3.1.3, and `--mkpath` missing
  on a DS224+ with DSM 7.2.2 (**reported**,
  <https://www.synoforum.com/threads/rsync-via-ssh-on-dsm-7.7924/>,
  <https://www.synology-forum.de/goto/post?id=1237006>,
  <https://www.synoforum.com/threads/rsync-fails-to-sync-remote-source.8754/>).
  No Synology document was found.
- Feature versions (**read**, rsync `NEWS.md`): 3.1.0 protocol 31,
  `--info`, extended `--stats`; 3.2.0 `--compress-choice` with zstd and
  lz4; 3.2.3 `--mkpath`; 3.2.6 `--secluded-args`; 3.4.0 protocol 32 and
  the CVE-2024-12084 to 12088 and 12747 fixes; 3.5.1 protocol 33.
- openrsync and rsync 3.5.1 completed delta transfers with each other in
  both directions (**measured**).

### Flags openrsync rejects

`--info=...`, `--compress-choice`, `--checksum-choice` and
`--protect-args`/`-s` are rejected by openrsync and accepted by 3.5.1;
`--timeout`, `--inplace`, `--partial`, `--fuzzy`, `--no-whole-file`,
`--temp-dir`, `--rsync-path`, `-I` and `--out-format` are accepted by
both (**measured**, parsing only).

### Stats

There is no JSON or key=value output in any version; `--out-format`
works per file and has no literal/matched split (**read**, `man rsync`
3.5.1; **measured**). The sending side prints the stats on stdout, so
for a push the local binary's format applies. One 20 MB file with 5,000
changed bytes (**measured**):

```
rsync 3.5.1                           openrsync
Literal data: 8,944 bytes             Unmatched data: 8944 B
Matched data: 19,991,056 bytes        Matched data: 19991056 B
Total bytes sent: 26,915              Total sent: 26919 B
```

Digits follow the locale: under `LC_ALL=de_DE.UTF-8` rsync 3.5.1
printed `Literal data: 1.408 bytes`, under `LC_ALL=C` with
`--no-human-readable` `Literal data: 1408 bytes` (**measured**).
openrsync rejects `--no-human-readable`.

### Exit codes and timeouts

- 3.5.1 exited 24 for files vanishing during the transfer; openrsync
  exited 23 in the same race (**measured**, one run).
- With `--timeout=3` and a transport that never answers, 3.5.1 exited 30
  ("io timeout") after about 6 s. openrsync with `--timeout=3` ran the
  full 16 s of a hung transport and exited 1 (**measured**, one run
  each).
- `--timeout` is an inactivity timer; SSH's `ConnectTimeout` and
  `ServerAlive*` options reach rsync's ssh through `-e` (**read**,
  `man rsync`).
- Exit codes 12, 23 (on 3.5.1) and 255 were not provoked.

### Hard-link seeding

The design seeds the delta basis with `ln <current> <new>` on the NAS,
then transfers onto `<new>` (**measured**, 3.5.1 and openrsync):

- Default mode (temp file, then rename): 8,944 bytes literal; the old
  name kept its bytes, the new name held the new package.
- With `--inplace` rsync wrote through the shared inode and the old name
  changed as well. `--inplace` and `--append` break the seeding.
- In default mode the receiver writes a complete temp file before the
  rename (**read**), so the NAS needs room for a second copy; not
  measured.

### Compression on the wire

`-z` (zlib) exists in 3.1.x and is accepted by both binaries;
`--compress-choice=zstd` needs 3.2.0 on both ends (**read**, NEWS). On
two folders of about 450 MB each, a local transfer with `-z` sent 1 to
17 % of the bytes it sent without it for a plain tar package, and only
the literal data plus about 100 bytes for a zstd rsyncable package
(**measured**).

### Cost and benefit

The executable brings the user's SSH setup through `-e`, uses the rsync
already on the NAS as the receiver, and is a GPL program run, not
linked. It costs a runtime dependency, version and output differences,
text parsing with a fixed locale, and exit code mapping. Whether DSM's
non-interactive SSH `PATH` finds rsync, or `--rsync-path` is needed, was
not verified.

## Option B: librsync and its bindings

- librsync is LGPL-2.1; latest release 2.3.4 on 2023-02-19 (**read**,
  GitHub API). It implements signature, delta and patch as separate
  steps in the rdiff format and cannot talk to `rsync --server`.
- `librsync` crate 0.2.6 (2026-08-26; 26,869 downloads; 2 reverse
  dependencies) over `librsync-sys` 0.1.4, which vendors librsync 2.2.2
  and builds without cmake (**read**, **measured**). Static linking of
  an LGPL library brings relinking obligations.

## Option C: pure Rust

- `fast_rsync` 0.2.0 (Dropbox, Apache-2.0; released 2023-03-31; 869,068
  downloads; 6 reverse dependencies): MD4 signatures only, in-memory
  `&[u8]` API, rdiff format, callers must verify the output of `apply`
  (**read**, README and source).
- Round trip of a 20 MB file with 5,000 changed bytes (**measured**,
  single runs): `librsync` defaults gave a 351,588-byte signature and an
  8,215-byte delta; `fast_rsync` with 4 KiB blocks gave 58,608 and
  11,546 bytes; both patched correctly.
- rsync wire protocol in Rust: `rsyn` 0.0.1 (2020) lists directories
  only; `arrsync` 0.2.1 receives only; `aerorsync` 0.0.2 is an empty
  placeholder; `ssync` 0.3.0 was first released 2026-11-01 with 18
  downloads; `sy` 0.4.0 needs its own server on the remote (**read**,
  READMEs and crates.io).

## Option D: own delta over SSH

The rsync algorithm needs a signature of the old file, which lives on
the NAS, and something on the NAS that applies the delta to it. A plain
SSH shell offers neither. The remaining routes are a helper binary per
NAS CPU architecture, or storing deltas as a chain, which breaks one
payload file per version. Neither was tested.

## Comparison

| Option | R1 delta | R2 stock NAS | R4 stats | R5 timeout | R7 licence | Maturity |
|---|---|---|---|---|---|---|
| rsync 3.1+ | yes | NAS rsync 3.1.x | text, `LC_ALL=C` | exit 30 | GPL program, run | protocol 31 since 2013 |
| openrsync | yes | yes | other labels, no `--info` | did not fire once | BSD | macOS default |
| `librsync` | algorithm only | no peer | own | own | LGPL C | 2 reverse deps |
| `fast_rsync` | algorithm only, in memory | no peer | own | own | Apache-2.0 | last release 2023 |
| Rust wire-protocol crates | none usable | | | | | pre-release |
| own delta | if built | helper or delta chain | own | own | own | none |

## Assessment

- The rsync executable is the only option that refreshes a package on a
  stock NAS: the NAS runs rsync 3.1.x as the receiver, and nothing else
  known there can apply a delta. No Rust crate implements the rsync
  sender protocol.
- The version probe accepts only `rsync  version` 3.1 or later and so
  refuses openrsync, whose stats labels, flags and timeout behaviour
  differ.
- Fixed command line: `LC_ALL=C`, `--stats`, `--no-human-readable`,
  `--no-whole-file`, `--timeout`, `-e` with antiq's ssh command. Never
  `--inplace` or `--append`.
- Exit code 24 tolerated, 30 is a timeout; missing stats count as
  unknown.
- `-z` only for uncompressed packages; no `--compress-choice` while the
  NAS runs 3.1.x.
- `fast_rsync` would be the library for a NAS helper of antiq's own, a
  route not needed for v1.

## Not verified

- Anything against the real NAS or over real SSH: the 3.1.x stats
  labels, `--rsync-path` on DSM, ControlMaster through `-e`.
- DSM's rsync versions beyond the forum reports.
- `-z` negotiation between 3.5.1 and 3.1.x.
- Library memory use on GB files.
- Disk space needed for the receiver's temp file.
