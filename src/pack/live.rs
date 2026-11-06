// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live packages.

use super::manifest::Manifest;
use super::pool::{Lease, Pool, workers_for_piece};
use super::zstd::{LIVE_JOB_SIZE, LIVE_LEVEL};
use crate::relpath::RelPath;
use crate::tree::{PieceTree, TreeEntry};
use serde_json::{Map, Value};

const MIB: u64 = 1024 * 1024;

/// What a live encoder counts against the memory budget whatever its worker
/// count (`docs/research/compression.md`).
pub const LIVE_FIXED_BYTES: u64 = 4 * MIB;

/// What each worker of a live encoder adds.
pub const LIVE_WORKER_BYTES: u64 = 2 * MIB;

/// Leases the workers for packing a piece of `piece_bytes`: one per job of
/// 512 KiB the piece fills, as many as the pool has free.
pub fn lease_for(pool: &Pool, piece_bytes: u64) -> Lease<'_> {
    let jobs = workers_for_piece(piece_bytes, u64::from(LIVE_JOB_SIZE));
    pool.lease(jobs, LIVE_FIXED_BYTES, LIVE_WORKER_BYTES)
}

/// The `encoder` record of a live payload: the zstd version that made the
/// bytes and the parameters that were set. The worker count is not part of
/// it, since it does not change the bytes.
pub fn encoder_record() -> Map<String, Value> {
    // The version is read from the linked library, so the record names
    // what really made the package.
    let mut record = Map::new();
    record.insert("zstd".into(), ::zstd::zstd_safe::version_string().into());
    record.insert("level".into(), LIVE_LEVEL.into());
    record.insert("rsyncable".into(), true.into());
    record.insert("job_size".into(), LIVE_JOB_SIZE.into());
    record.insert("long_distance_matching".into(), false.into());
    record.insert("checksum".into(), true.into());
    record
}

/// Whether `now` shows a change of the entry that `walk` recorded: a write
/// moves the modification time and the ctime, a replaced file has another
/// inode or kind, and a chmod moves the ctime alone.
pub fn differs(walk: &TreeEntry, now: &TreeEntry) -> bool {
    walk.kind != now.kind
        || walk.size != now.size
        || walk.mtime_ns != now.mtime_ns
        || walk.ctime_ns != now.ctime_ns
        || walk.inode != now.inode
}

/// A piece's tree without the entries the live exclude list names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Excluding {
    /// The tree to pack, and the entries of the list that took something
    /// out of it.
    Kept {
        tree: PieceTree,
        left_out: Vec<RelPath>,
    },
    /// The list names the piece's top directory or a directory above it.
    WholePiece,
}

impl Excluding {
    /// The tree to pack and the entries that left something out, or `None`
    /// when the piece has no live package.
    pub fn into_kept(self) -> Option<(PieceTree, Vec<RelPath>)> {
        match self {
            Excluding::Kept { tree, left_out } => Some((tree, left_out)),
            Excluding::WholePiece => None,
        }
    }
}

/// Whether `path` is `ancestor` or lies below it. The empty path is above
/// everything.
fn is_within(path: &[u8], ancestor: &[u8]) -> bool {
    ancestor.is_empty()
        || path == ancestor
        || (path.starts_with(ancestor) && path.get(ancestor.len()) == Some(&b'/'))
}

/// Applies the live exclude list, whose entries are relative to the
/// collection root, to the walk of one piece.
pub fn without_excluded(tree: &PieceTree, exclude: &[RelPath]) -> Excluding {
    let top = tree.top().as_bytes();
    let mut entries = tree.entries().to_vec();
    let mut left_out = Vec::new();
    for rule in exclude {
        let rule_bytes = rule.as_bytes();
        if is_within(top, rule_bytes) {
            return Excluding::WholePiece;
        }
        // The walk's paths start at the piece's top; a rule elsewhere in
        // the collection cannot match any of them.
        let inside = if top.is_empty() {
            Some(rule_bytes)
        } else {
            rule_bytes
                .strip_prefix(top)
                .and_then(|rest| rest.strip_prefix(b"/"))
        };
        let Some(inside) = inside else { continue };
        let before = entries.len();
        entries.retain(|entry| !is_within(entry.path.as_bytes(), inside));
        if entries.len() != before {
            left_out.push(rule.clone());
        }
    }
    Excluding::Kept {
        tree: PieceTree::new(tree.top().clone(), entries, tree.problems().to_vec()),
        left_out,
    }
}

/// The manifest of a walk as the live package of that piece holds it, or
/// `None` when the piece has no live package. Whoever compares a fresh walk
/// with the stored manifest builds the fresh side with this function, so
/// both sides leave out the same entries.
pub fn manifest_of(tree: &PieceTree, exclude: &[RelPath]) -> Option<Manifest> {
    let (kept, _) = without_excluded(tree, exclude).into_kept()?;
    Some(Manifest::from_entries(kept.entries()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::hashing::HashingReader;
    use crate::pack::zstd::{ZstdSettings, encoder};
    use crate::tree::EntryKind;
    use std::io::Write;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn entry(relative: &str, kind: EntryKind) -> TreeEntry {
        TreeEntry {
            path: path(relative),
            kind,
            size: 3,
            allocated: 4096,
            mtime_ns: 1_700_000_000_123_456_789,
            ctime_ns: 1_700_000_001_000_000_001,
            inode: 77,
            device: 1,
            mode: 0o644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
        }
    }

    fn tree(top: &str, entries: &[(&str, EntryKind)]) -> PieceTree {
        let entries = entries
            .iter()
            .map(|(relative, kind)| entry(relative, *kind))
            .collect();
        PieceTree::new(path(top), entries, vec![])
    }

    fn paths(tree: &PieceTree) -> Vec<String> {
        tree.entries().iter().map(|e| e.path.to_string()).collect()
    }

    // The memory figures are those of docs/research/compression.md.
    #[test]
    fn a_live_encoder_is_charged_4_mib_plus_2_mib_per_worker() {
        assert_eq!(LIVE_FIXED_BYTES, 4 * 1024 * 1024);
        assert_eq!(LIVE_WORKER_BYTES, 2 * 1024 * 1024);
    }

    #[test]
    fn a_piece_gets_one_worker_per_512_kib_job() {
        let pool = Pool::new(8, 1024 * MIB);
        let lease = lease_for(&pool, 3 * MIB);
        assert_eq!(lease.workers(), 6);
    }

    #[test]
    fn a_piece_below_one_job_gets_one_worker() {
        let pool = Pool::new(8, 1024 * MIB);
        let lease = lease_for(&pool, 1);
        assert_eq!(lease.workers(), 1);
    }

    #[test]
    fn the_memory_budget_limits_the_workers_of_a_big_piece() {
        let budget = LIVE_FIXED_BYTES + 3 * LIVE_WORKER_BYTES;
        let pool = Pool::new(8, budget);
        let lease = lease_for(&pool, 100 * MIB);
        assert_eq!(lease.workers(), 3);
    }

    #[test]
    fn the_lease_counts_the_documented_bytes_against_the_budget() {
        // A budget for exactly one worker: the second lease must not fit
        // beside the first.
        let pool = Pool::new(8, LIVE_FIXED_BYTES + LIVE_WORKER_BYTES);
        let first = lease_for(&pool, 100 * MIB);
        assert_eq!(first.workers(), 1);
        // Another thread asks without waiting, since a thread may hold
        // only one lease.
        let second = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    pool.try_lease(1, LIVE_FIXED_BYTES, LIVE_WORKER_BYTES)
                        .is_some()
                })
                .join()
                .expect("the probe thread finishes")
        });
        assert!(!second);
    }

    #[test]
    fn the_encoder_record_names_the_zstd_version_and_the_parameters() {
        let record = encoder_record();
        assert_eq!(
            record.get("zstd").and_then(Value::as_str),
            Some(::zstd::zstd_safe::version_string())
        );
        // The pinned version: a bump of zstd-sys fails here by name.
        assert_eq!(record["zstd"], "1.5.7");
        assert_eq!(record["level"], 3);
        assert_eq!(record["rsyncable"], true);
        assert_eq!(record["job_size"], 524_288);
        assert_eq!(record["long_distance_matching"], false);
        assert_eq!(record["checksum"], true);
    }

    #[test]
    fn a_size_change_is_a_difference() {
        let walk = entry("f", EntryKind::File);
        let mut now = walk.clone();
        now.size += 1;
        assert!(differs(&walk, &now));
    }

    #[test]
    fn a_modification_time_change_is_a_difference() {
        let walk = entry("f", EntryKind::File);
        let mut now = walk.clone();
        now.mtime_ns += 1;
        assert!(differs(&walk, &now));
    }

    #[test]
    fn a_change_time_change_is_a_difference() {
        let walk = entry("f", EntryKind::File);
        let mut now = walk.clone();
        now.ctime_ns += 1;
        assert!(differs(&walk, &now));
    }

    #[test]
    fn an_inode_change_is_a_difference() {
        let walk = entry("f", EntryKind::File);
        let mut now = walk.clone();
        now.inode += 1;
        assert!(differs(&walk, &now));
    }

    #[test]
    fn a_kind_change_is_a_difference() {
        let walk = entry("f", EntryKind::File);
        let mut now = walk.clone();
        now.kind = EntryKind::Symlink;
        assert!(differs(&walk, &now));
    }

    // A guard against a comparison that always reports a change: it
    // passes with any correct implementation.
    #[test]
    fn equal_records_do_not_differ() {
        let walk = entry("f", EntryKind::File);
        assert!(!differs(&walk, &walk.clone()));
    }

    #[test]
    fn an_excluded_directory_and_everything_below_it_leave_the_tree() {
        let walked = tree(
            "projects/p",
            &[
                ("", EntryKind::Directory),
                ("cache", EntryKind::Directory),
                ("cache/blob", EntryKind::File),
                ("cache-old", EntryKind::File),
                ("src", EntryKind::Directory),
            ],
        );
        let (tree, left_out) = without_excluded(&walked, &[path("projects/p/cache")])
            .into_kept()
            .expect("the piece keeps entries");
        assert_eq!(paths(&tree), ["", "cache-old", "src"]);
        assert_eq!(left_out, [path("projects/p/cache")]);
        assert_eq!(tree.top(), &path("projects/p"));
    }

    #[test]
    fn an_exclude_entry_that_matches_nothing_leaves_nothing_out() {
        let walked = tree("p", &[("", EntryKind::Directory), ("a", EntryKind::File)]);
        let (tree, left_out) = without_excluded(&walked, &[path("p/missing"), path("elsewhere/a")])
            .into_kept()
            .expect("the piece keeps entries");
        assert_eq!(paths(&tree), ["", "a"]);
        assert!(left_out.is_empty());
    }

    #[test]
    fn a_piece_at_the_collection_root_is_matched_by_the_whole_entry() {
        let walked = tree(
            "",
            &[
                ("", EntryKind::Directory),
                ("cache", EntryKind::Directory),
                ("keep", EntryKind::File),
            ],
        );
        let (tree, left_out) = without_excluded(&walked, &[path("cache")])
            .into_kept()
            .expect("the piece keeps entries");
        assert_eq!(paths(&tree), ["", "keep"]);
        assert_eq!(left_out, [path("cache")]);
    }

    #[test]
    fn an_exclude_entry_equal_to_the_top_leaves_the_piece_without_a_package() {
        let walked = tree("projects/p", &[("", EntryKind::Directory)]);
        assert_eq!(
            without_excluded(&walked, &[path("projects/p")]),
            Excluding::WholePiece
        );
    }

    #[test]
    fn an_exclude_entry_above_the_top_leaves_the_piece_without_a_package() {
        let walked = tree("projects/p", &[("", EntryKind::Directory)]);
        assert_eq!(
            without_excluded(&walked, &[path("projects")]),
            Excluding::WholePiece
        );
    }

    #[test]
    fn the_manifest_leaves_out_what_the_list_names_and_what_is_not_packed() {
        let walked = tree(
            "p",
            &[
                ("", EntryKind::Directory),
                ("cache", EntryKind::Directory),
                ("cache/blob", EntryKind::File),
                ("pipe", EntryKind::Fifo),
                ("link", EntryKind::Symlink),
            ],
        );
        let manifest = manifest_of(&walked, &[path("p/cache")]).expect("the piece has a package");
        let kept: Vec<String> = manifest
            .entries()
            .iter()
            .map(|e| e.path.to_string())
            .collect();
        assert_eq!(kept, ["", "link"]);
    }

    #[test]
    fn a_piece_below_an_excluded_path_has_no_manifest() {
        let walked = tree("projects/p", &[("", EntryKind::Directory)]);
        assert!(manifest_of(&walked, &[path("projects")]).is_none());
    }

    /// A stream of `len` bytes that is the same on every run, with enough
    /// variety that each 512 KiB job differs from the others.
    fn seeded_stream(len: usize) -> Vec<u8> {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if state.is_multiple_of(4) {
                    (state >> 20) as u8
                } else {
                    b'a' + ((state >> 33) % 10) as u8
                }
            })
            .collect()
    }

    fn live_frame(workers: u32, input: &[u8]) -> Vec<u8> {
        let mut encoder =
            encoder(Vec::new(), &ZstdSettings::live(workers)).expect("an encoder starts");
        encoder.write_all(input).expect("a vec accepts the stream");
        encoder.finish().expect("the frame ends")
    }

    fn sha256(bytes: &[u8]) -> String {
        let mut reader = HashingReader::new(bytes);
        std::io::copy(&mut reader, &mut std::io::sink()).expect("hashes");
        reader.sha256()
    }

    // Pins the bytes of the live encoder: rsyncable mode at level 3 with
    // 512 KiB jobs over six jobs, the same for any worker count. It fails
    // when a setting or the bundled zstd changes, which re-uploads every
    // live package once. The value is that of
    // `zstd -3 --rsyncable -B524288 -T1` on the same stream from stdin.
    #[test]
    fn the_live_encoder_gives_the_same_bytes_for_one_and_for_four_workers() {
        let input = seeded_stream(3 * 1024 * 1024);
        let one = sha256(&live_frame(1, &input));
        let four = sha256(&live_frame(4, &input));
        assert_eq!(one, four, "the worker count changes the bytes");
        assert_eq!(
            one,
            "b50bc5a6f10adbf3695cc523ca1f6f513c10648c043abca29189c008e5f50cc5"
        );
    }
}
