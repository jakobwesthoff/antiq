// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin files.

use crate::filesystem::FilesystemError;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::{Name, RelPath};
use crate::tree::EntryKind;
use std::fs::File;
use std::io;
use std::path::Path;

/// The file at the top of a piece that pins it.
pub const PIN_FILE: &str = ".antiq-pin";

/// Errors of pin files.
#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[error("cannot look for the pin file {path}: {source}")]
    Stat {
        path: RelPath,
        #[source]
        source: io::Error,
    },
    #[error("cannot create the pin file {path}: {source}")]
    Create {
        path: RelPath,
        #[source]
        source: io::Error,
    },
    #[error("cannot remove the pin file {path}: {source}")]
    Remove {
        path: RelPath,
        #[source]
        source: io::Error,
    },
    #[error("cannot make the pin of {path} durable: {source}")]
    Flush {
        path: RelPath,
        #[source]
        source: FilesystemError,
    },
    #[error("{path} is a directory, not a pin file")]
    NotAFile { path: RelPath },
}

impl crate::error::HasKind for PinError {}

/// The path of the pin file of the piece at `piece`.
fn pin_path(piece: &RelPath) -> RelPath {
    let name = Name::from_bytes(PIN_FILE.as_bytes().to_vec()).expect("the pin file name is valid");
    piece.join(&name)
}

/// What is at the pin path, or `None` when nothing is. A directory is
/// reported as such, since it is no pin.
fn lookup(collection: &TreeRoot, pin: &RelPath) -> io::Result<Option<EntryKind>> {
    match collection.lstat(pin) {
        Ok(entry) => Ok(Some(entry.kind)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether the piece at `piece`, relative to `collection`, carries a pin.
///
/// Anything that is not a directory pins the piece, a dangling symlink
/// included. A directory of that name is not a pin.
pub fn is_pinned(collection: &TreeRoot, piece: &RelPath) -> Result<bool, PinError> {
    let pin = pin_path(piece);
    match lookup(collection, &pin) {
        Ok(found) => Ok(matches!(found, Some(kind) if kind != EntryKind::Directory)),
        Err(source) => Err(PinError::Stat { path: pin, source }),
    }
}

/// How the pin and its directory are made durable. A parameter so that tests
/// can see the calls and make them fail.
pub(crate) type Flush<'a> = &'a dyn Fn(&File, &Path) -> Result<(), FilesystemError>;

/// Pins the piece at `piece` by creating its empty pin file. A piece that
/// is pinned already stays so. The marker has no content, so creating it
/// with `O_EXCL` needs no temp file and rename. The pin is durable when
/// this returns: the file and the entry in the piece's directory are
/// flushed.
pub fn create(collection: &TreeRoot, piece: &RelPath) -> Result<(), PinError> {
    create_with(collection, piece, &crate::filesystem::atomic::flush_file)
}

pub(crate) fn create_with(
    collection: &TreeRoot,
    piece: &RelPath,
    flush: Flush<'_>,
) -> Result<(), PinError> {
    let pin = pin_path(piece);
    match collection.create_file(&pin, 0o644) {
        Ok(file) => {
            flush(&file, Path::new(&pin.to_string())).map_err(|source| PinError::Flush {
                path: pin.clone(),
                source,
            })?
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match lookup(collection, &pin) {
                Ok(Some(EntryKind::Directory)) => return Err(PinError::NotAFile { path: pin }),
                Ok(_) => {}
                Err(source) => return Err(PinError::Stat { path: pin, source }),
            }
        }
        Err(source) => return Err(PinError::Create { path: pin, source }),
    }
    flush_piece(collection, piece, flush)
}

/// Removes the pin of the piece at `piece`. A piece without a pin is
/// left as it is. A directory of the pin's name is refused and left
/// untouched. The removal is durable when this returns.
pub fn remove(collection: &TreeRoot, piece: &RelPath) -> Result<(), PinError> {
    remove_with(collection, piece, &crate::filesystem::atomic::flush_file)
}

pub(crate) fn remove_with(
    collection: &TreeRoot,
    piece: &RelPath,
    flush: Flush<'_>,
) -> Result<(), PinError> {
    let pin = pin_path(piece);
    match lookup(collection, &pin) {
        Ok(None) => return Ok(()),
        Ok(Some(EntryKind::Directory)) => return Err(PinError::NotAFile { path: pin }),
        Ok(Some(_)) => {}
        Err(source) => return Err(PinError::Stat { path: pin, source }),
    }
    match collection.remove_file(&pin) {
        Err(source) if source.kind() != io::ErrorKind::NotFound => {
            return Err(PinError::Remove { path: pin, source });
        }
        _ => {}
    }
    flush_piece(collection, piece, flush)
}

/// Makes the entries of the piece's top directory durable, which is where
/// the pin file's name lives.
fn flush_piece(collection: &TreeRoot, piece: &RelPath, flush: Flush<'_>) -> Result<(), PinError> {
    let shown = piece.to_string();
    let flushed = collection
        .open_dir_file(piece)
        .map_err(crate::filesystem::io_error("open", Path::new(&shown)))
        .and_then(|directory| flush(&directory, Path::new(&shown)));
    flushed.map_err(|source| PinError::Flush {
        path: piece.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{deep_path, tree::Tree};
    use std::fs;
    use std::io::Write;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn open(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    #[test]
    fn creating_a_pin_makes_an_empty_file_at_the_top_of_the_piece() {
        let mut tree = Tree::new();
        tree.dir("projects/a");
        let root = open(&tree);

        create(&root, &path("projects/a")).expect("the pin is created");

        let pin = tree.path("projects/a/.antiq-pin");
        assert_eq!(fs::read(pin).expect("the pin exists"), b"");
    }

    #[test]
    fn a_piece_is_pinned_once_its_pin_exists() {
        let mut tree = Tree::new();
        tree.dir("a");
        tree.dir("b");
        tree.file("b/.antiq-pin", "");
        let root = open(&tree);

        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
        assert!(is_pinned(&root, &path("b")).expect("the lookup works"));
    }

    #[test]
    fn a_pin_made_by_create_is_found() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);

        create(&root, &path("a")).expect("the pin is created");

        assert!(is_pinned(&root, &path("a")).expect("the lookup works"));
    }

    #[test]
    fn removing_a_pin_deletes_the_file() {
        let mut tree = Tree::new();
        let pin = tree.file("a/.antiq-pin", "");
        let root = open(&tree);

        remove(&root, &path("a")).expect("the pin is removed");

        assert!(!pin.exists());
        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
    }

    #[test]
    fn removing_a_pin_that_is_not_there_is_not_an_error() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);

        remove(&root, &path("a")).expect("nothing to remove is fine");
        remove(&root, &path("a")).expect("and so is doing it twice");
    }

    #[test]
    fn creating_a_pin_twice_keeps_one_file() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);

        create(&root, &path("a")).expect("the first pin is created");
        create(&root, &path("a")).expect("an existing pin is success");

        let names: Vec<_> = fs::read_dir(tree.path("a"))
            .expect("the piece lists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names, [PIN_FILE]);
    }

    #[test]
    fn a_symlink_with_the_pin_name_pins_the_piece() {
        let mut tree = Tree::new();
        tree.dir("a");
        tree.symlink("a/.antiq-pin", "nowhere");
        let root = open(&tree);

        assert!(is_pinned(&root, &path("a")).expect("the lookup works"));
        remove(&root, &path("a")).expect("the link itself is removed");
        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
    }

    #[test]
    fn a_directory_with_the_pin_name_is_no_pin_and_is_left_alone() {
        let mut tree = Tree::new();
        tree.dir("a/.antiq-pin");
        let kept = tree.file("a/.antiq-pin/keep", "x");
        tree.dir("b");
        tree.file("b/.antiq-pin", "");
        let root = open(&tree);

        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
        assert!(is_pinned(&root, &path("b")).expect("the lookup works"));
        let expected = path("a/.antiq-pin").to_string();
        let removed = remove(&root, &path("a")).expect_err("a directory is refused");
        assert!(
            matches!(&removed, PinError::NotAFile { path } if path.to_string() == expected),
            "{removed:?}"
        );
        let created = create(&root, &path("a")).expect_err("a directory is refused");
        assert!(
            matches!(&created, PinError::NotAFile { path } if path.to_string() == expected),
            "{created:?}"
        );
        assert!(kept.is_file());
    }

    #[test]
    fn a_piece_deeper_than_the_path_limit_can_be_pinned_and_unpinned() {
        let mut tree = Tree::new();
        tree.dir("piece");
        let root = open(&tree);
        let deep = deep_path(25);
        let mut so_far = path("piece");
        for component in deep.components() {
            let component = Name::from_bytes(component.to_vec()).expect("valid name");
            so_far = so_far.join(&component);
            root.create_dir(&so_far, 0o755)
                .expect("the directory is created");
        }
        root.create_file(
            &so_far.join(&Name::from_bytes(b"f".to_vec()).expect("valid")),
            0o644,
        )
        .expect("the file is created")
        .write_all(b"x")
        .expect("the file is written");

        create(&root, &so_far).expect("the pin is created");
        assert!(is_pinned(&root, &so_far).expect("the lookup works"));
        remove(&root, &so_far).expect("the pin is removed");
        assert!(!is_pinned(&root, &so_far).expect("the lookup works"));
    }

    #[test]
    fn creating_a_pin_in_a_missing_piece_fails() {
        let tree = Tree::new();
        let root = open(&tree);

        let error = create(&root, &path("gone")).expect_err("there is no piece");
        assert!(matches!(error, PinError::Create { .. }), "{error:?}");
    }

    #[test]
    fn a_piece_that_is_a_file_cannot_be_looked_into() {
        let mut tree = Tree::new();
        tree.file("plain", "x");
        let root = open(&tree);

        let looked = is_pinned(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(looked, PinError::Stat { .. }), "{looked}");
        let removed = remove(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(removed, PinError::Stat { .. }), "{removed}");
        let created = create(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(created, PinError::Create { .. }), "{created}");
    }

    #[test]
    fn a_pin_that_cannot_be_deleted_is_reported() {
        let mut tree = Tree::new();
        tree.dir("a");
        let pin = tree.file("a/.antiq-pin", "");
        let piece = tree.path("a");
        let _restore = crate::filesystem::RestoreModes(vec![piece.clone()]);
        fs::set_permissions(&piece, std::os::unix::fs::PermissionsExt::from_mode(0o555))
            .expect("the piece becomes read-only");
        assert!(
            fs::remove_file(&pin).is_err(),
            "the test needs a user that a read-only directory stops"
        );
        let root = open(&tree);

        let error = remove(&root, &path("a")).expect_err("the pin stays");
        assert!(matches!(error, PinError::Remove { .. }), "{error}");
        assert!(pin.exists());
    }

    // =========================================================
    // Durability
    // =========================================================

    use std::cell::RefCell;

    /// A flush that records what it was asked to make durable, and fails
    /// for the path `fail_on`.
    fn recording<'a>(
        calls: &'a RefCell<Vec<String>>,
        fail_on: Option<&'a str>,
    ) -> impl Fn(&File, &Path) -> Result<(), FilesystemError> + 'a {
        move |_file, path| {
            let shown = path.display().to_string();
            calls.borrow_mut().push(shown.clone());
            if fail_on == Some(shown.as_str()) {
                return Err(FilesystemError::Io {
                    action: "flush",
                    path: shown,
                    source: io::Error::from_raw_os_error(5),
                });
            }
            Ok(())
        }
    }

    #[test]
    fn creating_a_pin_flushes_the_file_and_then_the_piece_directory() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        create_with(&root, &path("a"), &recording(&calls, None)).expect("the pin is created");

        assert_eq!(*calls.borrow(), ["a/.antiq-pin", "a"]);
    }

    #[test]
    fn creating_over_an_existing_pin_flushes_the_piece_directory() {
        let mut tree = Tree::new();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        create_with(&root, &path("a"), &recording(&calls, None)).expect("the pin stays");

        assert_eq!(*calls.borrow(), ["a"]);
    }

    #[test]
    fn removing_a_pin_flushes_the_piece_directory() {
        let mut tree = Tree::new();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        remove_with(&root, &path("a"), &recording(&calls, None)).expect("the pin is removed");

        assert_eq!(*calls.borrow(), ["a"]);
    }

    #[test]
    fn removing_a_pin_that_is_not_there_flushes_nothing() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        remove_with(&root, &path("a"), &recording(&calls, None)).expect("nothing to remove");

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_refused_directory_flushes_nothing() {
        let mut tree = Tree::new();
        tree.dir("a/.antiq-pin");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        create_with(&root, &path("a"), &recording(&calls, None)).expect_err("refused");
        remove_with(&root, &path("a"), &recording(&calls, None)).expect_err("refused");

        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn a_pin_file_that_cannot_be_flushed_is_reported() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        let error = create_with(&root, &path("a"), &recording(&calls, Some("a/.antiq-pin")))
            .expect_err("the flush fails");

        assert!(
            matches!(&error, PinError::Flush { path, .. } if path.to_string() == "a/.antiq-pin"),
            "{error}"
        );
    }

    #[test]
    fn a_piece_directory_that_cannot_be_flushed_after_creating_is_reported() {
        let mut tree = Tree::new();
        tree.dir("a");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        let error = create_with(&root, &path("a"), &recording(&calls, Some("a")))
            .expect_err("the flush fails");

        assert!(
            matches!(&error, PinError::Flush { path, .. } if path.to_string() == "a"),
            "{error}"
        );
    }

    #[test]
    fn a_piece_directory_that_cannot_be_flushed_after_removing_is_reported() {
        let mut tree = Tree::new();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let calls = RefCell::new(Vec::new());

        let error = remove_with(&root, &path("a"), &recording(&calls, Some("a")))
            .expect_err("the flush fails");

        assert!(
            matches!(&error, PinError::Flush { path, .. } if path.to_string() == "a"),
            "{error}"
        );
    }
}
