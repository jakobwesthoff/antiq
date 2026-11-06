// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live packages.

use super::hashing::{HashingReader, HashingWriter};
use super::manifest::Manifest;
use super::pool::{Lease, Pool, workers_for_piece};
use super::tar_write::{DirectoryStack, TarWriter};
use super::zstd::{self, LIVE_JOB_SIZE, LIVE_LEVEL, ZstdSettings};
use super::{PackError, PackNote};
use crate::fsx::tree::TreeRoot;
use crate::relpath::{Name, RelPath};
use crate::select::Rules;
use crate::tree::{EntryKind, PieceTree, Problem, TreeEntry};
use crate::walk::{self, RealFs, WalkError};
use serde_json::{Map, Value};
use std::cell::Cell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

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

// =========================================================
// Packing a live piece
// =========================================================

/// Where a live piece is walked from and what it leaves out.
pub struct LiveSource<'a> {
    pub collection: &'a TreeRoot,
    /// The piece's top directory, relative to the collection root.
    pub top: &'a RelPath,
    pub junk: &'a [Name],
    pub rules: &'a Rules,
    /// `collection.live_exclude`, relative to the collection root.
    pub exclude: &'a [RelPath],
}

/// How hard to try.
#[derive(Debug, Clone, Copy)]
pub struct LiveSettings {
    /// Workers of the compression pool the caller holds a lease for.
    pub workers: u32,
    /// `collection.live_retries`: how often a piece is started again after
    /// the first attempt.
    pub retries: u32,
}

/// A live package that was written and checked.
#[derive(Debug)]
pub struct LivePackage {
    pub size: u64,
    pub sha256: String,
    /// What the package was built from.
    pub manifest: Manifest,
    pub encoder: Map<String, Value>,
    pub notes: Vec<PackNote>,
    /// Files that were gone when the packer came to open them.
    pub dropped: Vec<RelPath>,
    /// The entries of `collection.live_exclude` that took something out of
    /// the package.
    pub left_out: Vec<RelPath>,
    /// Attempts it took, 1 for a piece that held still.
    pub attempts: u32,
}

/// The result of packing a live piece.
#[derive(Debug)]
pub enum LiveOutcome {
    Packed(LivePackage),
    /// The exclude list covers the piece, so it has no live package.
    NoPackage,
    /// Every attempt met a file that changed while it was read; `changed`
    /// is the last one.
    Stale {
        attempts: u32,
        changed: RelPath,
    },
    /// The walk found problems: the piece is not packed in this run.
    Blocked(Vec<Problem>),
}

/// Errors of packing a live piece.
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error(transparent)]
    Walk(#[from] WalkError),
    #[error(transparent)]
    Pack(#[from] PackError),
    #[error("the package {} failed its check: {reason}", path.display())]
    StreamCheck { path: PathBuf, reason: String },
}

impl crate::error::HasKind for LiveError {}

/// What a read-back of a package found.
#[derive(Debug, PartialEq, Eq)]
pub struct StreamCheck {
    pub size: u64,
    pub sha256: String,
}

/// Reads the package at `path` through the decoder, which verifies the
/// frame checksum, and hashes the file.
pub fn check_stream(path: &Path) -> Result<StreamCheck, LiveError> {
    let fail = |error: io::Error| LiveError::StreamCheck {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };
    let file = File::open(path).map_err(fail)?;
    let mut hashing = HashingReader::new(BufReader::new(file));
    let mut decoder = zstd::decoder(&mut hashing).map_err(fail)?;
    io::copy(&mut decoder, &mut io::sink()).map_err(fail)?;
    drop(decoder);
    // The decoder stops at the end of the last frame, and the hash has to
    // cover the whole file.
    io::copy(&mut hashing, &mut io::sink()).map_err(fail)?;
    Ok(StreamCheck {
        size: hashing.bytes_read(),
        sha256: hashing.sha256(),
    })
}

/// Points where a test can change the world in the middle of a pack.
pub(crate) trait LiveHooks {
    /// The hooks of the walks that restarts make.
    fn walk_hooks(&self) -> &dyn walk::Hooks {
        &RealFs
    }

    /// After attempt `attempt` has its tree, before anything is read.
    fn after_walk(&self, _attempt: u32, _tree: &PieceTree) {}

    /// Wraps the reader of a file about to be packed.
    fn wrap_read(&self, _entry: &TreeEntry, reader: Box<dyn Read>) -> Box<dyn Read> {
        reader
    }

    /// After a file was read, before its second `lstat`.
    fn after_read(&self, _entry: &TreeEntry, _attempt: u32) {}

    /// After the package was written, before it is read back.
    fn before_check(&self, _package: &Path) {}
}

/// The real world: nothing changes behind the packer's back.
struct NoHooks;

impl LiveHooks for NoHooks {}

/// Packs the live piece into the file `out`, starting again when a file
/// changes while it is read.
///
/// `first_tree` is the walk the caller compared with the manifest; a
/// restart walks again through `source`. A file that changes between
/// its walk and the end of its read cannot be in the package as it was
/// recorded, so the package is thrown away and the piece starts over, up
/// to `settings.retries` times.
pub fn pack_live(
    source: &LiveSource<'_>,
    first_tree: PieceTree,
    settings: &LiveSettings,
    out: &Path,
) -> Result<LiveOutcome, LiveError> {
    pack_live_with(source, first_tree, settings, out, &NoHooks)
}

pub(crate) fn pack_live_with(
    source: &LiveSource<'_>,
    first_tree: PieceTree,
    settings: &LiveSettings,
    out: &Path,
    hooks: &dyn LiveHooks,
) -> Result<LiveOutcome, LiveError> {
    let result = attempt_loop(source, first_tree, settings, out, hooks);
    if !matches!(result, Ok(LiveOutcome::Packed(_))) {
        // Nothing but a finished package is worth keeping. A file that
        // cannot be removed is not worth hiding the result for.
        let _ = fs::remove_file(out);
    }
    result
}

/// What one pass over a tree came to.
enum Written {
    Package {
        size: u64,
        sha256: String,
        notes: Vec<PackNote>,
        dropped: Vec<RelPath>,
        manifest: Manifest,
    },
    /// A file changed under the packer; the path is the one that did.
    Changed(RelPath),
}

fn attempt_loop(
    source: &LiveSource<'_>,
    first_tree: PieceTree,
    settings: &LiveSettings,
    out: &Path,
    hooks: &dyn LiveHooks,
) -> Result<LiveOutcome, LiveError> {
    let attempts = settings.retries.saturating_add(1);
    let mut first = Some(first_tree);
    let mut changed = None;
    for attempt in 1..=attempts {
        // The caller's walk is the first tree, so a piece that holds still
        // is walked once.
        let walked = match first.take() {
            Some(tree) => tree,
            None => match walk_again(source, hooks)? {
                Walked::Tree(tree) => tree,
                Walked::Vanished(path) => {
                    changed = Some(path);
                    continue;
                }
            },
        };
        let Some((tree, left_out)) = without_excluded(&walked, source.exclude).into_kept() else {
            return Ok(LiveOutcome::NoPackage);
        };
        if !tree.problems().is_empty() {
            return Ok(LiveOutcome::Blocked(tree.problems().to_vec()));
        }
        hooks.after_walk(attempt, &tree);
        match write_package(source, &tree, settings, out, hooks, attempt)? {
            Written::Package {
                size,
                sha256,
                notes,
                dropped,
                manifest,
            } => {
                return Ok(LiveOutcome::Packed(LivePackage {
                    size,
                    sha256,
                    manifest,
                    encoder: encoder_record(),
                    notes,
                    dropped,
                    left_out,
                    attempts: attempt,
                }));
            }
            Written::Changed(path) => changed = Some(path),
        }
    }
    Ok(LiveOutcome::Stale {
        attempts,
        changed: changed.expect("an attempt that does not return records the path that changed"),
    })
}

enum Walked {
    Tree(PieceTree),
    /// The walk lost an entry between listing its directory and reading it.
    Vanished(RelPath),
}

/// Walks the piece again after a restart.
///
/// The piece is being restarted because something in it changes, so the
/// walk is the one most likely to meet an entry that was listed and then
/// removed. A walk reports that as an unreadable entry; when it is the only
/// problem and the entry is gone now, it counts as one more change instead.
fn walk_again(source: &LiveSource<'_>, hooks: &dyn LiveHooks) -> Result<Walked, LiveError> {
    let tree = walk::walk_piece_with(
        source.collection,
        source.top,
        source.junk,
        source.rules,
        hooks.walk_hooks(),
    )?;
    if let [Problem::Unreadable { first }] = tree.problems() {
        let gone = source
            .collection
            .lstat(&source.top.concat(first))
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound);
        if gone {
            return Ok(Walked::Vanished(first.clone()));
        }
    }
    Ok(Walked::Tree(tree))
}

/// Counts the bytes that reach the tar writer.
struct Counting {
    inner: Box<dyn Read>,
    count: Rc<Cell<u64>>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.count.set(self.count.get() + count as u64);
        Ok(count)
    }
}

/// Hands the file that was opened for an entry to the tar writer.
fn content_of(opened: &mut Option<Counting>) -> io::Result<Box<dyn Read>> {
    let counting = opened
        .take()
        .ok_or(io::Error::other("no file was opened for this entry"))?;
    Ok(Box::new(counting))
}

/// What a failed open of a file means.
enum Opened {
    /// The file is gone, with its directory if need be.
    Gone,
    /// Something else is there now, or the path cannot be followed.
    Replaced,
    /// The file is as the walk saw it and still cannot be opened.
    Unreadable(io::Error),
}

fn classify_open_error(piece: &TreeRoot, entry: &TreeEntry, error: io::Error) -> Opened {
    match piece.lstat(&entry.path) {
        Err(now) if now.kind() == io::ErrorKind::NotFound => Opened::Gone,
        Err(_) => Opened::Replaced,
        Ok(now) if differs(entry, &now) => Opened::Replaced,
        Ok(_) => Opened::Unreadable(error),
    }
}

/// Writes the package of `tree` into `out` and checks it.
///
/// Every file is compared with the walk's record after its read. The
/// manifest is built from that record, so a package that passes holds
/// exactly what the manifest says.
fn write_package(
    source: &LiveSource<'_>,
    tree: &PieceTree,
    settings: &LiveSettings,
    out: &Path,
    hooks: &dyn LiveHooks,
    attempt: u32,
) -> Result<Written, LiveError> {
    if tree.entries().first().is_none_or(|top| !top.path.is_root()) {
        return Err(PackError::NoTopDirectory.into());
    }
    let folder = tree
        .top()
        .components()
        .last()
        .ok_or(PackError::NoContainerName)?;
    let container = Name::from_bytes(folder.to_vec()).expect("a component is a valid name");
    // A top that cannot be entered any more is no longer the directory
    // the walk saw; the walk of the next attempt tells what it is now.
    let Ok(piece) = source.collection.sub(source.top) else {
        return Ok(Written::Changed(RelPath::root()));
    };

    let file = File::create(out).map_err(PackError::Write)?;
    let hashing = HashingWriter::new(BufWriter::new(file));
    let encoder =
        zstd::encoder(hashing, &ZstdSettings::live(settings.workers)).map_err(PackError::Write)?;
    let mut writer = TarWriter::new(encoder, container);
    let mut stack = DirectoryStack::new(&piece);
    let mut linked = HashSet::new();
    let mut dropped = Vec::new();
    for entry in tree.entries() {
        // A second name of a file that was packed already becomes a link
        // member; the writer reads nothing for it, nor for a directory or
        // a symlink.
        let second_link = entry.nlink > 1 && linked.contains(&(entry.device, entry.inode));
        let mut opened = None;
        let mut bytes_read = None;
        if entry.kind == EntryKind::File && !second_link {
            // The file is opened here and not by the writer: a file that
            // is gone must not be registered as the first name of its
            // inode, or its other names would link to a member that does
            // not exist.
            let reader = match stack.open(entry) {
                Ok(reader) => reader,
                Err(error) => match classify_open_error(&piece, entry, error) {
                    Opened::Gone => {
                        dropped.push(entry.path.clone());
                        continue;
                    }
                    Opened::Replaced => return Ok(Written::Changed(entry.path.clone())),
                    Opened::Unreadable(source) => {
                        return Err(PackError::Read {
                            path: entry.path.clone(),
                            source,
                        }
                        .into());
                    }
                },
            };
            let count = Rc::new(Cell::new(0));
            opened = Some(Counting {
                inner: hooks.wrap_read(entry, reader),
                count: Rc::clone(&count),
            });
            bytes_read = Some(count);
        }
        writer.add(entry, &mut |_| content_of(&mut opened))?;
        let Some(bytes_read) = bytes_read else {
            continue;
        };
        if entry.nlink > 1 {
            linked.insert((entry.device, entry.inode));
        }

        let short = bytes_read.get() < entry.size;
        hooks.after_read(entry, attempt);
        let unchanged = piece
            .lstat(&entry.path)
            .is_ok_and(|now| !differs(entry, &now));
        if short || !unchanged {
            return Ok(Written::Changed(entry.path.clone()));
        }
    }

    let (encoder, _listing, notes) = writer.finish()?;
    let mut hashing = encoder.finish().map_err(PackError::Write)?;
    hashing.flush().map_err(PackError::Write)?;
    let (size, sha256) = (hashing.written(), hashing.sha256());
    drop(hashing);

    hooks.before_check(out);
    let check = check_stream(out)?;
    if (check.size, &check.sha256) != (size, &sha256) {
        return Err(LiveError::StreamCheck {
            path: out.to_path_buf(),
            reason: format!(
                "it holds {} bytes with SHA-256 {}, but {size} bytes with SHA-256 {sha256} were written",
                check.size, check.sha256
            ),
        });
    }

    // Entries are visited in path order, so `dropped` is sorted.
    let packed: Vec<TreeEntry> = tree
        .entries()
        .iter()
        .filter(|entry| dropped.binary_search(&entry.path).is_err())
        .cloned()
        .collect();
    Ok(Written::Package {
        size,
        sha256,
        notes,
        dropped,
        manifest: Manifest::from_entries(&packed),
    })
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
    pub(super) fn seeded_stream(len: usize) -> Vec<u8> {
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

/// Packing real trees. The hooks change the tree at the points where a
/// running system would.
#[cfg(test)]
mod pack_tests {
    use super::tests::seeded_stream;
    use super::*;
    use crate::pack::zstd::decoder;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use crate::walk::{Call, walk_piece};
    use std::cell::Cell;
    use std::fs;
    use std::io;
    use std::process::Command;

    const PIECE: &str = "p";

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    /// Hooks with a closure per point; a point without one does nothing.
    #[derive(Default)]
    struct Scripted<'a> {
        after_walk: Option<Box<dyn Fn(u32) + 'a>>,
        wrap_read: Option<Box<WrapRead<'a>>>,
        after_read: Option<Box<AfterRead<'a>>>,
        before_check: Option<Box<BeforeCheck<'a>>>,
        walk_fault: Option<Box<WalkFault<'a>>>,
    }

    type WrapRead<'a> = dyn Fn(&TreeEntry, Box<dyn Read>) -> Box<dyn Read> + 'a;
    type AfterRead<'a> = dyn Fn(&TreeEntry, u32) + 'a;
    type BeforeCheck<'a> = dyn Fn(&Path) + 'a;
    type WalkFault<'a> = dyn Fn(&RelPath, Call) -> Option<io::Error> + 'a;

    impl LiveHooks for Scripted<'_> {
        fn walk_hooks(&self) -> &dyn walk::Hooks {
            self
        }

        fn after_walk(&self, attempt: u32, _tree: &PieceTree) {
            if let Some(hook) = &self.after_walk {
                hook(attempt);
            }
        }

        fn wrap_read(&self, entry: &TreeEntry, reader: Box<dyn Read>) -> Box<dyn Read> {
            match &self.wrap_read {
                Some(hook) => hook(entry, reader),
                None => reader,
            }
        }

        fn after_read(&self, entry: &TreeEntry, attempt: u32) {
            if let Some(hook) = &self.after_read {
                hook(entry, attempt);
            }
        }

        fn before_check(&self, package: &Path) {
            if let Some(hook) = &self.before_check {
                hook(package);
            }
        }
    }

    impl walk::Hooks for Scripted<'_> {
        fn fault(&self, path: &RelPath, call: Call) -> Option<io::Error> {
            self.walk_fault.as_ref().and_then(|hook| hook(path, call))
        }
    }

    /// A collection root holding the piece `p`, and a directory for the
    /// packages.
    struct Fixture {
        tree: Tree,
        root: TreeRoot,
        out_dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut tree = Tree::new();
            tree.dir(PIECE);
            let root = TreeRoot::open(tree.root()).expect("the collection opens");
            Fixture {
                tree,
                root,
                out_dir: tempfile::tempdir().expect("a temp dir"),
            }
        }

        /// A path inside the piece, on disk.
        fn disk(&self, relative: &str) -> PathBuf {
            self.tree.path(&format!("{PIECE}/{relative}"))
        }

        fn out(&self) -> PathBuf {
            self.out_dir.path().join("package.tar.zst")
        }

        fn walk(&self) -> PieceTree {
            let rules = Rules::new(&[], &[]).expect("no patterns are valid");
            walk_piece(&self.root, &path(PIECE), &[], &rules).expect("the piece walks")
        }

        fn pack_to(
            &self,
            out: &Path,
            exclude: &[RelPath],
            settings: LiveSettings,
            hooks: &dyn LiveHooks,
        ) -> Result<LiveOutcome, LiveError> {
            let rules = Rules::new(&[], &[]).expect("no patterns are valid");
            let top = path(PIECE);
            let source = LiveSource {
                collection: &self.root,
                top: &top,
                junk: &[],
                rules: &rules,
                exclude,
            };
            pack_live_with(&source, self.walk(), &settings, out, hooks)
        }

        fn pack(&self, retries: u32, hooks: &dyn LiveHooks) -> Result<LiveOutcome, LiveError> {
            let settings = LiveSettings {
                workers: 1,
                retries,
            };
            self.pack_to(&self.out(), &[], settings, hooks)
        }

        fn package(&self, retries: u32, hooks: &dyn LiveHooks) -> LivePackage {
            expect_package(self.pack(retries, hooks).expect("the piece packs"))
        }
    }

    fn expect_package(outcome: LiveOutcome) -> LivePackage {
        match outcome {
            LiveOutcome::Packed(package) => package,
            other => panic!("expected a package, got {other:?}"),
        }
    }

    fn expect_stale(outcome: LiveOutcome) -> (u32, RelPath) {
        match outcome {
            LiveOutcome::Stale { attempts, changed } => (attempts, changed),
            other => panic!("expected a stale piece, got {other:?}"),
        }
    }

    fn expect_blocked(outcome: LiveOutcome) -> Vec<Problem> {
        match outcome {
            LiveOutcome::Blocked(problems) => problems,
            other => panic!("expected a blocked piece, got {other:?}"),
        }
    }

    type Member = (String, tar::EntryType, Option<String>, Vec<u8>);

    /// (path, type, link name, data) of every member of a package.
    fn members(package: &Path) -> Vec<Member> {
        let file = fs::File::open(package).expect("the package exists");
        let mut archive = tar::Archive::new(decoder(file).expect("a zstd stream"));
        archive
            .entries()
            .expect("a tar stream")
            .map(|entry| {
                let mut entry = entry.expect("a member");
                let mut data = Vec::new();
                entry.read_to_end(&mut data).expect("member data");
                (
                    String::from_utf8_lossy(&entry.path_bytes()).into_owned(),
                    entry.header().entry_type(),
                    entry
                        .link_name_bytes()
                        .map(|name| String::from_utf8_lossy(&name).into_owned()),
                    data,
                )
            })
            .collect()
    }

    fn member_paths(package: &Path) -> Vec<String> {
        members(package).into_iter().map(|m| m.0).collect()
    }

    fn data_of(package: &Path, member: &str) -> Vec<u8> {
        members(package)
            .into_iter()
            .find(|m| m.0 == member)
            .unwrap_or_else(|| panic!("no member {member}"))
            .3
    }

    fn manifest_paths(manifest: &Manifest) -> Vec<String> {
        manifest
            .entries()
            .iter()
            .map(|e| e.path.to_string())
            .collect()
    }

    fn append(path: &Path, bytes: &[u8]) {
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("the file opens")
            .write_all(bytes)
            .expect("the file grows");
    }

    // ----- what a package holds ---------------------------------------

    #[test]
    fn a_piece_packs_into_members_below_a_container_with_its_size_and_sha256() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/notes.txt", "hello");
        fixture.tree.file("p/src/main.rs", "fn main() {}");
        fixture.tree.symlink("p/src/link", "main.rs");
        fixture
            .tree
            .hard_link("p/notes.txt", "p/src/notes-copy.txt");
        let package = fixture.package(3, &Scripted::default());
        let found = members(&fixture.out());
        let described: Vec<_> = found
            .iter()
            .map(|m| (m.0.as_str(), m.1, m.2.as_deref()))
            .collect();
        assert_eq!(
            described,
            [
                ("p/", tar::EntryType::Directory, None),
                ("p/notes.txt", tar::EntryType::Regular, None),
                ("p/src/", tar::EntryType::Directory, None),
                ("p/src/link", tar::EntryType::Symlink, Some("main.rs")),
                ("p/src/main.rs", tar::EntryType::Regular, None),
                (
                    "p/src/notes-copy.txt",
                    tar::EntryType::Link,
                    Some("p/notes.txt")
                ),
            ]
        );
        assert_eq!(data_of(&fixture.out(), "p/notes.txt"), b"hello");
        let bytes = fs::read(fixture.out()).expect("the package exists");
        assert_eq!(package.size, bytes.len() as u64);
        let mut reader = HashingReader::new(&bytes[..]);
        io::copy(&mut reader, &mut io::sink()).expect("hashes");
        assert_eq!(package.sha256, reader.sha256());
        assert_eq!(package.attempts, 1);
        assert!(package.dropped.is_empty() && package.left_out.is_empty());
        assert_eq!(package.encoder, encoder_record());
    }

    use crate::pack::hashing::HashingReader;

    #[test]
    fn the_manifest_is_that_of_a_fresh_walk_of_the_packed_piece() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "alpha");
        fixture.tree.file("p/dir/b", "beta");
        let package = fixture.package(3, &Scripted::default());
        let fresh = manifest_of(&fixture.walk(), &[]).expect("the piece has a package");
        assert_eq!(package.manifest, fresh);
        assert_eq!(manifest_paths(&fresh), ["", "a", "dir", "dir/b"]);
    }

    // A guard: the package is the same for any number of workers. The
    // files add up to six jobs of 512 KiB, so four workers all run.
    #[test]
    fn one_and_four_workers_give_the_same_package() {
        let mut fixture = Fixture::new();
        let stream = seeded_stream(3 * 1024 * 1024 + 4096);
        for (index, chunk) in stream.chunks(1024 * 1024).enumerate() {
            fixture.tree.file(&format!("p/data{index}"), chunk);
        }
        let hashes: Vec<String> = [1, 4]
            .into_iter()
            .map(|workers| {
                let out = fixture.out_dir.path().join(format!("w{workers}.tar.zst"));
                let settings = LiveSettings {
                    workers,
                    retries: 3,
                };
                let outcome = fixture
                    .pack_to(&out, &[], settings, &Scripted::default())
                    .expect("the piece packs");
                expect_package(outcome).sha256
            })
            .collect();
        assert_eq!(hashes[0], hashes[1]);
    }

    // A guard: a time moved within the same second is a change for the
    // manifest and none for the package, whose headers hold whole seconds.
    #[test]
    fn a_modification_time_moved_within_its_second_changes_the_manifest_only() {
        let mut fixture = Fixture::new();
        fixture.tree.file_with("p/a", "same", 0o644, 1_000_000_000);
        let before = fixture.package(3, &Scripted::default());
        fixture
            .root
            .set_times(&path("p/a"), 1_000_000_000_500_000_000)
            .expect("the time is set");
        let after = fixture.package(3, &Scripted::default());
        assert_eq!(before.sha256, after.sha256);
        let diff = before.manifest.diff(&after.manifest);
        assert!(!diff.is_empty(), "the manifest sees the new time");
        assert!(diff.added.is_empty() && diff.removed.is_empty());
    }

    // A guard: other content gives other bytes.
    #[test]
    fn changed_content_changes_the_sha256() {
        let mut fixture = Fixture::new();
        fixture.tree.file_with("p/a", "aaaa", 0o644, 1_000_000_000);
        let before = fixture.package(3, &Scripted::default());
        fixture.tree.file_with("p/a", "bbbb", 0o644, 1_000_000_000);
        let after = fixture.package(3, &Scripted::default());
        assert_ne!(before.sha256, after.sha256);
    }

    #[test]
    fn an_excluded_directory_is_in_neither_the_package_nor_the_manifest() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/keep", "k");
        fixture.tree.file("p/cache/blob", "b");
        let settings = LiveSettings {
            workers: 1,
            retries: 3,
        };
        let outcome = fixture
            .pack_to(
                &fixture.out(),
                &[path("p/cache")],
                settings,
                &Scripted::default(),
            )
            .expect("the piece packs");
        let package = expect_package(outcome);
        assert_eq!(member_paths(&fixture.out()), ["p/", "p/keep"]);
        assert_eq!(manifest_paths(&package.manifest), ["", "keep"]);
        assert_eq!(package.left_out, [path("p/cache")]);
    }

    #[test]
    fn a_piece_below_an_excluded_path_has_no_package_and_no_file() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "a");
        let settings = LiveSettings {
            workers: 1,
            retries: 3,
        };
        let outcome = fixture
            .pack_to(&fixture.out(), &[path("p")], settings, &Scripted::default())
            .expect("the exclusion is no error");
        assert!(matches!(outcome, LiveOutcome::NoPackage), "{outcome:?}");
        assert!(!fixture.out().exists());
    }

    #[test]
    fn a_walk_after_packing_gives_an_empty_manifest_diff_with_an_exclude_list() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/keep", "k");
        fixture.tree.file("p/cache/blob", "b");
        fixture.tree.fifo("p/pipe");
        fixture.tree.symlink("p/link", "keep");
        let exclude = [path("p/cache")];
        let settings = LiveSettings {
            workers: 1,
            retries: 3,
        };
        let outcome = fixture
            .pack_to(&fixture.out(), &exclude, settings, &Scripted::default())
            .expect("the piece packs");
        let package = expect_package(outcome);
        let fresh = manifest_of(&fixture.walk(), &exclude).expect("the piece has a package");
        assert!(package.manifest.diff(&fresh).is_empty());
    }

    #[test]
    fn the_second_of_two_links_is_the_file_when_the_first_is_excluded() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "shared");
        fixture.tree.hard_link("p/a", "p/b");
        let settings = LiveSettings {
            workers: 1,
            retries: 3,
        };
        let outcome = fixture
            .pack_to(
                &fixture.out(),
                &[path("p/a")],
                settings,
                &Scripted::default(),
            )
            .expect("the piece packs");
        let package = expect_package(outcome);
        assert_eq!(package.attempts, 1);
        let found = members(&fixture.out());
        let kinds: Vec<_> = found.iter().map(|m| (m.0.as_str(), m.1)).collect();
        assert_eq!(
            kinds,
            [
                ("p/", tar::EntryType::Directory),
                ("p/b", tar::EntryType::Regular)
            ]
        );
        assert_eq!(found[1].3, b"shared");
    }

    #[test]
    fn a_fifo_and_a_time_before_1970_are_reported_as_notes() {
        let mut fixture = Fixture::new();
        fixture.tree.fifo("p/pipe");
        let old = fixture.tree.file("p/old", "x");
        filetime::set_file_mtime(&old, filetime::FileTime::from_unix_time(-5, 0))
            .expect("a time before 1970 can be set");
        let package = fixture.package(3, &Scripted::default());
        let notes: Vec<String> = package.notes.iter().map(PackNote::to_string).collect();
        assert_eq!(
            notes,
            [
                "the modification time of `p/old` is before 1970 and is stored as 1970",
                "`p/pipe` is a fifo and is not archived",
            ]
        );
    }

    #[test]
    fn a_path_past_1024_bytes_packs_and_walks_to_the_same_manifest() {
        let fixture = Fixture::new();
        let deep = path(PIECE).concat(&deep_path(6));
        for level in 2..=7 {
            let parts: Vec<&[u8]> = deep.components().take(level).collect();
            fixture
                .root
                .create_dir(
                    &RelPath::from_bytes(parts.join(&b'/')).expect("valid path"),
                    0o755,
                )
                .expect("the directory is created");
        }
        {
            use std::io::Write;
            let file = deep.concat(&path("data.txt"));
            fixture
                .root
                .create_file(&file, 0o644)
                .expect("the file is created")
                .write_all(b"deep")
                .expect("written");
        }
        let package = fixture.package(3, &Scripted::default());
        let found = members(&fixture.out());
        let last = found.last().expect("members");
        assert!(last.0.len() > 1024, "{} bytes", last.0.len());
        assert_eq!(last.3, b"deep");
        let fresh = manifest_of(&fixture.walk(), &[]).expect("the piece has a package");
        assert!(package.manifest.diff(&fresh).is_empty());
    }

    #[test]
    fn the_public_entry_point_packs_with_the_real_world() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let rules = Rules::new(&[], &[]).expect("no patterns are valid");
        let top = path(PIECE);
        let source = LiveSource {
            collection: &fixture.root,
            top: &top,
            junk: &[],
            rules: &rules,
            exclude: &[],
        };
        let settings = LiveSettings {
            workers: 1,
            retries: 3,
        };
        let outcome =
            pack_live(&source, fixture.walk(), &settings, &fixture.out()).expect("the piece packs");
        assert_eq!(expect_package(outcome).attempts, 1);
    }

    #[test]
    fn the_real_world_hooks_change_nothing() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let walked = fixture.walk();
        let hooks = NoHooks;
        hooks.after_walk(1, &walked);
        hooks.after_read(&walked.entries()[0], 1);
        hooks.before_check(&fixture.out());
        let mut reader = hooks.wrap_read(&walked.entries()[0], Box::new(&b"bytes"[..]));
        let mut text = String::new();
        reader.read_to_string(&mut text).expect("the reader reads");
        assert_eq!(text, "bytes");
        assert!(hooks.walk_hooks().fault(&path("a"), Call::Stat).is_none());
        assert!(!fixture.out().exists());
    }

    #[test]
    fn a_piece_replaced_by_a_file_after_the_walk_is_blocked_by_the_next_walk() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let piece = fixture.tree.path(PIECE);
        let hooks = Scripted {
            after_walk: Some(Box::new(|attempt| {
                if attempt == 1 {
                    fs::remove_dir_all(&piece).expect("the piece goes");
                    fs::write(&piece, "now a file").expect("the file is made");
                }
            })),
            ..Scripted::default()
        };
        let outcome = fixture.pack(3, &hooks).expect("a problem is no error");
        assert_eq!(
            expect_blocked(outcome),
            [Problem::Unreadable {
                first: RelPath::root()
            }]
        );
    }

    // ----- a file that changes while it is read -----------------------

    #[test]
    fn a_file_that_grew_during_its_read_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "one");
        let hooks = Scripted {
            after_read: Some(Box::new(|entry, attempt| {
                if attempt == 1 && entry.path.to_string() == "a" {
                    append(&file, b"-more");
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 2);
        assert_eq!(data_of(&fixture.out(), "p/a"), b"one-more");
    }

    #[test]
    fn a_file_whose_time_moved_during_its_read_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file_with("p/a", "same", 0o644, 1_000_000_000);
        let hooks = Scripted {
            after_read: Some(Box::new(|entry, attempt| {
                if attempt == 1 && entry.path.to_string() == "a" {
                    filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0))
                        .expect("the time is set");
                }
            })),
            ..Scripted::default()
        };
        assert_eq!(fixture.package(3, &hooks).attempts, 2);
    }

    #[test]
    fn a_file_that_read_short_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "12345");
        let shortened = Cell::new(false);
        let hooks = Scripted {
            wrap_read: Some(Box::new(|_, reader| {
                if shortened.replace(true) {
                    reader
                } else {
                    Box::new(reader.take(2))
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 2);
        assert_eq!(data_of(&fixture.out(), "p/a"), b"12345");
    }

    #[test]
    fn a_file_that_vanished_after_its_read_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        fixture.tree.file("p/b", "y");
        let hooks = Scripted {
            after_read: Some(Box::new(|entry, attempt| {
                if attempt == 1 && entry.path.to_string() == "a" {
                    fs::remove_file(&file).expect("the file goes");
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 2);
        assert_eq!(member_paths(&fixture.out()), ["p/", "p/b"]);
    }

    #[test]
    fn a_piece_that_never_holds_still_goes_stale_after_the_retries() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            after_read: Some(Box::new(|_, _| append(&file, b"x"))),
            ..Scripted::default()
        };
        let outcome = fixture.pack(2, &hooks).expect("staleness is no error");
        let (attempts, changed) = expect_stale(outcome);
        assert_eq!((attempts, changed), (3, path("a")));
        assert!(!fixture.out().exists(), "no half package is left");
    }

    #[test]
    fn one_retry_means_two_attempts() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            after_read: Some(Box::new(|_, _| append(&file, b"x"))),
            ..Scripted::default()
        };
        let outcome = fixture.pack(1, &hooks).expect("staleness is no error");
        assert!(
            matches!(outcome, LiveOutcome::Stale { attempts: 2, .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_restart_walks_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let added = fixture.disk("added");
        let hooks = Scripted {
            after_read: Some(Box::new(|_, attempt| {
                if attempt == 1 {
                    append(&file, b"y");
                    fs::write(&added, "new").expect("the file is added");
                }
            })),
            ..Scripted::default()
        };
        fixture.package(3, &hooks);
        assert_eq!(member_paths(&fixture.out()), ["p/", "p/a", "p/added"]);
    }

    #[test]
    fn a_file_replaced_by_a_symlink_after_the_walk_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            after_walk: Some(Box::new(|attempt| {
                if attempt == 1 {
                    fs::remove_file(&file).expect("the file goes");
                    std::os::unix::fs::symlink("elsewhere", &file).expect("the link is made");
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 2);
        let found = members(&fixture.out());
        assert_eq!(found[1].0, "p/a");
        assert_eq!(found[1].1, tar::EntryType::Symlink);
    }

    #[test]
    fn a_file_replaced_by_a_fifo_after_the_walk_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            after_walk: Some(Box::new(|attempt| {
                if attempt == 1 {
                    fs::remove_file(&file).expect("the file goes");
                    let status = Command::new("mkfifo")
                        .arg(&file)
                        .status()
                        .expect("the mkfifo executable exists");
                    assert!(status.success());
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 2);
        assert_eq!(member_paths(&fixture.out()), ["p/"]);
        assert_eq!(package.notes.len(), 1);
    }

    #[test]
    fn a_directory_replaced_by_a_file_after_the_walk_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/d/f", "x");
        let directory = fixture.disk("d");
        let hooks = Scripted {
            after_walk: Some(Box::new(|attempt| {
                if attempt == 1 {
                    fs::remove_dir_all(&directory).expect("the directory goes");
                    fs::write(&directory, "now a file").expect("the file is made");
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 2);
        assert_eq!(data_of(&fixture.out(), "p/d"), b"now a file");
    }

    #[test]
    fn a_file_deleted_before_it_is_opened_is_dropped() {
        let mut fixture = Fixture::new();
        let gone = fixture.tree.file("p/gone", "x");
        fixture.tree.file("p/stays", "y");
        let hooks = Scripted {
            after_walk: Some(Box::new(|_| {
                let _ = fs::remove_file(&gone);
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 1);
        assert_eq!(package.dropped, [path("gone")]);
        assert_eq!(member_paths(&fixture.out()), ["p/", "p/stays"]);
        assert_eq!(manifest_paths(&package.manifest), ["", "stays"]);
    }

    #[test]
    fn a_hard_link_whose_first_name_was_deleted_leaves_the_second_as_the_file() {
        let mut fixture = Fixture::new();
        let first = fixture.tree.file("p/a", "shared");
        fixture.tree.hard_link("p/a", "p/b");
        let hooks = Scripted {
            after_walk: Some(Box::new(|attempt| {
                if attempt == 1 {
                    fs::remove_file(&first).expect("the first name goes");
                }
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        // Removing a name moves the ctime of the file, so the second name
        // may or may not be seen as changed in the first attempt.
        assert!((1..=2).contains(&package.attempts), "{}", package.attempts);
        let found = members(&fixture.out());
        let kinds: Vec<_> = found.iter().map(|m| (m.0.as_str(), m.1)).collect();
        assert_eq!(
            kinds,
            [
                ("p/", tar::EntryType::Directory),
                ("p/b", tar::EntryType::Regular)
            ]
        );
        assert_eq!(found[1].3, b"shared");
    }

    // ----- problems and errors ----------------------------------------

    #[test]
    fn an_unreadable_directory_blocks_the_piece() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let locked = fixture.tree.unreadable_dir("p/locked");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let outcome = fixture
            .pack(3, &Scripted::default())
            .expect("a problem is no error");
        let problems = expect_blocked(outcome);
        assert_eq!(
            problems,
            [Problem::Unreadable {
                first: path("locked")
            }]
        );
        assert!(!fixture.out().exists());
    }

    #[test]
    fn a_problem_in_a_later_walk_blocks_the_piece() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        fixture.tree.dir("p/d");
        let hooks = Scripted {
            after_read: Some(Box::new(|_, attempt| {
                if attempt == 1 {
                    append(&file, b"y");
                }
            })),
            walk_fault: Some(Box::new(|at, call| {
                (call == Call::Open && at.to_string() == "d")
                    .then(|| io::Error::from(io::ErrorKind::PermissionDenied))
            })),
            ..Scripted::default()
        };
        let outcome = fixture.pack(3, &hooks).expect("a problem is no error");
        assert!(matches!(outcome, LiveOutcome::Blocked(_)), "{outcome:?}");
        assert!(!fixture.out().exists());
    }

    #[test]
    fn a_file_that_vanished_between_listing_and_stat_in_a_later_walk_starts_the_piece_again() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let fleeting = fixture.tree.file("p/b", "y");
        let faulted = Cell::new(false);
        let hooks = Scripted {
            after_read: Some(Box::new(|_, attempt| {
                if attempt == 1 {
                    append(&file, b"z");
                }
            })),
            walk_fault: Some(Box::new(|at, call| {
                if call == Call::Stat && at.to_string() == "b" && !faulted.replace(true) {
                    fs::remove_file(&fleeting).expect("the file goes");
                    return Some(io::Error::from(io::ErrorKind::NotFound));
                }
                None
            })),
            ..Scripted::default()
        };
        let package = fixture.package(3, &hooks);
        assert_eq!(package.attempts, 3);
        assert_eq!(member_paths(&fixture.out()), ["p/", "p/a"]);
    }

    #[test]
    fn running_out_of_descriptors_in_a_later_walk_is_an_error_of_the_run() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            after_read: Some(Box::new(|_, attempt| {
                if attempt == 1 {
                    append(&file, b"y");
                }
            })),
            walk_fault: Some(Box::new(|at, call| {
                (call == Call::Stat && at.to_string() == "a")
                    .then(|| io::Error::from_raw_os_error(rustix::io::Errno::MFILE.raw_os_error()))
            })),
            ..Scripted::default()
        };
        let error = fixture.pack(3, &hooks).expect_err("the run fails");
        assert!(
            matches!(error, LiveError::Walk(WalkError::OutOfDescriptors { .. })),
            "{error:?}"
        );
    }

    #[test]
    fn a_file_that_cannot_be_opened_for_another_reason_is_an_error_not_a_restart() {
        let mut fixture = Fixture::new();
        let file = fixture.tree.file("p/secret", "x");
        fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("the mode is set");
        assert!(
            fs::File::open(&file).is_err(),
            "the test needs a user that mode 000 locks out"
        );
        let error = fixture
            .pack(3, &Scripted::default())
            .expect_err("the file cannot be read");
        assert!(
            matches!(&error, LiveError::Pack(PackError::Read { path, .. }) if path.to_string() == "secret"),
            "{error:?}"
        );
        assert!(!fixture.out().exists());
    }

    #[test]
    fn a_tree_without_its_top_directory_or_a_container_name_is_refused() {
        let fixture = Fixture::new();
        let rules = Rules::new(&[], &[]).expect("no patterns are valid");
        let settings = LiveSettings {
            workers: 1,
            retries: 1,
        };
        let top = path(PIECE);
        let source = LiveSource {
            collection: &fixture.root,
            top: &top,
            junk: &[],
            rules: &rules,
            exclude: &[],
        };
        let empty = PieceTree::new(top.clone(), vec![], vec![]);
        let error = pack_live(&source, empty, &settings, &fixture.out()).expect_err("no top");
        assert!(
            matches!(error, LiveError::Pack(PackError::NoTopDirectory)),
            "{error:?}"
        );
        let root = fixture.walk().entries()[0].clone();
        let nameless = PieceTree::new(RelPath::root(), vec![root], vec![]);
        let error = pack_live(&source, nameless, &settings, &fixture.out()).expect_err("no name");
        assert!(
            matches!(error, LiveError::Pack(PackError::NoContainerName)),
            "{error:?}"
        );
    }

    #[test]
    fn a_piece_that_disappears_is_stale_with_its_top_as_the_changed_path() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let piece = fixture.tree.path(PIECE);
        let hooks = Scripted {
            after_walk: Some(Box::new(|_| {
                let _ = fs::remove_dir_all(&piece);
            })),
            ..Scripted::default()
        };
        let outcome = fixture
            .pack(1, &hooks)
            .expect("a vanished piece is no error");
        let (attempts, changed) = expect_stale(outcome);
        assert_eq!((attempts, changed), (2, RelPath::root()));
    }

    // ----- the check of the written package ---------------------------

    fn flip_a_byte(package: &Path) {
        let mut bytes = fs::read(package).expect("the package exists");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xFF;
        fs::write(package, bytes).expect("the package is rewritten");
    }

    fn truncate(package: &Path) {
        let bytes = fs::read(package).expect("the package exists");
        fs::write(package, &bytes[..bytes.len() - 8]).expect("the package is rewritten");
    }

    #[test]
    fn the_check_of_a_good_package_reports_its_size_and_sha256() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let package = fixture.package(3, &Scripted::default());
        let check = check_stream(&fixture.out()).expect("the package is good");
        assert_eq!((check.size, check.sha256), (package.size, package.sha256));
    }

    #[test]
    fn the_check_refuses_a_truncated_package() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        fixture.package(3, &Scripted::default());
        truncate(&fixture.out());
        let error = check_stream(&fixture.out()).expect_err("the stream is cut");
        assert!(matches!(error, LiveError::StreamCheck { .. }), "{error:?}");
    }

    #[test]
    fn the_check_refuses_a_package_with_a_flipped_byte() {
        let mut fixture = Fixture::new();
        let stream = seeded_stream(64 * 1024);
        fixture.tree.file("p/a", stream);
        fixture.package(3, &Scripted::default());
        flip_a_byte(&fixture.out());
        let error = check_stream(&fixture.out()).expect_err("the stream is damaged");
        assert!(matches!(error, LiveError::StreamCheck { .. }), "{error:?}");
    }

    #[test]
    fn the_check_refuses_a_package_that_is_not_there() {
        let fixture = Fixture::new();
        let error = check_stream(&fixture.out()).expect_err("no file");
        assert!(matches!(error, LiveError::StreamCheck { .. }), "{error:?}");
    }

    #[test]
    fn a_package_damaged_after_writing_is_an_error_and_leaves_no_file() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", seeded_stream(64 * 1024));
        let hooks = Scripted {
            before_check: Some(Box::new(flip_a_byte)),
            ..Scripted::default()
        };
        let error = fixture.pack(3, &hooks).expect_err("the check fails");
        assert!(matches!(error, LiveError::StreamCheck { .. }), "{error:?}");
        assert!(!fixture.out().exists());
    }

    // A skippable frame decodes to nothing, so the stream passes its
    // checksum while the file is no longer the one that was written.
    #[test]
    fn a_file_that_decodes_but_differs_from_what_was_written_is_an_error() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            before_check: Some(Box::new(|package| {
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(package)
                    .expect("the package opens")
                    .write_all(&[0x50, 0x2A, 0x4D, 0x18, 0, 0, 0, 0])
                    .expect("the frame is appended");
            })),
            ..Scripted::default()
        };
        let error = fixture.pack(3, &hooks).expect_err("the check fails");
        assert!(
            matches!(&error, LiveError::StreamCheck { reason, .. } if reason.contains("were written")),
            "{error:?}"
        );
        assert!(!fixture.out().exists());
    }

    #[test]
    fn a_package_cut_short_after_writing_is_an_error_and_leaves_no_file() {
        let mut fixture = Fixture::new();
        fixture.tree.file("p/a", "x");
        let hooks = Scripted {
            before_check: Some(Box::new(truncate)),
            ..Scripted::default()
        };
        let error = fixture.pack(3, &hooks).expect_err("the check fails");
        assert!(matches!(error, LiveError::StreamCheck { .. }), "{error:?}");
        assert!(!fixture.out().exists());
    }
}
