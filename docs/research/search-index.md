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
