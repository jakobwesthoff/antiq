// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Steps 2 to 4 of `archive`: pack a piece into the staging, verify the
//! packing against the folder and move the version into the store.

use super::ArchiveError;
use super::seams::Seams;
use crate::archive::check::{ChecksumFile, file_sha256};
use crate::archive::names::{MemberKind, MemberName, portability_warnings};
use crate::archive::pool::{
    ARCHIVE_FIXED_BYTES, ARCHIVE_JOB_BYTES, ARCHIVE_WORKER_BYTES, Lease, Pool, workers_for_piece,
};
use crate::archive::tar_write::sparse_warning;
use crate::archive::zstd::ZstdSettings;
use crate::archive::{PackError, PackNote, PackSettings, Packager, TarZstd, Verified, WalkRules};
use crate::clock::Clock;
use crate::collection::Collection;
use crate::filesystem::atomic::{create_dir_chain, flush_dir_of, named};
use crate::filesystem::io_error;
use crate::filesystem::remove::remove_tree;
use crate::filesystem::space::{ReserveError, SpaceLedger, is_out_of_space};
use crate::filesystem::tree::TreeRoot;
use crate::listing::{self, Header, ListingEntry, ListingType};
use crate::relpath::{Name, RelPath};
use crate::report::Warning;
use crate::rules::PieceRules;
use crate::tree::{EntryKind, PieceTree};
use crate::versions::VersionsError;
use crate::versions::names::VersionFileNames;
use crate::versions::stamp::Stamp;
use crate::versions::store::{fresh_stamp, move_in_with};
use jiff::Timestamp;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

/// What the steps share with the pieces running beside them.
pub(crate) struct StoreContext<'a> {
    pub collection: &'a Collection,
    pub clock: &'a dyn Clock,
    pub pool: &'a Pool,
    pub ledger: &'a SpaceLedger,
    pub seams: &'a dyn Seams,
    /// Held while a chain below `tmp/pack/` is created or removed, so a
    /// piece that finishes cannot remove a `<stamp>/` folder that another
    /// piece is about to use.
    pub run_dirs: &'a Mutex<()>,
    /// The compression for the number of workers the pool granted.
    pub zstd: &'a (dyn Fn(u32) -> ZstdSettings + Sync),
}

/// The piece to store.
pub(crate) struct StoreRequest<'a> {
    /// The folder as it lies in the collection, in its live spelling.
    pub live: &'a RelPath,
    /// The path of the store folder, in its stored spelling.
    pub stored: &'a RelPath,
    pub tree: &'a PieceTree,
    pub rules: &'a PieceRules,
    pub imported: Option<Timestamp>,
    /// The folder itself, opened as a root, when it lies outside the
    /// collection. Without it the folder is `live` in the collection.
    pub outside: Option<&'a TreeRoot>,
    /// The version time. Without it the version gets the time of the clock
    /// once the piece has its room and its workers.
    pub time: Option<Timestamp>,
}

/// A version in the store, complete and verified against the folder.
pub(crate) struct Stored {
    pub stamp: Stamp,
    pub names: VersionFileNames,
    pub verified: Verified,
    pub archive_bytes: u64,
    pub files: u64,
    pub bytes: u64,
    pub notes: Vec<Warning>,
}

/// Why no version was stored, or why the piece may go no further.
pub(crate) enum StoreFailure {
    /// The piece did not fit, or the disk filled up. Nothing is left in
    /// the staging.
    NoRoom(Box<ArchiveError>),
    /// The folder changed while it was packed.
    Changed(Box<ArchiveError>),
    /// The folder could not be read for lack of file descriptors, so it is
    /// unknown whether it changed. Nothing is left in the staging.
    OutOfDescriptors(Box<ArchiveError>),
    /// The version is in the store, but the store folder may not be durable.
    NotDurable(Box<ArchiveError>),
    /// Any other failure. Of the staging, only a move into the store that
    /// stopped half way leaves something behind, for repair 1.
    Failed(Box<ArchiveError>),
}

/// Packs the piece, checks the packing against the folder and moves the
/// version into the store.
///
/// The steps stop at the first failure and take back what they made. The
/// exception is a move into the store that stopped half way and could not
/// be undone: the staging stays for repair 1, whatever the cause was.
///
/// After a success the staging folder stays, empty. If the folder of the
/// piece cannot be moved to `trash/`, the version goes back through it, so
/// the caller removes it with [`remove_staging`] once that move succeeded.
pub(crate) fn store_version(
    context: &StoreContext<'_>,
    request: &StoreRequest<'_>,
) -> Result<Stored, StoreFailure> {
    let piece = request.stored.to_string();
    let apparent = apparent_size(request.tree);

    // The reservation covers the packed entries only: ignored ones are not
    // written. It lasts until the version is stored or the staging is gone.
    let antiq = context.collection.antiq_dir();
    let _reservation = match context
        .ledger
        .try_reserve(apparent, || context.seams.free(&antiq))
    {
        Ok(reservation) => reservation,
        Err(ReserveError::Short { needed, available }) => {
            return Err(StoreFailure::NoRoom(Box::new(ArchiveError::NoRoom {
                piece,
                needed,
                available,
            })));
        }
        Err(ReserveError::Free(source)) => {
            return Err(StoreFailure::Failed(Box::new(ArchiveError::Space(source))));
        }
    };

    let lease = context.pool.lease(
        workers_for_piece(apparent, ARCHIVE_JOB_BYTES),
        ARCHIVE_FIXED_BYTES,
        ARCHIVE_WORKER_BYTES,
    );

    // The clock is read once the piece has its room and its workers, since
    // a piece may wait for both for a long time.
    let time = request.time.unwrap_or_else(|| context.clock.now());
    let tmp_dir = context.collection.tmp_dir();
    let stamp = fresh_stamp(
        &context.collection.pieces_dir(),
        &tmp_dir.join("pack"),
        request.stored,
        time,
    )
    .map_err(|source| {
        StoreFailure::Failed(Box::new(ArchiveError::Stamp {
            piece: piece.clone(),
            source: Box::new(source),
        }))
    })?;

    let chain = staging_chain(&stamp, request.stored);
    let result = stage_and_store(context, request, &stamp, &chain, lease);
    match result {
        Ok(stored) => Ok(stored),
        Err(abort) => {
            if !abort.keep_staging {
                remove_staging(context.run_dirs, &tmp_dir, &chain);
            }
            Err(abort.failure)
        }
    }
}

/// A failure and whether the staging must stay.
struct Abort {
    failure: StoreFailure,
    keep_staging: bool,
}

impl Abort {
    /// A failure that takes the staging with it. A full disk anywhere on
    /// the way is a piece without room.
    fn of(error: ArchiveError) -> Abort {
        let failure = if is_out_of_space(&error) {
            StoreFailure::NoRoom(Box::new(error))
        } else {
            StoreFailure::Failed(Box::new(error))
        };
        Abort {
            failure,
            keep_staging: false,
        }
    }
}

/// What a failed verify means for the piece. A folder that differs, or
/// that no longer walks without a problem, changed while it was packed.
fn verify_abort(error: ArchiveError) -> Abort {
    let failure = match &error {
        ArchiveError::Verify {
            source: PackError::Differs(_) | PackError::TreeHasProblems(_),
            ..
        } => StoreFailure::Changed(Box::new(error)),
        // The walk fails with nothing but a shortage of file descriptors.
        ArchiveError::Verify {
            source: PackError::Walk(_),
            ..
        } => StoreFailure::OutOfDescriptors(Box::new(error)),
        _ => return Abort::of(error),
    };
    Abort {
        failure,
        keep_staging: false,
    }
}

/// `pack/<stamp>/<piece>` below `tmp/`.
pub(crate) fn staging_chain(stamp: &Stamp, stored: &RelPath) -> RelPath {
    let pack = Name::from_bytes(b"pack".to_vec()).expect("`pack` is a name");
    let stamp = Name::from_bytes(stamp.to_string().into_bytes()).expect("a stamp is a name");
    RelPath::root().join(&pack).join(&stamp).concat(stored)
}

fn stage_and_store(
    context: &StoreContext<'_>,
    request: &StoreRequest<'_>,
    stamp: &Stamp,
    chain: &RelPath,
    lease: Lease<'_>,
) -> Result<Stored, Abort> {
    let piece = request.stored.to_string();
    let stamp_text = stamp.to_string();
    let names = VersionFileNames::new(request.stored, stamp);
    let durable = context.seams.durable();
    let stage_error = |source| ArchiveError::Stage {
        piece: piece.clone(),
        stamp: stamp_text.clone(),
        source,
    };

    // =========================================================
    // Step 2: pack into the staging
    // =========================================================

    let tmp_dir = context.collection.tmp_dir();
    let tmp = TreeRoot::open(&tmp_dir)
        .map_err(|source| Abort::of(stage_error(io_error("open", &tmp_dir)(source))))?;
    {
        let _guard = context
            .run_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        create_dir_chain(durable, &tmp, chain).map_err(|source| Abort::of(stage_error(source)))?;
    }
    let staging = tmp.sub(chain).map_err(|source| {
        Abort::of(stage_error(
            io_error("open", &tmp_dir.join(path_of(chain)))(source),
        ))
    })?;
    let staging_path = tmp_dir.join(path_of(chain));

    let collection_root = TreeRoot::open(context.collection.root()).map_err(|source| {
        Abort::of(stage_error(io_error("open", context.collection.root())(
            source,
        )))
    })?;
    let live_in_collection;
    let live_root = match request.outside {
        Some(folder) => folder,
        None => {
            live_in_collection = collection_root.sub(request.live).map_err(|source| {
                Abort::of(stage_error(io_error(
                    "open",
                    Path::new(&request.live.to_string()),
                )(source)))
            })?;
            &live_in_collection
        }
    };

    let create = |file: &str| -> Result<(File, RelPath), Abort> {
        let rel = RelPath::parse(file).expect("a version file name is a plain name");
        let created = staging.create_file(&rel, 0o644).map_err(|source| {
            Abort::of(ArchiveError::Write {
                piece: piece.clone(),
                stamp: stamp_text.clone(),
                file: file.to_string(),
                source,
            })
        })?;
        Ok((created, rel))
    };
    let flush = |file: &File, rel: &RelPath, name: &str| -> Result<(), Abort> {
        durable.file(file, &named(&staging, rel)).map_err(|source| {
            Abort::of(ArchiveError::Flush {
                piece: piece.clone(),
                stamp: stamp_text.clone(),
                file: name.to_string(),
                source: Box::new(source),
            })
        })
    };

    let (archive, archive_rel) = create(&names.archive)?;
    let workers = u32::try_from(lease.workers()).unwrap_or(u32::MAX);
    let settings = PackSettings {
        zstd: (context.zstd)(workers),
    };
    let packed = {
        let mut sink = context.seams.pack_sink(Box::new(BufWriter::new(&archive)));
        TarZstd.pack(live_root, request.tree, &settings, &mut *sink)
    }
    .map_err(|source| {
        Abort::of(ArchiveError::Pack {
            piece: piece.clone(),
            stamp: stamp_text.clone(),
            source,
        })
    })?;
    let mut notes: Vec<Warning> = lease.warning().into_iter().cloned().collect();
    drop(lease);
    flush(&archive, &archive_rel, &names.archive)?;

    let header = Header {
        piece: request.stored.clone(),
        archived: stamp.time(),
        archive: names.archive.clone(),
        archive_bytes: packed.size,
        imported: request.imported,
        rebuilt: None,
    };
    let (listing_file, listing_rel) = create(&names.listing)?;
    listing::write(&header, &packed.listing, &listing_file).map_err(|source| {
        Abort::of(ArchiveError::Listing {
            piece: piece.clone(),
            stamp: stamp_text.clone(),
            source,
        })
    })?;
    flush(&listing_file, &listing_rel, &names.listing)?;

    let (checksum_file, checksum_rel) = create(&names.checksum)?;
    let checksum = ChecksumFile::new(packed.sha256.clone(), names.archive.clone());
    (&checksum_file)
        .write_all(&checksum.to_bytes())
        .map_err(|source| {
            Abort::of(ArchiveError::Write {
                piece: piece.clone(),
                stamp: stamp_text.clone(),
                file: names.checksum.clone(),
                source,
            })
        })?;
    flush(&checksum_file, &checksum_rel, &names.checksum)?;
    flush_dir_of(durable, &staging, &RelPath::root())
        .map_err(|source| Abort::of(stage_error(source)))?;

    // =========================================================
    // Step 3: read the archive back and compare it with the folder
    // =========================================================

    let verify_error = |source| ArchiveError::Verify {
        piece: piece.clone(),
        stamp: stamp_text.clone(),
        source,
    };
    let archive_path = staging_path.join(&names.archive);
    context.seams.before_read_back(&staging_path);
    let read_back = file_sha256(&archive_path).map_err(|source| Abort::of(verify_error(source)))?;
    if read_back != packed.sha256 {
        return Err(Abort::of(verify_error(PackError::ArchiveDiffers {
            file: names.archive.clone(),
            what: format!(
                "it reads back with the hash {read_back}, and {} was computed while writing it",
                packed.sha256
            ),
        })));
    }

    context.seams.before_verify(&staging_path);
    let rules = WalkRules {
        rules: request.rules.clone(),
        staged: false,
    };
    let verified = TarZstd
        .verify_strict(&archive_path, live_root, &rules)
        .map_err(|source| verify_abort(verify_error(source)))?;
    if verified.listing() != packed.listing.as_slice() {
        return Err(Abort::of(verify_error(PackError::Damaged(
            "the listing of the packing differs from the members the verify read".into(),
        ))));
    }

    // =========================================================
    // Step 4: move the version into the store
    // =========================================================

    let pieces = TreeRoot::open(&context.collection.pieces_dir()).map_err(|source| {
        Abort::of(stage_error(io_error(
            "open",
            &context.collection.pieces_dir(),
        )(source)))
    })?;
    let moved = move_in_with(
        durable,
        &staging,
        &pieces,
        request.stored,
        request.stored,
        stamp,
        &names,
    );
    if let Err(source) = moved {
        let partly_moved = matches!(source, VersionsError::PartlyMoved { .. });
        let not_durable = matches!(source, VersionsError::StoredNotDurable { .. });
        let error = ArchiveError::MoveIn {
            piece: piece.clone(),
            stamp: stamp_text.clone(),
            source: Box::new(source),
        };
        return Err(if partly_moved {
            // Repair 1 finishes the move from the staging.
            Abort {
                failure: StoreFailure::Failed(Box::new(error)),
                keep_staging: true,
            }
        } else if not_durable {
            // The version exists. The piece stops before its folder goes.
            Abort {
                failure: StoreFailure::NotDurable(Box::new(error)),
                keep_staging: false,
            }
        } else {
            Abort::of(error)
        });
    }

    notes.extend(notes_of(
        &piece,
        request.tree,
        &packed.notes,
        &packed.listing,
    ));
    let (files, bytes) = listing::counts(&packed.listing).unwrap_or((0, 0));
    Ok(Stored {
        stamp: *stamp,
        names,
        verified,
        archive_bytes: packed.size,
        files,
        bytes,
        notes,
    })
}

/// Removes the staging of a version and the folders above it that are
/// empty, up to and including `pack/`.
///
/// Pieces of one run share `pack/` and a stamp folder. The last one to finish
/// finds them empty and removes them, so a run leaves `tmp/` empty. A failure
/// leaves the folder for repair 2, which deletes what `tmp/` holds, so it is
/// not reported.
pub(crate) fn remove_staging(run_dirs: &Mutex<()>, tmp_dir: &Path, chain: &RelPath) {
    let Ok(tmp) = TreeRoot::open(tmp_dir) else {
        return;
    };
    let _guard = run_dirs.lock().unwrap_or_else(PoisonError::into_inner);
    if remove_tree(&tmp, chain).is_err() {
        return;
    }
    // `pack` and `<stamp>` are the first two components of the chain.
    let components: Vec<&[u8]> = chain.components().collect();
    for length in (1..components.len()).rev() {
        let mut folder = RelPath::root();
        for component in &components[..length] {
            let name = Name::from_bytes(component.to_vec()).expect("a component is a name");
            folder = folder.join(&name);
        }
        if tmp.remove_dir(&folder).is_err() {
            return;
        }
    }
}

/// The sum of the sizes of the regular files that are packed, a file with
/// several names counted once.
fn apparent_size(tree: &PieceTree) -> u64 {
    let mut seen = HashSet::new();
    let mut sum = 0u64;
    for entry in tree.packed_entries() {
        if entry.kind != EntryKind::File {
            continue;
        }
        if entry.nlink > 1 && !seen.insert((entry.device, entry.inode)) {
            continue;
        }
        sum = sum.saturating_add(entry.size);
    }
    sum
}

/// What the packing found worth telling, each note naming the piece.
fn notes_of(
    piece: &str,
    tree: &PieceTree,
    pack_notes: &[PackNote],
    listing: &[ListingEntry],
) -> Vec<Warning> {
    let mut notes = Vec::new();
    for note in pack_notes {
        notes.push(Warning::new(format!("{piece}: {note}")));
    }
    if let Some(sparse) = sparse_warning(tree) {
        notes.push(Warning::new(format!(
            "{piece}: its files add up to {} bytes and take {} bytes on disk; sparse files are stored in full",
            sparse.apparent, sparse.allocated
        )));
    }
    let members: Vec<MemberName> = listing
        .iter()
        .map(|entry| MemberName {
            path: entry.path.clone(),
            kind: match (&entry.kind, &entry.link) {
                (ListingType::Symlink, Some(target)) => MemberKind::Symlink {
                    target_len: target.as_bytes().len(),
                },
                (ListingType::Hardlink, _) => MemberKind::HardLink,
                _ => MemberKind::Plain,
            },
        })
        .collect();
    for warning in portability_warnings(&members) {
        notes.push(Warning::new(format!("{piece}: {warning}")));
    }
    notes
}

/// A relative path as a filesystem path.
fn path_of(path: &RelPath) -> &Path {
    Path::new(OsStr::from_bytes(path.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::check::{ChecksumFile, file_sha256};
    use crate::archive::pool::Pool;
    use crate::clock::FixedClock;
    use crate::commands::archive::seams::RealSeams;
    use crate::filesystem::FilesystemError;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use crate::filesystem::atomic::{Durable, RealDurable};
    use crate::filesystem::tree::TreeRoot;
    use crate::listing::read_full;
    use crate::select::Rules;
    use crate::testing::shared_pool;
    use crate::testing::tree::TempCollection;
    use crate::walk::walk_piece;
    use std::fs;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};

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
        free: Option<u64>,
        full_disk: bool,
        before_read_back: Option<Change>,
        before_verify: Option<Change>,
    }

    struct FullDisk;

    impl Write for FullDisk {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        }
    }

    impl Seams for Probe {
        fn durable(&self) -> &dyn Durable {
            match &self.recording {
                Some(recording) => recording,
                None => &RealDurable,
            }
        }

        fn free(&self, dir: &Path) -> Result<u64, FilesystemError> {
            match self.free {
                Some(free) => Ok(free),
                None => RealSeams.free(dir),
            }
        }

        fn pack_sink<'a>(&self, sink: Box<dyn Write + 'a>) -> Box<dyn Write + 'a> {
            if self.full_disk {
                Box::new(FullDisk)
            } else {
                sink
            }
        }

        fn before_read_back(&self, staging: &Path) {
            if let Some(change) = &self.before_read_back {
                change(staging);
            }
        }

        fn before_verify(&self, staging: &Path) {
            if let Some(change) = &self.before_verify {
                change(staging);
            }
        }
    }

    struct World {
        temp: TempCollection,
        clock: FixedClock,
        ledger: SpaceLedger,
        run_dirs: Mutex<()>,
    }

    /// A collection with the piece `piece`: two small files and an ignored
    /// `node_modules/`.
    fn world() -> World {
        let mut temp = TempCollection::minimal();
        temp.tree()
            .file_with("piece/a.txt", "alpha", 0o644, LONG_AGO);
        temp.tree()
            .file_with("piece/sub/b.txt", "beta", 0o600, LONG_AGO);
        temp.tree()
            .file_with("piece/node_modules/x.js", "ignored", 0o644, LONG_AGO);
        World {
            temp,
            clock: FixedClock::new(WHEN.parse().expect("a time")),
            ledger: SpaceLedger::new(),
            run_dirs: Mutex::new(()),
        }
    }

    fn store(world: &World, probe: &dyn Seams) -> Result<Stored, StoreFailure> {
        store_in(world, probe, &world.ledger, shared_pool())
    }

    fn store_in(
        world: &World,
        probe: &dyn Seams,
        ledger: &SpaceLedger,
        pool: &Pool,
    ) -> Result<Stored, StoreFailure> {
        let collection = Collection::find(world.temp.root()).expect("a collection");
        let rules = PieceRules::new(&["node_modules/".to_string()], &[]).expect("valid rules");
        let root = TreeRoot::open(world.temp.root()).expect("the root opens");
        let piece = path("piece");
        let selection = Rules::new(&[], &[]).expect("no patterns are valid");
        let tree = walk_piece(&root, &piece, &selection, &rules).expect("the walk succeeds");
        let zstd = |_workers: u32| ZstdSettings::fast();
        let context = StoreContext {
            collection: &collection,
            clock: &world.clock,
            pool,
            ledger,
            seams: probe,
            run_dirs: &world.run_dirs,
            zstd: &zstd,
        };
        let request = StoreRequest {
            live: &piece,
            stored: &piece,
            tree: &tree,
            rules: &rules,
            imported: None,
            outside: None,
            time: None,
        };
        store_version(&context, &request)
    }

    fn stored(world: &World, probe: &dyn Seams) -> Stored {
        match store(world, probe) {
            Ok(stored) => stored,
            Err(failure) => panic!("the store succeeds, but {}", describe(&failure)),
        }
    }

    fn failure(world: &World, probe: &dyn Seams) -> StoreFailure {
        match store(world, probe) {
            Ok(_) => panic!("the store fails"),
            Err(failure) => failure,
        }
    }

    fn describe(failure: &StoreFailure) -> String {
        match failure {
            StoreFailure::NoRoom(error)
            | StoreFailure::Changed(error)
            | StoreFailure::OutOfDescriptors(error)
            | StoreFailure::NotDurable(error)
            | StoreFailure::Failed(error) => format!("{error:?}"),
        }
    }

    fn names(stamp: &Stamp) -> VersionFileNames {
        VersionFileNames::new(&path("piece"), stamp)
    }

    fn store_folder(world: &World) -> PathBuf {
        world.temp.root().join(".antiq/pieces/piece")
    }

    fn entries_of(directory: &Path) -> Vec<String> {
        let mut found: Vec<String> = match fs::read_dir(directory) {
            Ok(read) => read
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        found.sort();
        found
    }

    fn staging_is_gone(world: &World) {
        let pack = world.temp.root().join(".antiq/tmp/pack");
        assert!(!pack.exists(), "tmp/pack/ is gone");
    }

    /// Two pieces staged under one stamp, as parallel pieces of a run are.
    fn stage_two_chains(world: &World) -> (RelPath, RelPath) {
        let stamp: Stamp = "2026-11-04T090211".parse().expect("a stamp");
        let first = staging_chain(&stamp, &path("one"));
        let second = staging_chain(&stamp, &path("two"));
        for chain in [&first, &second] {
            fs::create_dir_all(world.temp.root().join(".antiq/tmp").join(chain.to_string()))
                .expect("the chain is made");
        }
        (first, second)
    }

    #[test]
    fn removing_the_last_staging_removes_pack_too() {
        let world = world();
        let (first, second) = stage_two_chains(&world);
        let tmp = world.temp.root().join(".antiq/tmp");

        remove_staging(&world.run_dirs, &tmp, &first);
        assert_eq!(
            entries_of(&tmp.join("pack/2026-11-04T090211")),
            ["two"],
            "the sibling piece stays staged"
        );
        remove_staging(&world.run_dirs, &tmp, &second);

        assert!(!tmp.join("pack").exists(), "pack/ goes with the last one");
        assert!(tmp.is_dir(), "tmp/ itself stays");
    }

    /// The staging of a stored version is still there, empty, until the
    /// folder of the piece has moved to `trash/`.
    fn staging_is_kept_empty(world: &World, stamp: &Stamp) {
        let staging = world
            .temp
            .root()
            .join(".antiq/tmp/pack")
            .join(stamp.to_string())
            .join("piece");
        assert!(staging.is_dir(), "{} stays", staging.display());
        assert_eq!(entries_of(&staging), Vec::<String>::new());
    }

    fn store_is_empty(world: &World) {
        let pieces = world.temp.root().join(".antiq/pieces");
        assert_eq!(
            entries_of(&pieces),
            Vec::<String>::new(),
            "pieces/ is empty"
        );
    }

    // =========================================================
    // A folder outside the collection
    // =========================================================

    /// Stores a folder `folder` that lies in a directory of its own, outside
    /// the collection, as a version of the piece `folder`. Its tree is named
    /// after the folder, as the packer needs.
    fn store_outside(
        world: &World,
        source: &Path,
        time: Option<Timestamp>,
    ) -> Result<Stored, StoreFailure> {
        let collection = Collection::find(world.temp.root()).expect("a collection");
        let rules = PieceRules::new(&[], &[]).expect("valid rules");
        let root = TreeRoot::open(source).expect("the source opens");
        let selection = Rules::new(&[], &[]).expect("no patterns are valid");
        let walked =
            walk_piece(&root, &RelPath::root(), &selection, &rules).expect("the walk succeeds");
        let piece = path("folder");
        let tree = PieceTree::new(
            piece.clone(),
            walked.entries().to_vec(),
            walked.problems().to_vec(),
        );
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
            outside: Some(&root),
            time,
        };
        store_version(&context, &request)
    }

    fn outside_source() -> tempfile::TempDir {
        let source = tempfile::tempdir().expect("a temporary directory");
        fs::write(source.path().join("a.txt"), "alpha").expect("a file");
        source
    }

    #[test]
    fn a_folder_outside_the_collection_is_stored_as_a_version_of_the_given_path() {
        let world = world();
        let source = outside_source();

        let stored = store_outside(&world, source.path(), None)
            .unwrap_or_else(|failure| panic!("the store succeeds, but {}", describe(&failure)));

        let folder = world.temp.root().join(".antiq/pieces/folder");
        assert_eq!(entries_of(&folder).len(), 3);
        assert_eq!(stored.files, 1);
        assert_eq!(entries_of(source.path()), ["a.txt"], "the source stays");
    }

    #[test]
    fn the_archive_of_a_folder_outside_has_the_folder_name_as_its_top_directory() {
        let world = world();
        let source = outside_source();

        let stored = store_outside(&world, source.path(), None)
            .unwrap_or_else(|failure| panic!("the store succeeds, but {}", describe(&failure)));

        let archive = world
            .temp
            .root()
            .join(".antiq/pieces/folder")
            .join(&stored.names.archive);
        let decoded = zstd::stream::Decoder::new(fs::File::open(archive).expect("the archive"))
            .expect("zstd");
        let members: Vec<String> = tar::Archive::new(decoded)
            .entries()
            .expect("entries")
            .map(|entry| {
                let entry = entry.expect("an entry");
                entry.path().expect("a path").to_string_lossy().into_owned()
            })
            .collect();
        assert_eq!(members, ["folder/", "folder/a.txt"]);
    }

    #[test]
    fn the_version_time_is_the_given_one() {
        let world = world();
        let source = outside_source();
        let given: Timestamp = "2020-01-02T03:04:05Z".parse().expect("a time");

        let stored = store_outside(&world, source.path(), Some(given))
            .unwrap_or_else(|failure| panic!("the store succeeds, but {}", describe(&failure)));

        assert_eq!(stored.stamp.to_string(), "2020-01-02T030405");
    }

    // =========================================================
    // A piece that is stored
    // =========================================================

    #[test]
    fn an_idle_piece_is_stored_as_three_files_under_their_final_names() {
        let world = world();

        let stored = stored(&world, &RealSeams);

        let names = names(&stored.stamp);
        let mut expected = vec![
            names.archive.clone(),
            names.checksum.clone(),
            names.listing.clone(),
        ];
        expected.sort();
        assert_eq!(entries_of(&store_folder(&world)), expected);
        assert_eq!(stored.stamp.to_string(), "2026-11-04T090211");
        staging_is_kept_empty(&world, &stored.stamp);
    }

    #[test]
    fn the_checksum_file_names_the_archive_and_its_hash() {
        let world = world();

        let stored = stored(&world, &RealSeams);

        let archive = store_folder(&world).join(&stored.names.archive);
        let expected = ChecksumFile::new(
            file_sha256(&archive).expect("the archive is hashed"),
            stored.names.archive.clone(),
        );
        let found = fs::read(store_folder(&world).join(&stored.names.checksum))
            .expect("the checksum file exists");
        assert_eq!(found, expected.to_bytes());
    }

    #[test]
    fn the_listing_holds_the_head_and_the_packed_members_only() {
        let world = world();

        let stored = stored(&world, &RealSeams);

        let file = fs::File::open(store_folder(&world).join(&stored.names.listing))
            .expect("the listing exists");
        let listing = read_full(file).expect("the listing reads");
        assert_eq!(listing.header.piece, path("piece"));
        assert_eq!(listing.header.archived, WHEN.parse().expect("a time"));
        assert_eq!(listing.header.archive, stored.names.archive);
        assert_eq!(listing.header.archive_bytes, stored.archive_bytes);
        assert_eq!(listing.header.imported, None);
        assert_eq!((listing.files, listing.bytes), (2, 9));
        let members: Vec<String> = listing
            .entries
            .iter()
            .map(|entry| entry.path.to_string())
            .collect();
        assert_eq!(members, ["a.txt", "sub", "sub/b.txt"]);
        assert_eq!((stored.files, stored.bytes), (2, 9));
    }

    #[test]
    fn the_files_are_flushed_in_the_staging_and_then_again_in_the_order_they_move() {
        let world = world();
        let probe = Probe {
            recording: Some(Recording::new()),
            ..Probe::default()
        };

        stored(&world, &probe);

        let files: Vec<String> = probe
            .recording
            .as_ref()
            .expect("recording")
            .flushes()
            .into_iter()
            .filter_map(|flush| match flush {
                Flush::File(path) => Some(path.to_string_lossy().into_owned()),
                Flush::Dir(_) => None,
            })
            .collect();
        let suffixes: Vec<&str> = files
            .iter()
            .map(|file| {
                [".tar.zst.sha256", ".sha256.zst", ".tar.zst"]
                    .into_iter()
                    .find(|suffix| file.ends_with(suffix))
                    .expect("a flushed file is a file of the version")
            })
            .collect();
        assert_eq!(files.len(), 6, "three in the staging, three on the move");
        assert_eq!(
            &suffixes[3..],
            [".sha256.zst", ".tar.zst.sha256", ".tar.zst"],
            "listing, checksum file, archive"
        );
    }

    #[test]
    fn a_second_version_in_the_same_second_gets_the_counter_2() {
        let world = world();
        stored(&world, &RealSeams);

        let second = stored(&world, &RealSeams);

        assert_eq!(second.stamp.to_string(), "2026-11-04T090211-2");
        assert_eq!(entries_of(&store_folder(&world)).len(), 6);
    }

    #[test]
    fn a_name_that_macos_refuses_is_a_note_naming_the_piece() {
        let entry = |name: &str| ListingEntry {
            path: path(name),
            kind: ListingType::File,
            size: 0,
            mtime: LONG_AGO,
            mode: 0o644,
            link: None,
            sha256: Some("0".repeat(64)),
        };
        let tree = PieceTree::new(path("piece"), Vec::new(), Vec::new());

        let notes = notes_of("piece", &tree, &[], &[entry("README"), entry("Readme")]);

        let notes: Vec<&str> = notes.iter().map(|n| n.message.as_str()).collect();
        assert!(
            notes
                .iter()
                .any(|note| note.starts_with("piece: ") && note.contains("become one name")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_fifo_is_a_note_naming_the_piece() {
        let mut world = world();
        world.temp.tree().fifo("piece/pipe");

        let stored = stored(&world, &RealSeams);

        let notes: Vec<&str> = stored.notes.iter().map(|n| n.message.as_str()).collect();
        assert!(
            notes
                .iter()
                .any(|note| note.starts_with("piece: ") && note.contains("pipe")),
            "{notes:?}"
        );
    }

    // =========================================================
    // A folder that changes, an archive that does not read back
    // =========================================================

    #[test]
    fn a_file_changed_after_packing_is_a_change_and_leaves_nothing() {
        let world = world();
        let file = world.temp.root().join("piece/a.txt");
        let probe = Probe {
            before_verify: Some(Box::new(move |_| {
                fs::write(&file, "ALPHA").expect("the file changes");
            })),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::Changed(_)),
            "{}",
            describe(&failure)
        );
        staging_is_gone(&world);
        store_is_empty(&world);
    }

    #[test]
    fn an_archive_that_reads_back_with_another_hash_fails_and_leaves_nothing() {
        let world = world();
        let probe = Probe {
            before_read_back: Some(Box::new(|staging| {
                let archive = entries_of(staging)
                    .into_iter()
                    .find(|name| name.ends_with(".tar.zst"))
                    .expect("the archive is in the staging");
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(staging.join(archive))
                    .expect("the archive opens");
                file.write_all(b"x").expect("the archive grows");
            })),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::Failed(_)),
            "{}",
            describe(&failure)
        );
        assert!(
            describe(&failure).contains("ArchiveDiffers"),
            "{}",
            describe(&failure)
        );
        staging_is_gone(&world);
        store_is_empty(&world);
    }

    // =========================================================
    // Room
    // =========================================================

    #[test]
    fn the_room_is_counted_over_the_packed_entries_only() {
        let mut world = world();
        world.temp.tree().sparse("piece/node_modules/huge", 1 << 30);
        let probe = Probe {
            free: Some(10),
            ..Probe::default()
        };

        stored(&world, &probe);
    }

    #[test]
    fn a_piece_that_does_not_fit_is_skipped_before_anything_is_created() {
        let world = world();
        let probe = Probe {
            free: Some(8),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::NoRoom(_)),
            "{}",
            describe(&failure)
        );
        assert_eq!(
            entries_of(&world.temp.root().join(".antiq/tmp")),
            Vec::<String>::new()
        );
        store_is_empty(&world);
    }

    #[test]
    fn pieces_running_beside_each_other_reserve_their_sum() {
        let world = world();
        let other = world
            .ledger
            .try_reserve(5, || Ok(1000))
            .expect("the other piece reserved");
        let probe = Probe {
            free: Some(12),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::NoRoom(_)),
            "{}",
            describe(&failure)
        );
        drop(other);
        stored(&world, &probe);
    }

    #[test]
    fn a_full_disk_while_packing_removes_the_staging_and_skips_the_piece() {
        let world = world();
        let probe = Probe {
            full_disk: true,
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::NoRoom(_)),
            "{}",
            describe(&failure)
        );
        staging_is_gone(&world);
        store_is_empty(&world);
    }

    // =========================================================
    // The move into the store
    // =========================================================

    #[test]
    fn a_move_that_stops_half_way_and_cannot_be_undone_keeps_the_staging() {
        let world = world();
        let recording = Recording::new();
        // Flushes 0 to 2 are the files in the staging, 3 is the listing on
        // its way, 4 the checksum file, which fails, and 5 the listing on
        // its way back, which fails as well.
        recording.fail_files_from(4);
        let probe = Probe {
            recording: Some(recording),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::Failed(_)),
            "{}",
            describe(&failure)
        );
        assert!(
            describe(&failure).contains("PartlyMoved"),
            "{}",
            describe(&failure)
        );
        let pack = world.temp.root().join(".antiq/tmp/pack");
        assert_eq!(
            entries_of(&pack),
            ["2026-11-04T090211"],
            "the staging stays"
        );
        let staged = pack.join("2026-11-04T090211/piece");
        assert!(
            entries_of(&staged)
                .iter()
                .any(|name| name.ends_with(".tar.zst")),
            "the archive stays staged for repair 1"
        );
    }

    #[test]
    fn a_store_folder_that_cannot_be_flushed_after_the_archive_moved_keeps_the_version() {
        let world = world();
        let counting = Probe {
            recording: Some(Recording::new()),
            ..Probe::default()
        };
        stored(&world, &counting);
        let dir_flushes = counting
            .recording
            .as_ref()
            .expect("recording")
            .flushes()
            .iter()
            .filter(|flush| matches!(flush, Flush::Dir(_)))
            .count();
        let fresh = self::world();
        let recording = Recording::new();
        recording.fail_dirs_at(dir_flushes - 1);
        let probe = Probe {
            recording: Some(recording),
            ..Probe::default()
        };

        let failure = failure(&fresh, &probe);

        assert!(
            matches!(failure, StoreFailure::NotDurable(_)),
            "{}",
            describe(&failure)
        );
        assert!(
            entries_of(&store_folder(&fresh))
                .iter()
                .any(|name| name.ends_with(".tar.zst")),
            "the version stays in the store"
        );
        staging_is_gone(&fresh);
    }

    #[test]
    fn a_stamp_whose_files_are_taken_is_skipped_by_the_counter() {
        let world = world();
        let stored_first = stored(&world, &RealSeams);
        fs::write(
            store_folder(&world).join(stored_first.names.archive.replace("090211", "090211-2")),
            "taken",
        )
        .expect("a file takes the second name");
        // The counter skips a taken stamp, so a third store does not clash.
        let third = stored(&world, &RealSeams);

        assert_eq!(third.stamp.to_string(), "2026-11-04T090211-3");
        staging_is_kept_empty(&world, &third.stamp);
    }

    fn store_folder_with_mode(world: &World, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let folder = store_folder(world);
        fs::create_dir_all(&folder).expect("the store folder is made");
        fs::set_permissions(&folder, fs::Permissions::from_mode(mode)).expect("the mode is set");
    }

    fn unlock_store_folder(world: &World) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(store_folder(world), fs::Permissions::from_mode(0o755))
            .expect("the mode is restored");
    }

    #[test]
    fn a_store_folder_that_cannot_be_read_stops_before_the_staging_is_made() {
        let world = world();
        store_folder_with_mode(&world, 0o000);
        assert!(
            fs::read_dir(store_folder(&world)).is_err(),
            "the test runs as a user the mode binds"
        );

        let failure = failure(&world, &RealSeams);

        let StoreFailure::Failed(error) = &failure else {
            panic!("the stamp fails, but {}", describe(&failure));
        };
        assert_eq!(error.to_string(), "cannot choose a version stamp for piece");
        assert!(entries_of(&world.temp.root().join(".antiq/tmp/pack")).is_empty());
        unlock_store_folder(&world);
    }

    #[test]
    fn a_file_that_cannot_be_flushed_in_the_staging_names_the_file_and_removes_the_staging() {
        let world = world();
        let recording = Recording::new();
        recording.fail_files_at(0);
        let probe = Probe {
            recording: Some(recording),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        let StoreFailure::Failed(error) = &failure else {
            panic!("the flush fails, but {}", describe(&failure));
        };
        assert_eq!(
            error.to_string(),
            "cannot make piece--2026-11-04T090211.tar.zst in tmp/pack/2026-11-04T090211/piece durable"
        );
        staging_is_gone(&world);
        store_is_empty(&world);
    }

    #[test]
    fn a_store_folder_that_refuses_the_files_undoes_the_move_and_removes_the_staging() {
        let world = world();
        store_folder_with_mode(&world, 0o555);
        assert!(
            fs::File::create(store_folder(&world).join("probe")).is_err(),
            "the test runs as a user the mode binds"
        );

        let failure = failure(&world, &RealSeams);

        let StoreFailure::Failed(error) = &failure else {
            panic!("the move fails, but {}", describe(&failure));
        };
        assert_eq!(
            error.to_string(),
            "cannot move the version 2026-11-04T090211 of piece into the store"
        );
        staging_is_gone(&world);
        assert!(entries_of(&store_folder(&world)).is_empty());
        unlock_store_folder(&world);
    }

    // =========================================================
    // What the failures say
    // =========================================================

    #[test]
    fn a_failure_names_the_step_and_the_place() {
        let world = world();
        let file = world.temp.root().join("piece/a.txt");
        let probe = Probe {
            before_verify: Some(Box::new(move |_| {
                fs::write(&file, "ALPHA").expect("the file changes");
            })),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        let StoreFailure::Changed(error) = failure else {
            panic!("the folder changed");
        };
        assert_eq!(
            error.to_string(),
            "cannot verify the packing of piece in tmp/pack/2026-11-04T090211"
        );
    }

    #[test]
    fn an_archive_that_cannot_be_read_in_the_verify_is_a_failure_and_no_change_of_the_folder() {
        let world = world();
        let probe = Probe {
            before_verify: Some(Box::new(|staging| {
                for entry in fs::read_dir(staging).expect("the staging is listed") {
                    let file = entry.expect("an entry").path();
                    if file.extension().is_some_and(|extension| extension == "zst") {
                        fs::write(&file, "not an archive").expect("the archive is replaced");
                    }
                }
            })),
            ..Probe::default()
        };

        let failure = failure(&world, &probe);

        assert!(
            matches!(failure, StoreFailure::Failed(_)),
            "{}",
            describe(&failure)
        );
        staging_is_gone(&world);
        store_is_empty(&world);
    }

    #[test]
    fn a_verify_that_runs_out_of_descriptors_is_told_apart_from_a_changed_folder() {
        let out_of_descriptors = |source| ArchiveError::Verify {
            piece: "piece".into(),
            stamp: "2026-11-04T090211".into(),
            source,
        };
        let walk = PackError::Walk(crate::walk::WalkError::OutOfDescriptors {
            piece: path("piece"),
            path: path("piece/sub"),
            source: io::Error::from_raw_os_error(libc::EMFILE),
        });

        let abort = verify_abort(out_of_descriptors(walk));

        assert!(
            matches!(abort.failure, StoreFailure::OutOfDescriptors(_)),
            "a failure other than OutOfDescriptors"
        );
        assert!(!abort.keep_staging);
    }
}
