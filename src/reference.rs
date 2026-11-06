// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Piece references.

use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::index::{Index, IndexError};
use crate::relpath::{RelPath, RelPathError, below_root, escape};
use serde::Serialize;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A piece a reference could mean: its full ULID and its path in the
/// escaped form. Brake reports list their items the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub id: String,
    pub path: String,
}

/// Errors of resolving a piece reference.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceError {
    #[error("`{reference}` matches {} pieces", candidates.len())]
    Ambiguous {
        reference: String,
        candidates: Vec<Candidate>,
    },
    #[error("`{reference}` matches no piece")]
    Unknown { reference: String },
    #[error("`{path}` is outside the collection at `{root}`")]
    Outside { path: String, root: String },
    #[error("cannot look at the collection root {path}: {source}")]
    Root { path: String, source: io::Error },
    #[error(transparent)]
    Path(#[from] RelPathError),
    #[error(transparent)]
    Index(#[from] IndexError),
}

impl HasKind for ReferenceError {
    fn kind(&self) -> ErrorKind {
        match self {
            ReferenceError::Ambiguous { .. } => ErrorKind::AmbiguousReference,
            ReferenceError::Unknown { .. } => ErrorKind::UnknownReference,
            ReferenceError::Outside { .. } => ErrorKind::OutsideCollection,
            ReferenceError::Root { .. } | ReferenceError::Path(_) | ReferenceError::Index(_) => {
                ErrorKind::Other
            }
        }
    }
}

/// The shortest and longest ULID prefix a reference may be. Fewer
/// characters are too likely to name more than one object.
const PREFIX_LENGTHS: std::ops::RangeInclusive<usize> = 6..=26;

/// The characters of a ULID, in either case.
const CROCKFORD: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The object a piece reference names.
///
/// `given` is a path or a ULID prefix. A path that an object is recorded
/// at wins: `src` and `01ARZ3` can be folder names as well as ULID
/// characters. At a path where an object's folder is (or is arriving or
/// leaving) that object is meant; the other objects recorded there, such
/// as the archived old ID after a fork, only count when no object holds
/// the folder. A folder that no object is recorded at is not found here;
/// [`resolve_path`] gives its path.
///
/// `cwd` is the directory relative paths start at: the canonical form of
/// the `-C` directory, else the process's current directory. That is the
/// directory `Collection::find` started from.
pub fn resolve(
    collection: &Collection,
    index: &Index,
    cwd: &Path,
    given: &OsStr,
) -> Result<ObjectId, ReferenceError> {
    let reference = escape(given.as_bytes());

    let outside = match resolve_path(collection, cwd, Path::new(given)) {
        Ok(path) => {
            let at = index.at_path(&path);
            if let Some(holder) = at.claimant {
                return Ok(holder);
            }
            match at.others.as_slice() {
                [] => None,
                [only] => return Ok(*only),
                _ => return Err(ambiguous(reference, index, &at.others)),
            }
        }
        Err(error @ ReferenceError::Outside { .. }) => Some(error),
        Err(error) => return Err(error),
    };

    if let Some(text) = given.to_str().filter(|text| is_prefix(text)) {
        return match index.ids_with_prefix(text).as_slice() {
            [] => Err(ReferenceError::Unknown { reference }),
            [only] => Ok(*only),
            several => Err(ambiguous(reference, index, several)),
        };
    }
    Err(outside.unwrap_or(ReferenceError::Unknown { reference }))
}

fn is_prefix(text: &str) -> bool {
    PREFIX_LENGTHS.contains(&text.len())
        && text
            .chars()
            .all(|c| CROCKFORD.contains(c.to_ascii_uppercase()))
}

fn ambiguous(reference: String, index: &Index, ids: &[ObjectId]) -> ReferenceError {
    ReferenceError::Ambiguous {
        reference,
        candidates: ids
            .iter()
            .map(|&id| Candidate {
                id: id.to_string(),
                path: index
                    .path_of(id)
                    .map(RelPath::to_string)
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

/// The path `given` leads to, relative to the collection root.
///
/// `.` and `..` are folded from the text alone. Then the shortest leading
/// part that is the collection root itself, by device and inode, stands
/// for the root, so another spelling of the root (`/var` for
/// `/private/var` on macOS, or a different case on a case-insensitive
/// volume) still lies below it. What follows is taken literally: nothing
/// below the root is looked at on disk, so a symlink there is never
/// followed. A path with no such part is outside the collection.
///
/// The root itself and paths below `.antiq/` are not refused here; callers
/// that cannot take them refuse them. `cwd` is as for [`resolve`].
pub fn resolve_path(
    collection: &Collection,
    cwd: &Path,
    given: &Path,
) -> Result<RelPath, ReferenceError> {
    let root = collection.root();
    let root_identity = identity(root).map_err(|source| ReferenceError::Root {
        path: escape(root.as_os_str().as_bytes()),
        source,
    })?;

    let folded = match below_root(Path::new("/"), given, cwd) {
        Ok(folded) => folded,
        Err(RelPathError::AboveRoot(_)) => {
            return Err(ReferenceError::Outside {
                path: escape(given.as_os_str().as_bytes()),
                root: escape(root.as_os_str().as_bytes()),
            });
        }
        Err(error) => return Err(error.into()),
    };

    let names: Vec<&[u8]> = folded.components().collect();
    let mut prefix = PathBuf::from("/");
    for taken in 0..=names.len() {
        if taken > 0 {
            prefix.push(OsStr::from_bytes(names[taken - 1]));
        }
        if identity(&prefix).ok() == Some(root_identity) {
            return Ok(RelPath::from_bytes(names[taken..].join(&b'/'))?);
        }
    }
    Err(ReferenceError::Outside {
        path: escape(
            Path::new("/")
                .join(OsStr::from_bytes(folded.as_bytes()))
                .as_os_str()
                .as_bytes(),
        ),
        root: escape(root.as_os_str().as_bytes()),
    })
}

/// Device and inode of the directory `path` leads to, following symlinks
/// in `path` itself.
fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let entry = TreeRoot::open(path)?.lstat(&RelPath::root())?;
    Ok((entry.device, entry.inode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::error::Error;
    use crate::lock::{self, RunLock};
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use crate::tracking::{self, EntryState, TrackingEntry, VersionTakenOut};
    use serde_json::json;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    const A: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    /// Shares all but the last character with `A`.
    const A2: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
    const B: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    struct Fixture {
        temp: TempCollection,
        collection: Collection,
        lock: RunLock,
        clock: FixedClock,
        /// A directory next to the collection, for paths outside it.
        outside: tempfile::TempDir,
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
                clock,
                outside: tempfile::tempdir().expect("a temp directory"),
            }
        }

        fn root(&self) -> PathBuf {
            self.collection.root().to_path_buf()
        }

        /// Writes `meta.1` of an archived `object` at `piece`.
        fn object(&mut self, object: &str, piece: &str) {
            let document = format!(
                concat!(
                    r#"{{"format":1,"id":"{}","collection":"{}","path":"{}","name":"x","#,
                    r#""created":"2020-09-13T12:26:40Z","created_source":"birth","#,
                    r#""lifecycle":"archived","payloads":[],"history":[]}}"#
                ),
                object, COLLECTION_ID, piece
            );
            self.temp
                .tree()
                .file(&format!(".antiq/objects/{object}/meta.1"), document);
        }

        fn track(&mut self, object: &str, piece: &str, state: EntryState) {
            let mut entry = TrackingEntry::new(
                id(object),
                path(piece),
                state,
                VersionTakenOut::Committed(1),
                "2026-01-02T03:04:05Z".parse().expect("valid"),
            );
            tracking::write(&self.collection, &self.lock, &self.clock, &mut entry)
                .expect("the entry is written");
        }

        fn resolve(&self, given: impl AsRef<OsStr>) -> Result<ObjectId, ReferenceError> {
            self.resolve_from(&self.root(), given)
        }

        fn resolve_from(
            &self,
            cwd: &Path,
            given: impl AsRef<OsStr>,
        ) -> Result<ObjectId, ReferenceError> {
            let index = Index::compute(&self.collection).expect("computes");
            resolve(&self.collection, &index, cwd, given.as_ref())
        }

        fn resolve_path(&self, given: impl AsRef<Path>) -> Result<RelPath, ReferenceError> {
            resolve_path(&self.collection, &self.root(), given.as_ref())
        }
    }

    fn kind(error: &ReferenceError) -> ErrorKind {
        error.kind()
    }

    fn ambiguous_parts(error: ReferenceError) -> Option<(String, Vec<Candidate>)> {
        match error {
            ReferenceError::Ambiguous {
                reference,
                candidates,
            } => Some((reference, candidates)),
            _ => None,
        }
    }

    fn outside_parts(error: ReferenceError) -> Option<(String, String)> {
        match error {
            ReferenceError::Outside { path, root } => Some((path, root)),
            _ => None,
        }
    }

    // ---------------------------------------------------------
    // ULID prefixes
    // ---------------------------------------------------------

    #[test]
    fn a_unique_six_character_prefix_resolves() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        fx.object(B, "b");
        assert_eq!(fx.resolve("01ARZ3").expect("resolves"), id(A));
        assert_eq!(fx.resolve("01BX5Z").expect("resolves"), id(B));
    }

    #[test]
    fn a_prefix_in_lower_case_resolves() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        assert_eq!(fx.resolve("01arz3nd").expect("resolves"), id(A));
    }

    #[test]
    fn a_full_ulid_resolves() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        fx.object(A2, "a2");
        assert_eq!(fx.resolve(A).expect("resolves"), id(A));
        assert_eq!(fx.resolve(A2).expect("resolves"), id(A2));
    }

    #[test]
    fn a_five_character_prefix_is_unknown_even_when_it_is_unique() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        let error = fx.resolve("01ARZ").expect_err("too short");
        assert!(matches!(error, ReferenceError::Unknown { .. }), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::UnknownReference);
    }

    #[test]
    fn a_prefix_that_matches_nothing_is_unknown() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        let error = fx.resolve("01ZZZZ").expect_err("no such object");
        assert!(matches!(error, ReferenceError::Unknown { .. }), "{error:?}");
        assert_eq!(error.to_string(), "`01ZZZZ` matches no piece");
    }

    #[test]
    fn a_prefix_of_two_objects_is_ambiguous_and_lists_both() {
        let mut fx = Fixture::new();
        fx.object(A2, "second");
        fx.object(A, "first");
        let error = fx
            .resolve("01ARZ3NDEKTSV4RRFFQ69G5FA")
            .expect_err("two match");
        assert_eq!(kind(&error), ErrorKind::AmbiguousReference);
        let (reference, candidates) = ambiguous_parts(error).expect("is ambiguous");
        assert_eq!(reference, "01ARZ3NDEKTSV4RRFFQ69G5FA");
        assert_eq!(
            candidates,
            vec![
                Candidate {
                    id: A.into(),
                    path: "first".into()
                },
                Candidate {
                    id: A2.into(),
                    path: "second".into()
                },
            ]
        );
    }

    #[test]
    fn the_candidates_reach_the_json_error_object() {
        let mut fx = Fixture::new();
        fx.object(A, "first");
        fx.object(A2, "second");
        let error: Error = fx.resolve("01ARZ3").expect_err("two match").into();
        assert_eq!(
            serde_json::Value::Object(error.details()),
            json!({"candidates": [
                {"id": A, "path": "first"},
                {"id": A2, "path": "second"},
            ]})
        );
    }

    #[test]
    fn an_id_known_only_from_a_tracking_entry_counts_for_uniqueness() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        fx.track(A2, "lost", EntryState::Gone);
        let error = fx.resolve("01ARZ3").expect_err("two are known");
        let (_, candidates) = ambiguous_parts(error).expect("is ambiguous");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].path, "lost");
    }

    #[test]
    fn an_object_whose_metadata_cannot_be_read_counts_for_uniqueness() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        fx.temp
            .tree()
            .file(&format!(".antiq/objects/{A2}/meta.1"), "garbage");
        let error = fx.resolve("01ARZ3").expect_err("two are known");
        let (_, candidates) = ambiguous_parts(error).expect("is ambiguous");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].path, "", "no path is known for it");
    }

    #[test]
    fn an_object_directory_without_metadata_counts_for_uniqueness() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        fx.temp.tree().file(
            &format!(".antiq/objects/{A2}/0000000000000001.listing.zst"),
            "x",
        );
        let error = fx.resolve("01ARZ3").expect_err("two are known");
        assert_eq!(kind(&error), ErrorKind::AmbiguousReference);
    }

    // ---------------------------------------------------------
    // Paths
    // ---------------------------------------------------------

    #[test]
    fn the_path_of_an_archived_piece_resolves_without_its_folder() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha");
        assert!(!fx.root().join("projects").exists(), "no folder on disk");
        let given = fx.root().join("projects/alpha");
        assert_eq!(fx.resolve(given).expect("resolves"), id(A));
    }

    #[test]
    fn a_relative_path_starts_at_the_current_directory() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha");
        let cwd = fx.root().join("projects");
        assert_eq!(fx.resolve_from(&cwd, "alpha").expect("resolves"), id(A));
        assert_eq!(
            fx.resolve_from(&cwd, "../projects/./alpha")
                .expect("resolves"),
            id(A)
        );
    }

    #[test]
    fn dot_and_dotdot_are_folded_from_the_text() {
        let mut fx = Fixture::new();
        fx.object(A, "proj");
        assert_eq!(fx.resolve("./a/../proj").expect("resolves"), id(A));
        assert_eq!(fx.resolve("a/b/../../proj/").expect("resolves"), id(A));
    }

    #[test]
    fn an_absolute_path_through_a_symlink_to_the_root_resolves() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha");
        let link = fx.outside.path().join("rootlink");
        symlink(fx.root(), &link).expect("symlink");
        assert_eq!(
            fx.resolve(link.join("projects/alpha")).expect("resolves"),
            id(A)
        );
        assert_eq!(fx.resolve_path(&link).expect("the root"), RelPath::root());
    }

    #[test]
    fn a_symlink_below_the_root_is_not_followed() {
        let mut fx = Fixture::new();
        fx.object(A, "other");
        fx.temp.tree().dir("other");
        // `link` leads to the folder of `other`; `back` leads to the root.
        symlink(fx.root().join("other"), fx.root().join("link")).expect("symlink");
        symlink(fx.root(), fx.root().join("back")).expect("symlink");
        for given in ["link", "back/other"] {
            let error = fx.resolve(given).expect_err("a link is not the piece");
            assert!(
                matches!(error, ReferenceError::Unknown { .. }),
                "{given}: {error:?}"
            );
        }
        assert_eq!(
            fx.resolve_path("back/other").expect("inside"),
            path("back/other")
        );
    }

    #[test]
    fn dotdot_after_a_symlink_folds_lexically() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha");
        // Physically `dirlink/..` is the outside directory's parent.
        let elsewhere = fx.outside.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("dir");
        symlink(&elsewhere, fx.root().join("dirlink")).expect("symlink");
        assert_eq!(
            fx.resolve("dirlink/../projects/alpha").expect("resolves"),
            id(A)
        );
    }

    #[test]
    fn a_path_outside_the_collection_is_refused() {
        let fx = Fixture::new();
        let given = fx.outside.path().join("proj");
        let error = fx.resolve(&given).expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        let (path, root) = outside_parts(error).expect("is outside");
        assert_eq!(path, given.to_string_lossy());
        assert_eq!(root, fx.root().to_string_lossy());
        let error = fx.resolve_path(&given).expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
    }

    #[test]
    fn dotdot_out_of_the_root_is_refused() {
        let fx = Fixture::new();
        let error = fx.resolve("../sibling").expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
    }

    #[test]
    fn dotdot_above_the_filesystem_root_is_refused_as_outside() {
        let fx = Fixture::new();
        let error = fx.resolve("/../../x").expect_err("above the root");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        assert!(matches!(
            fx.resolve_path("/..").expect_err("above the root"),
            ReferenceError::Outside { .. }
        ));
    }

    #[test]
    fn a_differently_cased_spelling_of_the_root_resolves() {
        let mut fx = Fixture::new();
        fx.object(A, "projects/alpha");
        let swapped: String = fx
            .root()
            .to_string_lossy()
            .chars()
            .map(|c| {
                if c.is_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        if std::fs::metadata(&swapped).is_err() {
            eprintln!("skipped: the temp volume is case sensitive");
            return;
        }
        let given = PathBuf::from(swapped).join("projects/alpha");
        assert_eq!(fx.resolve(given).expect("resolves"), id(A));
    }

    #[test]
    fn a_path_names_the_object_before_a_prefix_is_tried() {
        let mut fx = Fixture::new();
        // `src` is made of ULID characters, and `01ARZ3` is a prefix of `A`
        // and a folder name at once.
        fx.object(A, "01ARZ3");
        fx.object(B, "src");
        assert_eq!(fx.resolve("01ARZ3").expect("the path wins"), id(A));
        assert_eq!(fx.resolve("src").expect("resolves"), id(B));
        fx.object(A2, "other");
        assert_eq!(
            fx.resolve("01ARZ3NDEKTSV4RRFFQ69G5FAW")
                .expect("the prefix"),
            id(A2)
        );
    }

    #[test]
    fn after_a_fork_the_path_resolves_to_the_new_id() {
        let mut fx = Fixture::new();
        fx.object(A, "proj");
        fx.track(A, "proj", EntryState::Gone);
        fx.object(B, "proj");
        fx.track(B, "proj", EntryState::Live);
        assert_eq!(fx.resolve("proj").expect("resolves"), id(B));
    }

    #[test]
    fn two_archived_objects_at_one_path_are_ambiguous() {
        let mut fx = Fixture::new();
        fx.object(B, "proj");
        fx.object(A, "proj");
        let error = fx.resolve("proj").expect_err("two objects");
        let (reference, candidates) = ambiguous_parts(error).expect("is ambiguous");
        assert_eq!(reference, "proj");
        assert_eq!(
            candidates.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec![A, B]
        );
    }

    #[test]
    fn a_folder_without_an_object_is_an_unknown_reference() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("new-piece");
        let error = fx.resolve("new-piece").expect_err("no object");
        assert!(matches!(error, ReferenceError::Unknown { .. }), "{error:?}");
        assert_eq!(
            fx.resolve_path("new-piece").expect("a path"),
            path("new-piece")
        );
    }

    #[test]
    fn the_collection_root_is_a_path_but_no_object() {
        let fx = Fixture::new();
        assert_eq!(fx.resolve_path(".").expect("the root"), RelPath::root());
        let error = fx.resolve(".").expect_err("the root is no piece");
        assert!(matches!(error, ReferenceError::Unknown { .. }), "{error:?}");
    }

    #[test]
    fn a_name_that_is_not_utf8_resolves_by_its_bytes() {
        let mut fx = Fixture::new();
        fx.object(A, "d%FF");
        let mut given = fx.root().into_os_string().into_vec();
        given.extend_from_slice(b"/d\xff");
        let given = OsStr::from_bytes(&given);
        assert_eq!(fx.resolve(given).expect("resolves"), id(A));
        let error = fx
            .resolve(OsStr::from_bytes(b"/elsewhere/d\xff"))
            .expect_err("outside");
        let (path, _) = outside_parts(error).expect("is outside");
        assert_eq!(path, "/elsewhere/d%FF", "the escaped form names the input");
    }

    #[test]
    fn a_prefix_shaped_text_that_matches_nothing_is_unknown_even_when_cwd_is_outside() {
        let mut fx = Fixture::new();
        fx.object(A, "a");
        let outside = fx.outside.path().to_path_buf();
        let error = fx.resolve_from(&outside, "01ZZZZ").expect_err("nothing");
        assert!(matches!(error, ReferenceError::Unknown { .. }), "{error:?}");
        let error = fx.resolve_from(&outside, "plain").expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        assert_eq!(fx.resolve_from(&outside, "01ARZ3").expect("prefix"), id(A));
    }

    #[test]
    fn a_collection_root_that_cannot_be_looked_at_is_an_error() {
        let fx = Fixture::new();
        let gone = Collection::for_tests();
        let error = resolve_path(&gone, &fx.root(), Path::new("x")).expect_err("no root");
        assert!(matches!(error, ReferenceError::Root { .. }), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
        assert!(
            error.to_string().contains("collection-for-tests"),
            "{error}"
        );
    }

    #[test]
    fn a_collection_root_that_cannot_be_looked_at_fails_a_reference_too() {
        let fx = Fixture::new();
        let gone = Collection::for_tests();
        let index = Index::compute(&fx.collection).expect("computes");
        let error = resolve(&gone, &index, &fx.root(), OsStr::new("proj")).expect_err("no root");
        assert!(matches!(error, ReferenceError::Root { .. }), "{error:?}");
    }

    #[test]
    fn a_current_directory_that_is_not_absolute_is_a_path_error() {
        let fx = Fixture::new();
        let error = resolve_path(&fx.collection, Path::new("relative"), Path::new("x"))
            .expect_err("not absolute");
        assert!(matches!(error, ReferenceError::Path(_)), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
    }

    #[test]
    fn an_index_error_is_passed_on() {
        let error: ReferenceError = IndexError::Io {
            action: "list",
            path: "x".into(),
            source: io::Error::other("boom"),
        }
        .into();
        assert!(matches!(error, ReferenceError::Index(_)));
        assert_eq!(kind(&error), ErrorKind::Other);
    }
}
