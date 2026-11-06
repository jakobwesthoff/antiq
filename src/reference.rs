// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Piece references.

use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::fsx::tree::TreeRoot;
use crate::relpath::{RelPath, RelPathError, below_root, escape};
use serde::Serialize;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A piece a reference could mean: its full ULID and its path in the
/// escaped form.
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
}

impl HasKind for ReferenceError {
    fn kind(&self) -> ErrorKind {
        match self {
            ReferenceError::Ambiguous { .. } => ErrorKind::AmbiguousReference,
            ReferenceError::Unknown { .. } => ErrorKind::UnknownReference,
            ReferenceError::Outside { .. } => ErrorKind::OutsideCollection,
            ReferenceError::Root { .. } | ReferenceError::Path(_) => ErrorKind::Other,
        }
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
/// that cannot take them refuse them.
///
/// `cwd` is the directory relative paths start at: the canonical form of
/// the `-C` directory, else the process's current directory. That is the
/// directory `Collection::find` started from.
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
    use crate::testing::tree::TempCollection;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    struct Fixture {
        temp: TempCollection,
        collection: Collection,
        /// A directory next to the collection, for paths outside it.
        outside: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Fixture {
            let temp = TempCollection::minimal();
            let collection = Collection::find(temp.root()).expect("a collection");
            Fixture {
                temp,
                collection,
                outside: tempfile::tempdir().expect("a temp directory"),
            }
        }

        fn root(&self) -> PathBuf {
            self.collection.root().to_path_buf()
        }

        fn resolve_path(&self, given: impl AsRef<Path>) -> Result<RelPath, ReferenceError> {
            resolve_path(&self.collection, &self.root(), given.as_ref())
        }
    }

    fn kind(error: &ReferenceError) -> ErrorKind {
        error.kind()
    }

    fn outside_parts(error: ReferenceError) -> Option<(String, String)> {
        match error {
            ReferenceError::Outside { path, root } => Some((path, root)),
            _ => None,
        }
    }

    #[test]
    fn a_path_below_the_root_resolves_without_its_folder() {
        let fx = Fixture::new();
        assert!(!fx.root().join("projects").exists(), "no folder on disk");
        let given = fx.root().join("projects/alpha");
        assert_eq!(
            fx.resolve_path(given).expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_relative_path_starts_at_the_current_directory() {
        let fx = Fixture::new();
        let cwd = fx.root().join("projects");
        let from = |given: &str| resolve_path(&fx.collection, &cwd, Path::new(given));
        assert_eq!(from("alpha").expect("resolves"), path("projects/alpha"));
        assert_eq!(
            from("../projects/./alpha").expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn dot_and_dotdot_are_folded_from_the_text() {
        let fx = Fixture::new();
        assert_eq!(
            fx.resolve_path("./a/../proj").expect("resolves"),
            path("proj")
        );
        assert_eq!(
            fx.resolve_path("a/b/../../proj/").expect("resolves"),
            path("proj")
        );
    }

    #[test]
    fn an_absolute_path_through_a_symlink_to_the_root_resolves() {
        let fx = Fixture::new();
        let link = fx.outside.path().join("rootlink");
        symlink(fx.root(), &link).expect("symlink");
        assert_eq!(
            fx.resolve_path(link.join("projects/alpha"))
                .expect("resolves"),
            path("projects/alpha")
        );
        assert_eq!(fx.resolve_path(&link).expect("the root"), RelPath::root());
    }

    #[test]
    fn a_symlink_below_the_root_is_not_followed() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("other");
        // `link` leads to the folder `other`; `back` leads to the root.
        symlink(fx.root().join("other"), fx.root().join("link")).expect("symlink");
        symlink(fx.root(), fx.root().join("back")).expect("symlink");
        assert_eq!(fx.resolve_path("link").expect("inside"), path("link"));
        assert_eq!(
            fx.resolve_path("back/other").expect("inside"),
            path("back/other")
        );
    }

    #[test]
    fn dotdot_after_a_symlink_folds_lexically() {
        let fx = Fixture::new();
        // Physically `dirlink/..` is the outside directory's parent.
        let elsewhere = fx.outside.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("dir");
        symlink(&elsewhere, fx.root().join("dirlink")).expect("symlink");
        assert_eq!(
            fx.resolve_path("dirlink/../projects/alpha")
                .expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_path_outside_the_collection_is_refused() {
        let fx = Fixture::new();
        let given = fx.outside.path().join("proj");
        let error = fx.resolve_path(&given).expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        let (path, root) = outside_parts(error).expect("is outside");
        assert_eq!(path, given.to_string_lossy());
        assert_eq!(root, fx.root().to_string_lossy());
    }

    #[test]
    fn dotdot_out_of_the_root_is_refused() {
        let fx = Fixture::new();
        let error = fx.resolve_path("../sibling").expect_err("outside");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
    }

    #[test]
    fn dotdot_above_the_filesystem_root_is_refused_as_outside() {
        let fx = Fixture::new();
        let error = fx.resolve_path("/../../x").expect_err("above the root");
        assert_eq!(kind(&error), ErrorKind::OutsideCollection);
        assert!(matches!(
            fx.resolve_path("/..").expect_err("above the root"),
            ReferenceError::Outside { .. }
        ));
    }

    #[test]
    fn a_differently_cased_spelling_of_the_root_resolves() {
        let fx = Fixture::new();
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
        assert_eq!(
            fx.resolve_path(given).expect("resolves"),
            path("projects/alpha")
        );
    }

    #[test]
    fn a_folder_without_an_object_resolves_to_its_path() {
        let mut fx = Fixture::new();
        fx.temp.tree().dir("new-piece");
        assert_eq!(
            fx.resolve_path("new-piece").expect("a path"),
            path("new-piece")
        );
    }

    #[test]
    fn the_collection_root_resolves_to_the_empty_path() {
        let fx = Fixture::new();
        assert_eq!(fx.resolve_path(".").expect("the root"), RelPath::root());
    }

    #[test]
    fn a_name_that_is_not_utf8_resolves_by_its_bytes() {
        let fx = Fixture::new();
        let mut given = fx.root().into_os_string().into_vec();
        given.extend_from_slice(b"/d\xff");
        let given = OsStr::from_bytes(&given);
        assert_eq!(fx.resolve_path(given).expect("resolves"), path("d%FF"));
        let error = fx
            .resolve_path(OsStr::from_bytes(b"/elsewhere/d\xff"))
            .expect_err("outside");
        let (path, _) = outside_parts(error).expect("is outside");
        assert_eq!(path, "/elsewhere/d%FF", "the escaped form names the input");
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
    fn a_current_directory_that_is_not_absolute_is_a_path_error() {
        let fx = Fixture::new();
        let error = resolve_path(&fx.collection, Path::new("relative"), Path::new("x"))
            .expect_err("not absolute");
        assert!(matches!(error, ReferenceError::Path(_)), "{error:?}");
        assert_eq!(kind(&error), ErrorKind::Other);
    }
}
