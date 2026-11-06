# Searching the listings directly (`antiq grep`)

Research on how fast the paths of all archived files can be searched in
the listing files themselves, with no index. It records measurements, not
decisions. Sections are added as the measurements finish.

## Machine, build and tools

All numbers come from one MacBook Pro Mac16,8 (Apple M4 Pro, 14 cores,
24 GB), from a release build (`cargo build --release`, default release
profile, cargo 1.97.0, rustc 1.97.0) of a throwaway crate that is not
committed. Other work ran on the machine (load average 3.7 to 4.7 during
the series), which only adds time. Crates, the newest on crates.io when
measured: `grep-searcher` 0.1.17, `grep-regex` 0.1.14, `grep-matcher`
0.1.9, `regex` 1.13.1, `memchr` 2.8.3, `zstd` 0.14.3 (zstd 1.5.7).

Peak memory is `ru_maxrss` of the measuring process, read at its end.
Every table row is one process that ran 5 searches over all files after one
unmeasured search; the figure is the best of the 5 unless stated. One
search covers the 30 listings.

## Listing format searched

The listings follow the planned format: one header line, then per entry a
readable aligned comment line followed, for files, by the checksum line.
Directories have a comment line only.

    # antiq listing 1 · projects/piece-000 · archived 2026-11-04 09:02
    # dir   755  2026-07-02 01:25               ParserTmp/
    # file  644  2024-01-04 19:03       233988  ParserTmp/0a098d65.bin
    559a262a5632c23cc92022fc9faf2c473323b373b68e1fefee3a305224a44998  ParserTmp/0a098d65.bin

The path starts at byte 44 of a comment line and at byte 66 of a hash line
(64 hex digits and two spaces). A listing is stored zstd-compressed (level
3, content checksum, as `.sha256.zst`); both forms are measured.

## Data

The generator is a new throwaway program that follows the description in
`picker-search.md`.

- 30 listings of 50 000 entries each. Every listing has its own random
  directory tree (1 234 directory entries in piece 0, 48 766 file entries),
  so nothing repeats between listings. 300 pieces are extrapolated from
  these 30 (factor 10, linear), never measured.
- Names: snake_case, kebab-case and CamelCase words from 73 directory and
  74 stem words, `IMG_nnnn.jpg`, dated report names, 8-digit hex names,
  numbered names, 26 extensions (`rs` three times as likely, `md` and `json`
  twice as likely). Directories are 1 to 4 levels deep.
- The average file path of piece 0 is 46.8 bytes (the target was about
  49).
- The digest on a hash line is 32 random bytes in hex, not a real SHA-256.
  Length and entropy are those of a digest.
- Sizes: 299 276 958 bytes plain (9.98 MB per listing) and 92 172 932
  bytes as zstd (3.07 MB per listing, ratio 3.25) for the set `A` used in
  the warm series. A second set `B` (299 641 063 and 92 178 017 bytes) was
  made for the cold parallel run.

## Cold and warm page cache

`sudo purge` asks for a password here, so the cache could not be emptied.
Instead the generator writes every file with `fcntl(F_NOCACHE)` and
`fsync`, and the first search reads files that no process has read since.
That the pages are then really read from the SSD was checked on 12 other
listings (118 710 084 bytes plain): `cat` of them took 0.04 s the first
time and 0.01 s the second and third time.

Searched with `grep-reader` (see the engines below), hash lines,
literal `scanner`, anchored pattern, line numbers on, one process per row,
runs 1 to 3 in order without warm-up. Run 1 is cold, runs 2 and 3 warm.
Set `A` for one thread, set `B` for 14 threads (each set is cold once).
Milliseconds.

| Files | Threads | Run 1 (cold) | Run 2 | Run 3 |
| --- | ---: | ---: | ---: | ---: |
| zstd | 1 | 580.2 | 543.4 | 541.3 |
| plain | 1 | 418.8 | 381.0 | 382.6 |
| zstd | 14 | 105.1 | 82.0 | 89.6 |
| plain | 14 | 98.8 | 87.5 | 83.2 |

The cold run was 7 to 28 % slower than the best warm run.

## Which lines to search, and how

Every entry has two lines that hold its path. Both lines also hold other
text (digits and `a-f` on the hash line; kind, mode, date and size on the
comment line), and the header line starts with `# `. A pattern for the
path alone therefore needs one of two forms:

- Anchored: the pattern starts with the line prefix, so only entry lines
  can match. Hash line: `^[0-9a-f]{64}  .*?PATH`. Comment line:
  `^# (?:file|dir ).{38}.*?PATH`, 44 bytes before the path.
- Prefiltered: the searcher runs `PATH` alone over the whole file, and
  every candidate line is checked afterwards. The line must have the right
  kind (no `#` first for a hash line; `# file` or `# dir ` first for a
  comment line) and `PATH` must match in the part of the line after byte 66
  (hash) or 44 (comment). A candidate on the other line of the same entry
  or in the header is dropped.

Queries (`PATH`): `lit` is `scanner`, `ci` is `Scanner` case-insensitive,
`re` is `src/.*\.rs$`, `miss` is `qxzv` (no entry holds it). The same
patterns give the same hit counts in both forms (lines on hash lines: 106 446
for `lit`, 116 391 for `ci`, 4 980 for `re`, 0 for `miss`; on comment
lines 108 702, 118 803, 4 980 and 0, as directory entries match there too).

### Plain files, one thread, `grep-searcher` on a slice

`SearcherBuilder` with line numbers, `Searcher::search_slice` over the
whole file read into memory (`read_to_end`, included in the time), a
`RegexMatcher` of `grep-regex` with `multi_line(true)`. Set `A`, page
cache warm, best of 5, milliseconds (and MB/s of the 299.3 MB):

| Form | Lines | `lit` | `ci` | `re` | `miss` |
| --- | --- | ---: | ---: | ---: | ---: |
| anchored | hash | 377.4 (793) | 389.0 (769) | 381.4 (785) | 376.4 (795) |
| anchored | comment | 210.6 (1421) | 211.5 (1415) | 219.0 (1367) | 206.5 (1449) |
| prefiltered | hash | 37.4 (8001) | 64.0 (4675) | 90.5 (3305) | 21.7 (13812) |
| prefiltered | comment | 38.0 (7872) | 64.4 (4644) | 91.7 (3265) | 20.9 (14348) |

In the anchored form every query takes the same time, and the hash line
takes 1.8 times as long as the comment line. In the prefiltered form hash
and comment lines take the same time.

## Engines, formats and threads

Hash lines, prefiltered form, set `A`, page cache warm. Each cell is
milliseconds for the 30 listings with, in brackets, MB/s of the 299.3 MB of
plain text (also for the zstd files, so the figure includes decoding).

Engines:

- `grep-reader`: `grep-searcher` `Searcher::search_reader` with a
  `grep-regex` matcher over a `File`, or over a `zstd::Decoder` for zstd
  files (streaming, no whole-file buffer). Line numbers on.
- `grep-slice`: the same matcher through `search_slice` over the file read
  or decoded into a buffer of the thread.
- `regex`: `regex::bytes::Regex::find_at` over that buffer, then the line
  check (a match inside one line is counted once).
- `memmem`: `memchr::memmem::Finder` over that buffer, `memrchr` and
  `memchr` for the line bounds, then the line check. Literal queries only.

Threads: `std::thread::scope`, each thread takes the next file from an
atomic counter, with its own buffer. 14 threads = the 14 cores.

| Engine | Query | plain, 1 thread | plain, 14 threads | zstd, 1 thread | zstd, 14 threads |
| --- | --- | ---: | ---: | ---: | ---: |
| `grep-reader` | `lit` | 37.5 (7980) | 7.1 (42110) | 196.6 (1522) | 22.1 (13556) |
| `grep-reader` | `ci` | 68.1 (4392) | 10.2 (29454) | 223.5 (1339) | 25.1 (11920) |
| `grep-reader` | `re` | 92.0 (3251) | 36.8 (8124) | 249.2 (1201) | 40.5 (7381) |
| `grep-reader` | `miss` | 28.0 (10684) | 3.9 (76875) | 185.1 (1617) | 19.7 (15173) |
| `grep-slice` | `lit` | 37.9 (7899) | 7.4 (40179) | 196.3 (1524) | 22.8 (13107) |
| `grep-slice` | `ci` | 64.7 (4623) | 11.4 (26303) | 222.9 (1342) | 24.7 (12102) |
| `grep-slice` | `re` | 90.9 (3292) | 31.1 (9633) | 254.1 (1178) | 38.4 (7799) |
| `grep-slice` | `miss` | 22.0 (13599) | 6.0 (50227) | 178.8 (1674) | 19.4 (15395) |
| `regex` | `lit` | 29.1 (10277) | 7.4 (40492) | 186.7 (1603) | 21.5 (13900) |
| `regex` | `ci` | 57.3 (5223) | 9.8 (30536) | 213.3 (1403) | 25.5 (11759) |
| `regex` | `re` | 30.9 (9687) | 7.3 (40783) | 189.9 (1576) | 20.9 (14296) |
| `regex` | `miss` | 21.2 (14099) | 6.1 (49060) | 179.9 (1664) | 19.4 (15448) |
| `memmem` | `lit` | 27.3 (10945) | 6.5 (46147) | 185.7 (1612) | 21.5 (13928) |
| `memmem` | `miss` | 21.2 (14138) | 6.1 (49087) | 179.4 (1668) | 19.3 (15541) |

Hit counts were identical in all engines for the same query (106 446 for
`lit`, 116 391 for `ci`, 4 980 for `re`, 0 for `miss`).

Line numbers in `grep-slice`, plain, 1 thread, hash lines: `lit` 29.0 ms
without and 38.5 ms with line numbers, `re` 86.2 ms without and 91.6 ms with
(one process per figure, best of 5).

Where the pattern with the line prefix is used instead (anchored form,
`grep-reader`, zstd, 14 threads): hash lines 92.6 ms for `lit` and 74.6 ms
for `re`, comment lines 62.9 ms and 49.7 ms.

### Decompression share

The `decode` run reads (plain) or decodes (zstd, `zstd::Decoder` into the
thread's buffer) every file and does nothing else. Best of 5, milliseconds
for the 30 listings:

| | 1 thread | 14 threads |
| --- | ---: | ---: |
| read plain files into memory | 16.1 | 4.9 |
| decode zstd files into memory | 173.4 | 18.4 |

Decoding is 93 % of the `regex` `lit` search on zstd files with one thread
(173.4 of 186.7 ms), 88 % of `grep-reader` `lit` (173.4 of 196.6 ms), and 86
% and 83 % of those with 14 threads (18.4 of 21.5 and 22.1 ms). Decoding
runs at 1.73 GB/s of plain text per thread. With 14 threads the 30 files
give each thread 2 or 3 files. Three decodes take 3 times 173.4 / 30 =
17.3 ms.

### Peak memory

`ru_maxrss` of the process, by engine and format. With one thread
`grep-reader` needs 4 MB (plain) and 7 MB (zstd). The engines that hold a
file in memory (`grep-slice`, `regex`, `memmem`) need 13 to 15 MB (plain)
and 16 to 17 MB (zstd) with one thread, and 146 to 148 MB (plain) and 186
to 188 MB (zstd) with 14 threads, each thread holding a buffer for a
listing of 10 MB. `grep-reader` with 14 threads needs 6 MB (plain) and 46
MB (zstd).

### Extrapolation to 300 pieces

The times above multiplied by 10 (300 listings of the same size, no
measurement at that size). With 14 threads the 30 listings leave
threads with 3 files while the mean is 2.1; with 300 listings it is 22
against 21.4, so those figures are upper bounds. In a cache the 300
listings take 2.99 GB plain and 0.92 GB as zstd.

| Engine, query | plain, 1 thread | plain, 14 threads | zstd, 1 thread | zstd, 14 threads |
| --- | ---: | ---: | ---: | ---: |
| `memmem` `lit` | 273 ms | 65 ms | 1.86 s | 215 ms |
| `regex` `ci` | 573 ms | 98 ms | 2.13 s | 255 ms |
| `regex` `re` | 309 ms | 73 ms | 1.90 s | 209 ms |
| `grep-reader` `lit` | 375 ms | 71 ms | 1.97 s | 221 ms |
| `grep-reader` `ci` | 681 ms | 102 ms | 2.24 s | 251 ms |
| `grep-reader` `re` | 920 ms | 368 ms | 2.49 s | 405 ms |

## Cold page cache with the prefiltered search

A third set `D` (298 471 180 bytes plain, 92 150 306 bytes zstd, written
with `F_NOCACHE` like the others) searched with `regex`, hash lines,
literal `scanner` (91 226 hits), runs 1 to 3 in order without warm-up,
milliseconds. Run 1 is cold. Two combinations only, so that each file set
is read cold once.

| Files | Threads | Run 1 (cold) | Run 2 | Run 3 |
| --- | ---: | ---: | ---: | ---: |
| zstd | 14 | 56.9 | 28.2 | 22.2 |
| plain | 1 | 65.2 | 29.0 | 29.1 |

The cold run took 34.7 ms (zstd, 14 threads) and 36.2 ms (plain, one
thread) more than the best warm run. Times 10 (not measured at that size)
the cold runs are 569 ms (zstd, 14 threads) and 652 ms (plain, 1 thread).

## Summary of the figures

All from the tables above, 30 listings of 50 000 entries (299.3 MB plain,
92.2 MB zstd), times 10 for 300 listings.

- A pattern with the line prefix takes 377 ms (hash lines) or 211 ms
  (comment lines) on plain files with one thread, whatever the query. The
  path-only pattern with a check of the candidate lines takes 21 to 92
  ms, and is the same for hash and comment lines.
- `memchr::memmem` with the line check takes 27.3 ms for a literal
  (10.9 GB/s) and the `regex` crate 29.1 ms (literal), 57.3 ms (case-insensitive
  literal) and 30.9 ms (`src/.*\.rs$`), plain, one thread. `grep-searcher`
  with `grep-regex` takes 37.5, 68.1 and 92.0 ms.
- zstd files take 186 to 254 ms with one thread, of which decoding is 173.4
  ms (1.73 GB/s of plain text).
- `regex` and `memmem` with 14 threads take 6.1 to 9.8 ms (plain) and 19.3
  to 25.5 ms (zstd) for the 30 listings. `grep-searcher` with the `re`
  query takes 31.1 to 36.8 ms (plain) and 38.4 to 40.5 ms (zstd).
- Times 10 for 300 listings, `regex`: with 14 threads 61 to 98 ms plain and
  194 to 255 ms zstd; with one thread 0.21 to 0.57 s plain and 1.8 to 2.1 s
  zstd.
- Cold cache adds 35 to 36 ms for 30 listings in the two combinations
  measured.
