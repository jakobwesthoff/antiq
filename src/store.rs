// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The local object store in .antiq/objects/.

use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::FsxError;
use crate::fsx::atomic::{flush_dir, flush_file, write_atomic};
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta::{self, FileName, MetaError, Metadata};
use crate::pack::hashing::hex;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Errors of the local object store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error("the recorded change (meta.pending) cannot be read: {reason}")]
    PendingUnreadable { reason: String },
    #[error(
        "the highest metadata version {number} cannot be read, so nothing is written on top of it"
    )]
    UnreadableHighest { number: u32 },
    #[error("no version of the object can be read; unreadable versions: {}", numbers(.unreadable))]
    NothingReadable { unreadable: Vec<Unreadable> },
    #[error("there is no recorded change to commit")]
    NoPending,
    #[error("version {number} is not above the highest local version {highest}")]
    VersionNotNewer { number: u32, highest: u32 },
    #[error("the recorded change is not the document that was committed")]
    PendingChanged,
    #[error("version {number} exists with other content; a committed version never changes")]
    CommittedChanged { number: u32 },
    #[error("`{name}` exists with other content")]
    PayloadConflict { name: String },
    #[error("`{name}` is not the name of a payload or a listing")]
    NotAStoredName { name: String },
}

impl HasKind for StoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            StoreError::Meta(error) => error.kind(),
            // The versions are listed highest first. When the highest one
            // states a format this version does not know, the object is
            // out of reach because antiq is too old, not because it is
            // broken.
            StoreError::NothingReadable { unreadable }
                if unreadable
                    .first()
                    .and_then(|version| version.format)
                    .is_some_and(|format| format > meta::FORMAT) =>
            {
                ErrorKind::FormatTooNew
            }
            _ => ErrorKind::Other,
        }
    }
}

fn numbers(versions: &[Unreadable]) -> String {
    let numbers: Vec<String> = versions
        .iter()
        .map(|version| version.number.to_string())
        .collect();
    numbers.join(", ")
}

/// Where the current metadata of an object comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A recorded change that no sync has committed yet.
    Pending,
    /// The committed version with this number.
    Committed(u32),
}

/// A committed version above the one in use that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub number: u32,
    /// The `format` of the document, when it states one.
    pub format: Option<u32>,
    pub reason: String,
}

/// The metadata an object currently has.
#[derive(Debug, Clone, PartialEq)]
pub struct Current {
    pub metadata: Metadata,
    pub source: Source,
    /// Committed versions above the highest readable one that cannot be
    /// read, highest first.
    pub unreadable: Vec<Unreadable>,
}

/// What [`Store::promote_pending`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promoted {
    Renamed,
    /// The version was already in place; the leftover `meta.pending` is
    /// gone now.
    AlreadyCommitted,
}

/// What [`Store::put_file`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Stored,
    /// The same bytes were in place already; the source was left alone.
    AlreadyPresent,
}

/// The objects below `.antiq/objects/`.
///
/// Reading takes no lock. Every write takes the proof that the run lock
/// is held, which only `lock::acquire` hands out.
///
/// ```compile_fail,E0061
/// use antiq::meta::Metadata;
/// use antiq::store::Store;
///
/// // The lock proof is a parameter, and `RunLock` cannot be built outside
/// // the `lock` module (see its own doctest, E0451), so a write without a
/// // lock does not compile.
/// fn record(store: &Store<'_>, metadata: &Metadata) {
///     let _ = store.write_pending(metadata);
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Store<'c> {
    collection: &'c Collection,
}

impl<'c> Store<'c> {
    pub fn new(collection: &'c Collection) -> Store<'c> {
        Store { collection }
    }

    // =========================================================
    // Reading
    // =========================================================

    /// The objects that have a directory, in ID order. Names that are not
    /// the canonical (upper case) spelling of an ID are skipped, since
    /// every path of the store is built from that spelling.
    pub fn objects(&self) -> Result<Vec<ObjectId>, StoreError> {
        let mut ids = Vec::new();
        for name in self.names_in(&self.collection.objects_dir(), true)? {
            if let Ok(id) = name.parse::<ObjectId>()
                && id.to_string() == name
            {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// The numbers of the committed versions in the object's directory,
    /// ascending. `meta.pending`, claims and temp names are not versions.
    pub fn committed_versions(&self, id: ObjectId) -> Result<Vec<u32>, StoreError> {
        let mut numbers: Vec<u32> = self
            .names_in(&self.collection.object_dir(id), false)?
            .iter()
            .filter_map(|name| version_number(name))
            .collect();
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// The metadata the object has now: the recorded change when there is
    /// one, else the highest committed version that can be read.
    ///
    /// A recorded change that cannot be read is an error, since it holds
    /// the only copy of a change that no sync has committed, and falling
    /// back to an older version would let the next change overwrite it.
    /// Committed versions above the one in use that cannot be read are
    /// listed, whichever way the metadata comes.
    pub fn current(&self, id: ObjectId) -> Result<Option<Current>, StoreError> {
        let mut unreadable = Vec::new();
        let mut committed = None;
        for number in self.committed_versions(id)?.into_iter().rev() {
            let path = self.meta_path(id, number);
            let bytes = fs::read(&path).map_err(io_error("read", &path))?;
            match parse_for(id, &bytes) {
                Ok(metadata) => {
                    committed = Some((number, metadata));
                    break;
                }
                Err(reason) => unreadable.push(Unreadable {
                    number,
                    format: meta::read_format(&bytes).ok(),
                    reason,
                }),
            }
        }

        let pending_path = self.collection.meta_pending_path(id);
        if let Some(bytes) = read_if_present(&pending_path)? {
            let metadata =
                parse_for(id, &bytes).map_err(|reason| StoreError::PendingUnreadable { reason })?;
            return Ok(Some(Current {
                metadata,
                source: Source::Pending,
                unreadable,
            }));
        }
        match committed {
            Some((number, metadata)) => Ok(Some(Current {
                metadata,
                source: Source::Committed(number),
                unreadable,
            })),
            None if unreadable.is_empty() => Ok(None),
            None => Err(StoreError::NothingReadable { unreadable }),
        }
    }

    /// The path of the committed version `number` of the object.
    pub fn meta_path(&self, id: ObjectId, number: u32) -> PathBuf {
        self.collection
            .object_dir(id)
            .join(format!("meta.{number}"))
    }

    /// The names of the directories or of the files in `dir`; none when
    /// `dir` does not exist.
    fn names_in(&self, dir: &Path, directories: bool) -> Result<Vec<String>, StoreError> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error("list", dir)(error)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error("list", dir))?;
            let file_type = entry.file_type().map_err(io_error("stat", &entry.path()))?;
            if file_type.is_dir() == directories
                && let Some(name) = entry.file_name().to_str()
            {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    // =========================================================
    // Writing metadata
    // =========================================================

    /// Records `metadata` as the object's next version, replacing an
    /// earlier recorded change. A sync commits it later.
    ///
    /// Nothing is written on top of a committed version that has a higher
    /// `format` than this version knows (it would be a write to a file
    /// that is read-only to us) or that cannot be read: the document that
    /// sync would commit has to be built from a version that was seen. An
    /// earlier recorded change that cannot be read is not replaced either.
    pub fn write_pending(&self, _lock: &RunLock, metadata: &Metadata) -> Result<(), StoreError> {
        let bytes = meta::to_bytes(metadata)?;
        self.check_writable(metadata.id)?;
        self.ensure_object_dir(metadata.id)?;
        write_atomic(&self.collection.meta_pending_path(metadata.id), &bytes)?;
        Ok(())
    }

    /// Fails with the error [`Store::write_pending`] would give for the
    /// object, without writing. A caller that does something irreversible
    /// before its write asks first.
    pub fn check_writable(&self, id: ObjectId) -> Result<(), StoreError> {
        // An earlier recorded change that cannot be read is the only copy
        // of that change; a write would replace it.
        let pending_path = self.collection.meta_pending_path(id);
        if let Some(pending) = read_if_present(&pending_path)? {
            parse_for(id, &pending).map_err(|reason| StoreError::PendingUnreadable { reason })?;
        }
        self.check_highest_is_writable(id)
    }

    fn check_highest_is_writable(&self, id: ObjectId) -> Result<(), StoreError> {
        let Some(&number) = self.committed_versions(id)?.last() else {
            return Ok(());
        };
        let path = self.meta_path(id, number);
        let bytes = fs::read(&path).map_err(io_error("read", &path))?;
        if let Ok(found) = meta::read_format(&bytes)
            && found > meta::FORMAT
        {
            return Err(MetaError::FormatTooNew { found }.into());
        }
        parse_for(id, &bytes).map_err(|_| StoreError::UnreadableHighest { number })?;
        Ok(())
    }

    /// Stores the bytes of a version that was committed by someone else
    /// (pulled from the remote) as they are. They are not parsed: a
    /// version of a later format, or one that cannot be read, is kept so
    /// that [`Store::current`] can report it. A version that is there
    /// already is left alone if the bytes agree.
    pub fn store_committed(
        &self,
        _lock: &RunLock,
        id: ObjectId,
        number: NonZeroU32,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        let path = self.meta_path(id, number.get());
        match read_if_present(&path)? {
            Some(existing) if existing == bytes => Ok(()),
            Some(_) => Err(StoreError::CommittedChanged {
                number: number.get(),
            }),
            None => {
                self.ensure_object_dir(id)?;
                write_atomic(&path, bytes)?;
                Ok(())
            }
        }
    }

    /// Turns the recorded change into the committed version `number`,
    /// after the commit protocol gave that number and the bytes of
    /// `meta.pending` (whose SHA-256 is `committed_sha256`) were
    /// uploaded.
    ///
    /// A commit goes: [`Metadata::history_added_over`] and
    /// [`Metadata::stamp_committed`] on the document, `write_pending` of
    /// it, upload of the bytes of `meta.pending`, this call. The file is
    /// renamed and not rewritten, so the local version and the remote's
    /// hold the same bytes; a recorded change that has changed since the
    /// upload is refused.
    ///
    /// A crash between the rename on the remote and this call leaves a
    /// `meta.pending` whose content is also the committed version, which
    /// the next sync pulls as `meta.<number>`. Promoting again then finds
    /// the version in place and removes the leftover recorded change.
    ///
    /// When only the final flush of the directory fails, the rename or the
    /// removal has happened and the error means that it may not be
    /// durable.
    ///
    /// ```compile_fail,E0061
    /// use antiq::id::ObjectId;
    /// use antiq::store::Store;
    /// use std::num::NonZeroU32;
    ///
    /// // As for `write_pending`: the lock proof is a parameter that cannot
    /// // be forged, so promoting without it does not compile.
    /// fn promote(store: &Store<'_>, id: ObjectId, number: NonZeroU32) {
    ///     let _ = store.promote_pending(id, number, "0");
    /// }
    /// ```
    pub fn promote_pending(
        &self,
        _lock: &RunLock,
        id: ObjectId,
        number: NonZeroU32,
        committed_sha256: &str,
    ) -> Result<Promoted, StoreError> {
        let pending_path = self.collection.meta_pending_path(id);
        let Some(pending) = read_if_present(&pending_path)? else {
            return Err(StoreError::NoPending);
        };
        if sha256(&pending) != committed_sha256 {
            return Err(StoreError::PendingChanged);
        }

        let target = self.meta_path(id, number.get());
        let object_dir = self.collection.object_dir(id);
        if let Some(existing) = read_if_present(&target)?
            && sha256(&existing) == committed_sha256
        {
            fs::remove_file(&pending_path).map_err(io_error("remove", &pending_path))?;
            flush_dir(&object_dir)?;
            return Ok(Promoted::AlreadyCommitted);
        }

        let highest = self
            .committed_versions(id)?
            .last()
            .copied()
            .unwrap_or_default();
        if number.get() <= highest {
            return Err(StoreError::VersionNotNewer {
                number: number.get(),
                highest,
            });
        }
        fs::rename(&pending_path, &target).map_err(io_error("rename", &pending_path))?;
        flush_dir(&object_dir)?;
        Ok(Promoted::Renamed)
    }

    /// Removes the recorded change because a committed version above every
    /// version stored here holds its history, which makes the change a part
    /// of that version. The file is removed only while it still has the
    /// bytes that were committed, and durably: the directory is flushed.
    ///
    /// Only a settle that history proves may call this. The recorded change
    /// is the only local copy of those bytes, so a caller has to know that
    /// a committed version holds them.
    pub fn discard_pending(
        &self,
        _lock: &RunLock,
        id: ObjectId,
        committed_sha256: &str,
    ) -> Result<(), StoreError> {
        let pending_path = self.collection.meta_pending_path(id);
        let Some(pending) = read_if_present(&pending_path)? else {
            return Err(StoreError::NoPending);
        };
        if sha256(&pending) != committed_sha256 {
            return Err(StoreError::PendingChanged);
        }
        fs::remove_file(&pending_path).map_err(io_error("remove", &pending_path))?;
        flush_dir(&self.collection.object_dir(id))?;
        Ok(())
    }

    // =========================================================
    // Payloads and listings
    // =========================================================

    /// The path of a payload or listing of the object.
    pub fn file_path(&self, id: ObjectId, name: &FileName) -> PathBuf {
        self.collection.object_dir(id).join(name.as_str())
    }

    /// Moves the payload or listing `from`, written in `.antiq/tmp/` and
    /// hashing to `sha256`, under its final `name` in the object's
    /// directory. Files there are never replaced: a name that is taken
    /// accepts the put only when the file in place has the same full
    /// SHA-256, and then `from` is left to its owner. The name carries only
    /// 16 digits of it, so the full hash is what tells a repeated put from
    /// a different file with the same name.
    ///
    /// The file is flushed before it is renamed and the directory after,
    /// so a name in place always has all its bytes. Nothing else writes in
    /// the directory while the run lock is held, so the check for the name
    /// and the rename cannot race. A failure before the rename leaves
    /// `from` where it was. When only the final flush of the directory
    /// fails, the file is already in place and the error means that its
    /// entry may not be durable; putting it again finds the name and
    /// returns [`Put::AlreadyPresent`].
    ///
    /// ```compile_fail,E0061
    /// use antiq::id::ObjectId;
    /// use antiq::meta::FileName;
    /// use antiq::store::Store;
    /// use std::path::Path;
    ///
    /// // As for `write_pending`: the lock proof is a parameter that cannot
    /// // be forged, so a put without it does not compile.
    /// fn put(store: &Store<'_>, id: ObjectId, name: &FileName, from: &Path) {
    ///     let _ = store.put_file(id, name, from, "0");
    /// }
    /// ```
    pub fn put_file(
        &self,
        _lock: &RunLock,
        id: ObjectId,
        name: &FileName,
        from: &Path,
        sha256: &str,
    ) -> Result<Put, StoreError> {
        // Only the names of the remote layout go through here, so that a
        // metadata file cannot be put in place as if it were a payload.
        let text = name.as_str();
        if !meta::is_payload_file_name(text) && !meta::is_listing_file_name(text) {
            return Err(StoreError::NotAStoredName {
                name: text.to_string(),
            });
        }

        let target = self.file_path(id, name);
        if target.exists() {
            let present = file_sha256(&target).map_err(io_error("read", &target))?;
            return if present == sha256 {
                Ok(Put::AlreadyPresent)
            } else {
                Err(StoreError::PayloadConflict {
                    name: text.to_string(),
                })
            };
        }

        let source = File::open(from).map_err(io_error("open", from))?;
        let dir = self.ensure_object_dir(id)?;
        flush_file(&source, from)?;
        fs::rename(from, &target).map_err(io_error("move", from))?;
        flush_dir(&dir)?;
        Ok(Put::Stored)
    }

    // =========================================================
    // Directories
    // =========================================================

    fn ensure_object_dir(&self, id: ObjectId) -> Result<PathBuf, StoreError> {
        self.ensure_object_dir_with(id, &flush_dir)
    }

    /// Makes the object's directory, and flushes `objects/` and `.antiq/`
    /// whether or not it had to make anything. A run that was killed
    /// after it created a directory but before the flush leaves a
    /// directory the next run finds in place, so only flushing every time
    /// makes sure the entry is durable before the payload and the
    /// metadata that depend on it are.
    fn ensure_object_dir_with(
        &self,
        id: ObjectId,
        flush: &dyn Fn(&Path) -> Result<(), FsxError>,
    ) -> Result<PathBuf, StoreError> {
        let dir = self.collection.object_dir(id);
        fs::create_dir_all(&dir).map_err(io_error("create", &dir))?;
        flush(&self.collection.antiq_dir())?;
        flush(&self.collection.objects_dir())?;
        Ok(dir)
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> StoreError {
    let path = path.display().to_string();
    move |source| StoreError::Io {
        action,
        path,
        source,
    }
}

/// The file's content, or `None` when there is no such file.
fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("read", path)(error)),
    }
}

/// The metadata in `bytes`, if it can be read and belongs to the object
/// whose directory it is in; else the reason it cannot be used.
fn parse_for(id: ObjectId, bytes: &[u8]) -> Result<Metadata, String> {
    let metadata = meta::parse(bytes).map_err(|error| error.to_string())?;
    if metadata.id != id {
        return Err(format!("it belongs to {}", metadata.id));
    }
    Ok(metadata)
}

/// The number of `meta.<N>`: decimal digits only, no leading zero, above
/// zero, and small enough for a `u32`.
pub(crate) fn version_number(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("meta.")?;
    if digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The SHA-256 of a file, read in chunks: payloads do not fit in memory.
fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        match file.read(&mut chunk)? {
            0 => return Ok(hex(&hasher.finalize())),
            read => hasher.update(&chunk[..read]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::lock;
    use crate::meta::FORMAT;
    use crate::testing::tree::TempCollection;
    use std::cell::RefCell;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    const ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    const OTHER_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
    /// An ID that only ever appears in lower case, since the test volume
    /// may not tell the two spellings apart.
    const LOWER_CASE_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    /// A collection in a temp directory with the run lock held.
    struct Fixture {
        temp: TempCollection,
        collection: Collection,
        lock: RunLock,
    }

    impl Fixture {
        fn new() -> Fixture {
            let temp = TempCollection::minimal();
            let collection = Collection::find(temp.root()).expect("a collection");
            let clock = FixedClock::new("2026-01-02T03:04:05Z".parse().expect("valid"));
            let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("free");
            Fixture {
                temp,
                collection,
                lock,
            }
        }

        fn store(&self) -> Store<'_> {
            Store::new(&self.collection)
        }

        fn object_dir(&self, text: &str) -> PathBuf {
            self.collection.object_dir(id(text))
        }

        /// Puts `contents` at `name` in the object's directory.
        fn put(&mut self, object: &str, name: &str, contents: impl AsRef<[u8]>) {
            self.temp
                .tree()
                .file(&format!(".antiq/objects/{object}/{name}"), contents);
        }
    }

    /// A format 1 document of `object` whose history has `entries`
    /// entries, which tells versions of it apart.
    fn document(object: &str, entries: usize) -> String {
        let history: Vec<String> = (0..entries)
            .map(|n| {
                format!(
                    r#"{{"time":"2020-09-13T12:26:{n:02}Z","machine":"imperius","action":"sync"}}"#
                )
            })
            .collect();
        format!(
            concat!(
                r#"{{"format":1,"id":"{}","collection":"01BX5ZZKBKACTAV9WEVGEMMVRZ","#,
                r#""path":"a","name":"a","created":"2020-09-13T12:26:40Z","#,
                r#""created_source":"birth","lifecycle":"live","payloads":[],"history":[{}]}}"#
            ),
            object,
            history.join(",")
        )
    }

    fn metadata(object: &str, entries: usize) -> Metadata {
        meta::parse(document(object, entries).as_bytes()).expect("a valid document")
    }

    fn one() -> NonZeroU32 {
        NonZeroU32::new(1).expect("not zero")
    }

    fn number(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("not zero")
    }

    // ---------------------------------------------------------
    // Listing objects and versions
    // ---------------------------------------------------------

    #[test]
    fn objects_are_the_directories_named_by_a_canonical_ulid_sorted() {
        let mut fx = Fixture::new();
        fx.put(OTHER_ID, "meta.1", "x");
        fx.put(ID, "meta.1", "x");
        fx.put("not-an-id", "meta.1", "x");
        fx.put(&LOWER_CASE_ID.to_lowercase(), "meta.1", "x");
        fx.temp.tree().file(".antiq/objects/stray-file", "x");
        assert_eq!(
            fx.store().objects().expect("lists"),
            vec![id(ID), id(OTHER_ID)]
        );
    }

    #[test]
    fn an_objects_directory_that_cannot_be_listed_is_an_error_naming_it() {
        let fx = Fixture::new();
        let objects = fx.collection.objects_dir();
        fs::set_permissions(&objects, fs::Permissions::from_mode(0o000)).expect("chmod");
        let _restore = crate::fsx::RestoreModes(vec![objects.clone()]);
        assert!(
            fs::read_dir(&objects).is_err(),
            "the precondition: a 0000 directory cannot be listed, so run as a non-root user"
        );
        let error = fx.store().objects().expect_err("not listable");
        assert!(
            matches!(error, StoreError::Io { action: "list", .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("objects"), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_recorded_change_that_cannot_be_read_as_a_file_is_an_error() {
        let mut fx = Fixture::new();
        fx.temp
            .tree()
            .dir(&format!(".antiq/objects/{ID}/meta.pending"));
        let error = fx.store().current(id(ID)).expect_err("a directory");
        assert!(
            matches!(error, StoreError::Io { action: "read", .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_missing_objects_directory_has_no_objects() {
        let fx = Fixture::new();
        fs::remove_dir(fx.collection.objects_dir()).expect("empty");
        assert_eq!(fx.store().objects().expect("lists"), vec![]);
    }

    #[test]
    fn only_meta_files_with_a_plain_number_are_versions() {
        let mut fx = Fixture::new();
        for name in [
            "meta.10",
            "meta.9",
            "meta.2",
            "meta.pending",
            "meta.3.claim",
            "meta.03",
            "meta.0",
            "meta.x",
            "meta.",
            ".meta.3.1.2.tmp",
            "meta.99999999999",
        ] {
            fx.put(ID, name, "x");
        }
        assert_eq!(
            fx.store().committed_versions(id(ID)).expect("lists"),
            vec![2, 9, 10]
        );
    }

    #[test]
    fn an_object_without_a_directory_has_no_versions() {
        let fx = Fixture::new();
        assert_eq!(
            fx.store().committed_versions(id(ID)).expect("lists"),
            Vec::<u32>::new()
        );
    }

    // ---------------------------------------------------------
    // The current metadata
    // ---------------------------------------------------------

    #[test]
    fn an_object_without_metadata_has_no_current() {
        let fx = Fixture::new();
        assert_eq!(fx.store().current(id(ID)).expect("reads"), None);
    }

    #[test]
    fn the_current_metadata_is_the_highest_version_that_reads() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.2", document(ID, 2));
        fx.put(ID, "meta.3", "{ not json");
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.metadata, metadata(ID, 2));
        assert_eq!(current.source, Source::Committed(2));
        assert_eq!(current.unreadable.len(), 1);
        assert_eq!(current.unreadable[0].number, 3);
        assert_eq!(current.unreadable[0].format, None);
    }

    #[test]
    fn an_unreadable_version_reports_the_format_it_states() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.2", r#"{"format":2,"surprise":true}"#);
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Committed(1));
        assert_eq!(current.unreadable[0].format, Some(2));
    }

    #[test]
    fn a_version_of_a_later_format_is_current_when_it_fits() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(
            ID,
            "meta.2",
            document(ID, 2).replace(r#""format":1"#, r#""format":2"#),
        );
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Committed(2));
        assert_eq!(current.metadata.format, 2);
    }

    #[test]
    fn a_recorded_change_wins_over_the_committed_versions() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.pending", document(ID, 2));
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.metadata, metadata(ID, 2));
        assert_eq!(current.source, Source::Pending);
    }

    #[test]
    fn a_recorded_change_alone_is_current() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.pending", document(ID, 1));
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Pending);
        assert_eq!(current.unreadable, vec![]);
    }

    #[test]
    fn a_recorded_change_that_cannot_be_read_is_an_error_and_not_skipped() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.pending", "{ not json");
        let error = fx.store().current(id(ID)).expect_err("never skipped");
        assert!(
            matches!(error, StoreError::PendingUnreadable { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_unreadable_highest_version_is_reported_next_to_a_recorded_change() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.2", "{ not json");
        fx.put(ID, "meta.pending", document(ID, 2));
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Pending);
        assert_eq!(
            current
                .unreadable
                .iter()
                .map(|u| u.number)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn a_version_of_another_object_is_unreadable() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.2", document(OTHER_ID, 2));
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Committed(1));
        assert!(
            current.unreadable[0].reason.contains(OTHER_ID),
            "{current:?}"
        );
    }

    #[test]
    fn a_recorded_change_of_another_object_is_unreadable() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.pending", document(OTHER_ID, 1));
        let error = fx.store().current(id(ID)).expect_err("not this object's");
        assert!(
            matches!(error, StoreError::PendingUnreadable { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_object_with_nothing_readable_is_an_error_naming_the_versions() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", "{ not json");
        fx.put(ID, "meta.2", "{ not json");
        let error = fx.store().current(id(ID)).expect_err("nothing reads");
        assert!(
            matches!(&error, StoreError::NothingReadable { unreadable }
                if unreadable.iter().map(|version| version.number).collect::<Vec<_>>() == [2, 1]),
            "{error:?}"
        );
        assert!(error.to_string().ends_with("versions: 2, 1"), "{error}");
    }

    #[test]
    fn an_object_whose_only_version_is_of_a_later_format_is_too_new_and_not_just_unreadable() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", r#"{"format":2,"surprise":true}"#);
        let error = fx.store().current(id(ID)).expect_err("nothing fits");
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
        assert_eq!(
            crate::error::Error::from(error).kind(),
            ErrorKind::FormatTooNew
        );
    }

    // ---------------------------------------------------------
    // Recording a change
    // ---------------------------------------------------------

    #[test]
    fn a_recorded_change_is_written_and_read_back_with_its_unknown_keys() {
        let fx = Fixture::new();
        let text = document(ID, 1).replace(r#""history":"#, r#""future":{"a":1},"history":"#);
        let written = meta::parse(text.as_bytes()).expect("valid");
        fx.store()
            .write_pending(&fx.lock, &written)
            .expect("writes");

        let on_disk = fs::read(fx.collection.meta_pending_path(id(ID))).expect("a file");
        assert_eq!(on_disk, meta::to_bytes(&written).expect("bytes"));
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Pending);
        assert_eq!(
            current.metadata.extra.get("future"),
            written.extra.get("future")
        );
        assert!(current.metadata.extra.contains_key("future"));
    }

    #[test]
    fn a_second_recorded_change_replaces_the_first() {
        let fx = Fixture::new();
        fx.store()
            .write_pending(&fx.lock, &metadata(ID, 1))
            .expect("writes");
        fx.store()
            .write_pending(&fx.lock, &metadata(ID, 2))
            .expect("writes");
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.metadata, metadata(ID, 2));
    }

    #[test]
    fn a_document_of_a_later_format_is_not_recorded() {
        let fx = Fixture::new();
        let mut later = metadata(ID, 1);
        later.format = FORMAT + 1;
        let error = fx
            .store()
            .write_pending(&fx.lock, &later)
            .expect_err("read-only");
        assert!(matches!(
            &error,
            StoreError::Meta(MetaError::FormatTooNew { found: 2 })
        ));
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
        assert_eq!(
            crate::error::Error::from(error).kind(),
            ErrorKind::FormatTooNew
        );
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn nothing_is_recorded_on_top_of_a_committed_version_of_a_later_format() {
        let mut fx = Fixture::new();
        fx.put(
            ID,
            "meta.1",
            document(ID, 1).replace(r#""format":1"#, r#""format":2"#),
        );
        let error = fx
            .store()
            .write_pending(&fx.lock, &metadata(ID, 2))
            .expect_err("newer");
        assert!(matches!(
            &error,
            StoreError::Meta(MetaError::FormatTooNew { found: 2 })
        ));
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn a_later_format_that_does_not_fit_still_blocks_a_recorded_change_as_too_new() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", r#"{"format":2,"surprise":true}"#);
        let error = fx
            .store()
            .write_pending(&fx.lock, &metadata(ID, 2))
            .expect_err("newer");
        assert!(matches!(
            &error,
            StoreError::Meta(MetaError::FormatTooNew { found: 2 })
        ));
    }

    #[test]
    fn nothing_is_recorded_on_top_of_a_version_that_cannot_be_read() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.1", document(ID, 1));
        fx.put(ID, "meta.2", "{ not json");
        let error = fx
            .store()
            .write_pending(&fx.lock, &metadata(ID, 2))
            .expect_err("blocked");
        assert!(
            matches!(error, StoreError::UnreadableHighest { number: 2 }),
            "{error:?}"
        );
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn a_recorded_change_that_cannot_be_read_is_not_replaced() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.pending", "{ not json");
        let error = fx
            .store()
            .write_pending(&fx.lock, &metadata(ID, 1))
            .expect_err("the only copy of a change");
        assert!(
            matches!(error, StoreError::PendingUnreadable { .. }),
            "{error:?}"
        );
        assert_eq!(
            fs::read(fx.collection.meta_pending_path(id(ID))).expect("a file"),
            b"{ not json"
        );
    }

    #[test]
    fn a_failed_write_leaves_the_recorded_change_as_it_was() {
        let fx = Fixture::new();
        fx.store()
            .write_pending(&fx.lock, &metadata(ID, 1))
            .expect("writes");
        let directory = fx.object_dir(ID);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o555)).expect("chmod");
        let _restore = crate::fsx::RestoreModes(vec![directory.clone()]);
        assert!(
            fs::File::create(directory.join("probe")).is_err(),
            "the precondition: a 0555 directory refuses new files, so run as a non-root user"
        );

        let error = fx.store().write_pending(&fx.lock, &metadata(ID, 2));
        assert!(error.is_err());
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.metadata, metadata(ID, 1));
    }

    #[test]
    fn the_object_directory_is_made_and_its_parents_flushed_even_when_they_exist() {
        let fx = Fixture::new();
        let flushed = RefCell::new(Vec::new());
        let record = |path: &Path| {
            flushed.borrow_mut().push(path.to_path_buf());
            Ok(())
        };
        let dir = fx
            .store()
            .ensure_object_dir_with(id(ID), &record)
            .expect("made");
        assert_eq!(dir, fx.object_dir(ID));
        assert!(dir.is_dir());
        let expected = vec![fx.collection.antiq_dir(), fx.collection.objects_dir()];
        assert_eq!(*flushed.borrow(), expected);

        flushed.borrow_mut().clear();
        fx.store()
            .ensure_object_dir_with(id(ID), &record)
            .expect("exists");
        assert_eq!(*flushed.borrow(), expected);
    }

    #[test]
    fn a_missing_objects_directory_is_made_with_the_object_directory() {
        let fx = Fixture::new();
        fs::remove_dir(fx.collection.objects_dir()).expect("empty");
        fx.store()
            .write_pending(&fx.lock, &metadata(ID, 1))
            .expect("writes");
        assert!(fx.collection.meta_pending_path(id(ID)).is_file());
    }

    #[test]
    fn a_failed_flush_of_a_parent_is_reported() {
        let fx = Fixture::new();
        let fail = |path: &Path| {
            Err(FsxError::Io {
                action: "flush directory",
                path: path.display().to_string(),
                source: io::Error::other("no"),
            })
        };
        let error = fx
            .store()
            .ensure_object_dir_with(id(ID), &fail)
            .expect_err("fails");
        assert!(matches!(error, StoreError::Fsx(_)), "{error:?}");
    }

    // ---------------------------------------------------------
    // Pulled versions
    // ---------------------------------------------------------

    #[test]
    fn a_pulled_version_is_stored_byte_for_byte() {
        let fx = Fixture::new();
        let bytes = document(ID, 1).into_bytes();
        fx.store()
            .store_committed(&fx.lock, id(ID), number(4), &bytes)
            .expect("stores");
        assert_eq!(
            fs::read(fx.object_dir(ID).join("meta.4")).expect("a file"),
            bytes
        );
    }

    #[test]
    fn a_pulled_version_of_a_later_format_or_not_readable_at_all_is_stored_as_it_is() {
        let fx = Fixture::new();
        let bytes = br#"{"format":2,"surprise":true}"#;
        fx.store()
            .store_committed(&fx.lock, id(ID), number(4), bytes)
            .expect("stores");
        assert_eq!(
            fs::read(fx.object_dir(ID).join("meta.4")).expect("a file"),
            bytes
        );
    }

    #[test]
    fn storing_the_same_version_again_changes_nothing() {
        let fx = Fixture::new();
        let bytes = document(ID, 1).into_bytes();
        fx.store()
            .store_committed(&fx.lock, id(ID), number(4), &bytes)
            .expect("stores");
        let path = fx.object_dir(ID).join("meta.4");
        let before = fs::metadata(&path).expect("a file").ino();
        fx.store()
            .store_committed(&fx.lock, id(ID), number(4), &bytes)
            .expect("again");
        // A rewrite renames a new file over the old one.
        assert_eq!(fs::metadata(&path).expect("a file").ino(), before);
    }

    #[test]
    fn a_committed_version_never_changes() {
        let fx = Fixture::new();
        fx.store()
            .store_committed(&fx.lock, id(ID), number(4), b"one")
            .expect("stores");
        let error = fx
            .store()
            .store_committed(&fx.lock, id(ID), number(4), b"two")
            .expect_err("refused");
        assert!(
            matches!(error, StoreError::CommittedChanged { number: 4 }),
            "{error:?}"
        );
        assert_eq!(
            fs::read(fx.object_dir(ID).join("meta.4")).expect("a file"),
            b"one"
        );
    }

    // ---------------------------------------------------------
    // Promoting a recorded change
    // ---------------------------------------------------------

    fn pending_bytes(fx: &Fixture, entries: usize) -> Vec<u8> {
        fx.store()
            .write_pending(&fx.lock, &metadata(ID, entries))
            .expect("writes");
        fs::read(fx.collection.meta_pending_path(id(ID))).expect("a file")
    }

    #[test]
    fn promoting_renames_the_recorded_change_to_the_number_the_commit_gave() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.2", document(ID, 1));
        let bytes = pending_bytes(&fx, 2);
        let outcome = fx
            .store()
            .promote_pending(&fx.lock, id(ID), number(3), &sha256(&bytes))
            .expect("promotes");
        assert_eq!(outcome, Promoted::Renamed);
        assert_eq!(
            fs::read(fx.object_dir(ID).join("meta.3")).expect("a file"),
            bytes
        );
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
        let current = fx.store().current(id(ID)).expect("reads").expect("some");
        assert_eq!(current.source, Source::Committed(3));
    }

    #[test]
    fn nothing_is_promoted_when_nothing_is_recorded() {
        let fx = Fixture::new();
        let error = fx
            .store()
            .promote_pending(&fx.lock, id(ID), one(), &sha256(b"x"))
            .expect_err("nothing to promote");
        assert!(matches!(error, StoreError::NoPending), "{error:?}");
    }

    #[test]
    fn discarding_removes_the_recorded_change_that_was_committed() {
        let fx = Fixture::new();
        let bytes = pending_bytes(&fx, 1);
        fx.store()
            .discard_pending(&fx.lock, id(ID), &sha256(&bytes))
            .expect("discards");
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn nothing_is_discarded_when_nothing_is_recorded() {
        let fx = Fixture::new();
        let error = fx
            .store()
            .discard_pending(&fx.lock, id(ID), &sha256(b"x"))
            .expect_err("nothing to discard");
        assert!(matches!(error, StoreError::NoPending), "{error:?}");
    }

    #[test]
    fn a_recorded_change_that_is_not_what_was_committed_is_not_discarded() {
        let fx = Fixture::new();
        let bytes = pending_bytes(&fx, 1);
        let error = fx
            .store()
            .discard_pending(&fx.lock, id(ID), &sha256(b"something else"))
            .expect_err("refused");
        assert!(matches!(error, StoreError::PendingChanged), "{error:?}");
        assert_eq!(
            fs::read(fx.collection.meta_pending_path(id(ID))).expect("a file"),
            bytes
        );
    }

    #[test]
    fn a_recorded_change_that_is_not_what_was_committed_stays_where_it_is() {
        let fx = Fixture::new();
        let bytes = pending_bytes(&fx, 1);
        let error = fx
            .store()
            .promote_pending(&fx.lock, id(ID), one(), &sha256(b"something else"))
            .expect_err("refused");
        assert!(matches!(error, StoreError::PendingChanged), "{error:?}");
        assert_eq!(
            fs::read(fx.collection.meta_pending_path(id(ID))).expect("a file"),
            bytes
        );
        assert!(!fx.object_dir(ID).join("meta.1").exists());
    }

    #[test]
    fn a_number_that_is_not_above_the_highest_version_is_refused() {
        let mut fx = Fixture::new();
        fx.put(ID, "meta.4", document(ID, 1));
        let bytes = pending_bytes(&fx, 2);
        let digest = sha256(&bytes);
        for (attempt, highest) in [(4, 4), (3, 4)] {
            let error = fx
                .store()
                .promote_pending(&fx.lock, id(ID), number(attempt), &digest)
                .expect_err("refused");
            assert!(
                matches!(error, StoreError::VersionNotNewer { number, highest: h } if number == attempt && h == highest),
                "{error:?}"
            );
        }
        assert!(fx.collection.meta_pending_path(id(ID)).exists());
    }

    #[test]
    fn a_promotion_that_a_crash_cut_short_is_finished() {
        let mut fx = Fixture::new();
        let bytes = pending_bytes(&fx, 1);
        fx.put(ID, "meta.1", &bytes);
        let outcome = fx
            .store()
            .promote_pending(&fx.lock, id(ID), one(), &sha256(&bytes))
            .expect("finishes");
        assert_eq!(outcome, Promoted::AlreadyCommitted);
        assert!(!fx.collection.meta_pending_path(id(ID)).exists());
        assert_eq!(
            fs::read(fx.object_dir(ID).join("meta.1")).expect("a file"),
            bytes
        );
    }

    #[test]
    fn a_version_with_other_content_in_the_place_is_not_taken_for_the_commit() {
        let mut fx = Fixture::new();
        let bytes = pending_bytes(&fx, 1);
        fx.put(ID, "meta.1", document(ID, 3));
        let error = fx
            .store()
            .promote_pending(&fx.lock, id(ID), one(), &sha256(&bytes))
            .expect_err("someone else's version 1");
        assert!(
            matches!(error, StoreError::VersionNotNewer { .. }),
            "{error:?}"
        );
        assert!(fx.collection.meta_pending_path(id(ID)).exists());
    }

    // ---------------------------------------------------------
    // Payloads and listings
    // ---------------------------------------------------------

    const PAYLOAD: &str = "0123456789abcdef-a.tar.zst";
    const LISTING: &str = "0123456789abcdef.listing.zst";

    fn file_name(text: &str) -> FileName {
        FileName::new(text).expect("a plain name")
    }

    /// Puts `contents` into `.antiq/tmp/` as `source`.
    fn staged(fx: &mut Fixture, source: &str, contents: &[u8]) -> PathBuf {
        fx.temp
            .tree()
            .file(&format!(".antiq/tmp/{source}"), contents)
    }

    fn put(fx: &Fixture, name: &str, from: &Path, contents: &[u8]) -> Result<Put, StoreError> {
        fx.store()
            .put_file(&fx.lock, id(ID), &file_name(name), from, &sha256(contents))
    }

    #[test]
    fn a_payload_is_moved_under_its_final_name_into_a_directory_that_is_made() {
        let mut fx = Fixture::new();
        let from = staged(&mut fx, "upload", b"payload bytes");
        assert!(!fx.object_dir(ID).exists());
        assert_eq!(
            put(&fx, PAYLOAD, &from, b"payload bytes").expect("puts"),
            Put::Stored
        );
        assert_eq!(
            fs::read(fx.object_dir(ID).join(PAYLOAD)).expect("a file"),
            b"payload bytes"
        );
        assert!(!from.exists());
    }

    #[test]
    fn the_same_payload_again_is_already_there_and_its_source_stays() {
        let mut fx = Fixture::new();
        let first = staged(&mut fx, "one", b"payload bytes");
        put(&fx, PAYLOAD, &first, b"payload bytes").expect("puts");
        let second = staged(&mut fx, "two", b"payload bytes");
        assert_eq!(
            put(&fx, PAYLOAD, &second, b"payload bytes").expect("puts"),
            Put::AlreadyPresent
        );
        assert!(second.exists());
        assert_eq!(
            fs::read(fx.object_dir(ID).join(PAYLOAD)).expect("a file"),
            b"payload bytes"
        );
    }

    #[test]
    fn the_same_listing_again_is_already_there() {
        let mut fx = Fixture::new();
        let first = staged(&mut fx, "one", b"listing bytes");
        put(&fx, LISTING, &first, b"listing bytes").expect("puts");
        let second = staged(&mut fx, "two", b"listing bytes");
        assert_eq!(
            put(&fx, LISTING, &second, b"listing bytes").expect("puts"),
            Put::AlreadyPresent
        );
    }

    #[test]
    fn other_bytes_under_an_existing_name_are_refused_and_nothing_is_replaced() {
        let mut fx = Fixture::new();
        let first = staged(&mut fx, "one", b"payload bytes");
        put(&fx, PAYLOAD, &first, b"payload bytes").expect("puts");
        let second = staged(&mut fx, "two", b"different bytes");
        let error = put(&fx, PAYLOAD, &second, b"different bytes").expect_err("refused");
        assert!(
            matches!(&error, StoreError::PayloadConflict { name } if name == PAYLOAD),
            "{error:?}"
        );
        assert!(second.exists());
        assert_eq!(
            fs::read(fx.object_dir(ID).join(PAYLOAD)).expect("a file"),
            b"payload bytes"
        );
    }

    fn assert_not_a_stored_name(name: &str) {
        let mut fx = Fixture::new();
        let from = staged(&mut fx, "upload", b"x");
        let error = put(&fx, name, &from, b"x").expect_err("refused");
        assert!(
            matches!(error, StoreError::NotAStoredName { .. }),
            "{error:?}"
        );
        assert!(from.exists());
        assert!(!fx.object_dir(ID).exists());
    }

    #[test]
    fn a_version_of_the_metadata_does_not_go_through_the_payload_put() {
        assert_not_a_stored_name("meta.5");
    }

    #[test]
    fn the_recorded_change_does_not_go_through_the_payload_put() {
        assert_not_a_stored_name("meta.pending");
    }

    #[test]
    fn a_payload_name_with_an_upper_case_digest_is_not_a_stored_name() {
        assert_not_a_stored_name("0123456789ABCDEF-a.tar.zst");
    }

    #[test]
    fn a_source_that_is_missing_is_an_error_naming_it_and_nothing_is_made() {
        let fx = Fixture::new();
        let from = fx.collection.tmp_dir().join("missing");
        let error = put(&fx, PAYLOAD, &from, b"x").expect_err("no source");
        assert!(
            matches!(error, StoreError::Io { action: "open", .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("missing"), "{error}");
        assert!(!fx.object_dir(ID).exists());
    }

    #[test]
    fn a_file_is_found_at_its_name_in_the_object_directory() {
        let fx = Fixture::new();
        assert_eq!(
            fx.store().file_path(id(ID), &file_name(PAYLOAD)),
            fx.object_dir(ID).join(PAYLOAD)
        );
    }
}
