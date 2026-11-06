# Picker contents search: where the time goes

Research on why the contents search of the `find` picker is slow with
300 pieces of 50 000 members each (15 million paths), and what a faster
search could look like. It records measurements, not decisions.

Sections marked "not yet measured" are filled in as the measurements
finish.

## Machine and build

All numbers come from one MacBook Pro Mac16,8 (Apple M4 Pro, 10
performance and 4 efficiency cores, 24 GB), on a release build
(`cargo build --release`, profile `release` of `Cargo.toml`, no changes
to it). Other work ran on the machine during the measurements (load
average 13 to 14 at the start of the series, another `rustc` using a
core), so every figure is the best of N runs, with N stated per table.
Other work only adds time.

## What is searched today

The search in `src/cli/picker/state.rs` (`recompute`) goes through the
pieces whose row text does not match the query. For each it takes the
members from `Contents::members` unless the piece is retained (up to
`MEMBER_BUDGET` = 2 000 000 members in all). Every member is scored as
`"<piece path>/<member>"` with `Pattern::score` (nucleo, `Config::DEFAULT`,
smart case, smart normalization) on one thread. A query that extends the
previous one scores only the earlier hits, for retained pieces.

`FindContents::members` in `src/cli/cmd/find.rs` reads the members with
`Index::members` (cache file `.antiq/index/<id>.json`, else the zstd
listing) and turns every path into a new `String` with
`RelPath::to_string` and `sanitize`.

## Data

The test data is made by a throwaway program that is not committed.

- 60 distinct pieces of 50 000 members each. The 300 pieces reuse them: piece
  `i` has the members of piece `i % 60` and its own prefix
  (`projects/<year>/<dir word>-<stem word>-<i>`).
- Members are sorted paths of 2 to 7 directory levels (about 1 in 40
  members is a directory), file names built from 75 stem words (directories from 73 words) in snake_case,
  kebab-case and CamelCase, `IMG_nnnn`, dated report names, hex chunk
  names, and 24 extensions (three of them drawn twice as often).
- Average member path 49.0 bytes. 1.61 % of the members hold non-ASCII
  characters.
- The listings and the member caches of the 60 pieces are written with
  `antiq::listing::write` and the cache layout of `src/index.rs`
  (`{"format":1,"listing":...,"members":[...]}`). They take 168 MB
  (listings) and 150 MB (caches) on disk.

## Method of the scoring tables

The scoring program holds the members in memory and runs the keys of one
typing sequence one after the other. A key that extends the previous key
scores only the previous hits, as the picker does, but for every piece
(the picker keeps hits only for retained pieces). Each cell is the best
of 3 runs of that key, in milliseconds, over all 300 pieces
(15 000 000 members). Scoring is `Pattern::parse(query, Smart, Smart)`
and `Pattern::score` with a `Matcher` of `Config::DEFAULT`, on the joined
string `"<prefix>/<member>"` unless the variant says otherwise.

Variants:

- A: `Vec<String>` per piece, the picker's joined string built by
  `clear`, `push_str(prefix)`, `push_str(member)`.
- B: all members of a piece in one `String` with `u32` offsets, same
  join as A.
- C: as B, but the prefix is written once and the buffer is truncated back
  to it for every member.
- D: as B, scoring the member alone. This changes which queries match, as
  a query can no longer use characters of the piece path.
- E: as B, plus a 64-bit mask per member. A member is scored only if its
  mask holds the bits of every query character that the prefix lacks.
  The mask has one bit for each of `a-z`, `0-9`, `.`, `_`, `-`, space and
  `/`, and 22 shared bits for other ASCII bytes. A non-ASCII character
  counts as what nucleo's lower-casing and normalization turn it into,
  or as nothing if that is not ASCII. Memory: 8 bytes per member.
- F: as E, plus a mask per piece (the union of its member masks): a piece
  that lacks a needed bit is skipped without a look at its members.
- G: as B, stopping at the first hit of a piece. This changes the
  ranking, as the piece no longer has its best member score.
- H: C and E together.
- I: as B, with `Matcher::fuzzy_match_greedy` instead of `Pattern::score`.
  It changes scores.

## Results: single thread

Typing sequence `c`, `ct`, `ctrl`, then the backspace key back to `ctr`
(not a continuation of `ctrl`, so a full scan), then `qxz` (no member
holds it). Best of 3, milliseconds.

| Variant | `c` | `ct` | `ctrl` | `ctr` | `qxz` |
| --- | ---: | ---: | ---: | ---: | ---: |
| A `Vec<String>`, join each time | 1120.6 | 5131.4 | 5586.8 | 5646.7 | 668.0 |
| B compact buffer | 1125.1 | 5081.9 | 5305.3 | 5844.7 | 681.0 |
| C B with truncate join | 1082.5 | 5109.5 | 5188.9 | 5670.0 | 629.7 |
| D member only (other semantics) | 745.8 | 2269.7 | 1593.1 | 2242.9 | 453.5 |
| E member mask | 1107.3 | 5090.5 | 5039.4 | 5646.5 | 10.9 |
| F member and piece mask | 1109.5 | 5189.0 | 5422.0 | 6025.3 | 0.0 |
| G first hit only (other ranking) | 0.1 | 0.1 | 0.1 | 0.1 | 656.1 |
| H C and E | 1052.7 | 4982.2 | 5027.3 | 5751.1 | 11.3 |
| I greedy match (other scores) | 551.5 | 609.8 | 1779.9 | 1305.1 | 635.4 |

Pieces with a hit, and members that hit, per key (variant A): `c`
300 and 15 000 000, `ct` 300 and 15 000 000, `ctrl` 300 and 12 358 370,
`ctr` 300 and 14 698 032, `qxz` 0 and 0. The prefix of each piece holds
`c` and `t` in order, which is why so many members hit.

A second sequence with rarer letters, `w`, `wo`, `wof`, `wof2`, then
`qxz`. Best of 3, milliseconds.

| Variant | `w` | `wo` | `wof` | `wof2` | `qxz` |
| --- | ---: | ---: | ---: | ---: | ---: |
| A `Vec<String>`, join each time | 730.6 | 1096.5 | 563.4 | 247.7 | 699.2 |
| B compact buffer | 726.3 | 1107.6 | 543.8 | 214.9 | 669.7 |
| C B with truncate join | 664.1 | 1050.9 | 519.9 | 210.7 | 628.0 |
| D member only (other semantics) | 458.2 | 698.6 | 343.2 | 145.8 | 434.3 |
| E member mask | 322.2 | 1052.9 | 419.6 | 212.0 | 8.2 |
| F member and piece mask | 314.8 | 1028.6 | 417.8 | 210.6 | 0.0 |
| G first hit only (other ranking) | 0.2 | 0.0 | 0.0 | 0.0 | 664.2 |
| H C and E | 300.9 | 1027.0 | 419.3 | 210.5 | 10.2 |
| I greedy match (other scores) | 618.5 | 426.5 | 356.2 | 143.8 | 615.7 |

Hits per key (variant A): `w` 5 538 404, `wo` 4 638 620, `wof`
1 620 110, `wof2` 679 874, `qxz` 0, each in all 300 pieces except `qxz`.

Parts of the join, over all 15 000 000 members, best of 3:

- building `"<prefix>/<member>"` in a reused `String` (`clear` and two
  `push_str`): 197.3 ms in the first series, 194.8 ms in the second;
- the same plus `Utf32Str::new` on the result: 446.1 ms and 428.6 ms.

## Read path: cache file and listing

What the picker pays to get the members of one piece of 50 000 members
from disk and into the `Vec<String>` form of `Members`. The files were
read from the macOS page cache (they had just been written or read), so
disk latency is not part of these figures. Mean over 20 pieces, best of 3
program runs (the first run, made while another process loaded the
machine, took 28.5 ms and 56.0 ms in total and is left out):

| Step | Cache file | Listing |
| --- | ---: | ---: |
| `fs::read` of the file | 0.21 ms | |
| JSON parse into `Vec<RelPath>` (`serde_json::from_slice`) | 10.04 ms | |
| zstd decode and JSON parse of all entries (`listing::read_all`) | | 26.39 ms |
| `RelPath::to_string` and `sanitize` into a new `String` per member | 9.45 ms | 9.49 ms |
| Total per piece | 19.72 ms | 35.89 ms |

Decoding the listing with zstd alone, without the JSON parse, took
5.45 ms per piece (best of the same 3 runs). The files are 2 597 761
bytes (cache) and 2 874 061 bytes (listing) per piece.

For the 260 pieces that the 2 000 000 member budget does not retain, the
cache path is 260 times 19.72 ms = 5.1 s and the listing path 260 times
35.89 ms = 9.3 s for every key.

## Picker replica on files

A copy of the retain-and-narrow logic of `recompute` (retained members
up to 2 000 000, hits kept for retained pieces, a key that extends the
last narrows, a piece with no hit is skipped while narrowing) reading the
members of piece `i` from the files of piece `i % 60`. No row matches, so
every piece goes through its members. It is not the product code: the
picker state lives in the binary crate and cannot be linked from an
example. One run of each, the stages are the sum over the pieces read
or scored in that key; in the first key the 40 retained pieces are read
too. Milliseconds.

Members from the cache files:

| Key | Total | Read file | JSON parse | `to_string`+`sanitize` | Join and score |
| --- | ---: | ---: | ---: | ---: | ---: |
| `c` | 7293.4 | 88.0 | 3030.4 | 2855.4 | 1073.5 |
| `ct` | 11364.8 | 66.6 | 2937.6 | 2713.7 | 5399.1 |
| `ctrl` | 10578.3 | 54.8 | 2649.7 | 2479.4 | 5160.9 |
| `ctr` (backspace) | 11197.4 | 61.2 | 2702.5 | 2554.0 | 5642.8 |
| `qxz` | 6984.2 | 69.0 | 3020.2 | 2944.8 | 695.2 |

Members from the zstd listings (no cache files):

| Key | Total | zstd decode and parse | `to_string`+`sanitize` | Join and score |
| --- | ---: | ---: | ---: | ---: |
| `c` | 13731.2 | 8950.0 | 3194.4 | 1166.6 |
| `ct` | 16322.8 | 7763.2 | 2748.0 | 5428.5 |
| `ctrl` | 14946.0 | 6944.5 | 2512.6 | 5133.5 |
| `ctr` (backspace) | 16965.3 | 7738.4 | 2823.4 | 6008.1 |
| `qxz` | 12335.4 | 8300.2 | 2871.2 | 760.5 |

At the end of each run 2 000 000 of the 15 000 000 members were
retained. The run was made with load average 10 to 12 from other work.

## Scoring in parallel

Same sequence as the first single-thread table (`c`, `ct`, `ctrl`, `ctr`,
`qxz`), best of 3, milliseconds. The pieces are handed out one at a time to
`std::thread::scope` threads, each with its own `Matcher`; threads are
started again for every key. Spawning and joining empty scoped threads
that each build a `Matcher` took (best of 50): 0.018 ms for 1 thread,
0.036 ms for 4, 0.091 ms for 10 and 0.137 ms for 14.

The machine was loaded by other work during this series (load average
24.9 at the end, 15.7 over five minutes), so the thread counts of 10 and
14 are likely slower than on a quiet machine.

| Variant, threads | `c` | `ct` | `ctrl` | `ctr` | `qxz` |
| --- | ---: | ---: | ---: | ---: | ---: |
| B, 1 | 1268.7 | 5315.6 | 5074.8 | 5479.3 | 646.5 |
| B, 2 | 546.9 | 2544.9 | 2590.2 | 2816.3 | 327.3 |
| B, 4 | 278.3 | 1275.9 | 1294.7 | 1406.7 | 165.6 |
| B, 8 | 141.3 | 651.4 | 660.9 | 719.7 | 86.3 |
| B, 10 | 129.2 | 609.5 | 634.3 | 683.1 | 88.2 |
| B, 14 | 121.2 | 717.3 | 612.1 | 650.1 | 73.2 |
| C truncate join, 10 | 129.7 | 689.7 | 889.0 | 1004.5 | 81.0 |
| C truncate join, 14 | 111.6 | 615.0 | 600.3 | 638.6 | 67.0 |
| D member only, 10 | 107.9 | 410.0 | 261.5 | 429.5 | 68.9 |
| D member only, 14 | 79.3 | 298.4 | 205.6 | 330.9 | 50.0 |
| E member mask, 10 | 138.5 | 898.9 | 731.1 | 717.6 | 1.2 |
| E member mask, 14 | 115.8 | 592.8 | 570.1 | 664.0 | 1.5 |
| F member and piece mask, 10 | 131.2 | 614.6 | 605.1 | 669.9 | 0.2 |
| F member and piece mask, 14 | 122.1 | 624.2 | 590.9 | 653.5 | 0.2 |
| G first hit, 10 | 0.3 | 0.2 | 0.2 | 0.2 | 105.7 |
| G first hit, 14 | 0.2 | 0.2 | 0.2 | 0.3 | 77.8 |
| H C and E, 10 | 150.0 | 934.9 | 732.8 | 1207.0 | 2.6 |
| H C and E, 14 | 112.6 | 607.3 | 723.3 | 770.1 | 1.3 |
| J C, E and first hit, 10 | 0.3 | 0.2 | 0.2 | 0.2 | 2.9 |
| J C, E and first hit, 14 | 0.2 | 0.2 | 0.2 | 0.2 | 1.2 |

The speed-up of B over its single-thread time at `ct` is 2.1 times with
2 threads, 4.2 with 4, 8.2 with 8, 8.7 with 10 and 7.4 with 14.

## Two-stage search with a cancellable background scan

_Not yet measured._

## Memory for 15 million members

_Not yet measured._
