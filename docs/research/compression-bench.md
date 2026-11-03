# Compression benchmark: xz and zstd on real archives

Measured 2026-11-03 on an Apple M4 Pro (14 cores, 24 GiB RAM) with xz
5.8.4 and zstd 1.5.7, all settings with `-T0`. Ten archives made by
`archive.sh` were unpacked to tar and compressed again with each
setting; every output decompressed to bytes equal to the tar (`cmp`).
Times for archives under 200 MB are the median of three runs; the four
larger ones ran once.

## Sample

| Archive | Tar size | Files | xz -9 -T0 ratio | Content |
|---|---:|---:|---:|---|
| `2024_10_28_soap-client` | 1.9 MB | 25 | 19.1 % | source, JSON and LaTeX text |
| `2025_10_31_mascot-variants` | 11 MB | 15 | 99.0 % | PNG images |
| `2026_09_10_drive-temperatures` | 33 MB | 41 | 8.1 % | CSV and JSON (tabular) |
| `2025_04_28_template-generator` | 66 MB | 3,434 | 14.5 % | many small files (`node_modules`: js, ts, xsl, json) |
| `2024_08_29_pod-logs` | 86 MB | 4 | 3.1 % | three Kubernetes pod logs |
| `2024_04_24_vpn-debugging` | 182 MB | 4 | 52.8 % | one `.tar.gz` file and two scripts |
| `2025_07_16_sqlite-extraction` | 346 MB | 253 | 21.5 % | SQLite databases and gzip-compressed CSV |
| `2026_10_15_mascot-renders` | 635 MB | 7,001 | 74.3 % | PNG, `.npy`, SVG, TypeScript, Markdown |
| `2024_12_09_db-snapshots` | 865 MB | 11 | 27.2 % | gzip-named database snapshots |
| `2026_09_28_restic-recovery` | 1,223 MB | 1,556 | 99.5 % | encrypted restic repository |

## Totals

Over all ten archives (3.45 GB of tar). Peak memory is the largest of
the ten runs.

| Setting | Total size | vs xz -9 | Compress | Decompress | Peak RAM compress | Peak RAM decompress |
|---|---:|---:|---:|---:|---:|---:|
| `xz -9` | 2,120 MB | 100.0 % | 366 s | 10.0 s | 4,435 MB | 2,846 MB |
| `zstd -3` | 2,765 MB | 130.4 % | 0.7 s | 0.8 s | 303 MB | 8 MB |
| `zstd -19` | 2,318 MB | 109.3 % | 90 s | 1.3 s | 2,299 MB | 14 MB |
| `zstd -19 --long=27` | 2,089 MB | 98.6 % | 156 s | 1.1 s | 3,405 MB | 140 MB |
| `... --long=27 -B32MiB` | 2,094 MB | 98.8 % | 142 s | 1.1 s | 2,333 MB | 140 MB |
| `... --long=27 -B64MiB` | 2,089 MB | 98.6 % | 159 s | 1.1 s | 3,350 MB | 140 MB |
| `... --long=27 -B128MiB` | 2,084 MB | 98.3 % | 200 s | 1.1 s | 3,360 MB | 140 MB |

## By content

- Large repeats: plain `zstd -19` is 61 % larger than xz on the
  database snapshots and 51 % larger on the SQLite files. With
  `--long=27` the snapshots end 1.7 % above xz and the SQLite files
  31 % below it.
- Text and tables: xz is 15.9 % smaller than the best zstd setting on
  the CSV and JSON archive and 2.0 % smaller on the many small files.
  The logs are within 0.4 %.
- Images and mixed content: `--long=27` is 3.0 % smaller than xz.
- Incompressible data: the restic repository and the PNG archive differ
  by about 0.1 % between all settings except `zstd -3`. On the restic
  repository, xz took 144.5 s, `zstd -19` 18.7 s and `--long=27`
  40.6 s.
- Decompression with zstd is 5 to 15 times faster and needs at most
  140 MB against up to 2.8 GB for multithreaded xz.

## Job size

`-B64MiB` produced output byte-identical to the default `--long=27` on
all ten archives, so 64 MiB is the default job size at this level and
window. 32 MiB jobs were 0.2 % larger and used more cores (9.9 against
7.2 on the 1.2 GB archive); 128 MiB jobs were 0.25 % smaller and 10 to
50 % slower.

## Old archives made with preset 6

The SQLite archive made by `archive.sh` is 114 MB, while `xz -9` of the
same tar gives 74 MB. `xz -lvv` shows it was written with xz's default
preset 6 (8 MiB dictionary, 24 MiB blocks, CRC64), not with the
`-T0 -9 --check=sha256` of the current script; `xz -T0 -6` of the tar
reproduces it byte for byte (**measured**). Of the 313 archives, 222
have a 64 MiB dictionary with SHA-256 and 91 an 8 MiB dictionary with
CRC64 (**measured**, `xz -lvv`).

## Decompression flags

A frame with a window of up to 128 MiB (`--long=27`) decodes with
plain `zstd -d`; `--long=28` needs `--long=28` or `--memory` when
decompressing ("Window size larger than maximum"). The zstd man page
states the same for window logs above 27.

## Not measured

- Single-threaded runs; machines other than this Mac.
- The `zstd` and `liblzma` crates (`docs/research/compression.md` shows
  they produce the same bytes as the command-line tools).
