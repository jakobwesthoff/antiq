// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks that relate the files of a stored version to each other: the
//! archive against its checksum file and the size its listing records,
//! and the members of the archive against the listing.

use super::hashing::HashingReader;
use super::verify::{Difference, compare};
use super::{PackError, read};
use crate::listing::{ListingEntry, is_sha256_hex};
use std::fs::File;
use std::io;
use std::path::Path;

// =========================================================
// Hashing a file
// =========================================================

/// The size and SHA-256 of a file, from one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigest {
    pub bytes: u64,
    /// Lower-case hex.
    pub sha256: String,
}

/// Reads the whole file at `path`.
pub fn file_digest(path: &Path) -> Result<FileDigest, PackError> {
    let read_error = |source| PackError::ReadFile {
        path: path.to_path_buf(),
        source,
    };
    let file = File::open(path).map_err(read_error)?;
    let mut reader = HashingReader::new(file);
    io::copy(&mut reader, &mut io::sink()).map_err(read_error)?;
    Ok(FileDigest {
        bytes: reader.bytes_read(),
        sha256: reader.sha256(),
    })
}

/// The SHA-256 of the file at `path`, as lower-case hex.
pub fn file_sha256(path: &Path) -> Result<String, PackError> {
    file_digest(path).map(|digest| digest.sha256)
}

// =========================================================
// The checksum file
// =========================================================

/// The one line of a checksum file: `<hash>  <archive file name>` and a
/// newline. It is the format of `sha256sum`, so `shasum -a 256 -c` in the
/// store folder checks the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumFile {
    sha256: String,
    archive: String,
}

impl ChecksumFile {
    /// `sha256` is lower-case hex and `archive` the bare file name.
    pub fn new(sha256: String, archive: String) -> ChecksumFile {
        ChecksumFile { sha256, archive }
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn archive(&self) -> &str {
        &self.archive
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        format!("{}  {}\n", self.sha256, self.archive).into_bytes()
    }

    /// Reads exactly the line [`ChecksumFile::to_bytes`] writes: nothing
    /// else, such as a binary marker, an upper-case hash or a second line,
    /// is a checksum file of antiq.
    pub fn parse(bytes: &[u8]) -> Result<ChecksumFile, PackError> {
        let refuse = |reason: &str| PackError::ChecksumFile(reason.into());
        let Some(line) = bytes.strip_suffix(b"\n") else {
            return Err(refuse("it is not a line ending in a newline"));
        };
        if line.contains(&b'\n') {
            return Err(refuse("it holds more than one line"));
        }
        if line.len() < 67 || !is_sha256_hex(&line[..64]) || &line[64..66] != b"  " {
            return Err(refuse(
                "the line is not a lower-case SHA-256 hash and two spaces before the name",
            ));
        }
        let name = &line[66..];
        if name.starts_with(b"\\") || name.contains(&b'\r') {
            return Err(refuse("the archive file name is written escaped"));
        }
        let archive = String::from_utf8(name.to_vec())
            .map_err(|_| refuse("the archive file name is not UTF-8"))?;
        let sha256 = String::from_utf8(line[..64].to_vec()).expect("hex digits are ASCII");
        Ok(ChecksumFile { sha256, archive })
    }

    /// Whether the line names `archive_name`, the file it sits beside.
    pub fn check_for(&self, archive_name: &str) -> Result<(), PackError> {
        if self.archive == archive_name {
            return Ok(());
        }
        Err(PackError::ChecksumFile(format!(
            "it names `{}`, the archive is `{archive_name}`",
            self.archive
        )))
    }
}

// =========================================================
// The archive file
// =========================================================

fn differs(archive: &Path, what: String) -> PackError {
    PackError::ArchiveDiffers {
        file: archive
            .file_name()
            .unwrap_or(archive.as_os_str())
            .to_string_lossy()
            .into_owned(),
        what,
    }
}

/// Whether the archive has the size its listing records, by its metadata
/// alone.
pub fn check_archive_size(archive: &Path, expected: u64) -> Result<(), PackError> {
    let found = std::fs::metadata(archive)
        .map_err(|source| PackError::ReadFile {
            path: archive.to_path_buf(),
            source,
        })?
        .len();
    if found != expected {
        return Err(differs(
            archive,
            format!("it has {found} bytes, the listing says {expected}"),
        ));
    }
    Ok(())
}

/// What an archive is checked against. `rebuild` meets a version with one
/// of the two missing, so each can stand alone; having neither is not a
/// value.
#[derive(Debug, Clone, Copy)]
pub enum ArchiveCheck<'a> {
    /// The size its listing records (`archive-bytes`).
    Size(u64),
    /// The hash its checksum file holds.
    Checksum(&'a ChecksumFile),
    /// Both.
    Both {
        bytes: u64,
        checksum: &'a ChecksumFile,
    },
}

/// Whether the archive is the file that was stored. The size is checked
/// first, from the metadata, so a file of the wrong size is not read.
pub fn check_archive(archive: &Path, expected: ArchiveCheck<'_>) -> Result<(), PackError> {
    let (bytes, checksum) = match expected {
        ArchiveCheck::Size(bytes) => (Some(bytes), None),
        ArchiveCheck::Checksum(checksum) => (None, Some(checksum)),
        ArchiveCheck::Both { bytes, checksum } => (Some(bytes), Some(checksum)),
    };
    if let Some(expected) = bytes {
        check_archive_size(archive, expected)?;
    }
    if let Some(checksum) = checksum {
        let found = file_sha256(archive)?;
        if found != checksum.sha256 {
            return Err(differs(
                archive,
                format!(
                    "its SHA-256 is {found}, the checksum file says {}",
                    checksum.sha256
                ),
            ));
        }
    }
    Ok(())
}

/// Reads the archive to its end and compares every member with `listing`:
/// path, type, size, modification time, mode, link target and content
/// hash, directory times included. No folder is involved, so the members
/// and the listing are compared as they stand.
pub fn check_members_against_listing(
    archive: &Path,
    listing: &[ListingEntry],
) -> Result<(), PackError> {
    let members = read::list(archive)?;
    let differences: Vec<Difference> = compare(listing, &members, &[], true);
    if differences.is_empty() {
        Ok(())
    } else {
        Err(PackError::ListingDiffers(differences))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::listing::{LinkTarget, ListingType};
    use crate::pack::verify::Field;
    use crate::relpath::RelPath;
    use crate::testing::package::pack_piece;
    use crate::testing::tree::Tree;
    use std::fs;
    use std::process::Command;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn file_with(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("a.tar.zst");
        fs::write(&file, content).expect("the file is written");
        (dir, file)
    }

    // =========================================================
    // Hashing
    // =========================================================

    #[test]
    fn a_file_digest_has_the_size_and_the_hash() {
        let (_dir, file) = file_with(b"abc");
        assert_eq!(
            file_digest(&file).expect("reads"),
            FileDigest {
                bytes: 3,
                sha256: ABC.into()
            }
        );
        assert_eq!(file_sha256(&file).expect("reads"), ABC);
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_a_read_error_naming_it() {
        let (dir, _) = file_with(b"");
        let missing = dir.path().join("missing");
        match file_digest(&missing) {
            Err(PackError::ReadFile { path, .. }) => assert_eq!(path, missing),
            other => panic!("expected ReadFile, got {other:?}"),
        }
    }

    #[test]
    fn a_directory_cannot_be_hashed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(matches!(
            file_digest(dir.path()),
            Err(PackError::ReadFile { .. })
        ));
    }

    // =========================================================
    // The checksum file
    // =========================================================

    #[test]
    fn a_checksum_file_is_hash_two_spaces_name_and_a_newline() {
        let checksum = ChecksumFile::new(ABC.into(), "p--2026-11-04T090211.tar.zst".into());
        assert_eq!(
            checksum.to_bytes(),
            format!("{ABC}  p--2026-11-04T090211.tar.zst\n").into_bytes()
        );
        assert_eq!(
            ChecksumFile::parse(&checksum.to_bytes()).expect("parses"),
            checksum
        );
        assert_eq!(checksum.sha256(), ABC);
        assert_eq!(checksum.archive(), "p--2026-11-04T090211.tar.zst");
    }

    #[test]
    fn shasum_checks_the_archive_against_the_checksum_file_in_the_store_folder() {
        let (dir, file) = file_with(b"abc");
        let name = file
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        let checksum = ChecksumFile::new(file_sha256(&file).expect("hashes"), name.clone());
        let sidecar = format!("{name}.sha256");
        fs::write(dir.path().join(&sidecar), checksum.to_bytes()).expect("writes");
        let output = Command::new("shasum")
            .args(["-a", "256", "-c", &sidecar])
            .current_dir(dir.path())
            .output()
            .expect("shasum can be run");
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn refused_checksum(bytes: &[u8]) {
        assert!(
            matches!(ChecksumFile::parse(bytes), Err(PackError::ChecksumFile(_))),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }

    macro_rules! refuses {
        ($name:ident, $bytes:expr) => {
            #[test]
            fn $name() {
                refused_checksum(&$bytes);
            }
        };
    }

    refuses!(
        a_checksum_file_without_a_final_newline_is_refused,
        format!("{ABC}  a.tar.zst").into_bytes()
    );
    refuses!(
        a_checksum_file_of_two_lines_is_refused,
        format!("{ABC}  a.tar.zst\n{ABC}  b.tar.zst\n").into_bytes()
    );
    refuses!(
        a_binary_marker_is_refused,
        format!("{ABC} *a.tar.zst\n").into_bytes()
    );
    refuses!(
        an_upper_case_hash_is_refused,
        format!("{}  a.tar.zst\n", ABC.to_uppercase()).into_bytes()
    );
    refuses!(
        a_hash_of_63_digits_is_refused,
        format!("{}  a.tar.zst\n", &ABC[1..]).into_bytes()
    );
    refuses!(
        one_space_before_the_name_is_refused,
        format!("{ABC} a.tar.zst\n").into_bytes()
    );
    refuses!(
        an_escaped_name_is_refused,
        format!("\\{ABC}  a.tar.zst\n").into_bytes()
    );
    refuses!(
        a_name_starting_with_a_backslash_is_refused,
        format!("{ABC}  \\a.tar.zst\n").into_bytes()
    );
    refuses!(
        a_name_with_a_carriage_return_is_refused,
        format!("{ABC}  a\r.tar.zst\n").into_bytes()
    );
    refuses!(an_empty_checksum_file_is_refused, Vec::<u8>::new());
    refuses!(
        a_trailing_blank_line_is_refused,
        format!("{ABC}  a.tar.zst\n\n").into_bytes()
    );
    refuses!(a_missing_name_is_refused, format!("{ABC}  \n").into_bytes());
    refuses!(
        a_name_that_is_not_utf8_is_refused,
        [format!("{ABC}  a").as_bytes(), b"\xff\n"].concat()
    );

    #[test]
    fn the_line_must_name_the_archive_it_sits_beside() {
        let checksum = ChecksumFile::new(ABC.into(), "a.tar.zst".into());
        checksum.check_for("a.tar.zst").expect("the same name");
        assert!(matches!(
            checksum.check_for("b.tar.zst"),
            Err(PackError::ChecksumFile(reason)) if reason.contains("`a.tar.zst`") && reason.contains("`b.tar.zst`")
        ));
    }

    // =========================================================
    // The archive file
    // =========================================================

    fn checksum_of(file: &Path) -> ChecksumFile {
        ChecksumFile::new(file_sha256(file).expect("hashes"), "a.tar.zst".into())
    }

    fn differs_with(result: Result<(), PackError>) -> String {
        match result {
            Err(PackError::ArchiveDiffers { what, .. }) => what,
            other => panic!("expected ArchiveDiffers, got {other:?}"),
        }
    }

    #[test]
    fn the_archive_that_was_stored_passes_both_checks() {
        let (_dir, file) = file_with(b"abc");
        let checksum = checksum_of(&file);
        check_archive(
            &file,
            ArchiveCheck::Both {
                bytes: 3,
                checksum: &checksum,
            },
        )
        .expect("the same file");
    }

    #[test]
    fn a_hash_alone_passes_and_fails_on_another_hash() {
        let (_dir, file) = file_with(b"abc");
        check_archive(&file, ArchiveCheck::Checksum(&checksum_of(&file))).expect("the same hash");
        let other = ChecksumFile::new(sha_of_other(), "a.tar.zst".into());
        let what = differs_with(check_archive(&file, ArchiveCheck::Checksum(&other)));
        assert!(
            what.contains(ABC) && what.contains(&sha_of_other()),
            "{what}"
        );
    }

    fn sha_of_other() -> String {
        "0".repeat(64)
    }

    #[test]
    fn a_size_alone_fails_on_another_size_without_reading_the_file() {
        let (_dir, file) = file_with(b"abc");
        check_archive(&file, ArchiveCheck::Size(3)).expect("the same size");
        let what = differs_with(check_archive(&file, ArchiveCheck::Size(4)));
        assert!(what.contains("3 bytes") && what.contains('4'), "{what}");
    }

    #[test]
    fn a_size_check_does_not_open_the_file() {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            !rustix::process::geteuid().is_root(),
            "the test needs a user that the file mode keeps out"
        );
        let (_dir, file) = file_with(b"abc");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).expect("mode");
        let outcome = check_archive_size(&file, 3);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("mode");
        outcome.expect("the metadata is enough");
    }

    #[test]
    fn the_size_is_checked_before_the_hash() {
        let (_dir, file) = file_with(b"abc");
        let other = ChecksumFile::new(sha_of_other(), "a.tar.zst".into());
        let what = differs_with(check_archive(
            &file,
            ArchiveCheck::Both {
                bytes: 4,
                checksum: &other,
            },
        ));
        assert!(what.contains("bytes"), "{what}");
    }

    #[test]
    fn an_archive_that_cannot_be_read_is_a_read_error() {
        let (dir, _) = file_with(b"abc");
        let missing = dir.path().join("missing.tar.zst");
        assert!(matches!(
            check_archive_size(&missing, 3),
            Err(PackError::ReadFile { .. })
        ));
        let checksum = ChecksumFile::new(ABC.into(), "a.tar.zst".into());
        assert!(matches!(
            check_archive(&missing, ArchiveCheck::Checksum(&checksum)),
            Err(PackError::ReadFile { .. })
        ));
    }

    // =========================================================
    // Members against a listing
    // =========================================================

    fn mixed() -> Tree {
        let mut tree = Tree::new();
        tree.file_with("proj/src/main.rs", "fn main() {}", 0o600, 1_000_000_000);
        tree.file("proj/notes.txt", "hello");
        tree.symlink("proj/src/link", "main.rs");
        tree.hard_link("proj/notes.txt", "proj/src/notes-copy.txt");
        tree.dir("proj/empty");
        tree
    }

    /// The archive of [`mixed`] and the listing it was packed to.
    fn packed() -> (tempfile::TempDir, std::path::PathBuf, Vec<ListingEntry>) {
        let packaged = pack_piece(&mixed(), "proj");
        let (dir, file) = crate::testing::package::on_disk(&packaged.bytes);
        (dir, file, packaged.packed.listing)
    }

    fn differences_of(listing: &[ListingEntry], archive: &Path) -> Vec<Difference> {
        match check_members_against_listing(archive, listing) {
            Err(PackError::ListingDiffers(differences)) => differences,
            other => panic!("expected ListingDiffers, got {other:?}"),
        }
    }

    fn entry_mut<'a>(listing: &'a mut [ListingEntry], name: &str) -> &'a mut ListingEntry {
        listing
            .iter_mut()
            .find(|entry| entry.path.to_string() == name)
            .expect("the entry exists")
    }

    #[test]
    fn the_listing_an_archive_was_packed_to_matches_its_members() {
        let (_dir, file, listing) = packed();
        check_members_against_listing(&file, &listing).expect("equal");
    }

    #[test]
    fn a_member_of_another_size_is_named() {
        let (_dir, file, mut listing) = packed();
        entry_mut(&mut listing, "notes.txt").size += 1;
        assert_eq!(
            differences_of(&listing, &file),
            [Difference::Changed {
                path: path("notes.txt"),
                fields: vec![Field::Size]
            }]
        );
    }

    #[test]
    fn an_archived_path_the_listing_lacks_is_named() {
        let (_dir, file, mut listing) = packed();
        listing.retain(|entry| entry.path.to_string() != "empty");
        let differences = differences_of(&listing, &file);
        assert_eq!(differences, [Difference::Added(path("empty"))]);
        let message = PackError::ListingDiffers(differences).to_string();
        assert_eq!(
            message,
            "the archive differs from its listing: 1 difference, the first: `empty` is in the archive but not listed"
        );
    }

    #[test]
    fn several_differences_are_counted_and_the_first_is_described() {
        let (_dir, file, mut listing) = packed();
        entry_mut(&mut listing, "empty").mtime += 1;
        entry_mut(&mut listing, "notes.txt").size += 1;
        let message = PackError::ListingDiffers(differences_of(&listing, &file)).to_string();
        assert_eq!(
            message,
            "the archive differs from its listing: 2 differences, the first: `empty` differs in Mtime"
        );
    }

    #[test]
    fn a_directory_time_counts() {
        let (_dir, file, mut listing) = packed();
        entry_mut(&mut listing, "empty").mtime += 1;
        assert_eq!(
            differences_of(&listing, &file),
            [Difference::Changed {
                path: path("empty"),
                fields: vec![Field::Mtime]
            }]
        );
    }

    #[test]
    fn a_listed_path_the_archive_lacks_is_named() {
        let (_dir, file, mut listing) = packed();
        let mut extra = listing[0].clone();
        extra.path = path("zzz");
        extra.kind = ListingType::Dir;
        extra.size = 0;
        extra.sha256 = None;
        listing.push(extra);
        let differences = differences_of(&listing, &file);
        assert_eq!(differences, [Difference::Removed(path("zzz"))]);
        assert_eq!(
            PackError::ListingDiffers(differences).to_string(),
            "the archive differs from its listing: 1 difference, the first: `zzz` is listed but not in the archive"
        );
    }

    #[test]
    fn another_hard_link_target_is_named() {
        let (_dir, file, mut listing) = packed();
        entry_mut(&mut listing, "src/notes-copy.txt").link =
            Some(LinkTarget::from_bytes(b"src/main.rs".to_vec()));
        assert_eq!(
            differences_of(&listing, &file),
            [Difference::Changed {
                path: path("src/notes-copy.txt"),
                fields: vec![Field::Link]
            }]
        );
    }

    #[test]
    fn another_hard_link_hash_is_named() {
        let (_dir, file, mut listing) = packed();
        entry_mut(&mut listing, "src/notes-copy.txt").sha256 = Some(sha_of_other());
        assert_eq!(
            differences_of(&listing, &file),
            [Difference::Changed {
                path: path("src/notes-copy.txt"),
                fields: vec![Field::Content]
            }]
        );
    }

    #[test]
    fn a_damaged_archive_is_damaged_rather_than_different() {
        let packaged = pack_piece(&mixed(), "proj");
        let cut = &packaged.bytes[..packaged.bytes.len() / 2];
        let (_dir, file) = crate::testing::package::on_disk(cut);
        assert!(matches!(
            check_members_against_listing(&file, &packaged.packed.listing),
            Err(PackError::Damaged(_))
        ));
    }
}
