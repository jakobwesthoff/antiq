// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Steps 8 to 10: the folder of a stored piece leaves its path.
//!
//! The folder is renamed into the trash and walked again there. Equal to
//! what was verified, it is deleted. Anything else was written after the
//! verify and is not in the archive, so the tree goes back to its path as
//! an unearthed piece, or into `recovered/` when the path is taken. Every
//! step leaves a state the start-of-run repairs finish (spec "States").

use super::{Flow, Piece, Stop, chain};
use crate::clock::whole_seconds;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::ops::archive::durable::DurableArchive;
use crate::ops::archive::{Kept, KeptHow, Step};
use crate::pack::verify::LstatRecord;
use crate::relpath::RelPath;
use crate::repair::{Outcome, move_trashed, recovered_name};
use crate::report::Warning;
use crate::select::Rules;
use crate::tracking::{self, EntryState, TrackingEntry};
use crate::tree::{PieceTree, Problem};
use crate::walk;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// How a stored piece ended.
pub(super) enum Finished {
    /// The folder is deleted.
    Removed,
    /// The folder, or a copy of it, is still there.
    Kept(Kept),
}

/// The first way the tree differs from the `lstat` record of the verify,
/// or `None` when it is the same.
///
/// The top compares its type, inode and modification time only: moving the
/// piece changes its ctime. Directory times count for everything below the
/// top, so a `.DS_Store` written between the verify and the move sends the
/// piece back as unearthed, which loses nothing.
pub(super) fn differs(record: &[LstatRecord], tree: &PieceTree) -> Option<String> {
    let recorded: HashMap<&RelPath, &LstatRecord> =
        record.iter().map(|item| (&item.path, item)).collect();
    let mut seen = HashSet::new();
    for now in tree.entries() {
        seen.insert(&now.path);
        let Some(before) = recorded.get(&now.path) else {
            return Some(format!("{} is new", shown(&now.path)));
        };
        let same = if now.path.is_root() {
            (now.kind, now.inode, now.mtime_ns) == (before.kind, before.inode, before.mtime_ns)
        } else {
            (now.kind, now.size, now.mtime_ns, now.inode, now.ctime_ns)
                == (
                    before.kind,
                    before.size,
                    before.mtime_ns,
                    before.inode,
                    before.ctime_ns,
                )
        };
        if !same {
            return Some(format!("{} changed", shown(&now.path)));
        }
    }
    record
        .iter()
        .find(|item| !seen.contains(&item.path))
        .map(|item| format!("{} is gone", shown(&item.path)))
}

fn shown(path: &RelPath) -> String {
    if path.is_root() {
        "the folder itself".to_string()
    } else {
        format!("`{path}`")
    }
}

fn describe(problem: &Problem) -> String {
    match problem {
        Problem::Unreadable { first } => format!("`{first}` cannot be read"),
        Problem::MountPoint { other } => format!("another filesystem is mounted at `{other}`"),
        Problem::Excluded { path } => format!("`{path}` is on the exclude list"),
        Problem::NestedCollection { root } => format!("`{root}` is a collection of its own"),
    }
}

/// Why a walked tree is not the one that was verified, if it is not. A
/// walk that failed or found any problem is never equal.
fn difference(record: &[LstatRecord], walked: Result<PieceTree, String>) -> Option<String> {
    match walked {
        Err(error) => Some(error),
        Ok(tree) => match tree.problems().first() {
            Some(problem) => Some(describe(problem)),
            None => differs(record, &tree),
        },
    }
}

fn trash_name(id: ObjectId) -> RelPath {
    RelPath::parse(&id.to_string()).expect("an ID is a plain name")
}

impl Piece<'_, '_> {
    fn seam(&self, step: Step) -> io::Result<()> {
        self.cx.seams.at(step, self.path)
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(Warning::new(message));
    }

    /// The directory that holds the folder, as a path.
    fn parent_dir(&self) -> PathBuf {
        let folder = self
            .cx
            .collection
            .root()
            .join(OsStr::from_bytes(self.path.as_bytes()));
        folder
            .parent()
            .unwrap_or(self.cx.collection.root())
            .to_path_buf()
    }

    /// What is left to the repairs of the next run: the entry stays
    /// `removing`.
    fn left_for_the_repairs(&mut self, what: &str, reason: &str) -> Stop {
        let message = format!("{what}: {reason}; the entry stays removing");
        self.warn(message.clone());
        Stop::error(message)
    }

    pub(super) fn remove_folder(
        &mut self,
        durable: &DurableArchive,
        mut entry: TrackingEntry,
    ) -> Flow<Finished> {
        if let Err(refusal) = self.move_to_trash(durable) {
            return self.remove_in_place(durable, &mut entry, &refusal);
        }
        match self.rewalk(durable) {
            None => self.delete_trashed(durable, &mut entry),
            Some(reason) => self.give_back(durable, &mut entry, &reason),
        }
    }

    // =====================================================
    // Step 8: the rename into the trash
    // =====================================================

    /// The error is why the rename could not happen. A flush that fails
    /// after it is a warning: the archive is durable, and a rename that
    /// is lost puts the folder back at its path.
    fn move_to_trash(&mut self, durable: &DurableArchive) -> Result<(), String> {
        let cx = self.cx;
        self.seam(Step::MoveToTrash).map_err(|e| chain(&e))?;
        let trash_dir = cx.collection.trash_dir();
        let trash = TreeRoot::open(&trash_dir).map_err(|e| chain(&e))?;
        let effects = cx.seams.effects();
        let moved = effects
            .move_dir(&cx.root, self.path, &trash, &trash_name(durable.id()))
            .map_err(|e| chain(&e))?;
        if let Some(error) = moved.mode_restore_error {
            self.warn(format!("cannot restore the mode of a moved tree: {error}"));
        }
        for dir in [self.parent_dir(), trash_dir] {
            if let Err(error) = effects.flush_dir(&dir) {
                self.warn(format!(
                    "`{}` is in the trash but the move cannot be flushed: {error}",
                    self.path
                ));
            }
        }
        Ok(())
    }

    /// The rename was refused. The folder is judged where it stands, and
    /// deleted there only when nothing changed since the verify.
    fn remove_in_place(
        &mut self,
        durable: &DurableArchive,
        entry: &mut TrackingEntry,
        refusal: &str,
    ) -> Flow<Finished> {
        let cx = self.cx;
        let walked = walk::walk_piece_with(
            &cx.root,
            self.path,
            &cx.junk,
            &cx.rules,
            cx.seams.walk_hooks(),
        )
        .map_err(|e| chain(&e));
        if let Some(reason) = difference(durable.record(), walked) {
            self.warn(format!(
                "`{}` could not be moved to the trash ({refusal}) and has changed since it was verified ({reason}); it is kept as unearthed",
                self.path
            ));
            return self.mark_unearthed(durable, entry, Step::GiveBackEntry);
        }
        let removed = self
            .seam(Step::DeleteInPlace)
            .map_err(|e| chain(&e))
            .and_then(|()| {
                cx.seams
                    .effects()
                    .remove_tree(&cx.root, self.path)
                    .map_err(|e| chain(&e))
            });
        if let Err(reason) = removed {
            let what = format!(
                "`{}` could not be moved to the trash ({refusal}) and cannot be deleted",
                self.path
            );
            return Err(self.left_for_the_repairs(&what, &reason));
        }
        self.mark_gone(entry)
    }

    // =====================================================
    // Step 9: the trashed tree is walked again
    // =====================================================

    /// Why the trashed tree is not the verified one, or `None` when it is.
    fn rewalk(&mut self, durable: &DurableArchive) -> Option<String> {
        if let Err(error) = self.seam(Step::Rewalk) {
            return Some(chain(&error));
        }
        let walked = self.walk_trashed(durable.id());
        difference(durable.record(), walked)
    }

    /// The tree is walked as a piece of the trash directory, whose rules
    /// exclude nothing: whatever the folder holds now is compared.
    fn walk_trashed(&self, id: ObjectId) -> Result<PieceTree, String> {
        let cx = self.cx;
        let trash = TreeRoot::open(&cx.collection.trash_dir()).map_err(|e| chain(&e))?;
        let rules = Rules::new(&[], &[])
            .and_then(|rules| rules.bind(&trash))
            .map_err(|e| chain(&e))?;
        walk::walk_piece_with(
            &trash,
            &trash_name(id),
            &cx.junk,
            &rules,
            cx.seams.walk_hooks(),
        )
        .map_err(|e| chain(&e))
    }

    // =====================================================
    // Step 10: the tree equals the archive
    // =====================================================

    fn delete_trashed(
        &mut self,
        durable: &DurableArchive,
        entry: &mut TrackingEntry,
    ) -> Flow<Finished> {
        let cx = self.cx;
        let effects = cx.seams.effects();
        let trash = cx.collection.trash_path(durable.id());
        let removed = self
            .seam(Step::DeleteInTrash)
            .map_err(|e| chain(&e))
            .and_then(|()| effects.remove_tree_at(&trash).map_err(|e| chain(&e)));
        if let Err(reason) = removed {
            let what = format!("cannot delete the trashed tree of `{}`", self.path);
            return Err(self.left_for_the_repairs(&what, &reason));
        }
        if let Err(error) = effects.flush_dir(&cx.collection.trash_dir()) {
            self.warn(format!(
                "the trashed tree of `{}` is deleted but the trash cannot be flushed: {error}",
                self.path
            ));
        }
        self.mark_gone(entry)
    }

    /// The archive is complete, so a `gone` entry that does not reach the
    /// disk is the repairs' to write: the piece counts as archived.
    fn mark_gone(&mut self, entry: &mut TrackingEntry) -> Flow<Finished> {
        entry.state = EntryState::Gone;
        entry.removing_payload = None;
        if let Err(reason) = self.write_at(Step::MarkGone, entry) {
            self.warn(format!(
                "cannot mark `{}` gone ({reason}); the entry stays removing",
                self.path
            ));
        }
        Ok(Finished::Removed)
    }

    // =====================================================
    // A tree that is not the archive
    // =====================================================

    /// The tree goes back to its path, or into `recovered/` when it
    /// cannot.
    fn give_back(
        &mut self,
        durable: &DurableArchive,
        entry: &mut TrackingEntry,
        reason: &str,
    ) -> Flow<Finished> {
        let cx = self.cx;
        let outcome = match self.seam(Step::GiveBackMove) {
            Err(error) => Outcome::Refused(chain(&error)),
            Ok(()) => {
                let (outcome, warnings) = move_trashed(
                    cx.seams.effects(),
                    cx.collection,
                    durable.id(),
                    cx.collection.root(),
                    self.path,
                    &self.parent_dir(),
                );
                self.warnings.extend(warnings);
                outcome
            }
        };
        match outcome {
            Outcome::Done => {
                self.warn(format!(
                    "`{}` changed after it was verified ({reason}); it is kept as unearthed",
                    self.path
                ));
                self.mark_unearthed(durable, entry, Step::GiveBackEntry)
            }
            Outcome::Stuck => Err(Stop::error(
                "the tree went back to its path but the move cannot be flushed; the entry stays removing",
            )),
            Outcome::Refused(refusal) => self.recover(durable, entry, reason, &refusal),
        }
    }

    fn recover(
        &mut self,
        durable: &DurableArchive,
        entry: &mut TrackingEntry,
        reason: &str,
        refusal: &str,
    ) -> Flow<Finished> {
        let cx = self.cx;
        let name = recovered_name(durable.id(), whole_seconds(cx.clock.now()));
        let to = RelPath::parse(&name).expect("an ID and a time make a plain name");
        let recovered = cx.collection.recovered_dir();
        let outcome = match self.seam(Step::MoveRecovered) {
            Err(error) => Outcome::Refused(chain(&error)),
            Ok(()) => {
                let (outcome, warnings) = move_trashed(
                    cx.seams.effects(),
                    cx.collection,
                    durable.id(),
                    &recovered,
                    &to,
                    &recovered,
                );
                self.warnings.extend(warnings);
                outcome
            }
        };
        match outcome {
            Outcome::Done => {
                self.warn(format!(
                    "`{}` changed after it was verified ({reason}) and could not go back ({refusal}); it is kept in .antiq/recovered/{name}",
                    self.path
                ));
                entry.state = EntryState::Gone;
                entry.removing_payload = None;
                if let Err(written) = self.write_at(Step::RecoveredEntry, entry) {
                    let what = format!("cannot mark `{}` gone", self.path);
                    return Err(self.left_for_the_repairs(&what, &written));
                }
                Ok(Finished::Kept(Kept {
                    id: durable.id(),
                    path: self.path.clone(),
                    how: KeptHow::Recovered,
                    recovered_as: Some(name),
                }))
            }
            Outcome::Stuck => Err(Stop::error(
                "the tree moved to recovered/ but the move cannot be flushed; the entry stays removing",
            )),
            Outcome::Refused(second) => {
                let what = format!(
                    "the trashed tree of `{}` changed after it was verified ({reason}) and could not be moved ({refusal}; {second}); it stays in the trash",
                    self.path
                );
                Err(self.left_for_the_repairs(&what, "no other place took it"))
            }
        }
    }

    /// The folder is at its path and the archive stays as an unearthed
    /// piece's source.
    fn mark_unearthed(
        &mut self,
        durable: &DurableArchive,
        entry: &mut TrackingEntry,
        step: Step,
    ) -> Flow<Finished> {
        entry.state = EntryState::Live;
        entry.removing_payload = None;
        entry.unearthed_payload_version = Some(durable.payload_version());
        entry.unearthed = Some(whole_seconds(self.cx.clock.now()));
        if let Err(reason) = self.write_at(step, entry) {
            let what = format!("cannot mark `{}` unearthed", self.path);
            return Err(self.left_for_the_repairs(&what, &reason));
        }
        Ok(Finished::Kept(Kept {
            id: durable.id(),
            path: self.path.clone(),
            how: KeptHow::Unearthed,
            recovered_as: None,
        }))
    }

    fn write_at(&self, step: Step, entry: &mut TrackingEntry) -> Result<(), String> {
        let cx = self.cx;
        self.seam(step).map_err(|e| chain(&e))?;
        tracking::write(cx.collection, cx.lock, cx.clock, entry).map_err(|e| chain(&e))
    }
}
