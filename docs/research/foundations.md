# Foundations from Rust: globs, ignore rules, SHA-256, ULIDs, JSON, TOML, machine name

Researched 2026-11-03 with Rust 1.97.0 on macOS, Apple M4 Pro (arm64,
`hw.optional.arm.FEAT_SHA256: 1`). Linux was not available; every Linux
statement is read from source. Crate figures come from the crates.io API
on that day.

Marks: **measured** (run on this machine), **read** (documentation,
source or registry data), **reported** (third-party statement).

Crate versions used: globset 0.4.21, glob 0.3.4, wax 0.7.0, ignore
0.4.34, sha2 0.11.1, ring 0.17.14, aws-lc-rs 1.19.0, ulid 3.0.0,
rusty_ulid 2.0.1, serde_json 1.0.154, jiff 0.2.39, time 0.3.55, chrono
0.4.45, toml 1.1.8, toml_edit 0.25.17, hostname 0.4.2, gethostname
1.1.0, whoami 2.1.3, system-configuration 0.8.0.

## Requirements

| # | Requirement | Source |
|---|-------------|--------|
| R1 | Units selected by glob patterns relative to the managed folder, default `*`, `**` allowed, shallowest match wins | ADR 0012 |
| R2 | Entries whose name starts with `.` are units only when a pattern names them explicitly | design |
| R3 | Ignore rules in gitignore syntax, from the config and from antiq's own ignore files, for the idle check only | ADR 0013 |
| R4 | SHA-256, streaming, fast on arm64, for archives, payloads and per-file hashes | design |
| R5 | ULID minted from a given time (the folder's creation time), lower or upper case text, monotonic within a millisecond | ADR 0014 |
| R6 | Metadata JSON: first field `format`, fixed key order, unknown fields preserved, RFC 3339 UTC, no writing of a higher `format` | design |
| R7 | TOML config, machine section parsed strictly, comments kept when antiq edits the file | design |
| R8 | Machine name: macOS ComputerName, hostname on Linux | design |

## 1. Unit selection by glob

No crate implements "shallowest match wins". It is a walk rule: antiq
walks top-down with `read_dir` and `symlink_metadata`, tests each
entry's path relative to the managed folder, and does not descend into
an entry that matched. With this walk, `**/*` selects the same units as
`*`. The glob crate only answers whether a relative path matches a
pattern.

Measured, with separator-aware matching in all three crates:

| Pattern | Path | `glob` (literal leading dot) | `globset` (literal separator) | `wax` |
|---------|------|------|------|------|
| `*` | `.git` | no | yes | yes |
| `**/*` | `a/.b` | no | yes | yes |
| `**/*` | `.a/b` | no | yes | yes |
| `projects/*` | `projects/.x` | no | yes | yes |
| `.hidden` | `.hidden` | yes | yes | yes |
| `.*` | `.foo` | yes | yes | yes |
| `a/{b,c}` | `a/c` | no (no braces) | yes | yes |
| `**/src` | `src` | yes | yes | yes |
| `*` | `a/b` | no | no | no |

- **`glob`** (rust-lang, MIT OR Apache-2.0, 702M downloads, 160M
  recent, 0.3.4 on 2026-07-21): `MatchOptions` with
  `require_literal_leading_dot` implements R2 only in part: `*`, `?`,
  bracket classes and `**` do not match a leading dot, but a literal
  after a `*` is not checked (below). No brace alternation, no negation.
- **`globset`** (ripgrep repository, Unlicense OR MIT, 280M downloads,
  69M recent, 0.4.21 on 2026-10-27): braces, `**`, matching many patterns
  in one pass. No dot rule: `*` matches `.git`, so R2 would be antiq's
  own check.
- **`wax`** (MIT, 3.4M downloads, one maintainer, 0.7.0 on 2026-01-29):
  richer syntax and its own walker, no dot rule.

`.*` matches `.foo` in `glob`, and ADR 0028 counts a pattern component
with a literal leading dot, `.*` included, as naming a dot entry.

`glob` 0.3.4 applies `require_literal_leading_dot` only to `*` (after its
first character), `?` and `[...]`. A literal character that follows a `*`
is compared without the check: with the rule on, `*.txt` matches `.txt`
(measured by a unit test in `src/select.rs`; the code is `src/lib.rs`
lines 809-820 and 847-873 of the crate). ADR 0028 allows a dot entry
only through a pattern that names the dot, so antiq splits a pattern at
`/`, matches each component with `glob` and applies the dot rule itself.
Two more readings of the same source: `**` at the end of a pattern needs
at least one directory (`a/**` does not match `a`, lines 648-656 and
847-851), and `**` anywhere else may take none (`a/**/b` matches `a/b`,
lines 816-820).

## 2. Ignore rules

`ignore` 0.4.34 (ripgrep repository, Unlicense OR MIT, 205M downloads,
50M recent) offers its gitignore matcher without its walker:
`GitignoreBuilder::new(root)`, `add_line` for config entries, `add(path)`
for a file, then `matched(path, is_dir)`. Measured: `target/`,
`/other/*.tmp`, `*.o` and `!important.o` behaved as in git. The
`gitignore` crate describes itself as deprecated; `glob` and `globset`
have no gitignore semantics (anchoring, `!`, trailing `/`).

ADR 0013 does not fix the shape of antiq's own ignore files. Both shapes
were measured:

**Per-directory files** (for example `.antiqignore` at any level):
`WalkBuilder::add_custom_ignore_filename` with `standard_filters(false)`,
`hidden(false)` and `follow_links(false)` handled them directly; a file
with `sub/*.log` and `!sub/keep.log` hid `sub/b.log` and kept
`sub/keep.log`. Config rules are not an input of the walker and need a
separate `Gitignore` in a `filter_entry` closure. On `~/playground`
(51,578 files, 5,283 directories), three noisy rounds: a plain `std`
lstat walk took 550 to 880 ms; the `ignore` walker took 540 to 775 ms
with per-directory lookup and 400 to 500 ms without it. Such a file
travels with the archive and the live package. It is a regular file, so
it counts toward the newest modification time unless excluded.

**One file** (for example `.antiq/ignore`): `Gitignore::new(path)` reads
it, anchored at the file's directory; the idle walk stays a plain `std`
walk plus `matched`. Rules are written relative to the managed folder
and do not travel with an archive.

`WalkBuilder::add_ignore(path)` with the same file did not apply the
anchored rule `sub/*.log` (**measured**); the cause was not found.

## 3. SHA-256

Streaming 1 MiB reads of a 1 GiB random file in the page cache, four
runs each. All three produced the same digest as `shasum -a 256`.

| Crate | MB/s per run |
|-------|--------------|
| `aws-lc-rs` 1.19.0 | 2301, 2135, 2687, 2503 |
| `ring` 0.17.14 | 1594, 1784, 2524, 2582 |
| `sha2` 0.11.1 | 1953, 1629, 2172, 2283 |

The spread within one crate is as large as the gap between crates.

- **`sha2`** (RustCrypto, MIT OR Apache-2.0, 1.16B downloads, 349M
  recent, 0.11.1 on 2026-10-12): pure Rust. On aarch64 it detects the
  SHA-2 instructions at run time through `cpufeatures`; 0.11 has no
  `asm` feature, so nothing needs enabling (**read**, `cargo metadata`).
  RUSTSEC-2021-0100 affects 0.9.7 only (**read**, advisory-db).
- **`ring`** (Apache-2.0 AND ISC, 865M downloads, 0.17.14 on
  2025-03-11): C and assembly. RUSTSEC-2025-0007 was withdrawn;
  RUSTSEC-2025-0009 is fixed in 0.17.12; RUSTSEC-2025-0010 (unmaintained)
  lists 0.17 and later as unaffected (**read**, advisory-db).
- **`aws-lc-rs`** (AWS, 293M downloads, 112M recent, 1.19.0 on
  2026-10-28): wraps AWS-LC and compiles C code at build time. No RustSec
  entries.
- `openssl` was not measured.

Small inputs (per-file hashes of many small files) and cold-disk reads
were not measured.

## 4. ULID from a given time

`ulid` 3.0.0 (MIT, 45.8M downloads, 18M recent, 3.0.0 on 2026-07-16):

- `Ulid::from_datetime(SystemTime)` mints from a given instant;
  `timestamp_ms()` returns that instant (**measured**). `to_string()` is
  upper case; lower-case text parses.
- `Generator::generate_from_datetime` is monotonic within one
  millisecond (**measured**).
- A `Generator` that has minted a later time and is then asked for an
  earlier one returns the later timestamp (**measured**). One shared
  `Generator` therefore cannot mint IDs for different creation times;
  `Ulid::from_datetime` per unit avoids it.
- A time before 1970 gives timestamp 0 without an error (**measured**).
  The rule that ignores timestamps before 1990 has to run before
  minting.

`rusty_ulid` 2.0.1 (1.37M downloads, 102k recent) mints from a timestamp
and increments monotonically behind its `rand` feature; given an older
timestamp after a newer one it uses the older time and gives a smaller
ULID (**measured**).

## 5. Metadata JSON

`serde_json` 1.0.154 with a struct holding `format`, `id`, `created_at`,
a nested payload list and `#[serde(flatten)] extra: Map<String, Value>`
on both levels, fed a document with unknown keys in several positions
(**measured**):

- Known fields are written in declaration order, `format` first.
- Unknown keys survive at the top level and in nested objects. They are
  written after the known fields; an unknown key that stood between
  known keys moves behind them.
- With the `preserve_order` feature, unknown keys keep their input
  order, nested ones too. Without it they come out sorted.
- A first pass deserialising only `{ format }` detects a higher version
  before the full type is touched.

Timestamps, input `2020-09-13T14:26:40.123+02:00` (**measured**):

| Crate | UTC | Output |
|-------|-----|--------|
| `jiff::Timestamp` | always; the type has no offset | `2020-09-13T12:26:40.123Z` |
| `time::OffsetDateTime` | after `.to_offset(UTC)`, otherwise keeps `+02:00` | `...12:26:40.123Z` after conversion |
| `chrono::DateTime<Utc>` | after `with_timezone(&Utc)` | `...12:26:40.123Z` |

The number of fractional digits varies with the value in all three;
a fixed width needs a custom serializer. `jiff` 0.2.39 (Unlicense OR
MIT, 258M downloads, 103M recent) is the youngest of the three (first
release 2024-02-17).

## 6. TOML config

- **`toml` 1.1.8** with `#[serde(deny_unknown_fields)]` rejects an
  unknown key with line, column and the expected keys, for example
  ``unknown field `rsyc_path`, expected `name` or `rsync_path` ``
  (**measured**). A round trip through `toml::Table` drops comments and
  sorts keys.
- **`toml_edit` 0.25.17** (same repository): `DocumentMut` kept
  comments, blank lines and key order while adding a table, changing
  values and pushing to an array; new tables go to the end
  (**measured**).
- **`basic-toml` 0.1.10**: reading only, no document editing.

Strict parsing of one section next to a lenient section in the same file
was not tested.

## 7. Machine name

On the test Mac `scutil --get ComputerName` printed `imperius` and
`hostname` printed `imperius.local` (**measured**).

| Option | Result | Notes |
|--------|--------|-------|
| `hostname` 0.4.2, `gethostname` 1.1.0, `whoami::hostname()` | `imperius.local` | the hostname, not the ComputerName |
| `whoami::devicename()` 2.1.3 | `imperius` | macOS: `SCDynamicStoreCopyComputerName` via `objc2-system-configuration`; Linux: `PRETTY_HOSTNAME` from `/etc/machine-info`, an error when absent (**read**, `src/os/unix.rs`) |
| `system-configuration` 0.8.0 | `imperius` | raw `unsafe` FFI plus `core-foundation` |
| `scutil --get ComputerName` | `imperius` | a child process, about 5 ms |

`whoami` 2.1.3: Apache-2.0 OR BSL-1.0 OR MIT, 262M downloads, released
2026-08-11.

## Comparison

| Block | Option | Meets | Gap | Pure Rust | Downloads (recent) |
|-------|--------|-------|-----|-----------|--------------------|
| glob | `glob` | R1, R2 | no braces | yes | 702M (160M) |
| glob | `globset` | R1, braces | R2 is own code | yes | 280M (69M) |
| glob | `wax` | R1 | R2, small user base | yes | 3.4M (1M) |
| ignore | `ignore` | R3, both shapes | `add_ignore` anchoring | yes | 205M (50M) |
| SHA-256 | `sha2` | R4 | none found | yes | 1.16B (349M) |
| SHA-256 | `ring` | R4 | last release 2025-03-11 | no | 865M (208M) |
| SHA-256 | `aws-lc-rs` | R4 | C build | no | 293M (112M) |
| ULID | `ulid` | R5 | shared `Generator` clamps | yes | 46M (18M) |
| ULID | `rusty_ulid` | R5 with `rand` | small user base | yes | 1.4M (0.1M) |
| JSON | `serde_json` + `preserve_order` | R6 | none | yes | 1.54B (401M) |
| time | `jiff` | UTC by type | variable digits | yes | 258M (103M) |
| time | `time`, `chrono` | R6 after conversion | offset can leak | yes | 1.04B, 941M |
| TOML | `toml` + `toml_edit` | R7 | none found | yes | 1.09B, 979M |
| name | `whoami::devicename` | R8 on macOS | Linux fallback needed | yes | 262M (82M) |
| name | `scutil` | R8 on macOS | child process, macOS only | n/a | n/a |

## Assessment

- Glob: `glob` for each path component when braces are not needed, with
  the dot rule applied by antiq since the crate checks it only in part;
  otherwise `globset` with a dot rule in antiq. The shallowest-match walk
  is antiq's own code in both cases.
- Ignore rules: `ignore`'s `Gitignore` for matching. One file or
  per-directory files is a design choice the numbers leave open; the
  per-directory lookup costs tens to a few hundred milliseconds on the
  playground.
- SHA-256: `sha2`. Pure Rust, hardware instructions without a flag, the
  most used, and the lead of `aws-lc-rs` is within run-to-run noise.
- ULID: `ulid` with `Ulid::from_datetime` per unit.
- Metadata JSON: `serde_json` with `preserve_order`, flatten maps on
  every struct that can grow, a first pass for `format`, and
  `jiff::Timestamp`.
- TOML: `toml` for strict typed reads, `toml_edit` for edits.
- Machine name: `whoami::devicename()`, falling back to the hostname on
  error.

## Read later

Read 2026-11-03 (**read**, glob 0.3.4 `src/lib.rs`, `matches_path` and
`matches_path_with`): both convert the path with `to_str()` and answer
"no match" for a path that is not valid UTF-8, marked `FIXME (#9639)`
in the source. A name that is not UTF-8 therefore never matches a
pattern through these functions.

## Not verified

- Anything on Linux.
- SHA-256 on small files and from disk; the `openssl` crate.
- A dot rule for `globset`; `glob` beyond the patterns above.
- Serde formatting features of `time` and `chrono`; compile time and
  binary size of each crate.
- `scutil` and `whoami::devicename` without a ComputerName set.
- Per-section strictness in one TOML file.

## Open points for the spec

- Whether an ignore file inside a unit counts toward its idle clock.
