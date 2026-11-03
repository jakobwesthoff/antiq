# xz and zstd from Rust

Researched 2026-11-03 on macOS arm64 (14 cores, 24 GiB RAM) with Rust
1.97.0, the `xz` 5.8.4 and `zstd` 1.5.7 command-line tools, `liblzma`
0.4.9 (features `parallel`, `static`, bundling xz 5.8.4) and `zstd`
0.14.3 (features `experimental`, `zstdmt`, bundling zstd 1.5.7). Another
benchmark ran at the same time, so no timings are given. Crate figures
come from the crates.io API on that day.

Marks: **measured** (run on this machine), **read** (documentation or
source, named), **reported** (third party, linked).

## Requirements

- R1 Archives: xz preset 9 with a SHA-256 check in the stream.
- R2 Multithreaded xz; thread count from a memory limit (25 % of RAM by
  default) for compression and decompression.
- R3 Streaming for archives of several GB; decoding files from `xz` 5.x.
- R4 Live packages: zstd level 3, rsyncable, job size 512 KiB, at least
  one worker, no long-distance matching, byte-identical output across
  runs.
- R5 Integrity check by a full decode.
- R6 Licence compatible with an MPL-2.0 binary; maintenance; exposure to
  CVE-2024-3094.

## `liblzma` (xz)

- Fork of `xz2`/`lzma-sys` by the Portable-Network-Archive
  organisation; `liblzma` 0.1.x is API compatible with `xz2` 0.1.7
  (**read**, <https://github.com/portable-network-archive/liblzma-rs>).
  `liblzma-sys` vendors the C library and compiles only the liblzma
  sources with `cc` (**read**, `liblzma-sys-0.4.9/build.rs`). Feature
  `static` forces the vendored build; `parallel` enables threads.
- 0.4.9 released 2026-10-19; first release 2023-09-24; 26.4M downloads,
  8.3M recent; 125 reverse dependencies; releases every one to three
  months. MIT OR Apache-2.0; liblzma itself is 0BSD. No RustSec entries
  for `liblzma-sys` or `lzma-sys`.
- Bundled xz per line: 0.2.x xz 5.4, 0.3.x 5.6, 0.4.x 5.8 (**read**,
  README; **measured**, `xz/NEWS` in the crates). `liblzma-sys` 0.3.0
  bundled 5.6.0 and 0.3.1 to 0.3.3 bundled 5.6.1, the two releases with
  the CVE-2024-3094 backdoor; 0.3.4 and later do not. The vendored tree
  has no `m4/build-to-host.m4` and the build does not run autotools, so
  the backdoor's build path is absent; the C sources of those releases
  were not analysed (**measured**). 0.4.x is unaffected.
- `xz2` 0.1.7 was last released 2022-06-06. It cannot share a dependency
  graph with `liblzma`, since both link `lzma` (**measured**).

Results (**measured**):

- `MtStreamBuilder` with preset 9, `Check::Sha256`, 6 threads and a
  memory limit, wrapped in `XzEncoder`, produced output byte-identical
  to `xz -9 -T6 -C sha256` for a 150 MB tar; the same bytes with 2
  threads and with a 1.5 GB limit. `xz -t` and `xz -lv` accept it.
- `memusage()` reported 7,494 MiB for 6 threads and 2,498 MiB for 2,
  so 1,249 MiB per thread at preset 9. The liblzma header gives each
  thread about three times the block size, and the default block is
  three times the dictionary, 192 MiB at preset 9 (**read**,
  `container.h`). 674 MiB is the single-thread figure. With a 6 GiB limit
  (25 % of 24 GiB) that allows 4 threads.
- Threads split the input into blocks, so input below one block (192
  MiB) runs on one thread: a 143 MiB input did, in the crate and in the
  CLI, with the same bytes.
- `memlimit_threading` lowers the thread count, `memlimit_stop` aborts.
  The effective thread count under a lower limit was not observed.
- A streaming decoder with a memory limit decoded the 150 MB file; with
  30 MB it returned `MemLimit` (the file needs a 64 MiB dictionary).
  The multithreaded decoder is exposed (`MtStreamBuilder::decoder`) but
  was not run.

## `zstd`

- Wraps libzstd, vendored by `zstd-sys` and built with `cc`. 0.14.3
  uses `zstd-sys` 2.1.2+zstd.1.5.7. BSD-3-Clause crates; zstd is used
  under its BSD licence. 458M downloads, 111M recent; 2,725 reverse
  dependencies; first release 2016-02-22.
- RustSec: RUSTSEC-2026-0391 (data from an abandoned stream leaks into
  the next when a context is reused through `with_context`; patched in
  `^0.13.4` and `>= 0.14.3`) (**read**, advisory-db);
  RUSTSEC-2026-0392 (legacy-frame decoder, fixed in 0.8.0, code behind
  the `legacy` feature); RUSTSEC-2026-0393 (dictionary training,
  informational); `zstd-sys` RUSTSEC-2026-0410 (wasm32 only).
- `experimental` exposes `CParameter::RSyncable`; `zstdmt` compiles in
  multithreading; both off by default. Default features are `legacy`,
  `arrays`, `zdict_builder` (**read**, `zstd-safe` source).

Byte identity with the CLI (**measured**), 150,000,000-byte tar,
encoder at level 3 with `NbWorkers`, `JobSize(524288)`,
`RSyncable(true)`, `EnableLongDistanceMatching(false)`,
`ChecksumFlag(true)`:

| Producer | md5 | Bytes |
|---|---|---|
| CLI `zstd -q -3 --rsyncable -B524288 -c`, file input | `77d03adf` | 20,008,786 |
| CLI, same flags, `-T1`, stdin | `ed29df0e` | 20,008,782 |
| crate, 1 worker, two runs | `ed29df0e` | 20,008,782 |
| crate, 4 workers | `ed29df0e` | 20,008,782 |
| crate, 1 worker, `set_pledged_src_size` | `77d03adf` | 20,008,786 |
| CLI, `-T4`, file input | `77d03adf` | |

- From a file the CLI writes the content size into the frame header (4
  bytes); `set_pledged_src_size` reproduces it.
- The worker count does not change the bytes. `zstd -t` accepts the
  output; decoding returns the input.
- `NbWorkers(0)` with `RSyncable(true)` raised no error and produced
  different bytes (19,575,788): rsyncable is ignored without workers.
- Long-distance matching changes the output (17,669,048 bytes).

Output across zstd versions, same input and parameters (**measured**):

| Crate pair | Bundled zstd | md5 |
|---|---|---|
| `zstd` 0.13.0 / `zstd-sys` 2.0.9 | 1.5.5 | `2f784926` |
| `zstd` 0.13.2 / `zstd-sys` 2.0.13 | 1.5.6 | `2f784926` |
| `zstd` 0.14.3 / `zstd-sys` 2.1.2 | 1.5.7 | `ed29df0e` |

The zstd 1.5.7 release notes list better block boundaries at all levels
and a better ratio at levels 3 and 4
(<https://github.com/facebook/zstd/releases/tag/v1.5.7>); `zstd.h`
promises no identical output across versions (**read**). A bundled zstd
change therefore changes the bytes of every package; the rsync cost of
that change was not measured.

## Archive settings through the crate

Checked after the benchmark (`docs/research/compression-bench.md`) chose
zstd for archives. On the same 150,000,000-byte tar, `zstd` 0.14.3 with
level 19, `NbWorkers(14)`, `EnableLongDistanceMatching(true)`,
`WindowLog(27)` and `ChecksumFlag(true)`, fed as a stream of unknown
size, produced 12,714,549 bytes, byte-identical to
`zstd -19 --long=27 -T14 -c < in.tar` (same SHA-256). The crate's
`Decoder` with default settings decoded it to the original bytes;
`zstd -t` accepted it and `zstd -lv` showed a 128 MiB window and an
XXH64 check. The run took 17.7 s wall and 34.6 s CPU (**measured**).

## Pure Rust alternatives

- `lzma-rs` 0.3.0 (2023-01-04, 44.7M downloads): its encoder only emits
  literals and it decodes "a subset of the `.xz` file format"
  (**read**, README).
- `xz4rust` 0.2.3 (2026-08-06, 68.7k downloads, one maintainer): decoder
  only.
- `ruzstd` 0.9.1 (2026-10-11): encodes at roughly level 1 only, no
  rsyncable, no threads (**read**, README); RustSec lists
  RUSTSEC-2026-0344 and RUSTSEC-2024-0400.

## Executables

`xz -9 -T<n> -C sha256` and `zstd -q -3 --rsyncable -B524288 -T1` produce
the same bytes as the crates at the same library versions (**measured**).
Calling them means finding a compatible binary on each host and piping
through child processes; the output then depends on the installed zstd,
which antiq does not control.

## Comparison

| Option | R1 | R2 | R3 | R4 | R6 | Maintenance |
|---|---|---|---|---|---|---|
| `liblzma` 0.4.9 | identical to CLI | threads, memory limits | streaming, decodes xz 5.x | n/a | MIT/Apache, xz 5.8.4 | released 2026-10-19 |
| `xz2` 0.1.7 | older API | no `parallel` | yes | n/a | old xz | last release 2022 |
| `lzma-rs` 0.3.0 | no | no | partial | n/a | MIT | last release 2023 |
| `xz4rust` 0.2.3 | decoder only | no | decode | n/a | MIT | one maintainer |
| `zstd` 0.14.3 | n/a | n/a | n/a | identical to CLI | BSD-3, advisories fixed | 2,725 reverse deps |
| `ruzstd` 0.9.1 | n/a | n/a | n/a | no | MIT, 2 advisories | active |
| executables | yes | flags | yes | version-dependent | not linked | system-managed |

## Assessment

- Archives: `liblzma` with `parallel` and `static`. It matches the CLI
  byte for byte and covers SHA-256, memory limits and streaming. The
  thread count comes from `memusage()` or 1,249 MiB per thread at
  preset 9; threads help only above one 192 MiB block.
- Live packages: the `zstd` crate with `experimental` and `zstdmt`
  reproduces `zstd --rsyncable -B524288`; no executable is needed.
  `NbWorkers` at least 1; `zstd >= 0.14.3`; `legacy` feature off.
- Package bytes depend on the bundled zstd version. Pinning `zstd-sys`
  exactly, a test hashing a fixed input, and recording the zstd version
  and parameters with each package make a version change visible; a
  change means one full re-upload.

## Measured later

Measured 2026-11-03 (**measured**): a level-19 encoder of the `zstd`
crate reached about 150 MB of resident memory even for an input of
10 KB.

Measured 2026-11-04 (**measured**, release build, zstd 1.5.7 through the
`zstd` crate, level 19 with long-distance matching, window log 27,
default 64 MiB jobs, 400 MiB of semi-compressible input, peak resident
memory from `/usr/bin/time -l`): 552 MiB with one worker, 676 MiB with
two, 1002 MiB with four, about 430 MiB per encoder plus 140 MiB per
worker. The fixed part is the round buffer zstd allocates once per
encoder with long-distance matching, `MAX(windowSize, n*jobSize) +
3*jobSize`, 320 MiB for one or two workers (**read**,
`lib/compress/zstdmt_compress.c` of zstd 1.5.7). One macOS machine and
one synthetic input (75 % from a 10-letter alphabet, 25 % random bytes)
were measured; small inputs peak lower (150 MB to 300 MB above), so the
figures are a ceiling for large pieces, not a typical value.

`zstd-sys` 2.1.2 has the default features `legacy` and
`zdict_builder`; `zstd-safe` 8.2.0 depends on it with
`default-features = false` (**read**, their `Cargo.toml`). A direct
dependency on `zstd-sys` with its default features switches both back
on through feature unification.

## Not verified

- Byte identity on another machine or on Linux.
- The rsync cost of a zstd version change.
- The effective xz thread count below `memusage()`; the multithreaded xz
  decoder; smaller xz block sizes.
- Static builds on Linux.
- Patched versions of the `ruzstd` advisories.
