// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archiving unearthed pieces.
//!
//! An unearthed piece is compared with the archive it was unearthed from.
//! One that equals it is reclaimed: its folder is removed the way `archive`
//! removes any folder, and no new archive is made (spec "archive",
//! ADR 0035). One that differs gets a new archive, asking first when the
//! new archive would lose content (ADR 0036). A reclaim's steps leave a
//! state the start-of-run repairs finish:
//!
//! 1. record the metadata change: lifecycle `archived`, live payload
//!    removed, a `reclaim` history entry;
//! 2. set the entry to `removing`, naming the archive payload;
//! 3. seal the durable archive from the comparison that found no
//!    difference;
//! 4. remove the folder (see `finish`).

use super::apparent_size;
use super::durable::{self, Matched};
use super::new::finish::Finished;
use super::new::{Flow, Piece, PieceOutcome, Stop, chain};
use super::{ArchiveAction, Candidate, Forked, Lost, NeedsDecision, Reclaimed, SkipReason, Step};
use crate::clock::Clock;
use crate::clock::whole_seconds;
use crate::collection::Collection;
use crate::fsx::tree::TreeRoot;
use crate::id::IdSource;
use crate::id::ObjectId;
use crate::interact::{Choice, Question};
use crate::listing::{self, ListingEntry};
use crate::lock::RunLock;
use crate::meta::{HistoryAction, HistoryEntry, Lifecycle, Metadata, Payload, PayloadKind};
use crate::ops::fork::{self, ForkError, ForkReport, ForkRequest, RealSteps};
use crate::pack::WalkRules;
use crate::pack::live::check_stream;
use crate::pack::verify::{Compared, Difference, changes, compare_with_listing};
use crate::relpath::{Name, RelPath};
use crate::repair::{junk_outside, unearthed_candidates};
use crate::report::Warning;
use crate::store::{Source, Store};
use crate::tracking::{self, Answer, DecisionKind, EntryState, TrackingEntry, VersionTakenOut};
use crate::tree::PieceTree;
use jiff::Timestamp;
use std::fs::File;
use std::sync::PoisonError;

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
    /// The folder differs and a new archive may replace the old one without
    /// asking: nothing is lost, or the collection allows replacing.
    Replace,
    /// The folder differs and a new archive would lose content.
    Ask,
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
    /// What a new archive would lose, in path order.
    pub(super) loss: Vec<Difference>,
}

/// Whether a replacement loses the entry that `difference` names: a path
/// that was removed or an entry whose type, size, content, mode, link
/// target or (for a file) modification time changed. A path that was added
/// loses nothing, and the comparison leaves out what does not count: the
/// modification time of a directory, the mode of a symlink and junk.
fn loses(difference: &Difference) -> bool {
    matches!(
        difference,
        Difference::Removed(_) | Difference::Changed { .. }
    )
}

fn loss_of(differences: &[Difference]) -> Vec<Difference> {
    differences.iter().filter(|d| loses(d)).cloned().collect()
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

    let candidates = compared_with(&entry, &metadata);
    let rules = WalkRules {
        junk: junk.to_vec(),
        staged: false,
    };
    let mut unreadable = None;
    let mut found: Option<(Payload, Vec<ListingEntry>, Compared, Vec<Difference>)> = None;
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
        let loss = loss_of(compared.differences());
        // The archive the folder equals is the one it came from. Of
        // several that it differs from, the one it differs from least is
        // the one that shows the user the smallest loss.
        let closer = found
            .as_ref()
            .is_none_or(|(_, _, _, shown)| loss.len() < shown.len());
        if closer || equal {
            found = Some((payload.clone(), listing, compared, loss));
        }
        if equal {
            break;
        }
    }
    let Some((basis, listing, compared, loss)) = found else {
        return Err(Refusal::error(format!(
            "no listing of the archive can be read: {}",
            unreadable.unwrap_or_else(|| "the object has no archive".to_string())
        )));
    };
    stable(compared.differences())?;
    let verdict = if compared.differences().is_empty() {
        Verdict::Reclaim
    } else if loss.is_empty() || collection.config().collection.replace_without_asking {
        Verdict::Replace
    } else {
        Verdict::Ask
    };
    Ok(Classified {
        entry,
        metadata,
        source: current.source,
        basis,
        listing,
        compared,
        verdict,
        loss,
    })
}

/// A file that was replaced, resized or touched while the comparison read
/// it says nothing reliable about the folder.
fn stable(differences: &[Difference]) -> Result<(), Refusal> {
    match differences
        .iter()
        .find(|difference| matches!(difference, Difference::ChangedWhileRead(_)))
    {
        Some(unstable) => Err(Refusal::error(format!(
            "`{}` changed while it was read",
            unstable.path()
        ))),
        None => Ok(()),
    }
}

/// The archives an unearthed folder is compared with.
fn compared_with<'a>(entry: &TrackingEntry, metadata: &'a Metadata) -> Vec<&'a Payload> {
    let candidates = unearthed_candidates(entry, metadata);
    if candidates.is_empty() {
        newest_archive(metadata).into_iter().collect()
    } else {
        candidates
    }
}

/// The junk names for the whole handling of the unearthed piece `id`: the
/// configured ones less the names any archive the folder is compared with
/// holds as a file. A name that became junk after packing must not hide an
/// edit, a deletion or the content of a file the archive has, and the
/// comparison, the packing, the strict verify and the removal must all
/// agree on the list. A piece whose entry, metadata or listings cannot be
/// read gets the configured list; `classify` refuses such a piece.
pub(super) fn junk_of_piece(collection: &Collection, id: ObjectId) -> Vec<Name> {
    let store = Store::new(collection);
    let listings: Vec<Vec<ListingEntry>> = match (tracking::read(collection, id), store.current(id))
    {
        (Ok(Some(entry)), Ok(Some(current))) => compared_with(&entry, &current.metadata)
            .into_iter()
            .filter_map(|payload| read_listing(&store, id, payload).ok())
            .collect(),
        _ => Vec::new(),
    };
    junk_outside(collection, listings.iter().map(Vec::as_slice))
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
    let (path, size) = (candidate.path.clone(), size);
    Ok(match classified.verdict {
        Verdict::Reclaim => {
            let warning = gate(collection, &classified.entry, &classified.basis)?;
            (
                ArchiveAction::Reclaim { path, id, size },
                warning.map(Warning::new),
            )
        }
        Verdict::Replace => (ArchiveAction::Replace { path, id, size }, None),
        Verdict::Ask => (
            ArchiveAction::Ask {
                path,
                id,
                size,
                lost_count: classified.loss.len(),
            },
            None,
        ),
    })
}

// =========================================================
// Reclaiming
// =========================================================

/// Forks the unearthed piece `id`: its folder stays where it is and gets
/// a new ID, and the archive keeps the old one.
pub(crate) fn fork_unearthed(
    collection: &Collection,
    lock: &RunLock,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    id: ObjectId,
) -> Result<ForkReport, ForkError> {
    let planned = fork::plan(collection, &ForkRequest { id, name: None })?;
    fork::apply_with(
        collection,
        &planned,
        lock,
        clock,
        ids,
        &RealSteps,
        Vec::new(),
    )
}

/// How many times the folder may change between the question and the
/// package before the piece is left for the next run.
const MAX_CHANGES: usize = 3;

/// What the loss of a replacement may be for it to go ahead.
enum Bound {
    /// Nothing: the replacement was not asked about.
    Nothing,
    /// Anything: the collection replaces without asking.
    Any,
    /// What the user was shown.
    Within(Vec<Difference>),
}

impl Bound {
    fn allows(&self, found: &[Difference]) -> bool {
        match self {
            Bound::Nothing => found.is_empty(),
            Bound::Any => true,
            Bound::Within(shown) => found
                .iter()
                .all(|difference| shown.iter().any(|seen| covers(seen, difference))),
        }
    }
}

/// Whether the user, shown `seen`, was told of everything `found` loses.
fn covers(seen: &Difference, found: &Difference) -> bool {
    match (seen, found) {
        (Difference::Removed(shown), Difference::Removed(now)) => shown == now,
        (
            Difference::Changed {
                path: shown,
                fields: shown_fields,
            },
            Difference::Changed { path, fields },
        ) => shown == path && fields.iter().all(|field| shown_fields.contains(field)),
        _ => false,
    }
}

/// What came of packing the folder.
enum Replaced {
    Done(PieceOutcome),
    /// The folder lost more than was allowed when it was packed.
    Outside,
}

/// The answers a question about a replacement offers, with what the user
/// reads. `skip` is the one a question defaults to.
const CHOICES: [(Answer, &str, &str); 3] = [
    (
        Answer::Replace,
        "replace",
        "replace: archive the folder as it is now",
    ),
    (
        Answer::Fork,
        "fork",
        "fork: keep the folder as a new piece with a new ID",
    ),
    (
        Answer::Skip,
        "skip",
        "skip: leave the folder and its archive as they are",
    ),
];
const SKIP: usize = 2;

/// At most this many lost entries are listed in a question or a report.
const LISTED: usize = 10;

/// One line of a question for an entry in the loss: a removed path, or a
/// changed one with the fields that differ.
fn describe_loss(difference: &Difference) -> String {
    let path = difference.path();
    match difference {
        Difference::Changed { fields, .. } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|field| format!("{field:?}").to_lowercase())
                .collect();
            format!("changed ({}): {path}", fields.join(", "))
        }
        _ => format!("removed: {path}"),
    }
}

fn is_pending(entry: &TrackingEntry) -> bool {
    entry
        .pending_decisions
        .iter()
        .any(|decision| decision.kind == DecisionKind::ReplacingWithLoss)
}

fn question(path: &RelPath, loss: &[Difference]) -> Question {
    let mut details: Vec<String> = loss.iter().take(LISTED).map(describe_loss).collect();
    if loss.len() > LISTED {
        details.push(format!("and {} more", loss.len() - LISTED));
    }
    Question {
        title: format!("Replace the archive of {path}?"),
        details,
        choices: CHOICES
            .iter()
            .map(|(_, id, label)| Choice::new(*id, *label))
            .collect(),
        default: SKIP,
    }
}

fn lost_rows(loss: &[Difference]) -> Vec<Lost> {
    loss.iter()
        .take(LISTED)
        .map(|difference| match difference {
            Difference::Removed(path) => Lost {
                path: path.clone(),
                change: "removed",
            },
            other => Lost {
                path: other.path().clone(),
                change: "changed",
            },
        })
        .collect()
}

impl Piece<'_, '_> {
    /// An unearthed piece: reclaimed when it equals its archive, otherwise
    /// archived again. `tree` is the walk that found it idle.
    pub(super) fn rearchive(
        &mut self,
        mut tree: PieceTree,
        sub: &TreeRoot,
        mut size: u64,
    ) -> Flow<PieceOutcome> {
        let id = self.known.expect("an unearthed piece has an ID");
        let cx = self.cx;
        let mut outside = 0;
        loop {
            let classified = classify(cx.collection, &self.junk, sub, id)?;
            let bound = match classified.verdict {
                Verdict::Reclaim => return self.reclaim(classified, size),
                Verdict::Replace if classified.loss.is_empty() => Bound::Nothing,
                Verdict::Replace => Bound::Any,
                Verdict::Ask => match self.answer.or_else(|| self.ask(&classified)) {
                    Some(Answer::Replace) => {
                        // The folder may have changed while the user
                        // thought, and the package is made from a walk.
                        tree = self.walk_and_judge()?;
                        size = apparent_size(&tree);
                        Bound::Within(classified.loss.clone())
                    }
                    Some(Answer::Fork) => return self.fork(),
                    Some(_) => return self.decline(classified),
                    None => return self.leave_undecided(classified),
                },
            };
            match self.replace(&classified, &tree, sub, size, &bound)? {
                Replaced::Done(outcome) => return Ok(outcome),
                Replaced::Outside => {
                    outside += 1;
                    if outside == MAX_CHANGES {
                        return Err(Stop::Skip {
                            reason: SkipReason::Differs,
                            message: "the folder kept changing while it was packed".to_string(),
                        });
                    }
                    tree = self.walk_and_judge()?;
                    size = apparent_size(&tree);
                }
            }
        }
    }

    /// Asks what to do with the loss; `None` when nobody answers.
    fn ask(&self, classified: &Classified) -> Option<Answer> {
        let question = question(self.path, &classified.loss);
        let choice = self
            .cx
            .interaction
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .choose(&question)?;
        CHOICES
            .iter()
            .find(|(_, id, _)| *id == choice.id)
            .map(|(answer, _, _)| *answer)
    }

    /// Packs the folder as the next archive of the piece, when what it
    /// loses against `classified.basis` stays within `bound`.
    fn replace(
        &mut self,
        classified: &Classified,
        tree: &PieceTree,
        sub: &TreeRoot,
        size: u64,
        bound: &Bound,
    ) -> Flow<Replaced> {
        let placed = self.pack_and_place(tree, sub, size)?;
        // The folder may have changed since it was compared, so what the
        // package loses is worked out from the package.
        let found = loss_of(&changes(&classified.listing, placed.listing(), &self.junk));
        if !bound.allows(&found) {
            placed.discard();
            return Ok(Replaced::Outside);
        }
        let stored = self.record_placed(placed)?;
        Ok(Replaced::Done(self.remove_stored(stored, size)?))
    }

    /// The answer was to fork: the folder stays and gets a new ID.
    fn fork(&mut self) -> Flow<PieceOutcome> {
        let cx = self.cx;
        let id = self.known.expect("an unearthed piece has an ID");
        let report = fork_unearthed(cx.collection, cx.lock, cx.clock, cx.ids, id)
            .map_err(|error| Stop::error(chain(&error)))?;
        self.warnings.extend(report.warnings);
        Ok(PieceOutcome::Forked(Forked {
            id: report.id,
            from: report.from,
            path: report.path,
        }))
    }

    /// The answer was to skip: nothing stays pending.
    fn decline(&self, classified: Classified) -> Flow<PieceOutcome> {
        let mut entry = classified.entry;
        if is_pending(&entry) {
            entry.resolve_decision(DecisionKind::ReplacingWithLoss);
            self.write_entry_of(&mut entry)?;
        }
        Err(Stop::Skip {
            reason: SkipReason::Declined,
            message: "the archive stays as it is, the folder is kept".to_string(),
        })
    }

    /// Nobody could answer: the decision is left for `resolve`.
    fn leave_undecided(&self, classified: Classified) -> Flow<PieceOutcome> {
        let mut entry = classified.entry;
        if !is_pending(&entry) {
            entry.decide(DecisionKind::ReplacingWithLoss);
            self.write_entry_of(&mut entry)?;
        }
        Ok(PieceOutcome::NeedsDecision(NeedsDecision {
            id: entry.id,
            path: self.path.clone(),
            decision: DecisionKind::ReplacingWithLoss,
            options: DecisionKind::ReplacingWithLoss.options().to_vec(),
            lost_count: classified.loss.len(),
            lost: lost_rows(&classified.loss),
        }))
    }

    fn write_entry_of(&self, entry: &mut TrackingEntry) -> Flow<()> {
        let cx = self.cx;
        tracking::write(cx.collection, cx.lock, cx.clock, entry)
            .map_err(|error| Stop::from_error(&error))
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
        let durable = durable::seal(cx.lock, matched, &basis, &entry, &self.junk)
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
pub(crate) mod tests {
    use super::*;
    use crate::clock::{Clock, FixedClock};
    use crate::id::{IdSource, SeqIds};
    use crate::interact::{Choice, Interaction, Question, ScriptedInteraction};
    use crate::meta::PayloadKind;
    use crate::ops::archive::{
        ArchiveAction, ArchiveReport, ArchiveRequest, Asker, RealSeams, Seams, archive_with,
        plan_with,
    };
    use crate::ops::archive::{Forked, Lost, NeedsDecision};
    use crate::ops::status;
    use crate::ops::unearth::tests::{FILES, Fixture, ID, id, path, time};
    use crate::pack::verify::Field;
    use crate::relpath::RelPath;
    use crate::testing::tree::TempCollection;
    use crate::testing::{rerun_while_lock_unattributed, shared_pool};
    use crate::tracking::{Answer, PendingDecision};
    use serde_json::{Value, json};
    use std::fs;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) const PIECE: &str = "projects/work";
    /// The modification time `Fixture::archived` gives the files it packs.
    pub(crate) const FILE_TIME: i64 = 1_000_000_000;

    /// An unearthed piece at [`PIECE`] that was unearthed 30 days before
    /// the clock, with nobody holding the run lock.
    pub(crate) struct Scene {
        pub(crate) temp: TempCollection,
        pub(crate) collection: Collection,
        pub(crate) clock: FixedClock,
    }

    /// A piece archived with `archived` and unearthed again with
    /// `restored`; the same files make the folder equal its archive.
    pub(crate) fn scene_of(archived: &[(&str, &str)], restored: &[(&str, &str)]) -> Scene {
        scene_built(|_| {}, archived, &|_| {}, restored)
    }

    /// [`scene_of`] with more in the piece than plain files: `prepare`
    /// adds it to the archived piece and again to the restored folder. The
    /// collection is configured by `configure`.
    pub(crate) fn scene_built(
        configure: impl FnOnce(&mut crate::config::Config),
        archived: &[(&str, &str)],
        prepare: &dyn Fn(&mut crate::testing::tree::Tree),
        restored: &[(&str, &str)],
    ) -> Scene {
        let mut fx = Fixture::configured(configure);
        fx.archived_with(ID, PIECE, archived, |tree| prepare(tree));
        for (name, content) in restored {
            fx.tree()
                .file_with(&format!("{PIECE}/{name}"), content, 0o644, FILE_TIME);
        }
        prepare(fx.tree());
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

    pub(crate) fn scene() -> Scene {
        scene_of(FILES, FILES)
    }

    /// Seams that call `at` before each step.
    pub(crate) struct At<F>(pub(crate) F);

    impl<F: Fn(Step) -> io::Result<()> + Sync> Seams for At<F> {
        fn at(&self, step: Step, _: &RelPath) -> io::Result<()> {
            (self.0)(step)
        }
    }

    impl Scene {
        pub(crate) fn run(&self, seams: &dyn Seams) -> ArchiveReport {
            self.run_asking(seams, crate::ops::archive::nobody())
        }

        pub(crate) fn run_asking(&self, seams: &dyn Seams, asker: &Asker) -> ArchiveReport {
            self.run_at(seams, asker, &self.clock)
        }

        pub(crate) fn run_at(
            &self,
            seams: &dyn Seams,
            asker: &Asker,
            clock: &FixedClock,
        ) -> ArchiveReport {
            let ids = SeqIds::new(1);
            rerun_while_lock_unattributed(|| {
                archive_with(
                    &ArchiveRequest {
                        collection: &self.collection,
                        clock,
                        ids: &ids,
                        pool: shared_pool(),
                        interaction: asker,
                        workers: 1,
                        dry_run: false,
                    },
                    seams,
                )
            })
            .expect("the run finishes")
        }

        /// A run that dies before `step`, as a process that is killed
        /// there, followed by the repairs of the next run.
        pub(crate) fn crash_before_then_repair(&self, step: Step) {
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

        pub(crate) fn repair(&self) {
            drop(
                rerun_while_lock_unattributed(|| {
                    crate::run::start(&self.collection, &self.clock, false)
                })
                .expect("the repairs run"),
            );
        }

        pub(crate) fn entry(&self) -> TrackingEntry {
            tracking::read(&self.collection, id(ID))
                .expect("the entry reads")
                .expect("the entry exists")
        }

        pub(crate) fn metadata(&self) -> Metadata {
            Store::new(&self.collection)
                .current(id(ID))
                .expect("the metadata reads")
                .expect("the object has metadata")
                .metadata
        }

        pub(crate) fn folder(&self) -> std::path::PathBuf {
            self.temp.root().join(PIECE)
        }

        pub(crate) fn object_file(&self, name: &str) -> std::path::PathBuf {
            self.collection.object_dir(id(ID)).join(name)
        }

        /// The first payload's file name.
        pub(crate) fn payload_file(&self) -> String {
            self.metadata().payloads[0].file.as_str().to_string()
        }

        pub(crate) fn edit_meta(&self, edit: impl FnOnce(&mut Value)) {
            let path = self.object_file("meta.1");
            let mut document: Value =
                serde_json::from_slice(&fs::read(&path).expect("reads")).expect("json");
            edit(&mut document);
            fs::write(&path, serde_json::to_vec(&document).expect("json")).expect("written");
        }

        pub(crate) fn edit_entry(&self, edit: impl FnOnce(&mut TrackingEntry)) {
            let mut entry = self.entry();
            edit(&mut entry);
            let path = self.collection.tracking_path(id(ID));
            fs::write(path, serde_json::to_vec(&entry).expect("json")).expect("written");
        }

        /// A further archive payload of `version` whose listing is `lines`.
        /// Its file holds the bytes of the first payload.
        pub(crate) fn add_payload(&self, version: u32, lines: &[ListingEntry]) -> String {
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
        pub(crate) fn listing(&self) -> Vec<ListingEntry> {
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

    pub(crate) fn reclaimed(report: &ArchiveReport) -> usize {
        report
            .reclaimed
            .as_deref()
            .expect("a real run lists them")
            .len()
    }

    pub(crate) fn skipped(report: &ArchiveReport) -> &[crate::ops::archive::Skipped] {
        report.skipped.as_deref().expect("a real run lists them")
    }

    pub(crate) fn reclaim_entries(metadata: &Metadata) -> usize {
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

    /// The files of [`FILES`] and a `.DS_Store` that is junk by the
    /// configured list but was packed as a plain file.
    fn files_with_junk_name() -> Vec<(&'static str, &'static str)> {
        let mut files = FILES.to_vec();
        files.push((".DS_Store", "junk"));
        files
    }

    #[test]
    fn an_unchanged_file_with_a_junk_name_the_archive_holds_is_reclaimed() {
        let files = files_with_junk_name();
        let scene = scene_of(&files, &files);

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
        assert!(!scene.folder().exists());
    }

    #[test]
    fn an_edited_file_with_a_junk_name_the_archive_holds_is_a_loss() {
        let files = files_with_junk_name();
        let scene = scene_of(&files, &files);
        scene.write_in_folder(".DS_Store", "edited");

        nobody_answers(&scene, &[lost(".DS_Store", "changed")]);
    }

    #[test]
    fn a_deleted_file_with_a_junk_name_the_archive_holds_is_a_loss() {
        let files = files_with_junk_name();
        let scene = scene_of(&files, &files);
        fs::remove_file(scene.folder().join(".DS_Store")).expect("removed");

        nobody_answers(&scene, &[lost(".DS_Store", "removed")]);
    }

    #[test]
    fn a_replacement_packs_the_file_with_a_junk_name_the_archive_holds() {
        let files = files_with_junk_name();
        let scene = scene_of(&files, &files);
        scene.write_in_folder("added.txt", "new");

        let report = scene.run(&RealSeams);

        assert_eq!(archived_rows(&report).len(), 1, "{report:?}");
        let metadata = scene.metadata();
        let name = metadata.payloads[1].listing.clone().expect("a listing");
        let packed = listing::read_all(
            fs::File::open(scene.object_file(name.as_str())).expect("the listing opens"),
        )
        .expect("the listing reads");
        let paths: Vec<String> = packed.iter().map(|line| line.path.to_string()).collect();
        assert!(
            paths.contains(&".DS_Store".to_string()),
            "the new package keeps the file: {paths:?}"
        );
    }

    #[test]
    fn a_piece_without_an_entry_gets_the_configured_junk_list() {
        let scene = scene();

        let junk = junk_of_piece(&scene.collection, id("01J0000000000000000000000Z"));

        assert_eq!(
            junk,
            status::junk_names(&scene.collection.config().collection.junk, &mut Vec::new())
        );
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

    pub(crate) fn damage_payload(scene: &Scene) {
        let file = scene.object_file(&scene.payload_file());
        let mut bytes = fs::read(&file).expect("reads");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(file, bytes).expect("written");
    }

    pub(crate) fn record_remote_check(scene: &Scene) {
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

    // =========================================================
    // Replacing and asking
    // =========================================================

    pub(crate) const TITLE: &str = "Replace the archive of projects/work?";

    pub(crate) fn archived_rows(report: &ArchiveReport) -> &[crate::ops::archive::Archived] {
        report.archived.as_deref().expect("a real run lists them")
    }

    pub(crate) fn undecided(report: &ArchiveReport) -> &[NeedsDecision] {
        report
            .needs_decision
            .as_deref()
            .expect("a real run lists them")
    }

    pub(crate) fn forked(report: &ArchiveReport) -> &[Forked] {
        report.forked.as_deref().expect("a real run lists them")
    }

    pub(crate) fn lost(path: &str, change: &'static str) -> Lost {
        Lost {
            path: self::path(path),
            change,
        }
    }

    pub(crate) fn history_actions(metadata: &Metadata) -> Vec<String> {
        metadata
            .history
            .iter()
            .map(|entry| entry.action.clone())
            .collect()
    }

    pub(crate) fn pending(entry: &TrackingEntry) -> Vec<PendingDecision> {
        entry.pending_decisions.clone()
    }

    pub(crate) fn replacing() -> Vec<PendingDecision> {
        vec![PendingDecision {
            kind: DecisionKind::ReplacingWithLoss,
        }]
    }

    pub(crate) fn asker(interaction: impl Interaction + 'static) -> Asker {
        Mutex::new(Box::new(interaction))
    }

    /// Answers by the position of the question, records every question,
    /// and runs `before` with the position first so a test can change the
    /// folder while the user thinks.
    pub(crate) struct Hook {
        pub(crate) asked: Arc<Mutex<Vec<Question>>>,
        answers: Vec<Option<&'static str>>,
        before: Box<dyn FnMut(usize) + Send>,
    }

    impl Hook {
        fn new(
            answers: Vec<Option<&'static str>>,
            before: impl FnMut(usize) + Send + 'static,
        ) -> (Hook, Arc<Mutex<Vec<Question>>>) {
            let asked = Arc::new(Mutex::new(Vec::new()));
            let hook = Hook {
                asked: Arc::clone(&asked),
                answers,
                before: Box::new(before),
            };
            (hook, asked)
        }
    }

    impl Interaction for Hook {
        fn choose(&mut self, question: &Question) -> Option<Choice> {
            let position = {
                let mut asked = self.asked.lock().expect("the list is not poisoned");
                asked.push(question.clone());
                asked.len() - 1
            };
            (self.before)(position);
            let answer = self.answers.get(position).copied().flatten()?;
            question
                .choices
                .iter()
                .find(|choice| choice.id == answer)
                .cloned()
        }
    }

    /// Puts the old time back on the folder and its `src` directory, which
    /// removing a file below them has touched: a piece with a recent
    /// directory time is not idle.
    pub(crate) fn settle(folder: &std::path::Path) {
        for directory in [folder.to_path_buf(), folder.join("src")] {
            if directory.exists() {
                filetime::set_file_mtime(
                    directory,
                    filetime::FileTime::from_unix_time(FILE_TIME, 0),
                )
                .expect("the time is set");
            }
        }
    }

    pub(crate) fn replace_without_asking(config: &mut crate::config::Config) {
        config.collection.replace_without_asking = true;
    }

    impl Scene {
        /// Writes a file with an old modification time, so the piece stays
        /// idle.
        pub(crate) fn write_in_folder(&self, relative: &str, content: &str) {
            fs::write(self.folder().join(relative), content).expect("a file is written");
            self.set_time(relative, FILE_TIME);
        }

        pub(crate) fn set_time(&self, relative: &str, seconds: i64) {
            filetime::set_file_mtime(
                self.folder().join(relative),
                filetime::FileTime::from_unix_time(seconds, 0),
            )
            .expect("the time is set");
        }

        pub(crate) fn set_mode(&self, relative: &str, mode: u32) {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                self.folder().join(relative),
                std::fs::Permissions::from_mode(mode),
            )
            .expect("the mode is set");
        }

        /// The state of the piece as a run that leaves it alone keeps it.
        pub(crate) fn untouched(&self) -> (Metadata, bool) {
            (self.metadata(), self.folder().exists())
        }
    }

    #[test]
    fn a_file_added_to_an_unearthed_piece_is_archived_without_asking() {
        let scene = scene();
        scene.write_in_folder("added.txt", "new");

        let report = scene.run(&RealSeams);

        let rows = archived_rows(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!((rows[0].id, rows[0].payload_version), (id(ID), 2));
        assert_eq!(reclaimed(&report), 0);
        assert!(undecided(&report).is_empty());
        assert!(!scene.folder().exists());
        assert_eq!(scene.entry().state, EntryState::Gone);
        let metadata = scene.metadata();
        let archives: Vec<_> = metadata
            .payloads
            .iter()
            .filter(|payload| payload.kind == PayloadKind::Archive)
            .map(|payload| (payload.version, payload.superseded))
            .collect();
        assert_eq!(archives, [(1, false), (2, false)], "the old payload stays");
        assert_eq!(
            history_actions(&metadata).last().map(String::as_str),
            Some("archive")
        );
        assert!(!report.partial_for_test());
    }

    /// The report of a run that asked nobody about the loss `change` made.
    fn nobody_answers(scene: &Scene, lost_rows: &[Lost]) {
        let before = scene.untouched();
        let report = scene.run(&RealSeams);

        let rows = undecided(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(
            (rows[0].id, rows[0].path.to_string()),
            (id(ID), PIECE.into())
        );
        assert_eq!(rows[0].decision, DecisionKind::ReplacingWithLoss);
        assert_eq!(
            rows[0].options,
            [Answer::Replace, Answer::Fork, Answer::Skip]
        );
        assert_eq!(rows[0].lost_count, lost_rows.len());
        assert_eq!(rows[0].lost, lost_rows);
        assert!(archived_rows(&report).is_empty());
        assert!(skipped(&report).is_empty(), "{report:?}");
        assert!(report.partial_for_test());
        assert_eq!(scene.untouched(), before, "nothing but the entry changed");
        assert_eq!(pending(&scene.entry()), replacing());
        assert_eq!(scene.entry().state, EntryState::Live);
    }

    #[test]
    fn a_removed_file_is_a_loss_nobody_answered() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        nobody_answers(&scene, &[lost("notes.txt", "removed")]);
    }

    #[test]
    fn changed_content_is_a_loss() {
        let scene = scene();
        scene.write_in_folder("notes.txt", "HELLO");
        scene.set_time("notes.txt", FILE_TIME);
        nobody_answers(&scene, &[lost("notes.txt", "changed")]);
    }

    #[test]
    fn a_changed_mode_is_a_loss() {
        let scene = scene();
        scene.set_mode("notes.txt", 0o600);
        nobody_answers(&scene, &[lost("notes.txt", "changed")]);
    }

    #[test]
    fn a_changed_directory_mode_is_a_loss() {
        let scene = scene();
        scene.set_mode("src", 0o700);
        nobody_answers(&scene, &[lost("src", "changed")]);
    }

    #[test]
    fn a_changed_modification_time_of_a_file_is_a_loss() {
        let scene = scene();
        scene.set_time("notes.txt", FILE_TIME + 5);
        nobody_answers(&scene, &[lost("notes.txt", "changed")]);
    }

    #[test]
    fn a_changed_link_target_is_a_loss() {
        let scene = scene_built(
            |_| {},
            FILES,
            &|tree| {
                tree.symlink("projects/work/link", "notes.txt");
            },
            FILES,
        );
        fs::remove_file(scene.folder().join("link")).expect("removed");
        std::os::unix::fs::symlink("src/main.rs", scene.folder().join("link")).expect("a link");
        nobody_answers(&scene, &[lost("link", "changed")]);
    }

    #[test]
    fn a_changed_directory_time_and_a_new_junk_file_lose_nothing() {
        let scene = scene();
        scene.set_time("src", FILE_TIME + 500);
        scene.write_in_folder(".DS_Store", "junk");

        let report = scene.run(&RealSeams);

        assert!(undecided(&report).is_empty(), "{report:?}");
        assert_eq!(archived_rows(&report).len() + reclaimed(&report), 1);
    }

    #[test]
    fn a_collection_that_replaces_without_asking_replaces_a_loss() {
        let scene = scene_built(replace_without_asking, FILES, &|_| {}, FILES);
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");

        let report = scene.run(&RealSeams);

        assert_eq!(archived_rows(&report).len(), 1, "{report:?}");
        assert!(undecided(&report).is_empty());
        assert!(!scene.folder().exists());
        assert_eq!(scene.metadata().payloads.len(), 2);
    }

    #[test]
    fn the_question_names_the_piece_and_what_would_be_lost() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        scene.set_mode("src", 0o700);
        let (hook, asked) = Hook::new(vec![Some("skip")], |_| {});

        scene.run_asking(&RealSeams, &asker(hook));

        let asked = asked.lock().expect("the list");
        assert_eq!(asked.len(), 1);
        let question = &asked[0];
        assert_eq!(question.title, TITLE);
        assert_eq!(
            question.details,
            [
                "removed: notes.txt".to_string(),
                "changed (mode): src".to_string()
            ]
        );
        let ids: Vec<&str> = question.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["replace", "fork", "skip"]);
        assert_eq!(question.default_choice().id, "skip");
    }

    #[test]
    fn a_question_lists_ten_losses_and_counts_the_rest() {
        let files: Vec<(String, String)> = (0..13)
            .map(|number| (format!("f{number:02}.txt"), "x".to_string()))
            .collect();
        let borrowed: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, content)| (name.as_str(), content.as_str()))
            .collect();
        let scene = scene_of(&borrowed, &borrowed);
        for (name, _) in &files {
            fs::remove_file(scene.folder().join(name)).expect("removed");
        }
        scene.write_in_folder("keep.txt", "k");
        let (hook, asked) = Hook::new(vec![None], |_| {});

        let report = scene.run_asking(&RealSeams, &asker(hook));

        let details = asked.lock().expect("the list")[0].details.clone();
        assert_eq!(details.len(), 11, "{details:?}");
        assert_eq!(details[0], "removed: f00.txt");
        assert_eq!(details[10], "and 3 more");
        let row = &undecided(&report)[0];
        assert_eq!((row.lost_count, row.lost.len()), (13, 10));
    }

    #[test]
    fn replace_archives_the_piece_with_the_loss() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "replace");

        let report = scene.run_asking(&RealSeams, &asker(script));

        assert_eq!(archived_rows(&report).len(), 1, "{report:?}");
        assert_eq!(archived_rows(&report)[0].payload_version, 2);
        assert!(!scene.folder().exists());
        assert!(undecided(&report).is_empty());
        assert!(pending(&scene.entry()).is_empty());
        assert!(!report.partial_for_test());
    }

    #[test]
    fn fork_gives_the_folder_a_new_id() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "fork");

        let report = scene.run_asking(&RealSeams, &asker(script));

        let rows = forked(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(
            (rows[0].from, rows[0].path.to_string()),
            (id(ID), PIECE.into())
        );
        assert_ne!(rows[0].id, id(ID));
        assert!(scene.folder().exists(), "the folder stays");
        assert_eq!(scene.entry().state, EntryState::Gone);
        assert!(pending(&scene.entry()).is_empty());
        let new_entry = tracking::read(&scene.collection, rows[0].id)
            .expect("reads")
            .expect("the fork has an entry");
        assert_eq!(new_entry.state, EntryState::Live);
        assert_eq!(scene.metadata().payloads.len(), 1, "no new archive");
        assert!(!report.partial_for_test());
    }

    #[test]
    fn skip_leaves_the_piece_and_exits_partial() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        let before = scene.untouched();
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "skip");

        let report = scene.run_asking(&RealSeams, &asker(script));

        let rows = skipped(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].reason, SkipReason::Declined);
        assert!(undecided(&report).is_empty());
        assert!(report.partial_for_test());
        assert_eq!(scene.untouched(), before);
        assert!(
            pending(&scene.entry()).is_empty(),
            "nothing is left pending"
        );
    }

    #[test]
    fn a_skip_answer_clears_a_decision_an_earlier_run_left() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        scene.run(&RealSeams);
        assert_eq!(pending(&scene.entry()), replacing());
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "skip");

        scene.run_asking(&RealSeams, &asker(script));

        assert!(pending(&scene.entry()).is_empty());
    }

    #[test]
    fn a_decision_that_is_pending_is_not_written_again() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        scene.run(&RealSeams);
        let written = scene.entry().written;
        let later = FixedClock::new(Clock::now(&scene.clock) + jiff::SignedDuration::from_hours(1));

        let report = scene.run_at(&RealSeams, crate::ops::archive::nobody(), &later);

        assert_eq!(undecided(&report).len(), 1, "it is listed again");
        assert_eq!(scene.entry().written, written, "the entry is not rewritten");
    }

    #[test]
    fn a_folder_changed_back_is_reclaimed_and_its_decision_is_gone() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        scene.run(&RealSeams);
        assert_eq!(pending(&scene.entry()), replacing());
        scene.write_in_folder("notes.txt", "hello");
        scene.set_time("notes.txt", FILE_TIME);

        let report = scene.run(&RealSeams);

        assert_eq!(reclaimed(&report), 1, "{report:?}");
        assert!(pending(&scene.entry()).is_empty());
    }

    #[test]
    fn a_change_after_the_answer_is_not_replaced_unasked_and_asks_again() {
        let kept = [FILES[0], FILES[1], ("keep.txt", "k")];
        let scene = scene_of(&kept, &kept);
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        let folder = scene.folder();
        let (hook, asked) = Hook::new(vec![Some("replace"), Some("replace")], move |position| {
            if position == 0 {
                fs::remove_file(folder.join("src/main.rs")).expect("removed while the user thinks");
                settle(&folder);
            }
            if position == 1 {
                // Gaining a file loses nothing, and makes the package of
                // this try differ from the one of the first.
                fs::write(folder.join("extra.txt"), "x").expect("added while the user thinks");
                filetime::set_file_mtime(
                    folder.join("extra.txt"),
                    filetime::FileTime::from_unix_time(FILE_TIME, 0),
                )
                .expect("the time is set");
                settle(&folder);
            }
        });

        let report = scene.run_asking(&RealSeams, &asker(hook));

        let asked = asked.lock().expect("the list");
        assert_eq!(asked.len(), 2, "asked again");
        assert_eq!(asked[0].details, ["removed: notes.txt"]);
        assert_eq!(
            asked[1].details,
            ["removed: notes.txt", "removed: src/main.rs"]
        );
        assert_eq!(archived_rows(&report).len(), 1, "{report:?}");
        assert_eq!(
            object_names(&scene)
                .iter()
                .filter(|name| name.ends_with(".tar.zst"))
                .count(),
            2,
            "the package of the first try is gone"
        );
    }

    #[test]
    fn a_folder_that_keeps_changing_is_skipped_after_three_questions() {
        let files = [
            ("a.txt", "a"),
            ("b.txt", "b"),
            ("c.txt", "c"),
            ("d.txt", "d"),
            ("e.txt", "e"),
        ];
        let scene = scene_of(&files, &files);
        fs::remove_file(scene.folder().join("a.txt")).expect("removed");
        let folder = scene.folder();
        let (hook, asked) = Hook::new(vec![Some("replace"); 4], move |position| {
            let name = ["b.txt", "c.txt", "d.txt"][position];
            fs::remove_file(folder.join(name)).expect("removed while the user thinks");
            settle(&folder);
        });

        let report = scene.run_asking(&RealSeams, &asker(hook));

        assert_eq!(asked.lock().expect("the list").len(), 3);
        let rows = skipped(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].reason, SkipReason::Differs);
        assert!(
            rows[0].message.contains("kept changing"),
            "{}",
            rows[0].message
        );
        assert!(scene.folder().exists());
        assert_eq!(scene.metadata().payloads.len(), 1);
    }

    #[test]
    fn the_candidate_with_the_smallest_loss_is_the_basis() {
        let scene = scene();
        // A second payload of the version, listing only `notes.txt`.
        let listing: Vec<ListingEntry> = scene
            .listing()
            .into_iter()
            .filter(|line| line.path.to_string() != "src" && line.path.to_string() != "src/main.rs")
            .collect();
        scene.add_payload(1, &listing);
        scene.edit_entry(|entry| entry.local_payloads.clear());
        fs::remove_dir_all(scene.folder().join("src")).expect("removed");
        scene.write_in_folder("notes.txt", "HELLO");
        scene.set_time("notes.txt", FILE_TIME);

        let report = scene.run(&RealSeams);

        let rows = undecided(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].lost, [lost("notes.txt", "changed")]);
    }

    fn object_names(scene: &Scene) -> Vec<String> {
        fs::read_dir(scene.collection.object_dir(id(ID)))
            .expect("the object directory reads")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn removed(text: &str) -> Difference {
        Difference::Removed(path(text))
    }

    fn changed(text: &str, fields: &[crate::pack::verify::Field]) -> Difference {
        Difference::Changed {
            path: path(text),
            fields: fields.to_vec(),
        }
    }

    #[test]
    fn nothing_allows_no_loss() {
        assert!(Bound::Nothing.allows(&[]));
        assert!(!Bound::Nothing.allows(&[removed("a")]));
    }

    #[test]
    fn any_allows_every_loss() {
        assert!(Bound::Any.allows(&[removed("a"), changed("b", &[Field::Mode])]));
    }

    #[test]
    fn a_loss_within_what_was_shown_is_allowed() {
        let shown = Bound::Within(vec![
            removed("a"),
            changed("b", &[Field::Mode, Field::Content]),
        ]);
        assert!(shown.allows(&[removed("a")]));
        assert!(shown.allows(&[changed("b", &[Field::Content])]));
        assert!(shown.allows(&[]));
    }

    #[test]
    fn a_loss_beyond_what_was_shown_is_not_allowed() {
        let shown = Bound::Within(vec![removed("a"), changed("b", &[Field::Mode])]);
        assert!(!shown.allows(&[removed("c")]), "another path");
        assert!(!shown.allows(&[removed("a"), removed("c")]), "one more");
        assert!(
            !shown.allows(&[changed("b", &[Field::Mode, Field::Size])]),
            "another field"
        );
        assert!(
            !shown.allows(&[changed("a", &[Field::Mode])]),
            "removed, not changed"
        );
        assert!(!shown.allows(&[removed("b")]), "changed, not removed");
    }

    #[test]
    fn a_file_that_changed_while_it_was_read_refuses_the_piece() {
        let refusal = stable(&[removed("a"), Difference::ChangedWhileRead(path("b"))])
            .expect_err("the piece is refused");

        assert_eq!(refusal.reason, SkipReason::Error);
        assert!(refusal.message.contains("`b` changed while it was read"));
        assert!(stable(&[removed("a")]).is_ok());
    }

    #[test]
    fn a_fork_that_fails_skips_the_piece_and_names_the_cause() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        // The ID the fork would mint is taken.
        let taken = SeqIds::new(1).mint(Clock::now(&scene.clock));
        fs::create_dir_all(scene.collection.object_dir(taken)).expect("an object directory");
        let before = scene.untouched();
        let mut script = ScriptedInteraction::new();
        script.expect(TITLE, "fork");

        let report = scene.run_asking(&RealSeams, &asker(script));

        let rows = skipped(&report);
        assert_eq!(rows.len(), 1, "{report:?}");
        assert_eq!(rows[0].reason, SkipReason::Error);
        assert!(
            rows[0].message.contains(&taken.to_string()),
            "{}",
            rows[0].message
        );
        assert!(forked(&report).is_empty());
        assert_eq!(scene.untouched(), before);
        assert_eq!(scene.entry().state, EntryState::Live);
    }

    /// Answers by the path in the title and counts how many questions are
    /// open at the same time.
    struct Counting {
        open: Arc<std::sync::atomic::AtomicUsize>,
        most: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Interaction for Counting {
        fn choose(&mut self, question: &Question) -> Option<Choice> {
            let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            for _ in 0..50 {
                std::thread::yield_now();
            }
            self.open.fetch_sub(1, Ordering::SeqCst);
            let answer = if question.title.contains("/p0?") {
                "replace"
            } else if question.title.contains("/p1?") {
                "skip"
            } else if question.title.contains("/p2?") {
                "fork"
            } else {
                return None;
            };
            question
                .choices
                .iter()
                .find(|choice| choice.id == answer)
                .cloned()
        }
    }

    /// Four unearthed pieces `projects/p0` to `p3`, each missing `notes.txt`.
    fn four_pieces() -> Scene {
        let ids = [
            ID,
            "01D39ZY06FGSCTVN4T2V9PKHFZ",
            "01EJ3PX0000000000000000005",
            "01EJ3PX0000000000000000006",
        ];
        let mut fx = Fixture::new();
        for (number, object) in ids.iter().enumerate() {
            let piece = format!("projects/p{number}");
            fx.archived(object, &piece, FILES);
            for (name, content) in FILES {
                fx.tree()
                    .file_with(&format!("{piece}/{name}"), content, 0o644, FILE_TIME);
            }
            let mut entry = fx.entry(object);
            entry.state = EntryState::Live;
            entry.unearthed = Some(time("2026-10-05T12:00:00Z"));
            entry.unearthed_payload_version = Some(1);
            fx.write_entry(&mut entry);
            fx.edit_meta(object, |document| document["lifecycle"] = json!("live"));
            fs::remove_file(fx.at(&format!("{piece}/notes.txt"))).expect("removed");
        }
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

    #[test]
    fn workers_ask_one_question_at_a_time_and_each_piece_gets_its_own_answer() {
        for round in 0..50 {
            let scene = four_pieces();
            let (open, most) = Default::default();
            let counting = Counting {
                open: Arc::clone(&open),
                most: Arc::clone(&most),
            };
            let ids = SeqIds::new(1);
            let asker = asker(counting);

            let request = ArchiveRequest {
                collection: &scene.collection,
                clock: &scene.clock,
                ids: &ids,
                pool: shared_pool(),
                interaction: &asker,
                workers: 4,
                dry_run: false,
            };
            let report = rerun_while_lock_unattributed(|| archive_with(&request, &RealSeams))
                .unwrap_or_else(|error| panic!("round {round}: {error}"));

            assert_eq!(most.load(Ordering::SeqCst), 1, "round {round}");
            let archived: Vec<String> = archived_rows(&report)
                .iter()
                .map(|row| row.path.to_string())
                .collect();
            assert_eq!(archived, ["projects/p0"], "round {round}: {report:?}");
            let declined: Vec<String> = skipped(&report)
                .iter()
                .map(|row| row.path.to_string())
                .collect();
            assert_eq!(declined, ["projects/p1"], "round {round}");
            let forks: Vec<String> = forked(&report)
                .iter()
                .map(|row| row.path.to_string())
                .collect();
            assert_eq!(forks, ["projects/p2"], "round {round}");
            let waiting: Vec<String> = undecided(&report)
                .iter()
                .map(|row| row.path.to_string())
                .collect();
            assert_eq!(waiting, ["projects/p3"], "round {round}");
        }
    }

    // =========================================================
    // The dry run of a changed piece
    // =========================================================

    fn plan_of(scene: &Scene) -> ArchiveReport {
        plan_with(&scene.collection, &scene.clock, &RealSeams).expect("the plan is made")
    }

    #[test]
    fn a_dry_run_lists_a_replacement_without_loss() {
        let scene = scene();
        scene.write_in_folder("added.txt", "new");
        let before = (scene.entry(), scene.metadata());

        let plan = plan_of(&scene);

        assert_eq!(
            plan.plan.expect("a plan").actions,
            [ArchiveAction::Replace {
                path: path(PIECE),
                id: id(ID),
                size: 20,
            }]
        );
        assert_eq!((scene.entry(), scene.metadata()), before);
    }

    #[test]
    fn a_dry_run_lists_the_question_a_loss_would_raise() {
        let scene = scene();
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");
        let before = (scene.entry(), scene.metadata());

        let plan = plan_of(&scene);

        let actions = plan.plan.expect("a plan").actions;
        assert_eq!(
            actions,
            [ArchiveAction::Ask {
                path: path(PIECE),
                id: id(ID),
                size: 12,
                lost_count: 1,
            }]
        );
        assert_eq!((scene.entry(), scene.metadata()), before);
        assert!(
            pending(&scene.entry()).is_empty(),
            "a dry run decides nothing"
        );
    }

    #[test]
    fn a_dry_run_replaces_a_loss_where_the_collection_allows_it() {
        let scene = scene_built(replace_without_asking, FILES, &|_| {}, FILES);
        fs::remove_file(scene.folder().join("notes.txt")).expect("removed");

        let plan = plan_of(&scene);

        assert!(matches!(
            plan.plan.expect("a plan").actions[..],
            [ArchiveAction::Replace { .. }]
        ));
    }
}
