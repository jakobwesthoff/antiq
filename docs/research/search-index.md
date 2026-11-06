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
