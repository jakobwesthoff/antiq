// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unearthing from a local archive.
//!
//! The steps run in the order the start-of-run repairs rely on (spec
//! "States"): the tree is extracted and verified in staging, the entry
//! says `unearthing`, the tree is renamed into place, the metadata change
//! is recorded, and only then the entry says `live`. A crash after the
//! entry leaves an `unearthing` entry that the repair of the next run
//! finishes or drops.

use super::live;
use super::remote::{Arrived, Fetch, fetch};
use super::{
    Skipped, TargetProblem, UnearthError, UnearthPlan, UnearthReport, UnearthSource, absolute,
    ancestors, check_target_on_disk, io_error, target_error,
};
use crate::clock::Clock;
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::moves::{Moved, move_dir};
use crate::fsx::remove::remove_tree;
use crate::fsx::space::available_space;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta::PayloadKind;
use crate::ops::fork::forked_new;
use crate::pack::hashing::HashingReader;
use crate::pack::names::{MemberName, check_members, fold_path};
use crate::pack::{Packager, TarZstd, TargetRules, WalkRules};
use crate::relpath::RelPath;
use crate::remote::Remote;
use crate::repair::record_unearth;
use crate::report::Warning;
use crate::select::Rules;
use crate::store::{Source, Store};
use crate::tracking::{self, EntryState, LocalPayload, TrackingEntry, VersionTakenOut};
use jiff::Timestamp;
use std::io;
use std::path::Path;
use unicode_normalization::UnicodeNormalization;

/// Moves a directory from one tree to another: `from` in the first to `to`
/// in the second.
pub(super) type Rename<'a> =
    &'a dyn Fn(&TreeRoot, &RelPath, &TreeRoot, &RelPath) -> Result<Moved, FsxError>;

/// What the steps of an unearth reach through. Tests replace a part to
/// fail a step, to change the world between two steps, or to watch the
/// order; the real parts are what the command uses.
pub(crate) struct Environment<'a> {
    pub(crate) packager: &'a dyn Packager,
    /// The rules of the volume a tree is extracted on.
    pub(crate) probe: &'a dyn Fn(&TreeRoot) -> io::Result<TargetRules>,
    pub(crate) free_space: &'a dyn Fn(&Path) -> Result<u64, FsxError>,
    /// Moves the staged tree into the collection.
    pub(crate) rename: Rename<'a>,
}

impl Environment<'static> {
    pub(crate) fn real() -> Environment<'static> {
        Environment {
            packager: &TarZstd,
            probe: &TargetRules::probe,
            free_space: &available_space,
            rename: &move_dir,
        }
    }
}

/// The permission bits a directory made for staging starts with: the
/// owner can create entries in it while it is filled.
const STAGING_MODE: u32 = 0o700;

/// Folders made above the target. The umask narrows them as it does for
/// every folder the user makes.
const PARENT_MODE: u32 = 0o777;

pub(super) fn taken_out(source: Source) -> VersionTakenOut {
    match source {
        Source::Pending => VersionTakenOut::Pending,
        Source::Committed(number) => VersionTakenOut::Committed(number),
    }
}

/// Unearths the archive of `plan` from the store of this machine.
pub(crate) fn unearth_archive(
    collection: &Collection,
    plan: &UnearthPlan,
    lock: &RunLock,
    clock: &dyn Clock,
    env: &Environment<'_>,
    warnings: Vec<Warning>,
    remote: Option<&dyn Remote>,
) -> Result<UnearthReport, UnearthError> {
    let arrived = match plan.source {
        UnearthSource::RemoteArchive => {
            let remote = remote.ok_or_else(|| UnearthError::NoLocalArchive {
                path: plan.target.clone(),
            })?;
            match fetch(collection, plan, remote, env.free_space)? {
                Fetch::Arrived(arrived) => arrived,
                Fetch::NoSpace { needed, available } => {
                    return Ok(skipped(collection, plan, needed, available, warnings));
                }
            }
        }
        _ => {
            let package = Store::new(collection).file_path(plan.id, &plan.payload.file);
            check_archive(plan, &package)?;
            Arrived::held(collection, plan, package)?
        }
    };
    carry_out(
        collection,
        plan,
        lock,
        clock,
        env,
        warnings,
        &arrived,
        Subject::Own,
    )
}

/// Which object the unearth makes live.
#[derive(Clone, Copy)]
pub(super) enum Subject {
    /// The object of the plan.
    Own,
    /// A new object, minted at `time`, that takes the plan's live package
    /// as a copy of its own.
    Fork { id: ObjectId, time: Timestamp },
}

impl Subject {
    fn id(self, plan: &UnearthPlan) -> ObjectId {
        match self {
            Subject::Own => plan.id,
            Subject::Fork { id, .. } => id,
        }
    }

    /// The entry this machine had before the unearth: a fork's object is
    /// new.
    fn previous(self, plan: &UnearthPlan) -> Option<&TrackingEntry> {
        match self {
            Subject::Own => plan.entry.as_ref(),
            Subject::Fork { .. } => None,
        }
    }
}

/// Extracts the tree of `arrived` and puts it in place, once the volume has
/// room for it.
#[allow(clippy::too_many_arguments)]
pub(super) fn carry_out(
    collection: &Collection,
    plan: &UnearthPlan,
    lock: &RunLock,
    clock: &dyn Clock,
    env: &Environment<'_>,
    warnings: Vec<Warning>,
    arrived: &Arrived,
    subject: Subject,
) -> Result<UnearthReport, UnearthError> {
    let staging_dir = collection.staging_dir();
    let available = (env.free_space)(&staging_dir)?;
    if available < arrived.room {
        return Ok(skipped(collection, plan, arrived.room, available, warnings));
    }

    // The tree is extracted below `staging/`, in a folder named after the
    // object, which the next run empties if this one dies.
    let staging_root = TreeRoot::open(&staging_dir).map_err(io_error("open", &staging_dir))?;
    let staged_name = RelPath::parse(&plan.id.to_string()).expect("an ID is a valid name");
    staging_root
        .create_dir(&staged_name, STAGING_MODE)
        .map_err(io_error("create", &staging_dir.join(plan.id.to_string())))?;
    let placed = place(
        collection,
        plan,
        lock,
        clock,
        env,
        arrived,
        subject,
        &staging_root,
        &staged_name,
    );
    // Whatever is left of the staged tree is of no use: after a move
    // there is nothing left, and a failed run's next start empties
    // `staging/` anyway, so a removal that fails is not worth an error.
    let _ = remove_tree(&staging_root, &staged_name);

    match placed? {
        Placed::Done { warnings: mut more } => {
            let mut warnings = warnings;
            warnings.append(&mut more);
            Ok(UnearthReport {
                unearthed: true,
                id: subject.id(plan),
                path: plan.target.clone(),
                abs_path: absolute(collection, &plan.target),
                payload_version: (plan.payload.kind == PayloadKind::Archive)
                    .then_some(plan.payload.version),
                from: matches!(subject, Subject::Fork { .. }).then_some(plan.id),
                skipped: None,
                warnings,
            })
        }
        Placed::NoSpace => {
            let available = (env.free_space)(&staging_dir).unwrap_or(0);
            Ok(skipped(collection, plan, arrived.room, available, warnings))
        }
    }
}

/// The report of a piece that was left as it is for want of room.
pub(super) fn skipped(
    collection: &Collection,
    plan: &UnearthPlan,
    needed: u64,
    available: u64,
    warnings: Vec<Warning>,
) -> UnearthReport {
    UnearthReport {
        unearthed: false,
        id: plan.id,
        path: plan.target.clone(),
        abs_path: absolute(collection, &plan.target),
        payload_version: None,
        from: None,
        skipped: Some(Skipped::NoSpace { needed, available }),
        warnings,
    }
}

/// The archive file is the one the metadata describes.
pub(super) fn check_archive(plan: &UnearthPlan, package: &Path) -> Result<(), UnearthError> {
    let payload = &plan.payload;
    let differs = |what: String| UnearthError::ArchiveDiffers {
        file: payload.file.as_str().to_string(),
        what,
    };
    let file = std::fs::File::open(package).map_err(io_error("open", package))?;
    let mut reader = HashingReader::new(file);
    io::copy(&mut reader, &mut io::sink()).map_err(io_error("read", package))?;
    if reader.bytes_read() != payload.size {
        return Err(differs(format!(
            "it has {} bytes, the metadata says {}",
            reader.bytes_read(),
            payload.size
        )));
    }
    if reader.sha256() != payload.checksum.value {
        return Err(differs(format!(
            "its SHA-256 is {}, the metadata says {}",
            reader.sha256(),
            payload.checksum.value
        )));
    }
    Ok(())
}

enum Placed {
    Done { warnings: Vec<Warning> },
    NoSpace,
}

/// Extracts, verifies and moves the tree, and records it. Everything
/// below staging is removed by the caller.
#[allow(clippy::too_many_arguments)]
fn place(
    collection: &Collection,
    plan: &UnearthPlan,
    lock: &RunLock,
    clock: &dyn Clock,
    env: &Environment<'_>,
    arrived: &Arrived,
    subject: Subject,
    staging_root: &TreeRoot,
    staged_name: &RelPath,
) -> Result<Placed, UnearthError> {
    let package = arrived.archive.as_path();
    let id = subject.id(plan);
    let target = &plan.target;
    let root = TreeRoot::open(collection.root()).map_err(io_error("open", collection.root()))?;
    let staged = staging_root
        .sub(staged_name)
        .map_err(io_error("open", &collection.staging_dir()))?;
    let rules = Rules::new(
        &collection.config().collection.patterns,
        &collection.config().collection.exclude,
    )
    .expect("the patterns parsed when the unearth was planned")
    .bind(&root)?;

    // The volume decides what names it can hold, so it is asked before
    // anything is extracted, and the target's own names are held to the
    // same rules as the members'.
    let volume = (env.probe)(&staged).map_err(io_error("probe", &collection.staging_dir()))?;
    let missing = check_target_on_disk(&root, &rules, target)?;
    check_target_names(target, &missing, &volume)?;
    check_entries_on_volume(collection, id, target, &volume)?;

    let unpacked = match env.packager.unpack(package, &staged, &volume) {
        Ok(unpacked) => unpacked,
        Err(error) if is_out_of_space(&error) => return Ok(Placed::NoSpace),
        Err(error) => return Err(error.into()),
    };
    // Staging holds exactly the package, so no name is left out of the
    // comparison. The top's mode is not compared on staging; it is set
    // here and `move_dir` carries it.
    let verified = env.packager.verify_strict(
        package,
        &staged,
        &WalkRules {
            junk: Vec::new(),
            staged: true,
        },
    )?;
    arrived.check_listing(plan, verified.listing())?;
    staged
        .set_mode(&RelPath::root(), unpacked.top_mode)
        .map_err(io_error("set the mode of", &collection.staging_dir()))?;

    let machine = collection.machine_name()?;
    let stored = arrived.store(collection, lock, plan)?;
    let mut entry = unearthing_entry(plan, subject, clock);
    tracking::write(collection, lock, clock, &mut entry)?;

    let placed = move_into_place(&root, &rules, env, staging_root, staged_name, target);
    match placed {
        Ok(mut warnings) => {
            // The entry's time is the unearthing time, here and in the
            // repair that finishes an interrupted unearth.
            let (changed, unearthed) = match subject {
                Subject::Own => (
                    record_unearth(
                        &plan.current.metadata,
                        collection.id(),
                        &machine,
                        entry.written,
                        target,
                    ),
                    entry.written,
                ),
                Subject::Fork { id, time } => (
                    forked_new(id, collection.id(), target, &machine, time),
                    time,
                ),
            };
            Store::new(collection).write_pending(lock, &changed)?;
            entry.state = EntryState::Live;
            entry.unearthed = Some(unearthed);
            entry.version_taken_out = VersionTakenOut::Pending;
            tracking::write(collection, lock, clock, &mut entry)?;
            warnings.extend(Arrived::update_index(collection, lock, &stored));
            warnings.extend(live::finish(collection, plan));
            Ok(Placed::Done { warnings })
        }
        Err(error) => {
            // The tree did not arrive, so the entry goes back to what it
            // was. If that fails the next run's repair drops the
            // `unearthing` entry, which has no folder, and the files stay
            // because that entry lists the archive as held.
            let restored = match subject.previous(plan) {
                Some(previous) => {
                    tracking::write(collection, lock, clock, &mut previous.clone()).is_ok()
                }
                None => tracking::remove(collection, lock, id).is_ok(),
            };
            if restored {
                stored.undo(collection, plan);
            }
            Err(error)
        }
    }
}

/// The entry that says the folder is arriving.
fn unearthing_entry(plan: &UnearthPlan, subject: Subject, clock: &dyn Clock) -> TrackingEntry {
    match (subject, plan.source) {
        (Subject::Fork { id, time }, _) => return live::fork_entry(id, &plan.target, time),
        (Subject::Own, UnearthSource::LivePackage) => return live::restore_entry(plan),
        (Subject::Own, _) => {}
    }
    let version_taken_out = taken_out(plan.current.source);
    let mut entry = plan.entry.clone().unwrap_or_else(|| {
        TrackingEntry::new(
            plan.id,
            plan.target.clone(),
            EntryState::Unearthing,
            version_taken_out,
            clock.now(),
        )
    });
    entry.path = plan.target.clone();
    entry.state = EntryState::Unearthing;
    entry.version_taken_out = version_taken_out;
    entry.unearthed = None;
    entry.unearthed_payload_version = Some(plan.payload.version);
    entry.removing_payload = None;
    // A folder made from the current archive has not been changed since.
    entry.set_diverged(false);
    if !entry
        .local_payloads
        .iter()
        .any(|local| local.file == plan.payload.file)
    {
        entry.local_payloads.push(LocalPayload {
            kind: PayloadKind::Archive,
            version: plan.payload.version,
            file: plan.payload.file.clone(),
        });
    }
    entry
}

/// Checks the target once more, creates the folders above it and renames
/// the staged tree to it. Folders made here are removed again if the
/// rename does not happen.
fn move_into_place(
    root: &TreeRoot,
    rules: &Rules,
    env: &Environment<'_>,
    staging_root: &TreeRoot,
    staged_name: &RelPath,
    target: &RelPath,
) -> Result<Vec<Warning>, UnearthError> {
    // The world may have changed since planning: this is the check that
    // the target is free right before the rename (ADR 0034).
    let missing = check_target_on_disk(root, rules, target)?;
    let mut created = Vec::new();
    let moved = make_parents(root, &missing, &mut created)
        .and_then(|()| (env.rename)(staging_root, staged_name, root, target).map_err(Into::into));
    match moved {
        Ok(moved) => Ok(moved
            .mode_restore_error
            .map(|error| {
                Warning::new(format!(
                    "the mode of `{target}` could not be put back as the archive has it: {error}"
                ))
            })
            .into_iter()
            .collect()),
        Err(error) => {
            remove_created(root, &created);
            Err(error)
        }
    }
}

/// Makes the folders above the target, noting each as it is made.
fn make_parents(
    root: &TreeRoot,
    missing: &[RelPath],
    created: &mut Vec<RelPath>,
) -> Result<(), UnearthError> {
    for parent in missing {
        root.create_dir(parent, PARENT_MODE)
            .map_err(io_error("create", Path::new(&parent.to_string())))?;
        created.push(parent.clone());
    }
    Ok(())
}

/// Removes the folders `created`, deepest first. A folder that cannot be
/// removed stays; it is empty.
fn remove_created(root: &TreeRoot, created: &[RelPath]) {
    for parent in created.iter().rev() {
        let _ = root.remove_dir(parent);
    }
}

/// Entries keep the spelling their folder had when they were written. A
/// volume that folds case or Unicode normalisation reaches that folder
/// under the target's spelling too, so another entry that holds a folder
/// there, or above the target, makes the target taken.
fn check_entries_on_volume(
    collection: &Collection,
    id: ObjectId,
    target: &RelPath,
    volume: &TargetRules,
) -> Result<(), UnearthError> {
    let target_key = fold_path(target, volume);
    let above: Vec<Vec<u8>> = ancestors(target)
        .iter()
        .map(|ancestor| fold_path(ancestor, volume))
        .collect();
    let held = tracking::read_all(collection)?
        .entries
        .into_iter()
        .filter(|other| other.id != id)
        .filter(|other| {
            matches!(
                other.state,
                EntryState::Unearthing | EntryState::Live | EntryState::Removing
            )
        });
    for other in held {
        let key = fold_path(&other.path, volume);
        if key == target_key {
            return Err(target_error(target, TargetProblem::Taken));
        }
        if above.contains(&key) {
            return Err(target_error(
                target,
                TargetProblem::InsidePiece { piece: other.path },
            ));
        }
    }
    Ok(())
}

/// Holds the names of the target to the rules of the volume: every name
/// on the way is a directory, and the ones that are made here must be
/// stored as written.
fn check_target_names(
    target: &RelPath,
    missing: &[RelPath],
    volume: &TargetRules,
) -> Result<(), UnearthError> {
    let names: Vec<RelPath> = (1..=target.components().count())
        .map(|taken| {
            let parts: Vec<&[u8]> = target.components().take(taken).collect();
            RelPath::from_bytes(parts.join(&b'/')).expect("a prefix of a valid path is valid")
        })
        .collect();
    let members: Vec<MemberName> = names.iter().cloned().map(MemberName::plain).collect();
    let problems = check_members(&members, volume);
    if let Some(problem) = problems.first() {
        return Err(target_error(
            target,
            TargetProblem::Unholdable {
                reason: problem.to_string(),
            },
        ));
    }
    if volume.decomposes {
        for made in names
            .iter()
            .filter(|name| missing.contains(name) || *name == target)
        {
            let name = made.components().last().expect("a name has a last part");
            let changes =
                std::str::from_utf8(name).is_ok_and(|text| text.nfd().collect::<String>() != text);
            if changes {
                return Err(target_error(
                    target,
                    TargetProblem::Unholdable {
                        reason: format!("the volume would store `{made}` decomposed"),
                    },
                ));
            }
        }
    }
    Ok(())
}

/// Whether the failure is a full volume, wherever in the error it is.
pub(super) fn is_out_of_space(error: &dyn std::error::Error) -> bool {
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(found) = cause.downcast_ref::<io::Error>()
            && found.raw_os_error() == Some(libc::ENOSPC)
        {
            return true;
        }
        source = cause.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::remove::remove_tree;
    use crate::listing::ListingEntry;
    use crate::ops::unearth::tests::{FILES, Fixture, ID, NOW, OTHER_ID, id, path, time};
    use crate::ops::unearth::{Planned, UnearthRequest};
    use crate::pack::verify::compare_with_listing;
    use crate::pack::{PackError, Unpacked, Verified};
    use crate::repair;
    use crate::testing::deep_path;
    use crate::testing::package::{Packaged, RawMember, compress, on_disk, raw_tar};
    use sha2::Digest;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    /// A packager that does what `TarZstd` does and lets a test step in.
    struct Wrap<'a> {
        after_unpack: &'a dyn Fn(&TreeRoot),
        after_verify: &'a dyn Fn(),
        unpack_error: &'a dyn Fn() -> Option<PackError>,
    }

    fn untouched<'a>() -> Wrap<'a> {
        Wrap {
            after_unpack: &|_| {},
            after_verify: &|| {},
            unpack_error: &|| None,
        }
    }

    impl Packager for Wrap<'_> {
        fn format(&self) -> crate::pack::PackageFormat {
            TarZstd.format()
        }

        fn pack(
            &self,
            root: &TreeRoot,
            tree: &crate::tree::PieceTree,
            settings: &crate::pack::PackSettings,
            out: &mut dyn io::Write,
        ) -> Result<crate::pack::Packed, PackError> {
            TarZstd.pack(root, tree, settings, out)
        }

        fn verify_strict(
            &self,
            package: &Path,
            root: &TreeRoot,
            rules: &WalkRules,
        ) -> Result<Verified, PackError> {
            let verified = TarZstd.verify_strict(package, root, rules)?;
            (self.after_verify)();
            Ok(verified)
        }

        fn list(&self, package: &Path) -> Result<Vec<ListingEntry>, PackError> {
            TarZstd.list(package)
        }

        fn unpack(
            &self,
            package: &Path,
            staging: &TreeRoot,
            rules: &TargetRules,
        ) -> Result<Unpacked, PackError> {
            if let Some(error) = (self.unpack_error)() {
                return Err(error);
            }
            let unpacked = TarZstd.unpack(package, staging, rules)?;
            (self.after_unpack)(staging);
            Ok(unpacked)
        }
    }

    fn real_probe(root: &TreeRoot) -> io::Result<TargetRules> {
        TargetRules::probe(root)
    }

    fn real_move(
        from_root: &TreeRoot,
        from: &RelPath,
        to_root: &TreeRoot,
        to: &RelPath,
    ) -> Result<Moved, FsxError> {
        move_dir(from_root, from, to_root, to)
    }

    fn plenty(_: &Path) -> Result<u64, FsxError> {
        Ok(u64::MAX)
    }

    /// Unearths the planned piece in `fx` through `packager`, with every
    /// other part the real one.
    fn unearth_with(
        fx: &Fixture,
        request: &UnearthRequest,
        packager: &dyn Packager,
    ) -> Result<UnearthReport, UnearthError> {
        let env = Environment {
            packager,
            probe: &real_probe,
            free_space: &plenty,
            rename: &real_move,
        };
        unearth_in(fx, request, &env)
    }

    fn unearth_in(
        fx: &Fixture,
        request: &UnearthRequest,
        env: &Environment<'_>,
    ) -> Result<UnearthReport, UnearthError> {
        let plan = fx.planned(request);
        unearth_archive(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            env,
            Vec::new(),
            None,
        )
    }

    fn unearth(fx: &Fixture) -> UnearthReport {
        unearth_with(fx, &fx.request(ID), &untouched()).expect("the unearth works")
    }

    fn staging_is_empty(fx: &Fixture) -> bool {
        fs::read_dir(fx.collection.staging_dir())
            .expect("lists")
            .count()
            == 0
    }

    /// The archive of a tar written by hand, so that a test can put in
    /// what packing a real tree cannot.
    fn from_tar(members: &[RawMember<'_>]) -> Packaged {
        let bytes = compress(&raw_tar(members));
        let (_dir, file) = on_disk(&bytes);
        // An archive without members has no listing to read.
        let listing = TarZstd.list(&file).unwrap_or_default();
        Packaged {
            packed: crate::pack::Packed {
                size: bytes.len() as u64,
                sha256: crate::pack::hashing::hex(&sha2::Sha256::digest(&bytes)),
                listing,
                notes: Vec::new(),
            },
            bytes,
        }
    }

    fn container(mode: u32) -> RawMember<'static> {
        RawMember {
            mode,
            ..RawMember::dir(b"a/")
        }
    }

    // ---------------------------------------------------------------------
    // The whole unearth
    // ---------------------------------------------------------------------

    #[test]
    fn an_archived_piece_comes_back_as_it_was_archived() {
        let mut fx = Fixture::new();
        fx.archived_with(ID, "projects/a", FILES, |tree| {
            tree.file_with("projects/a/run.sh", "#!/bin/sh", 0o755, 1_100_000_000);
            tree.file_with("projects/a/secret", "x", 0o600, 1_200_000_000);
            tree.symlink("projects/a/link", "notes.txt");
            tree.symlink("projects/a/dangling", "../nowhere");
            tree.hard_link("projects/a/notes.txt", "projects/a/src/again.txt");
            fs::set_permissions(tree.path("projects/a"), fs::Permissions::from_mode(0o750))
                .expect("a mode");
        });

        let report = unearth(&fx);

        assert!(report.unearthed, "{report:?}");
        assert_eq!(report.payload_version, Some(1));
        assert_eq!(report.path, path("projects/a"));
        assert_eq!(report.abs_path, fx.at("projects/a").display().to_string());
        let root = TreeRoot::open(fx.temp.root()).expect("opens");
        let folder = root.sub(&path("projects/a")).expect("the folder is there");
        let listing = {
            let current = crate::store::Store::new(&fx.collection)
                .current(id(ID))
                .expect("reads")
                .expect("metadata");
            let name = current.metadata.payloads[0]
                .listing
                .clone()
                .expect("listing");
            let file =
                fs::File::open(crate::store::Store::new(&fx.collection).file_path(id(ID), &name))
                    .expect("opens");
            crate::listing::read_all(file).expect("reads")
        };
        let compared = compare_with_listing(
            &listing,
            &folder,
            &WalkRules {
                junk: Vec::new(),
                staged: false,
            },
        )
        .expect("compares");
        assert!(
            compared.differences().is_empty(),
            "{:?}",
            compared.differences()
        );
        let top = fs::metadata(fx.at("projects/a")).expect("the top");
        assert_eq!(top.mode() & 0o7777, 0o750);
        assert_eq!(top.mtime(), 1_000_000_000);
        assert!(staging_is_empty(&fx));
    }

    #[test]
    fn the_entry_and_the_metadata_say_unearthed_when_it_is_done() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);

        unearth(&fx);

        let entry = fx.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path("projects/a"));
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert_eq!(entry.unearthed_payload_version, Some(1));
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        let current = Store::new(&fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata");
        assert_eq!(current.source, Source::Pending);
        assert_eq!(current.metadata.lifecycle, crate::meta::Lifecycle::Live);
        let last = current.metadata.history.last().expect("a history");
        assert_eq!(
            (last.action.as_str(), last.machine.as_str(), last.time),
            ("unearth", "imperius", time(NOW))
        );
    }

    #[test]
    fn what_an_entry_already_knows_is_kept() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let mut entry = fx.entry(ID);
        entry.committed_versions = vec![1];
        entry.missing_on_remote = true;
        entry.decide(tracking::DecisionKind::DivergedFolder);
        fx.write_entry(&mut entry);

        unearth(&fx);

        let kept = fx.entry(ID);
        assert_eq!(kept.committed_versions, vec![1]);
        assert!(!kept.diverged);
        assert!(kept.pending_decisions.is_empty());
        assert!(kept.missing_on_remote, "what the remote said stays");
    }

    #[test]
    fn a_missing_entry_is_made_and_holds_the_payload() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("removed");

        unearth(&fx);

        let entry = fx.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.local_payloads.len(), 1);
        assert_eq!(entry.local_payloads[0].file, payload);
    }

    #[test]
    fn the_piece_can_go_to_another_path_that_gets_its_parents() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        let request = UnearthRequest {
            to: Some(path("projects/new/c")),
            ..fx.request(ID)
        };

        let report = unearth_with(&fx, &request, &untouched()).expect("works");

        assert_eq!(report.path, path("projects/new/c"));
        assert!(fx.at("projects/new/c/notes.txt").exists());
        assert_eq!(fx.entry(ID).path, path("projects/new/c"));
        let metadata = Store::new(&fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata")
            .metadata;
        assert_eq!(metadata.path, path("projects/new/c"));
        assert_eq!(metadata.name.as_bytes(), b"c");
    }

    #[test]
    fn a_change_that_is_recorded_already_is_built_upon() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let object = fx.collection.object_dir(id(ID));
        fs::copy(
            object.join("meta.1"),
            fx.collection.meta_pending_path(id(ID)),
        )
        .expect("pending");
        let seen = RefCell::new(None);
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            *seen.borrow_mut() = Some(fx.entry(ID).version_taken_out);
            move_dir(from_root, from, to_root, to)
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };

        unearth_in(&fx, &fx.request(ID), &env).expect("works");

        assert_eq!(*seen.borrow(), Some(VersionTakenOut::Pending));
        let metadata = Store::new(&fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata")
            .metadata;
        let actions: Vec<&str> = metadata
            .history
            .iter()
            .map(|entry| entry.action.as_str())
            .collect();
        assert_eq!(actions, ["archive", "unearth"]);
    }

    #[test]
    fn the_command_applies_a_plan_under_the_guard_of_the_run() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let plan = fx.planned(&fx.request(ID));
        let Fixture {
            collection,
            clock,
            lock,
            temp: _temp,
        } = fx;
        drop(lock);
        let crate::run::Started::Guarded(guard) =
            crate::run::start(&collection, &clock, false).expect("the lock is free")
        else {
            unreachable!("a real run is guarded");
        };

        let report = super::super::apply(&collection, &plan, &guard, &clock).expect("works");

        assert!(report.unearthed);
        assert!(!crate::report::CommandReport::partial(&report));
        assert!(crate::report::CommandReport::warnings(&report).is_empty());
        assert!(collection.root().join("projects/a/notes.txt").exists());
    }

    #[test]
    fn an_older_version_can_be_unearthed() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.add_archive(ID, 2, false);
        let request = UnearthRequest {
            version: Some(1),
            ..fx.request(ID)
        };

        let report = unearth_with(&fx, &request, &untouched()).expect("works");

        assert_eq!(report.payload_version, Some(1));
        assert_eq!(fx.entry(ID).unearthed_payload_version, Some(1));
    }

    #[test]
    fn unearthing_twice_finds_the_piece_where_it_is() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        unearth(&fx);

        let planned = fx.plan(&fx.request(ID)).expect("planned");

        assert!(
            matches!(&planned, Planned::AlreadyUnearthed { path: at, .. } if *at == path("projects/a")),
            "{planned:?}"
        );
    }

    #[test]
    fn a_piece_with_a_path_past_the_path_limit_comes_back_whole() {
        let deep = deep_path(6);
        let piece = format!("{deep}");
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["*/*/*/*/*/*".into()];
        });
        let root = TreeRoot::open(fx.temp.root()).expect("opens");
        let mut at = RelPath::root();
        for name in deep.components() {
            at = at.join(&crate::relpath::Name::from_bytes(name.to_vec()).expect("a name"));
            root.create_dir(&at, 0o755).expect("a level");
        }
        let mut file = root
            .create_file(
                &at.join(&crate::relpath::Name::from_bytes(b"deep.txt".to_vec()).expect("a name")),
                0o644,
            )
            .expect("a file");
        io::Write::write_all(&mut file, b"far down").expect("written");
        drop(file);
        let packaged = crate::testing::package::pack_piece(fx.tree(), &piece);
        fx.archive_package(ID, &piece, &packaged);
        assert!(
            piece.len() > 1024,
            "the path has to pass the platform limit"
        );

        let report = unearth_with(&fx, &fx.request(ID), &untouched()).expect("works");

        assert!(report.unearthed);
        let root = TreeRoot::open(fx.temp.root()).expect("opens");
        let found = root
            .lstat(&deep.concat(&path("deep.txt")))
            .expect("the file came back");
        assert_eq!(found.size, 8);
        remove_tree(&root, &deep).expect("cleans up");
    }

    // ---------------------------------------------------------------------
    // The order of the writes
    // ---------------------------------------------------------------------

    #[test]
    fn the_entry_says_unearthing_while_the_tree_moves_and_the_metadata_follows_the_move() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let seen = RefCell::new(Vec::new());
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            seen.borrow_mut().push((
                fx.entry(ID).state,
                fx.collection.meta_pending_path(id(ID)).exists(),
            ));
            move_dir(from_root, from, to_root, to)
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };

        unearth_in(&fx, &fx.request(ID), &env).expect("works");

        assert_eq!(*seen.borrow(), [(EntryState::Unearthing, false)]);
        assert_eq!(fx.entry(ID).state, EntryState::Live);
        assert!(fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn a_metadata_change_that_cannot_be_recorded_leaves_the_tree_and_an_unearthing_entry_that_the_repair_finishes()
     {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let object_dir = fx.collection.object_dir(id(ID));
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            let moved = move_dir(from_root, from, to_root, to);
            // The change cannot be written from here on.
            fs::set_permissions(&object_dir, fs::Permissions::from_mode(0o500)).expect("chmod");
            moved
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };
        let _restore = crate::fsx::RestoreModes(vec![object_dir.clone()]);

        let error = unearth_in(&fx, &fx.request(ID), &env).expect_err("fails");

        assert!(
            matches!(error, UnearthError::Fsx(_) | UnearthError::Store(_)),
            "{error}"
        );
        assert!(fx.at("projects/a/notes.txt").exists());
        assert_eq!(fx.entry(ID).state, EntryState::Unearthing);

        fs::set_permissions(&object_dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let warnings = repair::run(&fx.collection, &fx.lock, &fx.clock, &repair::RealEffects)
            .expect("repairs");

        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(fx.entry(ID).state, EntryState::Live);
        let metadata = Store::new(&fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata")
            .metadata;
        assert!(repair::records_unearth(&metadata, "imperius", time(NOW)));
    }

    #[test]
    fn an_entry_that_cannot_be_made_live_leaves_the_recorded_change_for_the_repair() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let tracking_dir = fx.collection.tracking_dir();
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            let moved = move_dir(from_root, from, to_root, to);
            // The entry cannot be rewritten from here on.
            fs::set_permissions(&tracking_dir, fs::Permissions::from_mode(0o500)).expect("chmod");
            moved
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };
        let _restore = crate::fsx::RestoreModes(vec![tracking_dir.clone()]);

        let error = unearth_in(&fx, &fx.request(ID), &env).expect_err("fails");

        assert!(matches!(error, UnearthError::Tracking(_)), "{error}");
        assert_eq!(fx.entry(ID).state, EntryState::Unearthing);
        fs::set_permissions(&tracking_dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        repair::run(&fx.collection, &fx.lock, &fx.clock, &repair::RealEffects).expect("repairs");
        assert_eq!(fx.entry(ID).state, EntryState::Live);
        let metadata = Store::new(&fx.collection)
            .current(id(ID))
            .expect("reads")
            .expect("metadata")
            .metadata;
        assert_eq!(
            metadata
                .history
                .iter()
                .filter(|entry| entry.action == "unearth")
                .count(),
            1,
            "the change is not recorded twice"
        );
    }

    // ---------------------------------------------------------------------
    // What stops it before anything is moved
    // ---------------------------------------------------------------------

    #[test]
    fn an_archive_with_other_bytes_than_its_checksum_is_refused_before_staging() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        let file = fx.collection.object_dir(id(ID)).join(payload.as_str());
        let mut bytes = fs::read(&file).expect("reads");
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&file, bytes).expect("written");

        let error = unearth_with(&fx, &fx.request(ID), &untouched()).expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. } if what.contains("SHA-256")),
            "{error}"
        );
        assert!(staging_is_empty(&fx));
        assert!(!fx.at("projects/a").exists());
    }

    #[test]
    fn an_archive_of_another_size_than_its_metadata_is_refused() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        let file = fx.collection.object_dir(id(ID)).join(payload.as_str());
        let mut bytes = fs::read(&file).expect("reads");
        bytes.push(0);
        fs::write(&file, bytes).expect("written");

        let error = unearth_with(&fx, &fx.request(ID), &untouched()).expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. } if what.contains("bytes")),
            "{error}"
        );
    }

    #[test]
    fn an_archive_that_cannot_be_read_is_an_error() {
        let mut fx = Fixture::new();
        let payload = fx.archived(ID, "projects/a", FILES);
        let plan = fx.planned(&fx.request(ID));
        fs::remove_file(fx.collection.object_dir(id(ID)).join(payload.as_str())).expect("gone");

        let error = unearth_archive(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            &Environment::real(),
            Vec::new(),
            None,
        )
        .expect_err("fails");

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
    }

    /// Nothing may be left of a refused unearth.
    fn assert_nothing_changed(fx: &Fixture, target: &str) {
        assert!(staging_is_empty(fx), "staging is empty");
        assert_eq!(
            fx.entry(ID).state,
            EntryState::Gone,
            "the entry is as it was"
        );
        assert!(!fx.at(target).exists(), "nothing at the target");
        assert!(
            !fx.collection.meta_pending_path(id(ID)).exists(),
            "no change recorded"
        );
    }

    #[test]
    fn names_that_collide_on_the_volume_refuse_before_anything_is_extracted() {
        let mut fx = Fixture::new();
        fx.archive_package(
            ID,
            "projects/a",
            &from_tar(&[
                container(0o755),
                RawMember::file(b"a/Foo", b"1"),
                RawMember::file(b"a/foo", b"2"),
            ]),
        );
        let probe = |_: &TreeRoot| Ok(TargetRules::macos());
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };

        let error = unearth_in(&fx, &fx.request(ID), &env).expect_err("refused");

        assert!(
            matches!(&error, UnearthError::Pack(PackError::UnholdableNames(_))),
            "{error}"
        );
        assert!(error.to_string().contains("Foo"), "{error}");
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn a_name_that_is_not_utf8_refuses_on_a_volume_that_requires_it() {
        let mut fx = Fixture::new();
        fx.archive_package(
            ID,
            "projects/a",
            &from_tar(&[container(0o755), RawMember::file(b"a/caf\xe9", b"1")]),
        );
        let probe = |_: &TreeRoot| {
            Ok(TargetRules {
                requires_utf8: true,
                ..TargetRules::linux()
            })
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };

        let error = unearth_in(&fx, &fx.request(ID), &env).expect_err("refused");

        assert!(
            matches!(&error, UnearthError::Pack(PackError::UnholdableNames(_))),
            "{error}"
        );
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn an_archive_without_members_refuses() {
        let mut fx = Fixture::new();
        fx.archive_package(ID, "projects/a", &from_tar(&[]));

        let error = unearth_with(&fx, &fx.request(ID), &untouched()).expect_err("refused");

        assert!(
            matches!(&error, UnearthError::Pack(PackError::NoMembers)),
            "{error}"
        );
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn a_probe_that_fails_is_an_error() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let probe = |_: &TreeRoot| Err(io::Error::other("refused by the test"));
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };

        let error = unearth_in(&fx, &fx.request(ID), &env).expect_err("fails");

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
        assert_nothing_changed(&fx, "projects/a");
    }

    fn with_rules(fx: &Fixture, to: &str, rules: TargetRules) -> UnearthError {
        let probe = move |_: &TreeRoot| Ok(rules.clone());
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };
        let request = UnearthRequest {
            to: Some(path(to)),
            ..fx.request(ID)
        };
        unearth_in(fx, &request, &env).expect_err("refused")
    }

    #[test]
    fn a_target_name_the_volume_stores_decomposed_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let rules = TargetRules {
            decomposes: true,
            ..TargetRules::linux()
        };

        let error = with_rules(&fx, "projects/caf\u{e9}", rules);

        assert!(error.to_string().contains("decomposed"), "{error}");
        assert_nothing_changed(&fx, "projects/caf\u{e9}");
    }

    #[test]
    fn a_made_parent_the_volume_stores_decomposed_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        let rules = TargetRules {
            decomposes: true,
            ..TargetRules::linux()
        };

        let error = with_rules(&fx, "projects/caf\u{e9}/a", rules);

        assert!(error.to_string().contains("decomposed"), "{error}");
        assert!(!fx.at("projects/caf\u{e9}").exists());
    }

    #[test]
    fn an_existing_parent_is_not_held_to_the_decomposing_rule() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        fx.tree().dir("projects/caf\u{e9}");
        let rules = TargetRules {
            decomposes: true,
            ..TargetRules::linux()
        };
        let probe = move |_: &TreeRoot| Ok(rules.clone());
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };
        let request = UnearthRequest {
            to: Some(path("projects/caf\u{e9}/a")),
            ..fx.request(ID)
        };

        unearth_in(&fx, &request, &env).expect("works");
    }

    #[test]
    fn a_target_that_another_entry_holds_under_another_spelling_is_taken_on_a_folding_volume() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.hold(OTHER_ID, "projects/A", EntryState::Live);

        let error = with_rules(&fx, "projects/a", TargetRules::macos());

        assert!(
            matches!(
                &error,
                UnearthError::Target {
                    problem: TargetProblem::Taken,
                    ..
                }
            ),
            "{error}"
        );
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn a_folder_above_the_target_that_another_entry_holds_under_another_spelling_is_a_piece() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        fx.hold(OTHER_ID, "projects/X", EntryState::Live);

        let error = with_rules(&fx, "projects/x/a", TargetRules::macos());

        assert!(
            matches!(
                &error,
                UnearthError::Target {
                    problem: TargetProblem::InsidePiece { .. },
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn another_entry_with_the_same_spelling_does_not_matter_where_the_volume_does_not_fold() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.hold(OTHER_ID, "projects/A", EntryState::Live);
        let probe = |_: &TreeRoot| Ok(TargetRules::linux());
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };

        unearth_in(&fx, &fx.request(ID), &env).expect("works");
    }

    #[test]
    fn an_entry_of_this_object_or_a_gone_one_is_no_claim_on_the_target() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        fx.archived(OTHER_ID, "projects/A", FILES);
        let probe = |_: &TreeRoot| Ok(TargetRules::macos());
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &plenty,
            rename: &real_move,
        };

        unearth_in(&fx, &fx.request(ID), &env).expect("works");
    }

    #[test]
    fn a_target_name_past_the_name_limit_is_refused() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);

        let error = with_rules(
            &fx,
            &format!("projects/{}", "n".repeat(256)),
            TargetRules::linux(),
        );

        assert!(error.to_string().contains("255"), "{error}");
    }

    #[test]
    fn a_target_name_that_is_not_utf8_is_refused_where_the_volume_requires_it() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let rules = TargetRules {
            requires_utf8: true,
            ..TargetRules::linux()
        };

        let error = with_rules(&fx, "projects/a%FF", rules);

        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[test]
    fn a_tree_that_differs_from_the_archive_after_extraction_moves_nothing() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let staged_file = fx.collection.staging_dir().join(ID).join("notes.txt");
        let after_unpack = |_: &TreeRoot| fs::write(&staged_file, "HELLO").expect("an edit");
        let packager = Wrap {
            after_unpack: &after_unpack,
            ..untouched()
        };

        let error = unearth_with(&fx, &fx.request(ID), &packager).expect_err("refused");

        assert!(
            matches!(&error, UnearthError::Pack(PackError::Differs(_))),
            "{error}"
        );
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn a_target_that_appears_after_the_verify_is_refused_and_nothing_moves() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        let calls = Cell::new(0);
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            calls.set(calls.get() + 1);
            move_dir(from_root, from, to_root, to)
        };
        let after_verify = || {
            fs::create_dir_all(fx.at("projects/new/c")).expect("the target appears");
        };
        let packager = Wrap {
            after_verify: &after_verify,
            ..untouched()
        };
        let env = Environment {
            packager: &packager,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };
        let request = UnearthRequest {
            to: Some(path("projects/new/c")),
            ..fx.request(ID)
        };

        let error = unearth_in(&fx, &request, &env).expect_err("refused");

        assert!(
            matches!(
                &error,
                UnearthError::Target {
                    problem: TargetProblem::Taken,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(calls.get(), 0, "the tree was not moved");
        assert!(staging_is_empty(&fx));
        assert_eq!(fx.entry(ID).state, EntryState::Gone);
        assert_eq!(
            fs::read_dir(fx.at("projects/new/c"))
                .expect("lists")
                .count(),
            0
        );
    }

    #[test]
    fn a_symlink_that_appears_above_the_target_after_the_verify_is_refused() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        let outside = tempfile::tempdir().expect("a folder outside");
        let calls = Cell::new(0);
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            calls.set(calls.get() + 1);
            move_dir(from_root, from, to_root, to)
        };
        let after_verify = || {
            symlink(outside.path(), fx.at("projects/new")).expect("a link appears");
        };
        let packager = Wrap {
            after_verify: &after_verify,
            ..untouched()
        };
        let env = Environment {
            packager: &packager,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };
        let request = UnearthRequest {
            to: Some(path("projects/new/c")),
            ..fx.request(ID)
        };

        let error = unearth_in(&fx, &request, &env).expect_err("refused");

        assert!(
            matches!(
                &error,
                UnearthError::Target {
                    problem: TargetProblem::NotAFolder { .. },
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(calls.get(), 0);
        assert_eq!(fs::read_dir(outside.path()).expect("lists").count(), 0);
    }

    #[test]
    fn a_failed_move_removes_the_made_parents_and_restores_the_entry() {
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*/*".into()];
        });
        fx.archived(ID, "projects/b/x/a", FILES);
        let rename = |_: &TreeRoot, from: &RelPath, _: &TreeRoot, to: &RelPath| {
            Err(FsxError::CrossesDevices {
                from: from.to_string(),
                to: to.to_string(),
            })
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };
        let request = UnearthRequest {
            to: Some(path("projects/new/deeper/c")),
            ..fx.request(ID)
        };

        let error = unearth_in(&fx, &request, &env).expect_err("fails");

        assert!(
            matches!(&error, UnearthError::Fsx(FsxError::CrossesDevices { .. })),
            "{error}"
        );
        assert!(!fx.at("projects/new").exists(), "the made parents are gone");
        assert!(staging_is_empty(&fx));
        assert_eq!(fx.entry(ID).state, EntryState::Gone);
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn a_failed_move_without_an_earlier_entry_leaves_none() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("removed");
        let rename = |_: &TreeRoot, from: &RelPath, _: &TreeRoot, to: &RelPath| {
            Err(FsxError::CrossesDevices {
                from: from.to_string(),
                to: to.to_string(),
            })
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };

        unearth_in(&fx, &fx.request(ID), &env).expect_err("fails");

        assert!(
            tracking::read(&fx.collection, id(ID))
                .expect("reads")
                .is_none()
        );
    }

    #[test]
    fn a_parent_that_cannot_be_made_is_an_error_and_leaves_no_folder() {
        use std::os::unix::fs::PermissionsExt;
        let mut fx = Fixture::configured(|config| {
            config.collection.patterns = vec!["projects/*/*".into()];
        });
        fx.archived(ID, "projects/b/a", FILES);
        let projects = fx.at("projects");
        let _restore = crate::fsx::RestoreModes(vec![projects.clone()]);
        let after_verify = || {
            fs::set_permissions(&projects, fs::Permissions::from_mode(0o500)).expect("chmod");
        };
        let packager = Wrap {
            after_verify: &after_verify,
            ..untouched()
        };
        let request = UnearthRequest {
            to: Some(path("projects/new/c")),
            ..fx.request(ID)
        };

        let error = unearth_with(&fx, &request, &packager).expect_err("fails");

        assert!(matches!(error, UnearthError::Io { .. }), "{error}");
        assert_eq!(fx.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_mode_that_cannot_be_put_back_is_a_warning() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let rename = |from_root: &TreeRoot, from: &RelPath, to_root: &TreeRoot, to: &RelPath| {
            // The move itself is real; only its report is made up.
            let _real = move_dir(from_root, from, to_root, to)?;
            Ok(Moved {
                mode_restore_error: Some(FsxError::Io {
                    action: "restore the mode of",
                    path: to.to_string(),
                    source: io::Error::other("refused by the test"),
                }),
            })
        };
        let env = Environment {
            packager: &TarZstd,
            probe: &real_probe,
            free_space: &plenty,
            rename: &rename,
        };

        let report = unearth_in(&fx, &fx.request(ID), &env).expect("works");

        assert!(report.unearthed);
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].message.contains("mode"),
            "{:?}",
            report.warnings
        );
    }

    // ---------------------------------------------------------------------
    // Tops without owner write
    // ---------------------------------------------------------------------

    #[test]
    fn a_top_without_owner_write_arrives_with_its_mode() {
        for mode in [0o555, 0o500] {
            let mut fx = Fixture::new();
            fx.archive_package(
                ID,
                "projects/a",
                &from_tar(&[container(mode), RawMember::file(b"a/f", b"1")]),
            );
            let _restore = crate::fsx::RestoreModes(vec![fx.at("projects/a")]);

            unearth(&fx);

            let top = fs::metadata(fx.at("projects/a")).expect("the top");
            assert_eq!(top.mode() & 0o7777, mode, "{mode:o}");
        }
    }

    // ---------------------------------------------------------------------
    // Too little room
    // ---------------------------------------------------------------------

    #[test]
    fn too_little_space_skips_the_piece_before_staging_is_made() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let probed = Cell::new(false);
        let probe = |root: &TreeRoot| {
            probed.set(true);
            TargetRules::probe(root)
        };
        let scarce = |_: &Path| Ok(10);
        let env = Environment {
            packager: &TarZstd,
            probe: &probe,
            free_space: &scarce,
            rename: &real_move,
        };

        let report = unearth_in(&fx, &fx.request(ID), &env).expect("skipped");

        assert!(!report.unearthed);
        assert_eq!(
            report.skipped,
            Some(Skipped::NoSpace {
                needed: 17,
                available: 10
            })
        );
        assert!(crate::report::CommandReport::partial(&report));
        assert!(!probed.get(), "staging was never made");
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn a_volume_that_fills_up_during_the_extraction_skips_the_piece() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let full = || {
            Some(PackError::Extract {
                path: path("notes.txt"),
                source: io::Error::from_raw_os_error(libc::ENOSPC),
            })
        };
        let packager = Wrap {
            unpack_error: &full,
            ..untouched()
        };

        let report = unearth_with(&fx, &fx.request(ID), &packager).expect("skipped");

        assert!(!report.unearthed);
        assert!(matches!(
            report.skipped,
            Some(Skipped::NoSpace { needed: 17, .. })
        ));
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn another_failure_of_the_extraction_is_an_error() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let broken = || {
            Some(PackError::Extract {
                path: path("notes.txt"),
                source: io::Error::from_raw_os_error(libc::EIO),
            })
        };
        let packager = Wrap {
            unpack_error: &broken,
            ..untouched()
        };

        let error = unearth_with(&fx, &fx.request(ID), &packager).expect_err("fails");

        assert!(matches!(error, UnearthError::Pack(_)), "{error}");
        assert_nothing_changed(&fx, "projects/a");
    }

    #[test]
    fn an_error_without_a_cause_is_no_full_volume() {
        assert!(!is_out_of_space(&PackError::NoMembers));
    }

    #[test]
    fn the_real_environment_unearths() {
        let mut fx = Fixture::new();
        fx.archived(ID, "projects/a", FILES);
        let plan = fx.planned(&fx.request(ID));

        let report = unearth_archive(
            &fx.collection,
            &plan,
            &fx.lock,
            &fx.clock,
            &Environment::real(),
            vec![Warning::new("from the start of the run")],
            None,
        )
        .expect("works");

        assert!(report.unearthed);
        assert_eq!(report.warnings.len(), 1);
    }
}
