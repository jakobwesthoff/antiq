// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archiving unearthed pieces.
//!
//! An unearthed piece is compared with the archive it was unearthed from.
//! One that equals it is reclaimed: its folder is removed the way `archive`
//! removes any folder, and no new archive is made (spec "archive",
//! ADR 0035). The steps leave a state the start-of-run repairs finish:
//!
//! 1. record the metadata change: lifecycle `archived`, live payload
//!    removed, a `reclaim` history entry;
//! 2. set the entry to `removing`, naming the archive payload;
//! 3. seal the durable archive from the comparison that found no
//!    difference;
//! 4. remove the folder (see `finish`).

use super::durable::{self, Matched};
use super::new::finish::Finished;
use super::new::{Flow, Piece, PieceOutcome, Stop, chain};
use super::{ArchiveAction, Candidate, Reclaimed, SkipReason, Step};
use crate::clock::whole_seconds;
use crate::collection::Collection;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::listing::{self, ListingEntry};
use crate::meta::{HistoryAction, HistoryEntry, Lifecycle, Metadata, Payload, PayloadKind};
use crate::pack::WalkRules;
use crate::pack::live::check_stream;
use crate::pack::verify::{Compared, compare_with_listing};
use crate::relpath::Name;
use crate::repair::unearthed_candidates;
use crate::report::Warning;
use crate::store::{Source, Store};
use crate::tracking::{self, DecisionKind, EntryState, TrackingEntry, VersionTakenOut};
use jiff::Timestamp;
use std::fs::File;

/// Why an unearthed piece is left alone.
pub(super) struct Refusal {
    pub(super) reason: SkipReason,
    pub(super) message: String,
}

impl Refusal {
    fn error(message: impl Into<String>) -> Refusal {
        Refusal {
            reason: SkipReason::Error,
            message: message.into(),
        }
    }
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Stop {
        Stop::Skip {
            reason: refusal.reason,
            message: refusal.message,
        }
    }
}

/// What the folder of an unearthed piece amounts to.
pub(super) enum Verdict {
    /// The folder equals the archive.
    Reclaim,
    /// The folder differs from the archive.
    Changed,
}

/// An unearthed piece compared with its archive.
pub(super) struct Classified {
    pub(super) entry: TrackingEntry,
    pub(super) metadata: Metadata,
    /// Where `metadata` comes from.
    pub(super) source: Source,
    /// The archive payload the folder was compared with.
    pub(super) basis: Payload,
    listing: Vec<ListingEntry>,
    compared: Compared,
    pub(super) verdict: Verdict,
}

/// Compares the folder `sub` of the piece `id` with the archive it was
/// unearthed from. Nothing is written.
///
/// The junk list is the one the caller walked and will compare the removal
/// with, so the folder, the listing and the lstat record agree on what
/// counts.
pub(super) fn classify(
    collection: &Collection,
    junk: &[Name],
    sub: &TreeRoot,
    id: ObjectId,
) -> Result<Classified, Refusal> {
    let entry = tracking::read(collection, id)
        .map_err(|error| Refusal::error(chain(&error)))?
        .ok_or_else(|| Refusal::error(format!("{id} has no tracking entry")))?;
    if entry.state != EntryState::Live {
        return Err(Refusal::error(format!(
            "the entry is {}",
            format!("{:?}", entry.state).to_lowercase()
        )));
    }
    if entry.diverged {
        return Err(Refusal::error(
            "the folder has diverged from the version it was taken out at",
        ));
    }
    let store = Store::new(collection);
    let current = store
        .current(id)
        .map_err(|error| Refusal::error(chain(&error)))?
        .ok_or_else(|| Refusal::error(format!("the object {id} has no metadata")))?;
    let metadata = current.metadata;
    if metadata.collection != collection.id() {
        return Err(Refusal::error("the object belongs to another collection"));
    }

    let mut candidates = unearthed_candidates(&entry, &metadata);
    if candidates.is_empty() {
        candidates = newest_archive(&metadata).into_iter().collect();
    }
    let rules = WalkRules {
        junk: junk.to_vec(),
        staged: false,
    };
    let mut unreadable = None;
    let mut found: Option<(Payload, Vec<ListingEntry>, Compared)> = None;
    for payload in candidates {
        let listing = match read_listing(&store, id, payload) {
            Ok(listing) => listing,
            Err(reason) => {
                unreadable.get_or_insert(reason);
                continue;
            }
        };
        let compared = compare_with_listing(&listing, sub, &rules)
            .map_err(|error| Refusal::error(chain(&error)))?;
        let equal = compared.differences().is_empty();
        if found.is_none() || equal {
            found = Some((payload.clone(), listing, compared));
        }
        if equal {
            break;
        }
    }
    let Some((basis, listing, compared)) = found else {
        return Err(Refusal::error(format!(
            "no listing of the archive can be read: {}",
            unreadable.unwrap_or_else(|| "the object has no archive".to_string())
        )));
    };
    let verdict = if compared.differences().is_empty() {
        Verdict::Reclaim
    } else {
        Verdict::Changed
    };
    Ok(Classified {
        entry,
        metadata,
        source: current.source,
        basis,
        listing,
        compared,
        verdict,
    })
}

/// The newest archive that is not superseded, for an entry that records no
/// version to compare with.
fn newest_archive(metadata: &Metadata) -> Option<&Payload> {
    metadata
        .payloads
        .iter()
        .filter(|payload| payload.kind == PayloadKind::Archive && !payload.superseded)
        .max_by_key(|payload| (payload.version, payload.created))
}

fn read_listing(
    store: &Store<'_>,
    id: ObjectId,
    payload: &Payload,
) -> Result<Vec<ListingEntry>, String> {
    let name = payload
        .listing
        .as_ref()
        .ok_or_else(|| format!("{} has no listing", payload.file.as_str()))?;
    let path = store.file_path(id, name);
    let file =
        File::open(&path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    listing::read_all(file).map_err(|error| error.to_string())
}

// =========================================================
// The gate
// =========================================================

/// Whether the folder may be deleted for the sake of the archive `basis`:
/// the payload file is in the store and passes the stream check with the
/// size and hash of the metadata, or a check of the remote copy is
/// recorded. `Ok` carries a warning when the remote copy is the reason.
pub(super) fn gate(
    collection: &Collection,
    entry: &TrackingEntry,
    basis: &Payload,
) -> Result<Option<String>, Refusal> {
    let path = Store::new(collection).file_path(entry.id, &basis.file);
    let failure = match check_stream(&path) {
        Ok(check)
            if basis.checksum.algorithm == "sha256"
                && check.size == basis.size
                && check.sha256 == basis.checksum.value =>
        {
            return Ok(None);
        }
        Ok(_) => "its size or hash differs from the metadata".to_string(),
        Err(error) => chain(&error),
    };
    let file = basis.file.as_str();
    if !matches!(basis.verified, Some(Some(_))) {
        return Err(Refusal {
            reason: SkipReason::PendingRemote,
            message: format!(
                "the archive {file} cannot be trusted here ({failure}) and no check of the remote copy is recorded"
            ),
        });
    }
    let held = entry
        .local_payloads
        .iter()
        .any(|local| local.file == basis.file);
    Ok((held || path.exists()).then(|| {
        format!(
            "the local archive {file} fails its check ({failure}); the remote copy is the good one"
        )
    }))
}

// =========================================================
// The dry run
// =========================================================

/// What a real run would do with the unearthed piece `candidate` whose
/// folder is `size` bytes, and the warning the gate gives. A refusal is
/// what would skip the piece.
pub(super) fn plan_action(
    collection: &Collection,
    junk: &[Name],
    root: &TreeRoot,
    candidate: &Candidate,
    size: u64,
) -> Result<(ArchiveAction, Option<Warning>), Refusal> {
    let id = candidate.id.expect("an unearthed piece has an ID");
    let sub = root
        .sub(&candidate.path)
        .map_err(|error| Refusal::error(chain(&error)))?;
    let classified = classify(collection, junk, &sub, id)?;
    match classified.verdict {
        Verdict::Reclaim => {
            let warning = gate(collection, &classified.entry, &classified.basis)?;
            Ok((
                ArchiveAction::Reclaim {
                    path: candidate.path.clone(),
                    id,
                    size,
                },
                warning.map(Warning::new),
            ))
        }
        Verdict::Changed => Err(differs(&classified)),
    }
}

fn differs(classified: &Classified) -> Refusal {
    Refusal {
        reason: SkipReason::Differs,
        message: format!(
            "the folder differs from the archive version {} it was unearthed from",
            classified.basis.version
        ),
    }
}

// =========================================================
// Reclaiming
// =========================================================

impl Piece<'_, '_> {
    /// An unearthed piece: reclaimed when it equals its archive.
    pub(super) fn rearchive(&mut self, sub: &TreeRoot, size: u64) -> Flow<PieceOutcome> {
        let id = self.known.expect("an unearthed piece has an ID");
        let cx = self.cx;
        let classified = classify(cx.collection, &cx.junk, sub, id)?;
        match classified.verdict {
            Verdict::Reclaim => self.reclaim(classified, size),
            Verdict::Changed => Err(differs(&classified).into()),
        }
    }

    fn reclaim(&mut self, classified: Classified, size: u64) -> Flow<PieceOutcome> {
        let Classified {
            mut entry,
            metadata,
            source,
            basis,
            listing,
            compared,
            ..
        } = classified;
        let cx = self.cx;
        if let Some(warning) = gate(cx.collection, &entry, &basis)? {
            self.warnings.push(Warning::new(warning));
        }
        // A trash tree in the way would stop the move after the metadata
        // change, so it is looked for before anything is written.
        self.check_trash_free(entry.id)?;

        let source = self.record_reclaim(&metadata, source, &entry)?;
        entry.version_taken_out = match source {
            Source::Pending => VersionTakenOut::Pending,
            Source::Committed(number) => VersionTakenOut::Committed(number),
        };
        entry.resolve_decision(DecisionKind::ReplacingWithLoss);
        self.mark_removing(&mut entry, &basis.file)?;

        let verified = compared
            .into_verified()
            .expect("the folder was compared and equals the listing");
        let matched = Matched::of_stored(verified, &listing)
            .expect("the verified listing is the one that was compared");
        let durable = durable::seal(cx.lock, matched, &basis, &entry)
            .expect("the entry was just marked removing for this payload");
        Ok(match self.remove_folder(&durable, entry)? {
            Finished::Removed => PieceOutcome::Reclaimed(Reclaimed {
                id: durable.id(),
                path: self.path.clone(),
                size,
            }),
            Finished::Kept(kept) => PieceOutcome::Kept(kept),
        })
    }

    /// Step 1. A change a crashed run recorded is not written again, so the
    /// history keeps one `reclaim` entry. Returns where the current
    /// metadata comes from afterwards.
    fn record_reclaim(
        &self,
        metadata: &Metadata,
        source: Source,
        entry: &TrackingEntry,
    ) -> Flow<Source> {
        let cx = self.cx;
        let since = entry.unearthed.unwrap_or(Timestamp::MIN);
        if is_reclaim_recorded(metadata, &cx.machine, since) {
            return Ok(source);
        }
        let mut changed = metadata.clone();
        changed.lifecycle = Lifecycle::Archived;
        changed
            .payloads
            .retain(|payload| payload.kind != PayloadKind::Live);
        changed.history.push(HistoryEntry::new(
            whole_seconds(cx.clock.now()),
            cx.machine.clone(),
            HistoryAction::Reclaim,
        ));
        self.at(Step::WriteMetadata)?;
        self.store()
            .write_pending(cx.lock, &changed)
            .map_err(|error| Stop::from_error(&error))?;
        Ok(Source::Pending)
    }
}

/// Whether `metadata` holds the change of a reclaim that this machine made
/// since `since`: archived, no live payload and a `reclaim` entry in the
/// history. An archived piece that was kept after an archive still carries
/// its live payload, and has no such entry, so it is not recorded.
fn is_reclaim_recorded(metadata: &Metadata, machine: &str, since: Timestamp) -> bool {
    metadata.lifecycle == Lifecycle::Archived
        && metadata
            .payloads
            .iter()
            .all(|payload| payload.kind != PayloadKind::Live)
        && metadata.history.iter().any(|entry| {
            entry.action == HistoryAction::Reclaim.as_str()
                && entry.machine == machine
                && entry.time >= since
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::id::SeqIds;
    use crate::meta::PayloadKind;
    use crate::ops::archive::{
        ArchiveAction, ArchiveReport, ArchiveRequest, RealSeams, Seams, archive_with, plan_with,
    };
    use crate::ops::status;
    use crate::ops::unearth::tests::{FILES, Fixture, ID, id, path, time};
    use crate::relpath::RelPath;
    use crate::testing::shared_pool;
    use crate::testing::tree::TempCollection;
    use serde_json::{Value, json};
    use std::fs;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const PIECE: &str = "projects/work";
    /// The modification time `Fixture::archived` gives the files it packs.
    const FILE_TIME: i64 = 1_000_000_000;

    /// An unearthed piece at [`PIECE`] that was unearthed 30 days before
    /// the clock, with nobody holding the run lock.
    struct Scene {
        temp: TempCollection,
        collection: Collection,
        clock: FixedClock,
    }

    /// A piece archived with `archived` and unearthed again with
    /// `restored`; the same files make the folder equal its archive.
    fn scene_of(archived: &[(&str, &str)], restored: &[(&str, &str)]) -> Scene {
        let mut fx = Fixture::new();
        fx.archived(ID, PIECE, archived);
        for (name, content) in restored {
            fx.tree()
                .file_with(&format!("{PIECE}/{name}"), content, 0o644, FILE_TIME);
        }
        let mut entry = fx.entry(ID);
        entry.state = EntryState::Live;
        entry.unearthed = Some(time("2026-10-05T12:00:00Z"));
        entry.unearthed_payload_version = Some(1);
        fx.write_entry(&mut entry);
        fx.edit_meta(ID, |document| document["lifecycle"] = json!("live"));
        let Fixture {
            temp,
            collection,
            clock,
            lock,
        } = fx;
        drop(lock);
        Scene {
            temp,
            collection,
            clock,
        }
    }

    fn scene() -> Scene {
        scene_of(FILES, FILES)
    }

    /// Seams that call `at` before each step.
    struct At<F>(F);

    impl<F: Fn(Step) -> io::Result<()> + Sync> Seams for At<F> {
        fn at(&self, step: Step, _: &RelPath) -> io::Result<()> {
            (self.0)(step)
        }
    }

    impl Scene {
        fn run(&self, seams: &dyn Seams) -> ArchiveReport {
            let ids = SeqIds::new(1);
            archive_with(
                &ArchiveRequest {
                    collection: &self.collection,
                    clock: &self.clock,
                    ids: &ids,
                    pool: shared_pool(),
                    workers: 1,
                    dry_run: false,
                },
                seams,
            )
            .expect("the run finishes")
        }

        /// A run that dies before `step`, as a process that is killed
        /// there, followed by the repairs of the next run.
        fn crash_before_then_repair(&self, step: Step) {
            let fired = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&fired);
            let seams = At(move |at| {
                if at == step {
                    seen.store(true, Ordering::SeqCst);
                    panic!("the process dies before {at:?}");
                }
                Ok(())
            });
            let result = catch_unwind(AssertUnwindSafe(|| self.run(&seams)));
            assert!(
                result.is_err() && fired.load(Ordering::SeqCst),
                "died at {step:?}"
            );
            self.repair();
        }

        fn repair(&self) {
            drop(crate::run::start(&self.collection, &self.clock, false).expect("the repairs run"));
        }

        fn entry(&self) -> TrackingEntry {
            tracking::read(&self.collection, id(ID))
                .expect("the entry reads")
                .expect("the entry exists")
        }

        fn metadata(&self) -> Metadata {
            Store::new(&self.collection)
                .current(id(ID))
                .expect("the metadata reads")
                .expect("the object has metadata")
                .metadata
        }

        fn folder(&self) -> std::path::PathBuf {
            self.temp.root().join(PIECE)
        }

        fn object_file(&self, name: &str) -> std::path::PathBuf {
            self.collection.object_dir(id(ID)).join(name)
        }

        /// The first payload's file name.
        fn payload_file(&self) -> String {
            self.metadata().payloads[0].file.as_str().to_string()
        }

        fn edit_meta(&self, edit: impl FnOnce(&mut Value)) {
            let path = self.object_file("meta.1");
            let mut document: Value =
                serde_json::from_slice(&fs::read(&path).expect("reads")).expect("json");
            edit(&mut document);
            fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("written");
        }

        fn edit_entry(&self, edit: impl FnOnce(&mut TrackingEntry)) {
            let mut entry = self.entry();
            edit(&mut entry);
            let path = self.collection.tracking_path(id(ID));
            fs::write(path, serde_json::to_vec(&entry).expect("json")).expect("written");
        }

        /// A further archive payload of `version` whose listing is `lines`.
        /// Its file holds the bytes of the first payload.
        fn add_payload(&self, version: u32, lines: &[ListingEntry]) -> String {
            let file = format!("{version:016x}-b.tar.zst");
            let listing_name = listing_file_for(u64::from(version));
            let out = fs::File::create(self.object_file(&listing_name)).expect("a file");
            listing::write(lines.iter(), out).expect("the listing is written");
            fs::copy(
                self.object_file(&self.payload_file()),
                self.object_file(&file),
            )
            .expect("the payload is copied");
            self.edit_meta(|document| {
                let mut payload = document["payloads"][0].clone();
                payload["version"] = json!(version);
                payload["file"] = json!(file);
                payload["listing"] = json!(listing_name);
                payload["created"] = json!("2026-11-03T11:00:00Z");
                document["payloads"]
                    .as_array_mut()
                    .expect("payloads")
                    .push(payload);
            });
            file
        }

        /// The listing of the first payload.
        fn listing(&self) -> Vec<ListingEntry> {
            let name = self.metadata().payloads[0]
                .listing
                .clone()
                .expect("a listing");
            listing::read_all(fs::File::open(self.object_file(name.as_str())).expect("opens"))
                .expect("reads")
        }

        fn classified(&self) -> Result<Classified, Refusal> {
            let root = TreeRoot::open(self.temp.root()).expect("the root opens");
            let sub = root.sub(&path(PIECE)).expect("the piece opens");
            let junk =
                status::junk_names(&self.collection.config().collection.junk, &mut Vec::new());
            classify(&self.collection, &junk, &sub, id(ID))
        }
    }

    /// A listing file name of metadata format 1 that is unique to `number`.
    fn listing_file_for(number: u64) -> String {
        format!("{:016x}.listing.zst", 0xa000 + number)
    }

    fn reclaimed(report: &ArchiveReport) -> usize {
        report
            .reclaimed
            .as_deref()
            .expect("a real run lists them")
            .len()
    }

    fn skipped(report: &ArchiveReport) -> &[crate::ops::archive::Skipped] {
        report.skipped.as_deref().expect("a real run lists them")
    }

    fn reclaim_entries(metadata: &Metadata) -> usize {
        metadata
            .history
            .iter()
            .filter(|entry| entry.action == HistoryAction::Reclaim.as_str())
            .count()
    }

    // =========================================================
    // Reclaiming
    // =========================================================

    #[test]
    fn an_unchanged_unearthed_piece_is_reclaimed() {
        let scene = scene();
        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "one reclaimed row: {report:?}");
        let row = &report.reclaimed.as_deref().expect("rows")[0];
        assert_eq!((row.id, row.path.to_string()), (id(ID), PIECE.to_string()));
        assert!(!scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Gone);
        let metadata = scene.metadata();
        assert_eq!(metadata.lifecycle, Lifecycle::Archived);
        assert_eq!(metadata.payloads.len(), 1, "no new payload");
        assert_eq!(reclaim_entries(&metadata), 1);
        assert!(!report.partial_for_test());
    }

    #[test]
    fn an_archive_kept_piece_loses_its_live_payload_when_it_is_reclaimed() {
        let scene = scene();
        scene.edit_meta(|document| {
            document["lifecycle"] = json!("archived");
            let mut live = document["payloads"][0].clone();
            live["kind"] = json!("live");
            document["payloads"]
                .as_array_mut()
                .expect("payloads")
                .push(live);
        });

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
        let metadata = scene.metadata();
        assert!(
            metadata
                .payloads
                .iter()
                .all(|payload| payload.kind == PayloadKind::Archive)
        );
        assert_eq!(reclaim_entries(&metadata), 1);
    }

    #[test]
    fn a_changed_unearthed_piece_is_left_alone() {
        let scene = scene_of(FILES, &[("notes.txt", "edited"), FILES[1]]);
        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 0);
        let rows = skipped(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].reason, SkipReason::Differs);
        assert!(scene.folder().exists());
        assert_eq!(scene.metadata().lifecycle, Lifecycle::Live);
    }

    #[test]
    fn an_entry_that_is_not_live_is_skipped() {
        let scene = scene();
        scene.edit_entry(|entry| entry.state = EntryState::Gone);

        let report = scene.run(&RealSeams);

        let rows = skipped(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].reason, SkipReason::Error);
        assert!(rows[0].message.contains("gone"), "{}", rows[0].message);
        assert!(scene.folder().exists());
    }

    #[test]
    fn a_diverged_entry_is_refused() {
        let scene = scene();
        scene.edit_entry(|entry| entry.diverged = true);

        let refusal = scene.classified().err().expect("the piece is refused");

        assert_eq!(refusal.reason, SkipReason::Error);
        assert!(refusal.message.contains("diverged"), "{}", refusal.message);
    }

    #[test]
    fn a_trash_tree_in_the_way_skips_the_reclaim_before_anything_is_written() {
        let scene = scene();
        let before = scene.metadata();
        let trash = scene.collection.trash_path(id(ID));
        // The repairs of the run start empty the trash, so the tree appears
        // after them.
        let seams = At(|at| {
            if at == Step::Walk {
                fs::create_dir_all(&trash).expect("a stale tree");
            }
            Ok(())
        });

        let report = scene.run(&seams);

        assert_eq!(skipped(&report).len(), 1, "{report:?}");
        assert!(
            skipped(&report)[0]
                .message
                .contains("in the way of the trash")
        );
        assert_eq!(scene.metadata(), before);
        assert_eq!(scene.entry().state, EntryState::Live);
    }

    #[test]
    fn an_object_of_another_collection_is_refused() {
        let scene = scene();
        scene.edit_meta(|document| {
            document["collection"] = json!(crate::ops::unearth::tests::OTHER_ID);
        });

        let refusal = scene.classified().err().expect("the piece is refused");

        assert!(
            refusal.message.contains("another collection"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_change_recorded_by_a_crashed_run_and_committed_since_keeps_its_number() {
        let scene = scene();
        scene.crash_before_then_repair(Step::MarkRemoving);
        // A sync commits the pending change as version 2.
        fs::rename(
            scene.object_file("meta.pending"),
            scene.object_file("meta.2"),
        )
        .expect("the change is committed");

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
        assert_eq!(
            scene.entry().version_taken_out,
            VersionTakenOut::Committed(2)
        );
        assert_eq!(reclaim_entries(&scene.metadata()), 1);
    }

    // =========================================================
    // Which archive the folder is compared with
    // =========================================================

    #[test]
    fn the_version_the_piece_was_unearthed_from_is_the_basis() {
        let scene = scene();
        // A listing that cannot be read: only the recorded version is read.
        scene.add_payload(2, &[]);
        fs::remove_file(scene.object_file(&listing_file_for(2))).expect("removed");

        let classified = scene.classified().ok().expect("the piece is classified");

        assert_eq!(classified.basis.version, 1);
        assert!(matches!(classified.verdict, Verdict::Reclaim));
    }

    #[test]
    fn of_two_payloads_of_the_version_the_one_the_folder_equals_is_the_basis() {
        let scene = scene();
        let second = scene.add_payload(1, &scene.listing());
        // The first payload's listing has a file the folder lacks.
        let mut other = scene.listing();
        let mut ghost = other[0].clone();
        ghost.path = path("ghost.txt").clone();
        other.push(ghost);
        let first_listing = listing_file_for(0);
        listing::write(
            other.iter(),
            fs::File::create(scene.object_file(&first_listing)).expect("a file"),
        )
        .expect("written");
        scene.edit_meta(|document| document["payloads"][0]["listing"] = json!(first_listing));
        scene.edit_entry(|entry| entry.local_payloads.clear());

        let classified = scene.classified().ok().expect("the piece is classified");

        assert_eq!(classified.basis.file.as_str(), second);
        assert!(matches!(classified.verdict, Verdict::Reclaim));
    }

    #[test]
    fn a_candidate_whose_listing_cannot_be_read_is_passed_over() {
        let scene = scene();
        let second = scene.add_payload(1, &scene.listing());
        let first_listing = scene.metadata().payloads[0]
            .listing
            .clone()
            .expect("a listing");
        fs::remove_file(scene.object_file(first_listing.as_str())).expect("removed");
        scene.edit_entry(|entry| entry.local_payloads.clear());

        let classified = scene.classified().ok().expect("the piece is classified");

        assert_eq!(classified.basis.file.as_str(), second);
    }

    #[test]
    fn a_superseded_payload_is_no_basis() {
        let scene = scene();
        let newer = scene.add_payload(2, &scene.listing());
        scene.edit_meta(|document| document["payloads"][0]["superseded"] = json!(true));

        let classified = scene.classified().ok().expect("the piece is classified");

        assert_eq!(classified.basis.file.as_str(), newer);
    }

    #[test]
    fn an_entry_without_a_version_is_compared_with_the_newest_archive() {
        let scene = scene();
        let newer = scene.add_payload(2, &scene.listing());
        scene.edit_entry(|entry| entry.unearthed_payload_version = None);

        let classified = scene.classified().ok().expect("the piece is classified");

        assert_eq!(classified.basis.file.as_str(), newer);
    }

    #[test]
    fn a_piece_none_of_whose_listings_can_be_read_is_refused() {
        let scene = scene();
        let listing_name = scene.metadata().payloads[0]
            .listing
            .clone()
            .expect("a listing");
        fs::remove_file(scene.object_file(listing_name.as_str())).expect("removed");

        let refusal = scene.classified().err().expect("the piece is refused");

        assert_eq!(refusal.reason, SkipReason::Error);
        assert!(
            refusal.message.contains("no listing"),
            "{}",
            refusal.message
        );
    }

    // =========================================================
    // Junk and times
    // =========================================================

    #[test]
    fn a_junk_file_in_the_listing_does_not_stop_the_reclaim() {
        let mut archived = FILES.to_vec();
        archived.push((".DS_Store", "junk"));
        let scene = scene_of(&archived, FILES);

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
    }

    #[test]
    fn a_new_junk_file_and_a_new_directory_time_do_not_count() {
        let scene = scene();
        fs::write(scene.folder().join(".DS_Store"), "new junk").expect("a junk file");
        filetime::set_file_mtime(
            scene.folder().join("src"),
            filetime::FileTime::from_unix_time(FILE_TIME + 5_000, 0),
        )
        .expect("the time is set");

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
    }

    // =========================================================
    // The gate
    // =========================================================

    fn damage_payload(scene: &Scene) {
        let file = scene.object_file(&scene.payload_file());
        let mut bytes = fs::read(&file).expect("reads");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(file, bytes).expect("written");
    }

    fn record_remote_check(scene: &Scene) {
        scene.edit_meta(|document| {
            document["payloads"][0]["verified"] = json!("2026-11-03T12:00:00Z");
        });
    }

    fn assert_pending_remote(scene: &Scene, report: &ArchiveReport) {
        let rows = skipped(report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].reason, SkipReason::PendingRemote);
        assert!(scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Live);
        assert_eq!(scene.metadata().lifecycle, Lifecycle::Live);
    }

    #[test]
    fn a_missing_payload_without_a_recorded_check_is_pending_remote() {
        let scene = scene();
        fs::remove_file(scene.object_file(&scene.payload_file())).expect("removed");
        let report = scene.run(&RealSeams);
        assert_pending_remote(&scene, &report);
    }

    #[test]
    fn a_damaged_payload_without_a_recorded_check_is_pending_remote() {
        let scene = scene();
        damage_payload(&scene);
        let report = scene.run(&RealSeams);
        assert_pending_remote(&scene, &report);
    }

    #[test]
    fn a_payload_that_decodes_but_is_not_the_recorded_one_is_pending_remote() {
        let scene = scene();
        let other = zstd::encode_all(&b"another package"[..], 1).expect("encodes");
        fs::write(scene.object_file(&scene.payload_file()), other).expect("written");

        let report = scene.run(&RealSeams);

        assert_pending_remote(&scene, &report);
        assert!(
            skipped(&report)[0]
                .message
                .contains("differs from the metadata")
        );
    }

    #[test]
    fn a_payload_of_the_recorded_hash_but_another_size_is_pending_remote() {
        let scene = scene();
        scene.edit_meta(|document| {
            let size = document["payloads"][0]["size"].as_u64().expect("a size");
            document["payloads"][0]["size"] = json!(size + 1);
        });

        let report = scene.run(&RealSeams);

        assert_pending_remote(&scene, &report);
    }

    #[test]
    fn a_payload_of_the_recorded_size_but_another_hash_is_pending_remote() {
        let scene = scene();
        scene.edit_meta(|document| {
            document["payloads"][0]["checksum"]["value"] = json!("ab".repeat(32));
        });

        let report = scene.run(&RealSeams);

        assert_pending_remote(&scene, &report);
    }

    #[test]
    fn a_damaged_payload_with_a_recorded_check_is_reclaimed_with_a_warning() {
        let scene = scene();
        damage_payload(&scene);
        record_remote_check(&scene);

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.message.contains("remote copy is the good one")),
            "{:?}",
            report.warnings
        );
        assert!(scene.object_file(&scene.payload_file()).exists());
        assert_eq!(scene.entry().local_payloads.len(), 1);
    }

    #[test]
    fn a_missing_payload_with_a_recorded_check_is_reclaimed() {
        let scene = scene();
        fs::remove_file(scene.object_file(&scene.payload_file())).expect("removed");
        record_remote_check(&scene);

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
    }

    // =========================================================
    // Crashes
    // =========================================================

    /// What every crash leaves: one `reclaim` entry at most, and a piece
    /// the next run can reclaim or that is already gone.
    fn finish_with_a_second_run(scene: &Scene) {
        let report = scene.run(&RealSeams);
        assert!(
            skipped(&report).is_empty()
                && report.unfinished.as_deref().is_some_and(<[_]>::is_empty),
            "{report:?}"
        );
        assert!(!scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Gone);
        assert_eq!(reclaim_entries(&scene.metadata()), 1);
    }

    #[test]
    fn a_crash_before_the_metadata_leaves_the_piece_as_it_was() {
        let scene = scene();
        scene.crash_before_then_repair(Step::WriteMetadata);

        assert!(scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Live);
        assert_eq!(scene.metadata().lifecycle, Lifecycle::Live);
        finish_with_a_second_run(&scene);
    }

    #[test]
    fn a_crash_before_removing_keeps_the_metadata_change_and_records_it_once() {
        let scene = scene();
        scene.crash_before_then_repair(Step::MarkRemoving);

        assert!(scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Live);
        assert_eq!(scene.metadata().lifecycle, Lifecycle::Archived);
        assert_eq!(reclaim_entries(&scene.metadata()), 1);
        finish_with_a_second_run(&scene);
    }

    #[test]
    fn a_folder_that_gained_a_file_before_the_repair_stays_unearthed() {
        let scene = scene();
        let seams = At(|at| {
            if at == Step::MoveToTrash {
                panic!("the process dies before the move");
            }
            Ok(())
        });
        assert!(catch_unwind(AssertUnwindSafe(|| scene.run(&seams))).is_err());
        fs::write(scene.folder().join("added.txt"), "new").expect("a file");

        scene.repair();

        assert!(scene.folder().join("added.txt").exists());
        let entry = scene.entry();
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed_payload_version, Some(1));
    }

    #[test]
    fn a_crash_after_the_move_is_finished_by_the_repair() {
        for step in [
            Step::MoveToTrash,
            Step::Rewalk,
            Step::DeleteInTrash,
            Step::MarkGone,
        ] {
            let scene = scene();
            scene.crash_before_then_repair(step);

            assert!(!scene.folder().exists(), "{step:?}");
            assert_eq!(scene.entry().state, EntryState::Gone, "{step:?}");
            assert_eq!(reclaim_entries(&scene.metadata()), 1, "{step:?}");
        }
    }

    #[test]
    fn a_refused_move_deletes_an_unchanged_folder_in_place() {
        let scene = scene();
        let seams = At(|at| match at {
            Step::MoveToTrash => Err(io::Error::from_raw_os_error(libc::EXDEV)),
            _ => Ok(()),
        });

        let report = scene.run(&seams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
        assert!(!scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Gone);
    }

    #[test]
    fn a_failed_rewalk_keeps_the_folder_as_unearthed() {
        let scene = scene();
        let seams = At(|at| match at {
            Step::Rewalk => Err(io::Error::other("the walk fails")),
            _ => Ok(()),
        });

        let report = scene.run(&seams);

        let kept = report.kept.as_deref().expect("rows");
        assert_eq!(kept.len(), 1, "{report:?}");
        assert!(scene.folder().exists());
        let entry = scene.entry();
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed_payload_version, Some(1));
    }

    // =========================================================
    // The dry run
    // =========================================================

    #[test]
    fn a_dry_run_lists_the_reclaim_and_writes_nothing() {
        let scene = scene();
        let before = (scene.entry(), scene.metadata());

        let plan =
            plan_with(&scene.collection, &scene.clock, &RealSeams).expect("the plan is made");

        let actions = plan.plan.expect("a dry run has a plan").actions;
        assert_eq!(
            actions,
            [ArchiveAction::Reclaim {
                path: path(PIECE),
                id: id(ID),
                size: 17,
            }]
        );
        assert_eq!(
            crate::ops::archive::tests::parts(&actions[0]),
            (PIECE.to_string(), Some(id(ID)))
        );
        assert_eq!((scene.entry(), scene.metadata()), before);
        assert!(scene.folder().exists());
    }

    #[test]
    fn a_dry_run_warns_of_a_piece_the_run_would_skip() {
        let scene = scene();
        fs::remove_file(scene.object_file(&scene.payload_file())).expect("removed");

        let plan =
            plan_with(&scene.collection, &scene.clock, &RealSeams).expect("the plan is made");

        assert!(plan.plan.expect("a plan").actions.is_empty());
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.message.contains("would be skipped")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_dry_run_warns_of_a_changed_piece() {
        let scene = scene_of(FILES, &[("notes.txt", "edited"), FILES[1]]);

        let plan =
            plan_with(&scene.collection, &scene.clock, &RealSeams).expect("the plan is made");

        assert!(plan.plan.expect("a plan").actions.is_empty());
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.message.contains("differs from the archive")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_dry_run_warns_when_the_remote_copy_carries_the_gate() {
        let scene = scene();
        damage_payload(&scene);
        record_remote_check(&scene);

        let plan =
            plan_with(&scene.collection, &scene.clock, &RealSeams).expect("the plan is made");

        assert_eq!(plan.plan.expect("a plan").actions.len(), 1);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.message.contains("remote copy is the good one"))
        );
    }
}
