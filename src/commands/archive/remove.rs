// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Steps 5 to 7 of `archive`: the folder of a stored piece leaves its path.

use super::seams::Seams;
use super::store::{remove_staging, staging_chain};
use super::{ArchiveError, chain, went};
use crate::archive::Verified;
use crate::archive::verify::LstatRecord;
use crate::collection::Collection;
use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, create_dir_chain, flush_dir_of};
use crate::filesystem::io_error;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::{Name, RelPath};
use crate::repair::{Destination, flush_dir_unless_gone, give_back as repair_give_back};
use crate::report::Warning;
use crate::rules::PieceRules;
use crate::select::{Rules, SelectError};
use crate::tree::{PieceTree, Problem};
use crate::versions::names::VersionFileNames;
use crate::versions::stamp::Stamp;
use crate::versions::store::{move_back_with, remove_empty_chain_with};
use crate::walk::walk_piece_with;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// What the steps share with the pieces running beside them.
pub(crate) struct RemoveContext<'a> {
    pub collection: &'a Collection,
    pub seams: &'a dyn Seams,
    /// Held while a chain below `trash/` or `doomed/` is created, a folder
    /// is renamed into one or an empty chain is removed, so a piece that
    /// finishes cannot remove a `<stamp>/` folder that another piece is
    /// about to use.
    pub run_dirs: &'a Mutex<()>,
}

/// The piece whose folder goes.
pub(crate) struct RemoveRequest<'a> {
    /// The folder as it lies in the collection.
    pub live: &'a RelPath,
    /// The path of the store folder, in its stored spelling.
    pub stored: &'a RelPath,
    pub stamp: &'a Stamp,
    pub names: &'a VersionFileNames,
    pub verified: &'a Verified,
    pub rules: &'a PieceRules,
    /// The exclude list of the collection, relative to its root.
    pub exclude: &'a [RelPath],
}

/// How the folder ended.
pub(crate) enum Outcome {
    /// The folder is gone, and the version is in the store.
    Removed,
    /// The folder changed after the verify. The tree went back to its path
    /// or into `recovered/`; the version stays.
    Differs {
        destination: Destination,
        reason: String,
    },
    /// The re-walk ran out of file descriptors, so the tree could not be
    /// compared. It went back as a changed one does; the version stays.
    OutOfDescriptors {
        destination: Destination,
        cause: String,
    },
    /// The folder, or a copy of it, is where the error says and the
    /// repairs of the next run finish or undo the step.
    Kept(Box<ArchiveError>),
}

pub(crate) struct Removal {
    pub outcome: Outcome,
    pub warnings: Vec<Warning>,
}

pub(crate) fn remove_folder(context: &RemoveContext<'_>, request: &RemoveRequest<'_>) -> Removal {
    let mut warnings = Vec::new();
    let outcome = remove(context, request, &mut warnings);
    Removal { outcome, warnings }
}

/// The roots every step works in.
struct Roots {
    collection: TreeRoot,
    trash: TreeRoot,
    doomed: TreeRoot,
    antiq: TreeRoot,
}

fn open_roots(collection: &Collection) -> Result<Roots, FilesystemError> {
    let open = |dir: PathBuf| TreeRoot::open(&dir).map_err(io_error("open", &dir));
    Ok(Roots {
        collection: open(collection.root().to_path_buf())?,
        trash: open(collection.trash_dir())?,
        doomed: open(collection.doomed_dir())?,
        antiq: open(collection.antiq_dir())?,
    })
}

/// The path without its last component; the root for a single one.
fn parent_of(path: &RelPath) -> RelPath {
    let bytes = path.as_bytes();
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(split) => RelPath::from_bytes(bytes[..split].to_vec())
            .expect("the front of a valid path is valid"),
        None => RelPath::root(),
    }
}

/// Where the tree lies in `trash/` and `doomed/`, and the folders above it
/// that the steps create and remove.
struct Places {
    stamp: Name,
    /// `<stamp>/<piece>`
    tree: RelPath,
    /// `<stamp>/<parent of the piece>`
    above: RelPath,
}

fn remove(
    context: &RemoveContext<'_>,
    request: &RemoveRequest<'_>,
    warnings: &mut Vec<Warning>,
) -> Outcome {
    let piece = request.live.to_string();
    let stamp_text = request.stamp.to_string();
    let stamp = Name::from_bytes(stamp_text.clone().into_bytes()).expect("a stamp is a name");
    let places = Places {
        tree: RelPath::root().join(&stamp).concat(request.live),
        above: RelPath::root()
            .join(&stamp)
            .concat(&parent_of(request.live)),
        stamp,
    };
    let durable = context.seams.durable();
    let guard = || {
        context
            .run_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    };
    let warn = |warnings: &mut Vec<Warning>, text: String| {
        warnings.push(Warning::new(format!("{piece}: {text}")));
    };

    // =========================================================
    // Step 5: the rename into trash/
    // =========================================================

    let trash_error = |source| ArchiveError::Trash {
        piece: piece.clone(),
        stamp: stamp_text.clone(),
        source,
    };
    let roots = match open_roots(context.collection) {
        Ok(roots) => roots,
        Err(source) => return take_version_back(context, request, trash_error(source)),
    };
    let renamed = {
        let _guard = guard();
        create_dir_chain(durable, &roots.trash, &places.above).and_then(|()| {
            context
                .seams
                .move_dir(&roots.collection, request.live, &roots.trash, &places.tree)
        })
    };
    match renamed {
        Ok(moved) => {
            if let Some(error) = moved.mode_restore_error {
                warn(
                    warnings,
                    format!("cannot restore the mode of the moved tree: {error}"),
                );
            }
            // The version no longer goes back through its staging.
            remove_staging(
                context.run_dirs,
                &context.collection.tmp_dir(),
                &staging_chain(request.stamp, request.stored),
            );
        }
        Err(source) => {
            // The chain made for the rename stays empty when it is refused.
            {
                let _guard = guard();
                remove_empty(durable, &roots.trash, &places.above);
            }
            return take_version_back(context, request, trash_error(source));
        }
    }
    // The archive is durable. A rename that does not reach the disk puts
    // the folder back at its path, so a failed flush is only a warning.
    for (root, dir) in [
        (&roots.collection, parent_of(request.live)),
        (&roots.trash, places.above.clone()),
    ] {
        if let Err(error) = flush_dir_of(durable, root, &dir) {
            warn(
                warnings,
                format!("the move into trash/ cannot be flushed: {error}"),
            );
        }
    }
    let trashed = context.collection.trash_dir().join(path_of(&places.tree));
    context.seams.after_trash_rename(&trashed);

    // =========================================================
    // Step 6: walk the trashed tree again and compare it
    // =========================================================

    if let Some(mismatch) = rewalk(context, request, &roots, &places) {
        return give_back(context, request, &roots, &places, mismatch, warnings);
    }

    // =========================================================
    // Step 7: move the tree to doomed/ and delete it there
    // =========================================================

    let doomed_error = |source| ArchiveError::Doomed {
        piece: piece.clone(),
        stamp: stamp_text.clone(),
        source,
    };
    // The flushes are under the lock as well: once the tree has left
    // `trash/<stamp>/..`, another piece may remove that chain if it is empty.
    let moved = {
        let _guard = guard();
        create_dir_chain(durable, &roots.doomed, &places.above)
            .and_then(|()| {
                context
                    .seams
                    .move_dir(&roots.trash, &places.tree, &roots.doomed, &places.tree)
            })
            .map(|moved| {
                let flushed = flush_dir_of(durable, &roots.doomed, &places.above)
                    .and_then(|()| flush_dir_of(durable, &roots.trash, &places.above));
                remove_empty(durable, &roots.trash, &places.above);
                (moved, flushed)
            })
    };
    match moved {
        Ok((moved, flushed)) => {
            if let Some(error) = moved.mode_restore_error {
                warn(
                    warnings,
                    format!("cannot restore the mode of the moved tree: {error}"),
                );
            }
            if let Err(source) = flushed {
                // The tree lies in doomed/ and is not deleted: whether the
                // rename survives a crash is unknown, and repair 3 deletes it.
                return Outcome::Kept(Box::new(doomed_error(source)));
            }
        }
        Err(source) => {
            let reason = format!("cannot move {piece} into doomed/{stamp_text}: {source}");
            return match give_back(
                context,
                request,
                &roots,
                &places,
                Mismatch::Differs(reason),
                warnings,
            ) {
                Outcome::Differs { destination, .. } => {
                    Outcome::Kept(Box::new(ArchiveError::DoomedRefused {
                        piece: piece.clone(),
                        stamp: stamp_text.clone(),
                        went: went(destination, request.stamp, request.live),
                        source: Box::new(source),
                    }))
                }
                kept => kept,
            };
        }
    }

    if let Err(source) = context.seams.remove(&roots.doomed, &places.tree) {
        return Outcome::Kept(Box::new(ArchiveError::Delete {
            piece: piece.clone(),
            stamp: stamp_text.clone(),
            source,
        }));
    }
    let _guard = guard();
    if let Err(source) = flush_dir_unless_gone(durable, &roots.doomed, &places.above) {
        warn(
            warnings,
            format!("the deletion cannot be flushed: {source}"),
        );
    }
    remove_empty(durable, &roots.doomed, &places.above);
    Outcome::Removed
}

/// Removes the empty folders of `chain` below `root`, deepest first. A
/// folder that is not empty ends it: another piece uses it. A failure is
/// not reported: repairs 3 and 4 remove the empty chains of `doomed/` and
/// `trash/` at the start of the next run.
fn remove_empty(durable: &dyn Durable, root: &TreeRoot, chain: &RelPath) {
    let _ = remove_empty_chain_with(durable, root, chain);
}

/// Step 5 did not happen. The version leaves the store again, so the
/// folder is not the only copy that a repair or a later run has to
/// reckon with.
fn take_version_back(
    context: &RemoveContext<'_>,
    request: &RemoveRequest<'_>,
    cause: ArchiveError,
) -> Outcome {
    let piece = request.live.to_string();
    let stamp_text = request.stamp.to_string();
    let durable = context.seams.durable();
    let failed = |source: Box<dyn std::error::Error + Send + Sync>| {
        Outcome::Kept(Box::new(ArchiveError::Rollback {
            piece: piece.clone(),
            stamp: stamp_text.clone(),
            cause: chain(&cause),
            source,
        }))
    };

    let tmp_dir = context.collection.tmp_dir();
    let chain = staging_chain(request.stamp, request.stored);
    let tmp = match TreeRoot::open(&tmp_dir) {
        Ok(tmp) => tmp,
        Err(source) => return failed(Box::new(io_error("open", &tmp_dir)(source))),
    };
    let pieces_dir = context.collection.pieces_dir();
    let back = TreeRoot::open(&pieces_dir)
        .map_err(io_error("open", &pieces_dir))
        .map_err(|source| Box::new(source) as Box<dyn std::error::Error + Send + Sync>)
        .and_then(|pieces| {
            let staging = tmp
                .sub(&chain)
                .map_err(io_error("open", &tmp_dir.join(path_of(&chain))))?;
            move_back_with(
                durable,
                &pieces,
                &staging,
                request.stored,
                request.stored,
                request.stamp,
                request.names,
            )
            .map_err(Into::into)
        });
    if let Err(source) = back {
        return failed(source);
    }
    remove_staging(context.run_dirs, &tmp_dir, &chain);
    Outcome::Kept(Box::new(cause))
}

/// Why the trashed tree is not the verified one.
enum Mismatch {
    /// The tree differs from the verified one, or cannot be walked.
    Differs(String),
    /// The tree could not be read for lack of file descriptors.
    OutOfDescriptors(String),
}

/// Why the trashed tree is not the verified one, or `None` when it is.
fn rewalk(
    context: &RemoveContext<'_>,
    request: &RemoveRequest<'_>,
    roots: &Roots,
    places: &Places,
) -> Option<Mismatch> {
    // The exclude list applies to the tree where it lies in trash/.
    let exclude: Vec<RelPath> = request
        .exclude
        .iter()
        .map(|path| RelPath::root().join(&places.stamp).concat(path))
        .collect();
    let select = match Rules::new(&[], &exclude)
        .and_then(|rules| rules.bind_with(&roots.trash, context.seams.walk_hooks()))
    {
        Ok(select) => select,
        Err(error @ SelectError::OutOfDescriptors { .. }) => {
            return Some(Mismatch::OutOfDescriptors(error.to_string()));
        }
        Err(error) => return Some(Mismatch::Differs(error.to_string())),
    };
    match walk_piece_with(
        &roots.trash,
        &places.tree,
        &select,
        request.rules,
        context.seams.walk_hooks(),
    ) {
        Err(error) => Some(Mismatch::OutOfDescriptors(error.to_string())),
        Ok(tree) => match tree.problems().first() {
            Some(problem) => Some(Mismatch::Differs(describe(problem))),
            None => differs(request.verified.record(), &tree).map(Mismatch::Differs),
        },
    }
}

/// The tree changed after the verify, so it is not what the archive holds.
/// It goes back to its path, or into `recovered/`, and the version stays.
fn give_back(
    context: &RemoveContext<'_>,
    request: &RemoveRequest<'_>,
    roots: &Roots,
    places: &Places,
    mismatch: Mismatch,
    warnings: &mut Vec<Warning>,
) -> Outcome {
    let durable = context.seams.durable();
    let given = repair_give_back(
        &roots.trash,
        &roots.collection,
        &roots.antiq,
        Some(&places.stamp),
        request.live,
        request.live,
        durable,
    );
    match given {
        Ok(given) => {
            if let Some(note) = given.note {
                warnings.push(Warning::new(format!("{}: {note}", request.live)));
            }
            let _guard = context
                .run_dirs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            remove_empty(durable, &roots.trash, &places.above);
            match mismatch {
                Mismatch::Differs(reason) => Outcome::Differs {
                    destination: given.destination,
                    reason,
                },
                Mismatch::OutOfDescriptors(cause) => Outcome::OutOfDescriptors {
                    destination: given.destination,
                    cause,
                },
            }
        }
        // The tree stays in trash/, where repair 4 gives it back.
        Err(source) => Outcome::Kept(Box::new(ArchiveError::GiveBack {
            piece: request.live.to_string(),
            stamp: request.stamp.to_string(),
            reason: match mismatch {
                Mismatch::Differs(reason) | Mismatch::OutOfDescriptors(reason) => reason,
            },
            source: Box::new(source),
        })),
    }
}

fn path_of(path: &RelPath) -> &Path {
    Path::new(OsStr::from_bytes(path.as_bytes()))
}

fn describe(problem: &Problem) -> String {
    match problem {
        Problem::Unreadable { first } => format!("`{first}` cannot be read"),
        Problem::MountPoint { other } => format!("another filesystem is mounted at `{other}`"),
        Problem::Excluded { path } => format!("`{path}` is on the exclude list"),
        Problem::NestedCollection { root } => format!("`{root}` is a collection of its own"),
        Problem::Locked { path } => format!("`{path}` is locked"),
        Problem::LockedParent { .. } => "the folder above it is locked".to_string(),
        Problem::RuleFile { path, .. } => format!("the rule file `{path}` cannot be used"),
        Problem::MarkerNotAFile { name } => format!("`{name}` is not a file"),
    }
}

/// The first way the tree differs from the `lstat` record of the verify,
/// or `None` when it is the same.
///
/// The top compares its type, inode and modification time only: moving the
/// piece changes its ctime. Directory times count for everything below the
/// top, so a `.DS_Store` written between the verify and the move sends the
/// piece back, which loses nothing.
pub(super) fn differs(record: &[LstatRecord], tree: &PieceTree) -> Option<String> {
    let recorded: HashMap<&RelPath, &LstatRecord> =
        record.iter().map(|item| (&item.path, item)).collect();
    let mut seen = HashSet::new();
    for now in tree.packed_entries() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::zstd::ZstdSettings;
    use crate::clock::FixedClock;
    use crate::commands::archive::seams::RealSeams;
    use crate::commands::archive::store::{StoreContext, StoreRequest, Stored, store_version};
    use crate::filesystem::FilesystemError;
    use crate::filesystem::atomic::recording::Recording;
    use crate::filesystem::atomic::{Durable, RealDurable};
    use crate::filesystem::io_error;
    use crate::filesystem::moves::Moved;
    use crate::filesystem::space::SpaceLedger;
    use crate::filesystem::tree::TreeRoot;
    use crate::select::Rules;
    use crate::testing::shared_pool;
    use crate::testing::tree::TempCollection;
    use crate::tree::{Class, EntryKind, TreeEntry};
    use crate::versions::names::VersionFile;
    use crate::walk::{Call, Hooks, walk_piece};
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const WHEN: &str = "2026-11-04T09:02:11Z";
    const LONG_AGO: i64 = 1_700_000_000;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    type Change = Box<dyn Fn(&Path) + Send + Sync>;

    /// What a test changes about the real run.
    #[derive(Default)]
    struct Probe {
        recording: Option<Recording>,
        after_trash_rename: Option<Change>,
        mount_at: Option<RelPath>,
        failing_delete: bool,
        /// Every move reports that the mode could not be restored.
        restore_fails: bool,
        /// Every `lstat` of the walk and of the exclude list runs out of
        /// descriptors.
        out_of_descriptors: bool,
        /// The first move, the one into `trash/`, is refused.
        refuse_trash_move: bool,
        moves: AtomicUsize,
        /// Runs once the tree is deleted from `doomed/`.
        after_delete: Option<Box<dyn Fn() + Send + Sync>>,
    }

    impl Hooks for Probe {
        fn mount_root(&self, path: &RelPath, reported: bool) -> bool {
            self.mount_at.as_ref() == Some(path) || reported
        }

        fn fault(&self, _path: &RelPath, call: Call) -> Option<io::Error> {
            (self.out_of_descriptors && call == Call::Stat)
                .then(|| io::Error::from_raw_os_error(libc::EMFILE))
        }
    }

    impl Seams for Probe {
        fn durable(&self) -> &dyn Durable {
            match &self.recording {
                Some(recording) => recording,
                None => &RealDurable,
            }
        }

        fn after_trash_rename(&self, tree: &Path) {
            if let Some(change) = &self.after_trash_rename {
                change(tree);
            }
        }

        fn walk_hooks(&self) -> &dyn Hooks {
            self
        }

        fn move_dir(
            &self,
            from_root: &TreeRoot,
            from: &RelPath,
            to_root: &TreeRoot,
            to: &RelPath,
        ) -> Result<Moved, FilesystemError> {
            if self.refuse_trash_move && self.moves.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(io_error("rename", Path::new("piece"))(
                    io::Error::from_raw_os_error(libc::EACCES),
                ));
            }
            let moved = RealSeams.move_dir(from_root, from, to_root, to)?;
            if self.restore_fails {
                let source = io::Error::from_raw_os_error(libc::EPERM);
                return Ok(Moved {
                    mode_restore_error: Some(io_error("restore the mode of", Path::new("moved"))(
                        source,
                    )),
                });
            }
            Ok(moved)
        }

        fn remove(&self, root: &TreeRoot, path: &RelPath) -> Result<(), FilesystemError> {
            if self.failing_delete {
                return Err(io_error("remove", Path::new(&path.to_string()))(
                    io::Error::from_raw_os_error(libc::EACCES),
                ));
            }
            RealSeams.remove(root, path)?;
            if let Some(change) = &self.after_delete {
                change();
            }
            Ok(())
        }
    }

    struct World {
        temp: TempCollection,
        clock: FixedClock,
        ledger: SpaceLedger,
        run_dirs: Mutex<()>,
    }

    /// A collection with the given pieces, each with two small files and
    /// an ignored `node_modules/`.
    fn world(pieces: &[&str]) -> World {
        let mut temp = TempCollection::minimal();
        for piece in pieces {
            temp.tree()
                .file_with(&format!("{piece}/a.txt"), "alpha", 0o644, LONG_AGO);
            temp.tree()
                .file_with(&format!("{piece}/sub/b.txt"), "beta", 0o600, LONG_AGO);
            temp.tree().file_with(
                &format!("{piece}/node_modules/x.js"),
                "ignored",
                0o644,
                LONG_AGO,
            );
        }
        World {
            temp,
            clock: FixedClock::new(WHEN.parse().expect("a time")),
            ledger: SpaceLedger::new(),
            run_dirs: Mutex::new(()),
        }
    }

    fn rules() -> PieceRules {
        PieceRules::new(&["node_modules/".to_string()], &[]).expect("valid rules")
    }

    fn collection(world: &World) -> Collection {
        Collection::find(world.temp.root()).expect("a collection")
    }

    /// Stores the version of `piece`, as the steps before the removal do.
    fn store(world: &World, piece: &str) -> Stored {
        let collection = collection(world);
        let rules = rules();
        let root = TreeRoot::open(world.temp.root()).expect("the root opens");
        let piece = path(piece);
        let selection = Rules::new(&[], &[]).expect("no patterns are valid");
        let tree = walk_piece(&root, &piece, &selection, &rules).expect("the walk succeeds");
        let zstd = |_workers: u32| ZstdSettings::fast();
        let context = StoreContext {
            collection: &collection,
            clock: &world.clock,
            pool: shared_pool(),
            ledger: &world.ledger,
            seams: &RealSeams,
            run_dirs: &world.run_dirs,
            zstd: &zstd,
        };
        let request = StoreRequest {
            live: &piece,
            stored: &piece,
            tree: &tree,
            rules: &rules,
            imported: None,
        };
        match store_version(&context, &request) {
            Ok(stored) => stored,
            Err(_) => panic!("the version is stored"),
        }
    }

    fn remove(
        world: &World,
        piece: &str,
        stored: &Stored,
        seams: &dyn Seams,
        exclude: &[RelPath],
    ) -> Removal {
        let collection = collection(world);
        let rules = rules();
        let piece = path(piece);
        let context = RemoveContext {
            collection: &collection,
            seams,
            run_dirs: &world.run_dirs,
        };
        let request = RemoveRequest {
            live: &piece,
            stored: &piece,
            stamp: &stored.stamp,
            names: &stored.names,
            verified: &stored.verified,
            rules: &rules,
            exclude,
        };
        remove_folder(&context, &request)
    }

    fn antiq(world: &World, below: &str) -> PathBuf {
        world.temp.root().join(".antiq").join(below)
    }

    fn entries(dir: &Path) -> Vec<String> {
        match fs::read_dir(dir) {
            Ok(read) => {
                let mut names: Vec<String> = read
                    .map(|entry| {
                        entry
                            .expect("an entry")
                            .file_name()
                            .to_string_lossy()
                            .into()
                    })
                    .collect();
                names.sort();
                names
            }
            Err(_) => Vec::new(),
        }
    }

    fn tree_changes(change: impl Fn(&Path) + Send + Sync + 'static) -> Probe {
        Probe {
            after_trash_rename: Some(Box::new(change)),
            ..Probe::default()
        }
    }

    fn expect_differs(removal: &Removal) -> (Destination, &str) {
        match &removal.outcome {
            Outcome::Differs {
                destination,
                reason,
            } => (*destination, reason),
            other => panic!("the folder differs, but {}", name_of(other)),
        }
    }

    fn expect_exhausted(removal: &Removal) -> (Destination, &str) {
        match &removal.outcome {
            Outcome::OutOfDescriptors { destination, cause } => (*destination, cause),
            other => panic!("the walk ran out of descriptors, but {}", name_of(other)),
        }
    }

    fn name_of(outcome: &Outcome) -> String {
        match outcome {
            Outcome::Removed => "the folder was removed".into(),
            Outcome::Differs { reason, .. } => format!("the folder differs: {reason}"),
            Outcome::OutOfDescriptors { cause, .. } => format!("out of descriptors: {cause}"),
            Outcome::Kept(error) => format!("{error:?}"),
        }
    }

    /// The OS error numbers on the chain of causes below `error`.
    fn os_errors(error: &(dyn std::error::Error + 'static)) -> Vec<i32> {
        let mut found = Vec::new();
        let mut link = Some(error);
        while let Some(cause) = link {
            if let Some(code) = cause
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error)
            {
                found.push(code);
            }
            link = cause.source();
        }
        found
    }

    fn expect_kept(removal: &Removal) -> &ArchiveError {
        match &removal.outcome {
            Outcome::Kept(error) => error,
            other => panic!("the folder is kept, but {}", name_of(other)),
        }
    }

    fn store_files(world: &World) -> Vec<String> {
        entries(&antiq(world, "pieces/piece"))
    }

    #[test]
    fn removes_the_folder_and_leaves_no_trace_in_trash_and_doomed() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");

        let removal = remove(&world, "piece", &stored, &RealSeams, &[]);

        assert!(
            matches!(removal.outcome, Outcome::Removed),
            "the folder is removed, but {}",
            name_of(&removal.outcome)
        );
        assert!(!world.temp.root().join("piece").exists());
        assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());
        assert_eq!(entries(&antiq(&world, "doomed")), Vec::<String>::new());
        assert_eq!(entries(&antiq(&world, "tmp/pack")), Vec::<String>::new());
        assert_eq!(store_files(&world).len(), 3, "the version stays");
    }

    #[test]
    fn a_refused_rename_takes_the_version_back_and_leaves_the_folder() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let target = antiq(&world, "trash")
            .join(stored.stamp.to_string())
            .join("piece/blocker");
        fs::create_dir_all(&target).expect("the target is made");

        let removal = remove(&world, "piece", &stored, &RealSeams, &[]);

        let error = expect_kept(&removal);
        assert_eq!(
            error.to_string(),
            format!("cannot move piece into trash/{}", stored.stamp)
        );
        assert_eq!(
            os_errors(error),
            [libc::EEXIST],
            "the error of the rename is on the chain"
        );
        assert!(world.temp.root().join("piece/a.txt").exists());
        assert_eq!(
            store_files(&world),
            Vec::<String>::new(),
            "the version is taken back"
        );
        assert_eq!(entries(&antiq(&world, "tmp/pack")), Vec::<String>::new());
    }

    #[test]
    fn a_refused_rename_leaves_no_empty_chain_in_trash() {
        let world = world(&["group/piece"]);
        let stored = store(&world, "group/piece");
        let seams = Probe {
            refuse_trash_move: true,
            ..Probe::default()
        };

        let removal = remove(&world, "group/piece", &stored, &seams, &[]);

        expect_kept(&removal);
        assert!(world.temp.root().join("group/piece/a.txt").exists());
        assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());
    }

    #[test]
    fn a_rollback_that_fails_too_keeps_the_version_and_names_both_causes() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let recording = Recording::new();
        recording.fail_dirs_from(0);
        let seams = Probe {
            recording: Some(recording),
            ..Probe::default()
        };

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let error = expect_kept(&removal);
        let text = error.to_string();
        assert!(
            text.contains("cannot put the version")
                && text.contains("cannot move piece into trash/")
                && text.contains("(os error 5)"),
            "both causes with their OS error: {text}"
        );
        assert_eq!(os_errors(error), [libc::EIO], "the cause of the rollback");
        let mut sidecars = vec![
            stored.names.of(VersionFile::Checksum).to_string(),
            stored.names.of(VersionFile::Listing).to_string(),
        ];
        sidecars.sort();
        assert_eq!(
            store_files(&world),
            sidecars,
            "the archive moved, its flush failed, the sidecars wait in the store"
        );
        let staged = antiq(&world, "tmp/pack")
            .join(stored.stamp.to_string())
            .join("piece");
        assert_eq!(
            entries(&staged),
            [stored.names.of(VersionFile::Archive)],
            "the archive waits in the staging for repair 1"
        );
        assert!(world.temp.root().join("piece/a.txt").exists());
    }

    #[test]
    fn a_change_after_the_verify_sends_the_folder_back_and_keeps_the_version() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = tree_changes(|tree| {
            fs::write(tree.join("a.txt"), "changed later").expect("the file is written");
        });

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let (destination, reason) = expect_differs(&removal);
        assert_eq!(destination, Destination::Live);
        assert!(reason.contains("a.txt"), "{reason}");
        assert_eq!(
            fs::read_to_string(world.temp.root().join("piece/a.txt")).expect("the file is back"),
            "changed later"
        );
        assert_eq!(store_files(&world).len(), 3);
        assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());
    }

    #[test]
    fn a_taken_path_sends_the_folder_to_recovered() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let root = world.temp.root().to_path_buf();
        let seams = tree_changes(move |tree| {
            fs::write(tree.join("a.txt"), "changed later").expect("the file is written");
            fs::create_dir(root.join("piece")).expect("the path is taken");
        });

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let (destination, _) = expect_differs(&removal);
        assert_eq!(destination, Destination::Recovered);
        let recovered = antiq(&world, "recovered")
            .join(stored.stamp.to_string())
            .join("piece/a.txt");
        assert!(recovered.exists(), "{}", recovered.display());
        assert_eq!(store_files(&world).len(), 3);
    }

    #[test]
    fn a_mount_point_in_an_ignored_subtree_counts_as_a_difference() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = Probe {
            mount_at: Some(path("node_modules")),
            ..Probe::default()
        };

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let (destination, reason) = expect_differs(&removal);
        assert_eq!(destination, Destination::Live);
        assert!(reason.contains("mounted"), "{reason}");
        assert!(world.temp.root().join("piece/a.txt").exists());
    }

    #[test]
    fn a_restore_marker_is_no_content_and_may_change_after_the_verify() {
        let world = world(&["piece"]);
        let marker = world.temp.root().join("piece/.antiq-restored");
        fs::write(&marker, "restored 2026-10-01T00:00:00Z\n").expect("the marker is written");
        let stored = store(&world, "piece");
        assert!(
            stored
                .verified
                .listing()
                .iter()
                .all(|entry| !entry.path.to_string().contains("antiq-restored")),
            "the marker is not packed"
        );
        let seams = tree_changes(|tree| {
            fs::write(
                tree.join(".antiq-restored"),
                "restored 2026-11-01T00:00:00Z\n",
            )
            .expect("the marker is rewritten");
        });

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        assert!(matches!(removal.outcome, Outcome::Removed));
        assert!(!world.temp.root().join("piece").exists());
        assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());
        assert_eq!(entries(&antiq(&world, "doomed")), Vec::<String>::new());
    }

    #[test]
    fn a_change_to_an_ignored_file_is_no_difference() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = tree_changes(|tree| {
            fs::write(tree.join("node_modules/x.js"), "ignored, changed")
                .expect("the file is written");
        });

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        assert!(matches!(removal.outcome, Outcome::Removed));
        assert!(!world.temp.root().join("piece").exists());
    }

    #[test]
    fn an_exclude_entry_created_in_an_ignored_subtree_after_the_verify_sends_the_folder_back() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = tree_changes(|tree| {
            fs::write(tree.join("node_modules/secret.txt"), "x").expect("the file is written");
        });

        let removal = remove(
            &world,
            "piece",
            &stored,
            &seams,
            &[path("piece/node_modules/secret.txt")],
        );

        let (destination, reason) = expect_differs(&removal);
        assert_eq!(destination, Destination::Live);
        assert!(reason.contains("exclude list"), "{reason}");
        assert!(
            world
                .temp
                .root()
                .join("piece/node_modules/secret.txt")
                .exists()
        );
    }

    #[test]
    fn a_failing_delete_leaves_the_tree_in_doomed_and_the_repair_finishes_it() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = Probe {
            failing_delete: true,
            ..Probe::default()
        };

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let error = expect_kept(&removal);
        assert_eq!(
            error.to_string(),
            format!("cannot delete doomed/{}/piece", stored.stamp)
        );
        let doomed = antiq(&world, "doomed")
            .join(stored.stamp.to_string())
            .join("piece/a.txt");
        assert!(doomed.exists());
        assert!(!world.temp.root().join("piece").exists());
        assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());

        let lock = crate::testing::hold_lock(world.temp.root());
        let outcome = crate::repair::run(&collection(&world), &lock);
        assert!(outcome.warnings.is_empty());
        assert_eq!(entries(&antiq(&world, "doomed")), Vec::<String>::new());
    }

    #[test]
    fn a_crash_after_the_store_leaves_a_whole_version_and_an_empty_staging_for_repair_2() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let staged = antiq(&world, "tmp/pack").join(stored.stamp.to_string());
        assert!(staged.join("piece").is_dir(), "the staging stays empty");

        let lock = crate::testing::hold_lock(world.temp.root());
        let outcome = crate::repair::run(&collection(&world), &lock);

        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
        assert_eq!(outcome.done.len(), 1, "{:?}", outcome.done);
        assert_eq!(entries(&antiq(&world, "tmp/pack")), Vec::<String>::new());
        assert_eq!(store_files(&world).len(), 3, "the version stays");
        assert!(world.temp.root().join("piece/a.txt").exists());
    }

    #[test]
    fn a_taken_doomed_target_gives_the_folder_back_and_says_where_it_went() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let target = antiq(&world, "doomed")
            .join(stored.stamp.to_string())
            .join("piece/blocker");
        fs::create_dir_all(&target).expect("the target is made");

        let removal = remove(&world, "piece", &stored, &RealSeams, &[]);

        let error = expect_kept(&removal);
        assert_eq!(
            error.to_string(),
            format!(
                "cannot move piece into doomed/{}, so the folder went back to its path",
                stored.stamp
            )
        );
        assert_eq!(
            os_errors(error),
            [libc::EEXIST],
            "the error of the rename is on the chain"
        );
        assert!(world.temp.root().join("piece/a.txt").exists());
        assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());
    }

    #[test]
    fn a_refused_doomed_move_names_recovered_when_the_path_is_taken() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let blocker = antiq(&world, "doomed")
            .join(stored.stamp.to_string())
            .join("piece/blocker");
        fs::create_dir_all(&blocker).expect("the target is made");
        let root = world.temp.root().to_path_buf();
        let seams = tree_changes(move |_| {
            fs::create_dir(root.join("piece")).expect("the path is taken");
        });

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let error = expect_kept(&removal);
        assert_eq!(
            error.to_string(),
            format!(
                "cannot move piece into doomed/{stamp}, so the folder went to recovered/{stamp}/piece",
                stamp = stored.stamp
            )
        );
        let recovered = antiq(&world, "recovered")
            .join(stored.stamp.to_string())
            .join("piece/a.txt");
        assert!(recovered.exists());
    }

    #[test]
    fn two_pieces_below_one_parent_finish_from_two_threads() {
        for _ in 0..20 {
            let world = world(&["group/one", "group/two"]);
            let one = store(&world, "group/one");
            let two = store(&world, "group/two");
            assert_eq!(one.stamp, two.stamp, "both pieces share the stamp");

            let (first, second) = std::thread::scope(|scope| {
                let first = scope.spawn(|| remove(&world, "group/one", &one, &RealSeams, &[]));
                let second = scope.spawn(|| remove(&world, "group/two", &two, &RealSeams, &[]));
                (
                    first.join().expect("no panic"),
                    second.join().expect("no panic"),
                )
            });

            assert!(
                matches!(first.outcome, Outcome::Removed),
                "{:?}",
                describe_removal(&first)
            );
            assert!(
                matches!(second.outcome, Outcome::Removed),
                "{:?}",
                describe_removal(&second)
            );
            assert_eq!(warning_texts(&first), Vec::<&str>::new());
            assert_eq!(warning_texts(&second), Vec::<&str>::new());
            assert_eq!(entries(&antiq(&world, "trash")), Vec::<String>::new());
            assert_eq!(entries(&antiq(&world, "doomed")), Vec::<String>::new());
        }
    }

    fn warning_texts(removal: &Removal) -> Vec<&str> {
        removal
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect()
    }

    fn failing_dir_flush(at: usize) -> Probe {
        let recording = Recording::new();
        recording.fail_dirs_at(at);
        Probe {
            recording: Some(recording),
            ..Probe::default()
        }
    }

    // The directory flushes of a removal of `piece`, in order: 0 and 1
    // create `trash/<stamp>`, 2 is the collection after the rename into
    // `trash/`, 3 is `trash/<stamp>`, 4 and 5 create `doomed/<stamp>`, 6 is
    // `doomed/<stamp>` after the move, 7 is `trash/<stamp>`, 8 removes the
    // empty `trash/<stamp>`, 9 is `doomed/<stamp>` after the deletion.
    const FLUSH_AFTER_TRASH_RENAME: usize = 2;
    const FLUSH_AFTER_DOOMED_MOVE: usize = 6;
    const FLUSH_AFTER_DELETE: usize = 9;

    #[test]
    fn a_flush_that_fails_after_the_rename_into_trash_is_a_warning() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = failing_dir_flush(FLUSH_AFTER_TRASH_RENAME);

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        assert!(matches!(removal.outcome, Outcome::Removed));
        let texts = warning_texts(&removal);
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(
            texts[0].starts_with("piece: the move into trash/ cannot be flushed"),
            "{texts:?}"
        );
    }

    #[test]
    fn a_flush_that_fails_after_the_move_to_doomed_keeps_the_tree_there() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = failing_dir_flush(FLUSH_AFTER_DOOMED_MOVE);

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let error = expect_kept(&removal);
        assert_eq!(
            error.to_string(),
            format!("cannot move piece into doomed/{}", stored.stamp)
        );
        let doomed = antiq(&world, "doomed")
            .join(stored.stamp.to_string())
            .join("piece/a.txt");
        assert!(doomed.exists(), "repair 3 deletes it on the next run");
        assert!(!world.temp.root().join("piece").exists());
    }

    #[test]
    fn a_doomed_chain_that_another_piece_removed_is_no_warning() {
        let world = world(&["group/piece"]);
        let stored = store(&world, "group/piece");
        let chain = antiq(&world, "doomed").join(stored.stamp.to_string());
        let seams = Probe {
            after_delete: Some(Box::new(move || {
                fs::remove_dir_all(&chain).expect("the chain is removed");
            })),
            ..Probe::default()
        };

        let removal = remove(&world, "group/piece", &stored, &seams, &[]);

        assert!(matches!(removal.outcome, Outcome::Removed));
        assert_eq!(warning_texts(&removal), Vec::<&str>::new());
    }

    #[test]
    fn a_flush_that_fails_after_the_deletion_is_a_warning() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = failing_dir_flush(FLUSH_AFTER_DELETE);

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        assert!(matches!(removal.outcome, Outcome::Removed));
        let texts = warning_texts(&removal);
        assert_eq!(texts.len(), 1, "{texts:?}");
        assert!(
            texts[0].starts_with("piece: the deletion cannot be flushed"),
            "{texts:?}"
        );
    }

    #[test]
    fn a_mode_that_cannot_be_restored_is_a_warning_for_each_move() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = Probe {
            restore_fails: true,
            ..Probe::default()
        };

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        assert!(matches!(removal.outcome, Outcome::Removed));
        let texts = warning_texts(&removal);
        assert_eq!(texts.len(), 2, "into trash/ and into doomed/: {texts:?}");
        assert!(
            texts.iter().all(|text| text.starts_with(
                "piece: cannot restore the mode of the moved tree: cannot restore the mode of moved"
            )),
            "{texts:?}"
        );
    }

    #[test]
    fn a_folder_that_cannot_go_back_stays_in_trash_for_repair_4() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let root = world.temp.root().to_path_buf();
        let seams = tree_changes(move |tree| {
            fs::write(tree.join("a.txt"), "changed later").expect("the file is written");
            // The path is taken, and `recovered/` cannot be made.
            fs::create_dir(root.join("piece")).expect("the path is taken");
            let recovered = root.join(".antiq/recovered");
            fs::remove_dir_all(&recovered).ok();
            fs::write(&recovered, "in the way").expect("recovered/ is a file");
        });

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let error = expect_kept(&removal);
        let text = error.to_string();
        assert!(
            text.starts_with(&format!(
                "cannot give piece back from trash/{} after it changed (`a.txt` changed)",
                stored.stamp
            )),
            "{text}"
        );
        let trashed = antiq(&world, "trash")
            .join(stored.stamp.to_string())
            .join("piece/a.txt");
        assert!(trashed.exists(), "the tree stays in trash/");
        assert_eq!(store_files(&world).len(), 3, "the version stays");
    }

    #[test]
    fn a_missing_doomed_folder_takes_the_version_back_and_leaves_the_folder() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        fs::remove_dir(antiq(&world, "doomed")).expect("doomed/ is empty");

        let removal = remove(&world, "piece", &stored, &RealSeams, &[]);

        let error = expect_kept(&removal);
        assert_eq!(
            error.to_string(),
            format!("cannot move piece into trash/{}", stored.stamp)
        );
        assert!(world.temp.root().join("piece/a.txt").exists());
        assert_eq!(store_files(&world), Vec::<String>::new());
    }

    #[test]
    fn a_walk_that_runs_out_of_descriptors_sends_the_folder_back() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = Probe {
            out_of_descriptors: true,
            ..Probe::default()
        };

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        let (destination, cause) = expect_exhausted(&removal);
        assert_eq!(destination, Destination::Live);
        assert!(cause.contains("descriptors"), "{cause}");
        assert!(world.temp.root().join("piece/a.txt").exists());
    }

    #[test]
    fn an_exclude_list_that_runs_out_of_descriptors_sends_the_folder_back() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let seams = Probe {
            out_of_descriptors: true,
            ..Probe::default()
        };

        let removal = remove(
            &world,
            "piece",
            &stored,
            &seams,
            &[path("piece/secret.txt")],
        );

        let (destination, cause) = expect_exhausted(&removal);
        assert_eq!(destination, Destination::Live);
        assert!(cause.contains("piece/secret.txt"), "{cause}");
        assert!(world.temp.root().join("piece/a.txt").exists());
    }

    #[test]
    fn the_rollback_puts_the_files_back_archive_first_and_listing_last() {
        let world = world(&["piece"]);
        let stored = store(&world, "piece");
        let target = antiq(&world, "trash")
            .join(stored.stamp.to_string())
            .join("piece/blocker");
        fs::create_dir_all(&target).expect("the target is made");
        let seams = Probe {
            recording: Some(Recording::new()),
            ..Probe::default()
        };

        let removal = remove(&world, "piece", &stored, &seams, &[]);

        expect_kept(&removal);
        let moved: Vec<String> = seams
            .recording
            .as_ref()
            .expect("recorded")
            .flushes()
            .into_iter()
            .filter_map(|flush| match flush {
                crate::filesystem::atomic::recording::Flush::File(file) => Some(
                    file.file_name()
                        .expect("a file name")
                        .to_string_lossy()
                        .into_owned(),
                ),
                crate::filesystem::atomic::recording::Flush::Dir(_) => None,
            })
            .collect();
        let names = &stored.names;
        assert_eq!(
            moved,
            [
                names.of(VersionFile::Archive),
                names.of(VersionFile::Checksum),
                names.of(VersionFile::Listing),
            ]
        );
    }

    #[test]
    fn describe_names_every_problem_of_a_trashed_tree() {
        let cases = [
            (
                Problem::Unreadable { first: path("a") },
                "`a` cannot be read",
            ),
            (
                Problem::MountPoint { other: path("m") },
                "another filesystem is mounted at `m`",
            ),
            (
                Problem::Excluded { path: path("x") },
                "`x` is on the exclude list",
            ),
            (
                Problem::NestedCollection { root: path("n") },
                "`n` is a collection of its own",
            ),
            (Problem::Locked { path: path("l") }, "`l` is locked"),
            (
                Problem::LockedParent { path: path("p") },
                "the folder above it is locked",
            ),
            (
                Problem::RuleFile {
                    path: path("r/.antiqignore"),
                    line: Some(2),
                },
                "the rule file `r/.antiqignore` cannot be used",
            ),
            (
                Problem::MarkerNotAFile {
                    name: Name::from_bytes(b".antiq-pin".to_vec()).expect("a name"),
                },
                "`.antiq-pin` is not a file",
            ),
        ];
        for (problem, text) in cases {
            assert_eq!(describe(&problem), text);
        }
    }

    fn describe_removal(removal: &Removal) -> String {
        name_of(&removal.outcome)
    }

    fn entry(kind: EntryKind, path_text: &str, size: u64, mtime: i128, ctime: i128) -> TreeEntry {
        TreeEntry {
            path: if path_text.is_empty() {
                RelPath::root()
            } else {
                path(path_text)
            },
            kind,
            size,
            allocated: size,
            mtime_ns: mtime,
            ctime_ns: ctime,
            inode: 7,
            device: 1,
            mode: 0o644,
            nlink: 1,
            link_target: None,
            birth_ns: None,
            mount_root: false,
            uid: 0,
            flag_locked: false,
            class: Class::Counted,
        }
    }

    fn record(entry: &TreeEntry) -> LstatRecord {
        LstatRecord {
            path: entry.path.clone(),
            kind: entry.kind,
            size: entry.size,
            mtime_ns: entry.mtime_ns,
            ctime_ns: entry.ctime_ns,
            inode: entry.inode,
        }
    }

    fn tree_of(entries: Vec<TreeEntry>) -> PieceTree {
        PieceTree::new(path("piece"), entries, Vec::new())
    }

    #[test]
    fn differs_tolerates_a_new_ctime_of_the_top_only() {
        let top = entry(EntryKind::Directory, "", 0, 5, 10);
        let file = entry(EntryKind::File, "a", 3, 5, 10);
        let recorded = vec![record(&top), record(&file)];

        let mut moved_top = top.clone();
        moved_top.ctime_ns = 99;
        assert_eq!(
            differs(&recorded, &tree_of(vec![moved_top, file.clone()])),
            None
        );

        let mut touched = file.clone();
        touched.ctime_ns = 99;
        let tree = tree_of(vec![top, touched]);
        assert_eq!(differs(&recorded, &tree), Some("`a` changed".to_string()));
    }

    #[test]
    fn differs_names_a_new_entry_and_a_gone_one() {
        let top = entry(EntryKind::Directory, "", 0, 5, 10);
        let file = entry(EntryKind::File, "a", 3, 5, 10);
        let recorded = vec![record(&top), record(&file)];

        let extra = entry(EntryKind::File, "b", 1, 5, 10);
        assert_eq!(
            differs(&recorded, &tree_of(vec![top.clone(), file.clone(), extra])),
            Some("`b` is new".to_string())
        );
        assert_eq!(
            differs(&recorded, &tree_of(vec![top])),
            Some("`a` is gone".to_string())
        );
    }

    #[test]
    fn differs_notices_a_top_that_changed_its_modification_time() {
        let top = entry(EntryKind::Directory, "", 0, 5, 10);
        let recorded = vec![record(&top)];
        let mut later = top;
        later.mtime_ns = 6;

        assert_eq!(
            differs(&recorded, &tree_of(vec![later])),
            Some("the folder itself changed".to_string())
        );
    }
}
