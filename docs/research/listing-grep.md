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
