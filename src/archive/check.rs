// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Checks that relate the files of a stored version to each other: the
//! archive against its checksum file and the size its listing records,
//! and the members of the archive against the listing.

use super::PackError;
use super::hashing::HashingReader;
use super::verify::{Difference, Hashed, compare, read_hashed};
use crate::listing::{ListingEntry, is_sha256_hex};
use crate::progress::Counter;
use crate::shown::Shown;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
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
///
/// The name is the bytes of the file name, not text: `shasum` opens the
/// file by the bytes it reads, so a name that is not UTF-8 is found only
/// when the line holds those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksumFile {
    sha256: String,
    archive: OsString,
}

impl ChecksumFile {
    /// `sha256` is lower-case hex and `archive` the bare file name.
    pub fn new(sha256: String, archive: OsString) -> ChecksumFile {
        ChecksumFile { sha256, archive }
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn archive(&self) -> &OsStr {
        &self.archive
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut line = format!("{}  ", self.sha256).into_bytes();
        line.extend_from_slice(self.archive.as_bytes());
        line.push(b'\n');
        line
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
        let archive = OsString::from_vec(name.to_vec());
        let sha256 = String::from_utf8(line[..64].to_vec()).expect("hex digits are ASCII");
        Ok(ChecksumFile { sha256, archive })
    }

    /// Whether the line names `archive_name`, the file it sits beside.
    pub fn check_for(&self, archive_name: &OsStr) -> Result<(), PackError> {
        if self.archive == archive_name {
            return Ok(());
        }
        Err(PackError::ChecksumFile(format!(
            "it names `{}`, the archive is `{}`",
            Shown(&self.archive),
            Shown(archive_name)
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

/// Compares the `members` an archive was read to with `listing`: path, type,
/// size, modification time, mode, link target and content hash, directory
/// times included. No folder is involved, so the members and the listing are
/// compared as they stand.
pub fn check_entries_against_listing(
    members: &[ListingEntry],
    listing: &[ListingEntry],
) -> Result<(), PackError> {
    let differences: Vec<Difference> = compare(listing, members, &[], true);
    if differences.is_empty() {
        Ok(())
    } else {
        Err(PackError::ListingDiffers(differences))
    }
}

// =========================================================
// Reading an archive once
// =========================================================

/// What one read of an archive found.
#[derive(Debug)]
pub struct ArchiveRead {
    /// The members, with the hash of every file computed from the stream.
    pub entries: Vec<ListingEntry>,
    /// The size and hash of the archive file.
    pub digest: FileDigest,
}

/// Reads the archive once, so that the members and the hash of the file come
/// from the same bytes. `content` counts the bytes of the files in the
/// archive as they are read.
///
/// With a `checksum`, a file that is not the one it names is reported as
/// that, ahead of whatever the stream shows.
pub fn read_archive_once(
    archive: &Path,
    checksum: Option<&ChecksumFile>,
    content: Option<&Counter>,
) -> Result<ArchiveRead, PackError> {
    let file = File::open(archive).map_err(PackError::OpenPackage)?;
    let Hashed { package, digest } = read_hashed(archive, file, content, &mut |_| Ok(()))?;
    if let Some(checksum) = checksum
        && digest.sha256 != checksum.sha256
    {
        return Err(differs(
            archive,
            format!(
                "its SHA-256 is {}, the checksum file says {}",
                digest.sha256, checksum.sha256
            ),
        ));
    }
    Ok(ArchiveRead {
        entries: package?.entries,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::read;
    use crate::archive::verify::Field;
    use crate::listing::{LinkTarget, ListingType};
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
        let checksum = ChecksumFile::new(file_sha256(&file).expect("hashes"), name.clone().into());
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
    #[test]
    fn a_name_that_is_not_utf8_is_written_and_read_by_its_bytes() {
        let name = OsStr::from_bytes(b"a\xffb.tar.zst");
        let checksum = ChecksumFile::new(ABC.into(), name.to_os_string());
        let line = checksum.to_bytes();
        assert_eq!(
            line,
            [format!("{ABC}  ").as_bytes(), b"a\xffb.tar.zst\n"].concat()
        );
        let parsed = ChecksumFile::parse(&line).expect("a name need not be text");
        assert_eq!(parsed.archive(), name);
        parsed.check_for(name).expect("the same bytes");
    }

    #[test]
    fn a_line_for_the_lossy_name_does_not_name_an_archive_that_is_not_utf8() {
        let archive = OsStr::from_bytes(b"a\xffb.tar.zst");
        let lossy = ChecksumFile::new(ABC.into(), "a\u{fffd}b.tar.zst".into());
        assert!(matches!(
            lossy.check_for(archive),
            Err(PackError::ChecksumFile(reason))
                if reason == "it names `a\u{fffd}b.tar.zst`, the archive is `a%FFb.tar.zst`"
        ));
    }

    #[test]
    fn the_line_must_name_the_archive_it_sits_beside() {
        let checksum = ChecksumFile::new(ABC.into(), "a.tar.zst".into());
        checksum
            .check_for("a.tar.zst".as_ref())
            .expect("the same name");
        assert!(matches!(
            checksum.check_for("b.tar.zst".as_ref()),
            Err(PackError::ChecksumFile(reason)) if reason.contains("`a.tar.zst`") && reason.contains("`b.tar.zst`")
        ));
    }

    // =========================================================
    // The archive file
    // =========================================================

    fn checksum_of(file: &Path) -> ChecksumFile {
        ChecksumFile::new(file_sha256(file).expect("hashes"), "a.tar.zst".into())
    }

    fn sha_of_other() -> String {
        "0".repeat(64)
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
        let members = read::list(archive).expect("the archive reads");
        match check_entries_against_listing(&members, listing) {
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
        let members = read::list(&file).expect("the archive reads");
        check_entries_against_listing(&members, &listing).expect("equal");
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

    // =========================================================
    // Reading an archive once
    // =========================================================

    #[test]
    fn one_read_gives_the_members_and_the_hash_of_the_file_and_counts_the_content() {
        let packaged = pack_piece(&mixed(), "proj");
        let (_dir, file) = file_with(&packaged.bytes);
        let counter = Counter::new();

        let read = read_archive_once(&file, None, Some(&counter)).expect("the archive reads");

        assert_eq!(read.entries, packaged.packed.listing);
        assert_eq!(read.digest, file_digest(&file).expect("hashes"));
        // `fn main() {}` and `hello`: the hard link adds none.
        assert_eq!(counter.done(), 17);
    }

    #[test]
    fn a_file_that_is_not_the_one_the_checksum_names_is_reported_before_the_stream() {
        let packaged = pack_piece(&mixed(), "proj");
        let cut = &packaged.bytes[..packaged.bytes.len() / 2];
        let (_dir, file) = file_with(cut);
        let checksum = ChecksumFile::new(ABC.into(), "a.tar.zst".into());

        let error = read_archive_once(&file, Some(&checksum), None).expect_err("it differs");

        assert!(
            matches!(error, PackError::ArchiveDiffers { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_damaged_stream_with_a_matching_checksum_is_damaged() {
        let packaged = pack_piece(&mixed(), "proj");
        let cut = &packaged.bytes[..packaged.bytes.len() / 2];
        let (_dir, file) = file_with(cut);
        let checksum = checksum_of(&file);

        assert!(matches!(
            read_archive_once(&file, Some(&checksum), None),
            Err(PackError::Damaged(_))
        ));
    }

    #[test]
    fn an_archive_that_is_gone_cannot_be_opened() {
        let (dir, _) = file_with(b"");
        assert!(matches!(
            read_archive_once(&dir.path().join("missing"), None, None),
            Err(PackError::OpenPackage(_))
        ));
    }

    #[test]
    fn an_archive_that_fails_to_read_is_a_read_error_with_and_without_a_checksum() {
        // A directory opens, and the first read of it fails.
        let dir = tempfile::tempdir().expect("a temp dir");
        let checksum = ChecksumFile::new(ABC.into(), "a.tar.zst".into());
        for checksum in [None, Some(&checksum)] {
            match read_archive_once(dir.path(), checksum, None) {
                Err(PackError::ReadFile { path, .. }) => assert_eq!(path, dir.path()),
                other => panic!("expected ReadFile, got {other:?}"),
            }
        }
    }
}
