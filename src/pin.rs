// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pin files.

use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::flush_dir_of;
use crate::filesystem::tree::TreeRoot;
use crate::markers::{self, MarkerError, RealSite, Site};
use crate::relpath::{Name, RelPath};
use crate::tree::EntryKind;
use std::io;

/// The file at the top of a piece that pins it.
pub const PIN_FILE: &str = ".antiq-pin";

/// Errors of pin files.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
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
        #[diagnostic_source]
        source: MarkerError,
    },
    #[error("cannot remove the pin file {path}: {source}")]
    Remove {
        path: RelPath,
        #[source]
        #[diagnostic_source]
        source: MarkerError,
    },
    #[error("cannot make the pin of {path} durable: {source}")]
    Flush {
        path: RelPath,
        #[source]
        #[diagnostic_source]
        source: FilesystemError,
    },
    #[error("{path} is neither a file nor a symlink, so it is no pin file")]
    NotAFile { path: RelPath },
}

impl crate::error::HasKind for PinError {}

/// The path of the pin file of the piece at `piece`.
fn pin_path(piece: &RelPath) -> RelPath {
    let name = Name::from_bytes(PIN_FILE.as_bytes().to_vec()).expect("the pin file name is valid");
    piece.join(&name)
}

/// What is at the pin path, or `None` when nothing is.
fn lookup(collection: &TreeRoot, pin: &RelPath) -> io::Result<Option<EntryKind>> {
    match collection.lstat(pin) {
        Ok(entry) => Ok(Some(entry.kind)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether `kind` is what a pin is: a regular file, or a symlink, which
/// may dangle.
fn is_pin(kind: EntryKind) -> bool {
    matches!(kind, EntryKind::File | EntryKind::Symlink)
}

/// Whether the piece at `piece`, relative to `collection`, carries a pin.
///
/// A regular file or a symlink of that name pins the piece, a dangling
/// symlink included. Any other entry of that name is not a pin.
pub fn is_pinned(collection: &TreeRoot, piece: &RelPath) -> Result<bool, PinError> {
    let pin = pin_path(piece);
    match lookup(collection, &pin) {
        Ok(found) => Ok(found.is_some_and(is_pin)),
        Err(source) => Err(PinError::Stat { path: pin, source }),
    }
}

/// Whether the piece at `piece` is pinned: a file or a symlink at the pin's
/// name is, nothing is not. An entry of that name that is neither is an
/// error, so that `pin` and `unpin` refuse it before they decide anything.
pub fn current(collection: &TreeRoot, piece: &RelPath) -> Result<bool, PinError> {
    let pin = pin_path(piece);
    match lookup(collection, &pin) {
        Ok(None) => Ok(false),
        Ok(Some(kind)) if is_pin(kind) => Ok(true),
        Ok(Some(_)) => Err(PinError::NotAFile { path: pin }),
        Err(source) => Err(PinError::Stat { path: pin, source }),
    }
}

/// What [`create_with`] would refuse, found without writing anything.
///
/// The device of the top matters only when the pin has to be written: over
/// an existing pin `create` only flushes.
#[allow(
    dead_code,
    reason = "the pin command (C3) is its caller and removes this"
)]
pub(crate) fn check_create(
    collection: &TreeRoot,
    piece: &RelPath,
    site: &dyn Site,
) -> Result<(), PinError> {
    if current(collection, piece)? {
        return Ok(());
    }
    markers::ensure_same_device(collection, piece, site).map_err(|source| PinError::Create {
        path: pin_path(piece),
        source,
    })
}

/// Pins the piece at `piece` by writing its empty pin file the way every
/// marker is written. A piece that is pinned already stays so. The pin is
/// durable when this returns: the file and the entry in the piece's
/// directory are flushed.
pub fn create(collection: &TreeRoot, piece: &RelPath) -> Result<(), PinError> {
    create_with(collection, piece, &RealSite)
}

pub(crate) fn create_with(
    collection: &TreeRoot,
    piece: &RelPath,
    site: &dyn Site,
) -> Result<(), PinError> {
    let pin = pin_path(piece);
    match lookup(collection, &pin) {
        Ok(None) => markers::put(collection, piece, PIN_FILE, b"", site)
            .map_err(|source| PinError::Create { path: pin, source }),
        Ok(Some(kind)) if is_pin(kind) => flush_piece(collection, piece, site),
        Ok(Some(_)) => Err(PinError::NotAFile { path: pin }),
        Err(source) => Err(PinError::Stat { path: pin, source }),
    }
}

/// Removes the pin of the piece at `piece`; a symlink is removed itself,
/// never its target. A piece without a pin is left as it is. An entry of
/// the pin's name that is neither a file nor a symlink is refused and left
/// untouched. The removal is durable when this returns.
pub fn remove(collection: &TreeRoot, piece: &RelPath) -> Result<(), PinError> {
    remove_with(collection, piece, &RealSite)
}

pub(crate) fn remove_with(
    collection: &TreeRoot,
    piece: &RelPath,
    site: &dyn Site,
) -> Result<(), PinError> {
    let pin = pin_path(piece);
    match lookup(collection, &pin) {
        Ok(None) => Ok(()),
        Ok(Some(kind)) if is_pin(kind) => markers::unlink(collection, piece, PIN_FILE, site)
            .map_err(|source| PinError::Remove { path: pin, source }),
        Ok(Some(_)) => Err(PinError::NotAFile { path: pin }),
        Err(source) => Err(PinError::Stat { path: pin, source }),
    }
}

/// Makes the entries of the piece's top directory durable, which is where
/// the pin file's name lives.
fn flush_piece(collection: &TreeRoot, piece: &RelPath, site: &dyn Site) -> Result<(), PinError> {
    flush_dir_of(site.durable(), collection, piece).map_err(|source| PinError::Flush {
        path: piece.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use crate::testing::{deep_path, tree::Tree};
    use std::fs;
    use std::io::Write;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    /// A collection with the folder markers are written through.
    fn collection() -> Tree {
        let mut tree = Tree::new();
        tree.dir(".antiq/tmp");
        tree
    }

    fn open(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    #[test]
    fn creating_a_pin_makes_an_empty_file_at_the_top_of_the_piece() {
        let mut tree = collection();
        tree.dir("projects/a");
        let root = open(&tree);

        create(&root, &path("projects/a")).expect("the pin is created");

        let pin = tree.path("projects/a/.antiq-pin");
        assert_eq!(fs::read(pin).expect("the pin exists"), b"");
    }

    #[test]
    fn a_piece_is_pinned_once_its_pin_exists() {
        let mut tree = collection();
        tree.dir("a");
        tree.dir("b");
        tree.file("b/.antiq-pin", "");
        let root = open(&tree);

        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
        assert!(is_pinned(&root, &path("b")).expect("the lookup works"));
    }

    #[test]
    fn a_pin_made_by_create_is_found() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);

        create(&root, &path("a")).expect("the pin is created");

        assert!(is_pinned(&root, &path("a")).expect("the lookup works"));
    }

    #[test]
    fn removing_a_pin_deletes_the_file() {
        let mut tree = collection();
        let pin = tree.file("a/.antiq-pin", "");
        let root = open(&tree);

        remove(&root, &path("a")).expect("the pin is removed");

        assert!(!pin.exists());
        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
    }

    #[test]
    fn removing_a_pin_that_is_not_there_is_not_an_error() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);

        remove(&root, &path("a")).expect("nothing to remove is fine");
        remove(&root, &path("a")).expect("and so is doing it twice");
    }

    #[test]
    fn creating_a_pin_twice_keeps_one_file() {
        let mut tree = collection();
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
        let mut tree = collection();
        tree.dir("a");
        tree.symlink("a/.antiq-pin", "nowhere");
        let root = open(&tree);

        assert!(is_pinned(&root, &path("a")).expect("the lookup works"));
        remove(&root, &path("a")).expect("the link itself is removed");
        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
    }

    #[test]
    fn a_directory_with_the_pin_name_is_no_pin_and_is_left_alone() {
        let mut tree = collection();
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
        let mut tree = collection();
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
        let tree = collection();
        let root = open(&tree);

        let error = create(&root, &path("gone")).expect_err("there is no piece");
        assert!(matches!(error, PinError::Create { .. }), "{error:?}");
    }

    #[test]
    fn a_piece_that_is_a_file_cannot_be_looked_into() {
        let mut tree = collection();
        tree.file("plain", "x");
        let root = open(&tree);

        let looked = is_pinned(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(looked, PinError::Stat { .. }), "{looked}");
        let removed = remove(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(removed, PinError::Stat { .. }), "{removed}");
        let created = create(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(created, PinError::Stat { .. }), "{created}");
    }

    // =========================================================
    // Pins as the walk sees them
    // =========================================================

    #[test]
    fn a_dangling_symlink_pins_and_removing_it_leaves_its_target() {
        let mut tree = collection();
        tree.file("outside", "keep");
        tree.symlink("a/.antiq-pin", "../outside");
        tree.symlink("b/.antiq-pin", "nowhere");
        let root = open(&tree);

        assert!(is_pinned(&root, &path("b")).expect("the lookup works"));
        remove(&root, &path("a")).expect("the link is removed");

        assert_eq!(fs::read(tree.path("outside")).expect("the target"), b"keep");
        assert!(fs::symlink_metadata(tree.path("a/.antiq-pin")).is_err());
    }

    #[test]
    fn a_fifo_with_the_pin_name_is_no_pin_and_is_left_alone() {
        let mut tree = collection();
        tree.fifo("a/.antiq-pin");
        let root = open(&tree);

        assert!(!is_pinned(&root, &path("a")).expect("the lookup works"));
        let created = create(&root, &path("a")).expect_err("a fifo is refused");
        assert!(matches!(created, PinError::NotAFile { .. }), "{created:?}");
        let removed = remove(&root, &path("a")).expect_err("a fifo is refused");
        assert!(matches!(removed, PinError::NotAFile { .. }), "{removed:?}");
        let kind = fs::symlink_metadata(tree.path("a/.antiq-pin")).expect("the fifo stays");
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(&kind.file_type()));
    }

    // =========================================================
    // Writing through the temporary folder
    // =========================================================

    /// A piece that cannot be written to, with the precondition checked.
    fn read_only(tree: &Tree, relative: &str) -> crate::filesystem::RestoreModes {
        let guard = crate::filesystem::RestoreModes(vec![tree.path(relative)]);
        fs::set_permissions(
            tree.path(relative),
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .expect("the piece becomes read-only");
        assert!(
            fs::write(tree.path(relative).join("probe"), "").is_err(),
            "the test needs a user that a read-only directory stops"
        );
        guard
    }

    fn mode_of(tree: &Tree, relative: &str) -> u32 {
        std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(tree.path(relative))
                .expect("metadata")
                .permissions(),
        ) & 0o7777
    }

    #[test]
    fn creating_a_pin_leaves_no_temp_file_behind() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);

        create(&root, &path("a")).expect("the pin is created");

        assert_eq!(
            fs::read_dir(tree.path(".antiq/tmp")).expect("tmp").count(),
            0
        );
    }

    #[test]
    fn a_read_only_piece_can_be_pinned_and_unpinned_and_keeps_its_mode() {
        let mut tree = collection();
        tree.dir("a");
        let _restore = read_only(&tree, "a");
        let root = open(&tree);

        create(&root, &path("a")).expect("the pin is created");
        assert!(tree.path("a/.antiq-pin").exists());
        assert_eq!(mode_of(&tree, "a"), 0o555);
        remove(&root, &path("a")).expect("the pin is removed");

        assert!(!tree.path("a/.antiq-pin").exists());
        assert_eq!(mode_of(&tree, "a"), 0o555);
    }

    #[test]
    fn a_piece_on_another_device_than_tmp_is_refused_as_a_mount_point() {
        struct Elsewhere(Recording);
        impl Site for Elsewhere {
            fn durable(&self) -> &dyn crate::filesystem::atomic::Durable {
                &self.0
            }
            fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
                let device = root.lstat(path)?.device;
                Ok(if path.to_string() == "a" {
                    device + 1
                } else {
                    device
                })
            }
        }
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);

        let error = create_with(&root, &path("a"), &Elsewhere(Recording::new()))
            .expect_err("the pin is refused");

        assert!(
            matches!(
                &error,
                PinError::Create {
                    source: MarkerError::MountPoint { .. },
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("mount point"));
        assert!(!tree.path("a/.antiq-pin").exists());
    }

    // =========================================================
    // Looking before writing
    // =========================================================

    /// A site that puts the piece `a` on another device than `.antiq/tmp/`.
    struct OtherDevice(Recording);

    impl Site for OtherDevice {
        fn durable(&self) -> &dyn crate::filesystem::atomic::Durable {
            &self.0
        }
        fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
            let device = root.lstat(path)?.device;
            Ok(if path.to_string() == "a" {
                device + 1
            } else {
                device
            })
        }
    }

    #[test]
    fn the_current_state_is_a_file_or_a_symlink_or_nothing() {
        let mut tree = collection();
        tree.dir("none");
        tree.file("file/.antiq-pin", "");
        tree.symlink("link/.antiq-pin", "nowhere");
        let root = open(&tree);

        assert!(!current(&root, &path("none")).expect("nothing is no pin"));
        assert!(current(&root, &path("file")).expect("a file is a pin"));
        assert!(current(&root, &path("link")).expect("a dangling symlink is a pin"));
    }

    #[test]
    fn the_current_state_refuses_an_entry_that_is_no_pin() {
        let mut tree = collection();
        tree.dir("dir/.antiq-pin");
        tree.fifo("fifo/.antiq-pin");
        let root = open(&tree);

        for piece in ["dir", "fifo"] {
            let error = current(&root, &path(piece)).expect_err("not a pin");
            let expected = format!("{piece}/.antiq-pin");
            assert!(
                matches!(&error, PinError::NotAFile { path } if path.to_string() == expected),
                "{piece}: {error:?}"
            );
        }
    }

    #[test]
    fn the_current_state_of_a_piece_that_is_a_file_is_a_failed_lookup() {
        let mut tree = collection();
        tree.file("plain", "x");
        let root = open(&tree);

        let error = current(&root, &path("plain")).expect_err("a file has no top");
        assert!(matches!(error, PinError::Stat { .. }), "{error:?}");
    }

    #[test]
    fn a_pin_check_accepts_a_top_it_could_pin_and_writes_nothing() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        check_create(&root, &path("a"), &site).expect("the top can be pinned");

        assert!(!tree.path("a/.antiq-pin").exists());
        assert_eq!(fs::read_dir(tree.path("a")).expect("lists").count(), 0);
        assert_eq!(
            fs::read_dir(tree.path(".antiq/tmp")).expect("tmp").count(),
            0
        );
        assert!(site.0.flushes().is_empty());
    }

    #[test]
    fn a_pin_check_refuses_what_create_refuses_before_writing() {
        let mut tree = collection();
        tree.dir("dir/.antiq-pin");
        tree.file("plain", "x");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        let directory = check_create(&root, &path("dir"), &site).expect_err("a directory");
        assert!(
            matches!(directory, PinError::NotAFile { .. }),
            "{directory:?}"
        );
        let file = check_create(&root, &path("plain"), &site).expect_err("a file has no top");
        assert!(matches!(file, PinError::Stat { .. }), "{file:?}");
    }

    #[test]
    fn a_pin_check_refuses_a_top_on_another_device_when_there_is_no_pin() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);

        let error = check_create(&root, &path("a"), &OtherDevice(Recording::new()))
            .expect_err("a mount point");

        assert!(
            matches!(
                &error,
                PinError::Create {
                    source: MarkerError::MountPoint { .. },
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("mount point"));
    }

    #[test]
    fn a_pin_check_accepts_a_pinned_top_on_another_device() {
        // `create` only flushes over an existing pin, so it never meets
        // the device.
        let mut tree = collection();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let site = OtherDevice(Recording::new());

        check_create(&root, &path("a"), &site).expect("an existing pin needs no write");
        create_with(&root, &path("a"), &site).expect("and create agrees");
    }

    #[test]
    fn a_pin_that_cannot_be_unlinked_is_reported() {
        struct Stuck(Recording);
        impl Site for Stuck {
            fn durable(&self) -> &dyn crate::filesystem::atomic::Durable {
                &self.0
            }
            fn unlink(&self, _: &TreeRoot, _: &RelPath) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(5))
            }
        }
        let mut tree = collection();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);

        let error =
            remove_with(&root, &path("a"), &Stuck(Recording::new())).expect_err("the unlink fails");

        assert!(matches!(error, PinError::Remove { .. }), "{error:?}");
        assert!(tree.path("a/.antiq-pin").exists());
    }

    // =========================================================
    // Durability
    // =========================================================

    struct Recorded(Recording);

    impl Site for Recorded {
        fn durable(&self) -> &dyn crate::filesystem::atomic::Durable {
            &self.0
        }
    }

    fn dir_flush(shown: &str) -> Flush {
        Flush::Dir(shown.into())
    }

    #[test]
    fn creating_a_pin_flushes_the_file_and_then_the_piece_directory() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        create_with(&root, &path("a"), &site).expect("the pin is created");

        let flushes = site.0.flushes();
        assert!(
            matches!(&flushes[0], Flush::File(file) if file.to_string_lossy().starts_with(".antiq/tmp/.antiq-pin.")),
            "{flushes:?}"
        );
        assert_eq!(flushes[1..], [dir_flush("a")]);
    }

    #[test]
    fn creating_over_an_existing_pin_flushes_the_piece_directory() {
        let mut tree = collection();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        create_with(&root, &path("a"), &site).expect("the pin stays");

        assert_eq!(site.0.flushes(), [dir_flush("a")]);
    }

    #[test]
    fn removing_a_pin_flushes_the_piece_directory() {
        let mut tree = collection();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        remove_with(&root, &path("a"), &site).expect("the pin is removed");

        assert_eq!(site.0.flushes(), [dir_flush("a")]);
    }

    #[test]
    fn removing_a_pin_that_is_not_there_flushes_nothing() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        remove_with(&root, &path("a"), &site).expect("nothing to remove");

        assert!(site.0.flushes().is_empty());
    }

    #[test]
    fn a_refused_directory_flushes_nothing() {
        let mut tree = collection();
        tree.dir("a/.antiq-pin");
        let root = open(&tree);
        let site = Recorded(Recording::new());

        create_with(&root, &path("a"), &site).expect_err("refused");
        remove_with(&root, &path("a"), &site).expect_err("refused");

        assert!(site.0.flushes().is_empty());
    }

    #[test]
    fn a_pin_file_that_cannot_be_flushed_is_reported() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);
        let site = Recorded(Recording::new());
        site.0.fail_files_from(0);

        let error = create_with(&root, &path("a"), &site).expect_err("the flush fails");

        assert!(
            matches!(
                &error,
                PinError::Create {
                    source: MarkerError::Filesystem(_),
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_piece_directory_that_cannot_be_flushed_after_creating_is_reported() {
        let mut tree = collection();
        tree.dir("a");
        let root = open(&tree);
        let site = Recorded(Recording::new());
        site.0.fail_dirs_from(0);

        let error = create_with(&root, &path("a"), &site).expect_err("the flush fails");

        assert!(matches!(&error, PinError::Create { .. }), "{error:?}");
    }

    #[test]
    fn a_piece_directory_that_cannot_be_flushed_over_an_existing_pin_is_reported() {
        let mut tree = collection();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let site = Recorded(Recording::new());
        site.0.fail_dirs_from(0);

        let error = create_with(&root, &path("a"), &site).expect_err("the flush fails");

        assert!(
            matches!(&error, PinError::Flush { path, .. } if path.to_string() == "a"),
            "{error:?}"
        );
    }

    #[test]
    fn a_piece_directory_that_cannot_be_flushed_after_removing_is_reported() {
        let mut tree = collection();
        tree.file("a/.antiq-pin", "");
        let root = open(&tree);
        let site = Recorded(Recording::new());
        site.0.fail_dirs_from(0);

        let error = remove_with(&root, &path("a"), &site).expect_err("the flush fails");

        assert!(matches!(&error, PinError::Remove { .. }), "{error:?}");
    }
}
