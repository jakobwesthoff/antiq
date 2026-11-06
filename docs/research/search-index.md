# Search index for the contents search of the picker

Research on whether an on-disk search index can serve the contents search
of the `find` picker, which `docs/research/picker-search.md` measured at
1.1 to 5.8 s per key on one thread, 130 to 610 ms on ten threads, and
about 800 MB of memory for 15 million member paths. It records
measurements, not decisions. Sections are added as the measurements
finish.

## Machine, build and tools

All numbers come from one MacBook Pro Mac16,8 (Apple M4 Pro, 24 GB), from
a release build (`cargo build --release`, default release profile, cargo
1.97.0) of a throwaway crate that is not committed. Other work ran on the
machine (load average 4.6 to 6.8 at the start of the tantivy runs), which
only adds time. Libraries: tantivy 0.27.0 (the newest release on
crates.io when measured), nucleo-matcher 0.3.1.

Peak memory is given as two figures from `/usr/bin/time -l`: "maximum
resident set size" (`ru_maxrss`), which also counts file pages that the
process mapped, and "peak memory footprint", which counts memory the
process allocated. Time figures are the best of 5 runs of a query unless
stated otherwise.

## Data

300 pieces of 50 000 members, 15 000 000 paths. The generator is new (the
one of `picker-search.md` was not committed) and follows its description:

- 60 distinct sets of 50 000 sorted, unique paths. Piece `i` has the paths
  of set `i % 60`.
- 2 to 7 directory levels, file names in snake_case, kebab-case and
  CamelCase, `IMG_nnnn`, dated report names, 8-digit hex names and
  numbered names, 24 extensions, 73 directory words and 75 stem words,
  40 % of the directory components joined from two words.
- Set 0: 48.9 bytes per path on average, 8.3 word tokens per path, 4 933
  distinct words, 744 of 50 000 paths with non-ASCII characters.

The index holds the member path only, not the piece prefix. A query that
matches letters of the piece path is then answered by the row text, as in
variant D of `picker-search.md`.

Because five pieces repeat the same set, the word dictionaries below are
small. Dictionary size and the time to scan it depend on the data and are
tested separately where stated.

## Tantivy: word index

Schema: `piece` (u64, indexed and fast), `idx` (u64, fast; the position of
the member in its piece) and `path` (text, stored, index option `Basic`,
so document ids only, no frequencies or positions). The `path` tokenizer
is a custom one that splits on every non-alphanumeric character, at a
lower-to-upper case change and between letters and digits, and
lower-cases (`CtrlManager_v2.tif` gives `ctrl`, `manager`, `v`, `2`,
`tif`). Writer heap 512 000 000 bytes, default thread count. A commit
after every 30 pieces (ten commits), then a wait for the background
merges.

### Build and size

| Figure | Value |
| --- | ---: |
| Add and commit 15 000 000 documents | 13.9 s |
| of which generating the paths (in the same thread) | 6.2 s |
| Wait for background merges afterwards | 0.3 s |
| Segments after the last commit, after merging | 10, 3 |
| Peak footprint / maximum resident set size | 555 MB / 1539 MB |

Index size on disk: 647.5 MB, of which the document store (`.store`, the
stored paths) 481.7 MB. Without it 165.8 MB: postings (`.idx`) 120.2 MB,
field norms 30.0 MB, fast fields 15.1 MB, term dictionary 0.5 MB.

### Queries

Index opened with `Index::open_in_dir` and a manually reloaded reader:
0.5 to 0.8 ms (3 segments). Three evaluations of each query, best of 5
runs each: `count` (the `Count` collector), `per piece` (a collector that
reads the fast field `piece` of every hit and counts hits per piece, 300
counters) and `top 50` (`TopDocs`, 50 hits by score). Two runs of the
whole table gave the same figures within 0.2 ms; the second is shown.
Peak memory of the query process: 44 MB maximum resident set size, 9 to
11 MB footprint.

Prefix and substring queries are `RegexQuery` on the term dictionary
(`scan.*`, `.*ctr.*`); `scanner/src` is the word `scanner` and the
prefix `src.*` in a `BooleanQuery`; fuzzy is `FuzzyTermQuery` with edit
distance 1.

| Query | Hits | Pieces | count ms | per piece ms | top 50 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `c` (prefix) | 7 586 635 | 300 | 19.16 | 36.91 | 27.78 |
| `ct` (substring) | 3 162 060 | 300 | 9.22 | 16.91 | 12.46 |
| `ctr` (substring) | 306 125 | 300 | 3.36 | 4.32 | 3.85 |
| `ctrl` (substring) | 306 125 | 300 | 3.41 | 4.44 | 3.93 |
| `scan` (prefix) | 2 387 240 | 300 | 6.38 | 12.36 | 8.75 |
| `scanner` (word) | 1 244 255 | 300 | 0.00 | 4.44 | 0.04 |
| `scanner/src` (word and prefix) | 84 180 | 300 | 5.47 | 9.87 | 9.75 |
| `.tif` (word `tif`) | 381 940 | 300 | 0.00 | 1.53 | 0.02 |
| `report` (word) | 306 020 | 300 | 0.00 | 1.27 | 0.01 |
| `qxz` (substring) | 0 | 0 | 2.00 | 2.21 | 2.04 |
| `scaner` (fuzzy, distance 1) | 1 244 255 | 300 | 3.96 | 7.17 | 5.25 |
| `scaner` (fuzzy prefix, distance 1) | 1 244 255 | 300 | 3.96 | 7.20 | 4.96 |

### Incremental update

Run on the same index, in one process, three rounds on pieces 42, 43 and
44. Each round removes the piece (`delete_term` on `piece`, commit,
reader reload), adds it again (50 000 `add_document` calls, commit,
reload) and replaces it (delete and add, one commit).

| Step | Piece 42 | Piece 43 | Piece 44 |
| --- | ---: | ---: | ---: |
| generate the 50 000 paths | 25.9 ms | 37.0 ms | 34.0 ms |
| remove: `delete_term` and commit | 18.6 ms | 37.2 ms | 38.6 ms |
| add: 50 000 `add_document` (includes cloning the documents) | 242.5 ms | 256.0 ms | 250.7 ms |
| add: with commit | 428.3 ms | 422.6 ms | 404.2 ms |
| replace: delete, add, commit | 560.6 ms | 489.7 ms | 573.1 ms |
| reader reload after a commit | 0.2 to 1.8 ms | | |

Each commit adds a segment: 3 segments before, 11, 12 and 13 after the
three replace rounds (the segment count was read after the commit of
each round), 6 after waiting for the merges. The `ctr` count over the
index with deleted documents took 6.08 ms against 3.36 ms before the
rounds. The index grew from 647.5 to 655.5 MB (`.del` files 1.5 MB). A new
process needed 4.3 ms to open the index, build the reader and answer the
first `ctr` count query. The update process peaked at 125 MB footprint
and 177 MB maximum resident set size.

## Tantivy: trigram index

Same schema and writer settings as the word index, with a different
`path` analyzer: `NgramTokenizer` with grams of exactly 3 characters
(`prefix_only` off) and a lower-case filter, index option `Basic`. A query
is run through the same analyzer and every trigram must match
(`BooleanQuery` intersection). Queries shorter than 3 characters produce
no trigram, so they cannot be asked of this index.

Build: 15 000 000 documents added and committed in 19.5 s (6.4 s of it
generating paths), 6.2 s waiting for merges (24 segments, then 10), 25.8
s in all. Peak memory 652 MB footprint, 1769 MB maximum resident set
size. Index size 1196.9 MB, of which the document store 481.1 MB, so
715.8 MB without it: postings 668.6 MB, field norms 30.0 MB, fast fields
16.5 MB, term dictionary 0.7 MB.

Queries (same three evaluations as above, best of 5, second of two runs;
the first run agreed within 0.4 ms). Index opened in 1.3 ms (10
segments). Peak memory of the query process: 49 MB maximum resident set
size, 7 MB footprint.

| Query | Hits | Pieces | count ms | per piece ms | top 50 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `ctr` | 312 790 | 300 | 0.20 | 1.26 | 0.84 |
| `ctrl` | 306 125 | 300 | 0.70 | 2.04 | 1.66 |
| `scan` | 2 387 420 | 300 | 8.11 | 15.97 | 14.89 |
| `scanner` | 1 265 190 | 300 | 15.22 | 25.57 | 29.61 |
| `scanner/src` | 12 770 | 300 | 21.45 | 7.47 | 8.06 |
| `.tif` | 762 365 | 300 | 1.99 | 4.04 | 3.60 |
| `report` | 1 246 065 | 300 | 14.26 | 21.93 | 23.30 |
| `qxz` | 0 | 0 | 0.00 | 0.19 | 0.00 |

Incremental update, same three rounds as for the word index (pieces 42 to
44), starting at 10 segments:

| Step | Piece 42 | Piece 43 | Piece 44 |
| --- | ---: | ---: | ---: |
| remove: `delete_term` and commit | 24.6 ms | 43.8 ms | 26.1 ms |
| add: 50 000 `add_document` (includes cloning the documents) | 211.1 ms | 243.4 ms | 247.6 ms |
| add: with commit | 396.8 ms | 428.6 ms | 428.6 ms |
| replace: delete, add, commit | 591.6 ms | 562.2 ms | 630.5 ms |

Reader reloads took 0.3 to 4.9 ms. Segments after the three replace
rounds: 18, 19, 27; after waiting for merges 20. The `ctr` count took
0.41 ms afterwards against 0.20 ms before. The index grew from 1196.9 to
1210.7 MB. A new process needed 2.8 ms to open the index and answer the
first `ctr` count query. Peak memory 133 MB footprint, 203 MB maximum
resident set size.

## Tantivy: effect of a large word dictionary

The word index above has 4 933 distinct words per set and 25 685
terms in its largest segment (read after the update rounds), because five pieces repeat each set. A
second dataset (`SIUNIQ=1` in the generator) gives every piece its own
50 000 paths and replaces 30 % of the file names by 8 random lowercase
letters, which makes the word dictionary large. Both index types were
built again on it, with the settings above.

| Figure | Word index | Trigram index |
| --- | ---: | ---: |
| Add and commit 15 000 000 documents | 14.2 s | 17.8 s |
| Total with merges (segments 17, then 3 for both) | 17.6 s | 23.0 s |
| Index size | 702.8 MB | 1220.2 MB |
| of which store / postings / term dictionary | 485.8 / 125.8 / 46.5 MB | 485.5 / 688.2 / 0.4 MB |
| Peak footprint / maximum resident set size, build | 784 MB / 1655 MB | 560 MB / 1721 MB |
| Terms per segment (12 M, 1.5 M, 1.5 M documents) | 3 689 468, 470 038, 469 697 | not read |
| Query process, maximum resident set size / footprint | 113 MB / 11.5 MB | 43 MB / 5 MB |

Queries, best of 5, second run (in the first run the `top 50`
column of the word index was 62.6 ms for `c` and 506 to 529 ms for `ct`,
`ctr`, `ctrl` and `qxz`, while the file pages were read in):

| Query | Word index hits | count ms | per piece ms | top 50 ms | Trigram index hits | count ms | per piece ms | top 50 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `c` (prefix) | 7 103 283 | 45.19 | 61.49 | 54.08 | not possible | | | |
| `ct` (substring) | 2 810 275 | 309.07 | 314.75 | 313.04 | not possible | | | |
| `ctr` | 214 471 | 290.48 | 288.68 | 290.60 | 219 338 | 0.12 | 0.95 | 0.55 |
| `ctrl` | 212 984 | 288.67 | 290.64 | 290.21 | 212 985 | 0.51 | 1.47 | 1.15 |
| `scan` (prefix) | 2 222 866 | 6.03 | 11.54 | 8.34 | 2 223 021 | 7.32 | 14.51 | 13.07 |
| `scanner` (word) | 1 158 274 | 0.00 | 4.09 | 0.04 | 1 173 172 | 13.47 | 22.28 | 25.29 |
| `scanner/src` | 78 133 | 5.46 | 9.72 | 9.54 | 12 107 | 6.94 | 7.10 | 7.29 |
| `.tif` | 381 050 | 0.00 | 1.48 | 0.01 | 762 201 | 1.99 | 4.38 | 3.70 |
| `report` | 213 547 | 0.00 | 0.92 | 0.01 | 1 162 027 | 13.38 | 20.35 | 21.39 |
| `qxz` | 1 643 | 289.47 | 289.77 | 288.75 | 1 643 | 0.00 | 0.19 | 0.01 |
| `scaner` (fuzzy, distance 1) | 1 158 274 | 4.11 | 7.15 | 5.10 | not run | | | |

The substring queries `ct`, `ctr`, `ctrl` and `qxz` on the word index
went from 2 to 17 ms to 289 to 315 ms. Prefix queries (`scan`, `c`)
and whole-word queries did not change.

## SQLite FTS5 with the trigram tokenizer

SQLite from `rusqlite` 0.41.1 with the `bundled` feature. One contentless
table:

    CREATE VIRTUAL TABLE f USING fts5(path, content='', contentless_delete=1,
                                      tokenize='trigram', detail=<level>);

The row id is `piece << 16 | member index`, so the piece of a hit is
`rowid >> 16` and a piece is removed with `DELETE ... WHERE rowid BETWEEN`.
The path itself is not stored. WAL journal, `synchronous=NORMAL`, one
transaction per piece, then `INSERT INTO f(f) VALUES('optimize')`. A match
is case-insensitive and finds the needle anywhere in the path.

`detail=full` is the only level that accepts a phrase, and the trigram
tokenizer turns a needle of more than 3 characters into a phrase of
trigrams. With `detail=none` and `detail=column` SQLite returned
"phrase queries are not supported (detail!=full)", and `detail=none` also
"column queries are not supported (detail=none)" for `path MATCH`. For
`detail=none` the queries below therefore use `f MATCH` with the AND of
the needle's 3-character grams (`"sca" AND "can" AND ...`), which can
return more rows than the substring matches. A needle of fewer than 3
characters has no gram and cannot be asked.

| Figure | `detail=full` | `detail=none` |
| --- | ---: | ---: |
| Insert 15 000 000 rows (includes about 6 s of path generation) | 125.9 s | 81.8 s |
| `optimize` afterwards | 20.7 s | 10.5 s |
| Database file after `wal_checkpoint(TRUNCATE)` | 2398.6 MB | 979.3 MB |
| Peak memory of the build, maximum resident set size / footprint | 22 MB / 16 MB | 20 MB / 16 MB |
| Peak memory of the query process, maximum resident set size / footprint | 7.3 MB / 5.0 MB | 7.2 MB / 4.9 MB |

Queries, one thread, best of 5, second of two runs (the first agreed
within 13 %; 119.96 against 134.98 ms was the largest gap). `count` is
`SELECT count(*)`, `per piece` iterates all row ids and counts by piece,
`first 50` is `LIMIT 50` without an order, so it returns the lowest row
ids and no ranking.

| Query | Hits (full) | count ms | per piece ms | first 50 ms | Hits (none) | count ms | per piece ms | first 50 ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `ctr` | 312 790 | 4.63 | 9.40 | 0.00 | 312 790 | 4.65 | 9.34 | 0.00 |
| `ctrl` | 306 125 | 13.52 | 18.53 | 0.01 | 306 125 | 11.23 | 15.47 | 0.01 |
| `scan` | 2 387 420 | 110.04 | 147.94 | 0.01 | 2 387 420 | 88.94 | 123.26 | 0.01 |
| `scanner` | 1 244 345 | 185.46 | 208.31 | 0.02 | 1 265 190 | 134.98 | 159.22 | 0.01 |
| `scanner/src` | 7 050 | 83.24 | 83.42 | 0.51 | 12 770 | 87.00 | 89.52 | 0.32 |
| `.tif` | 762 365 | 32.45 | 45.02 | 0.01 | 762 365 | 23.50 | 34.17 | 0.01 |
| `report` | 1 246 065 | 125.64 | 148.36 | 0.01 | 1 246 065 | 100.18 | 118.96 | 0.01 |
| `qxz` | 0 | 0.00 | 0.00 | 0.00 | 0 | 0.01 | 0.00 | 0.00 |

The `detail=none` hit counts are the same as the hit counts of the tantivy
trigram index for all eight queries. The `detail=full` counts are the
substring matches (`scanner` and `scanner/src` are smaller).

Incremental update, three rounds on pieces 42 to 44 in the same
database, one transaction per step:

| Step | `detail=full` | `detail=none` |
| --- | ---: | ---: |
| delete 50 000 rows | 69.0, 60.9, 115.2 ms | 67.3, 62.4, 115.9 ms |
| insert 50 000 rows | 215.1, 347.0, 301.8 ms | 171.6, 176.9, 167.4 ms |
| delete and insert in one transaction (includes generating the paths) | 294.3, 372.3, 440.9 ms | 262.2, 289.7, 331.9 ms |
| `ctr` count before / after the three rounds | 4.63 / 20.96 ms | 4.65 / 20.37 ms |
| database file after the rounds | 2414.7 MB | 982.5 MB |

## fst: one set per piece, searched with a subsequence automaton

`fst` 0.4.7. Each piece has its own `fst::Set` of its sorted member paths
(case as written, no prefix), in a file `<piece>.fst`, opened with
`memmap2`. A query is a custom `fst::Automaton` that accepts a path when
the lower-cased needle's bytes occur in it in order, ignoring ASCII case:
the candidate set of a subsequence matcher such as nucleo. Non-ASCII
bytes never advance it. Every set is searched and its matches counted.

| Figure | Value |
| --- | ---: |
| Build 300 sets of 50 000 keys (includes about 6 s of path generation) | 31.2 s |
| Size on disk | 569.1 MB (1897 KB per piece) |
| Peak memory of the build, maximum resident set size / footprint | 16.5 MB / 15.5 MB |
| Map the 300 files | 5.8 to 6.3 ms |
| Peak memory of the query process, maximum resident set size / footprint | 575 MB / 2.9 MB |

Best of 3 runs per cell, from the first of two passes (the
second pass was stopped after its first row, `c`, which took 6099.1 ms on
one thread and 666.4 ms on ten):

| Query | Matches | Pieces | 1 thread ms | 10 threads ms |
| --- | ---: | ---: | ---: | ---: |
| `c` | 13 034 540 | 300 | 6227.9 | 681.9 |
| `ct` | 11 442 745 | 300 | 6260.4 | 683.8 |
| `ctr` | 9 096 605 | 300 | 6241.8 | 692.2 |
| `ctrl` | 4 792 810 | 300 | 6188.6 | 696.9 |
| `scan` | 6 553 865 | 300 | 6149.3 | 664.0 |
| `scanner/src` | 375 875 | 300 | 5966.2 | 653.2 |
| `.tif` | 762 365 | 300 | 5968.3 | 686.9 |
| `qxz` | 0 | 0 | 6088.2 | 676.4 |

The ten threads take the pieces one at a time from a shared counter. The
resident set size of the query process is the 569 MB of mapped file
pages that the pass touched.

## Fuzzy matching and the two-step path

The picker scores with nucleo (`Pattern::parse` with smart case and smart
normalization, `Matcher` with `Config::DEFAULT`), which accepts a path
when the query's characters occur in it in order, anywhere. None of the
indexes above has that semantic.

- Tantivy `FuzzyTermQuery` is an edit distance (here 1, with transpositions)
  between the query and a single term of the dictionary. `scaner` found
  the word `scanner` (1 244 255 hits, the same as the word query) in
  3.96 ms (`count`) on the default data and 4.11 ms on the data with 4.6 M
  terms. The prefix form found the same hits in the same time.
- The word index accepts a path if it holds a word or a word prefix, the
  trigram index and SQLite FTS5 if it holds the needle as a substring, the
  fst automaton if the needle's characters occur in order.
- On set 0 (50 000 paths, in memory) nucleo accepted 30 072 paths for
  `ctr` (60.1 %), 15 803 for `ctrl` (31.6 %), 21 711 for `scan`, 1 249 for
  `scanner/src` and 2 472 for `.tif`. A literal, case-insensitive
  substring search found 1 012, 989, 7 986, 30 and 2 472.
- The fst subsequence automaton is the only index in this document that
  returns the nucleo candidate set (9 096 605 matches for `ctr` over 300
  pieces, 60.6 % of the 15 M paths, against 60.1 % of set 0 for nucleo).
  It took 6.2 s on one thread and 0.69 s on ten. Scoring everything took
  5.1 to 5.8 s and 0.61 to 0.68 s in `picker-search.md`.

Two-step path on the stored fields: the index returns up to 200 hits
(`TopDocs`, 200 by score), the stored paths of those hits are fetched
from the document store, nucleo scores them. A regex query scores every hit
the same (tantivy's automaton weight builds a constant scorer), so the 200
are not the best matches. Best of
5 per figure, one run of the program. The nucleo columns are over the
candidates of the 200.

Word index:

| Query | nucleo accepts in set 0 | substring in set 0 | index hits in piece 0 | index hits all | step 1 ms | fetch 200 ms | nucleo ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `ctr` | 30 072 | 1 012 | 989 | 306 125 | 4.77 | 0.09 | 0.033 |
| `ctrl` | 15 803 | 989 | 989 | 306 125 | 4.82 | 0.09 | 0.033 |
| `scan` | 21 711 | 7 986 | 7 985 | 2 387 240 | 8.90 | 0.08 | 0.041 |
| `scanner/src` | 1 249 | 30 | 310 | 84 180 | 9.73 | 0.72 | 0.006 |
| `.tif` | 2 472 | 2 472 | 1 291 | 381 940 | 0.02 | 0.07 | 0.006 |

Trigram index:

| Query | index hits in piece 0 | index hits all | step 1 ms | fetch 200 ms | nucleo ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| `ctr` | 1 012 | 312 790 | 1.03 | 0.71 | 0.007 |
| `ctrl` | 989 | 306 125 | 1.84 | 0.71 | 0.007 |
| `scan` | 7 986 | 2 387 420 | 15.16 | 0.81 | 0.004 |
| `scanner/src` | 44 | 12 770 | 8.30 | 0.67 | 0.033 |
| `.tif` | 2 472 | 762 365 | 3.94 | 0.77 | 0.005 |

For `.tif` the word index finds the word `tif`; the substring has 1 181
more paths (`.tiff`).

Scoring every hit of `ctrl` (306 125 hits in 300 pieces, all accepted by
nucleo) after the index lookup, with the paths fetched from the stored
field in the same process:

| Index | Collect all hits ms | Fetch and score ms (second and third run) | First run ms | Peak memory, maximum resident set size / footprint |
| --- | ---: | ---: | ---: | ---: |
| Word index (6 segments) | 18.6 to 19.0 | 422.3, 424.0 | 940.3 | 540 MB / 33 MB |
| Trigram index (20 segments) | 17.9 to 18.5 | 1271.4, 1276.2 | 2749.0 | 564 MB / 45 MB |

Both indexes had been through the update rounds above when this was
measured.

## Where an index lives and how it goes stale

What the repository does today (`docs/specs/spec-v1.md`, `src/index.rs`):

- `.antiq/index/` is "derived, rebuildable, private to this machine". It
  holds one file `<ULID>.json` per object whose listing is stored here:
  the member paths and the file name of the listing they come from.
- A listing's file name follows from its payload's checksum, so a cache
  file that names the object's current listing is up to date. The object
  table (which objects exist, their current listing) is computed from the
  metadata and the tracking entries on every call and never stored.
- `index::update` writes a cache file for every object that lacks a
  current one, then removes every entry of `.antiq/index/` that is not
  one of the wanted file names, files and directories alike. `rebuild`
  empties the directory first. Read-only commands that find a file
  missing or outdated read the listing and do not write the file.

Measured above and relevant for a search index: replacing one piece takes
0.26 to 0.63 s in every engine (tantivy 0.49 to 0.63 s, SQLite 0.26 to
0.44 s), removing one takes 19 to 116 ms, and an index that is opened
again in a new process answers its first query after 2.8 to 4.3 ms in
tantivy and 0.5 to 0.6 ms in SQLite.

Proposal, not measured:

- The search index sits next to the member cache but not inside the
  directory that `update` empties of unknown entries, or `update` learns
  to leave it alone. For example `.antiq/search/`.
- The index records, per piece, the listing file name it indexed. The
  object table already says which listing is current, so a piece is stale
  when the two names differ, missing from the index when it has none, and
  extra in the index when the object table lacks it. A refresh deletes
  the stale and extra pieces and adds the stale and missing ones in one
  commit. With tantivy the names could travel in the commit (the source of
  0.27.0 has `PreparedCommit::set_payload`; not tried) or in a small file
  written after the commit.
- A missing or unreadable index is rebuilt in the background while the
  picker falls back to what it does now.

## Summary of the figures

All from the sections above, default data (300 pieces, 15 000 000 paths),
size without the stored paths.

| | Tantivy words | Tantivy trigrams | SQLite FTS5 `detail=none` | SQLite FTS5 `detail=full` | fst per piece |
| --- | ---: | ---: | ---: | ---: | ---: |
| Build | 14.2 s | 25.8 s | 92 s | 147 s | 31 s |
| Size on disk | 165.8 MB | 715.8 MB | 979.3 MB | 2398.6 MB | 569.1 MB |
| Query process, maximum resident set size | 44 MB | 49 MB | 7 MB | 7 MB | 575 MB (mapped) |
| `ctr` (per piece counts) | 4.3 ms | 1.3 ms | 9.3 ms | 9.4 ms | 6242 ms |
| `scan` (per piece counts) | 12.4 ms | 16.0 ms | 123 ms | 148 ms | 6149 ms |
| `c` and `ct` | 37 and 17 ms | not possible | not possible | not possible | 6228 and 6260 ms |
| Replace one piece | 0.49 to 0.57 s | 0.56 to 0.63 s | 0.26 to 0.33 s | 0.29 to 0.44 s | not measured |

Where the stored paths are part of the index, add 482 MB (tantivy,
document store). With 4.6 M distinct words, the tantivy word index
answers substring queries of 2 to 4 characters in about 290 to 315 ms
and its prefix and whole-word queries in the same time as before.
