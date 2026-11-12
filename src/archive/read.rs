// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reading members and listings from a package.

use super::hashing::HashingReader;
use super::{PackError, zstd};
use crate::listing::{LinkTarget, ListingEntry, ListingType};
use crate::progress::{Counter, Metered};
use crate::relpath::{Name, RelPath};
use crate::shown::Shown;
use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::Path;
use std::rc::Rc;
use tar::EntryType;

/// Mode and modification time of the top-level directory, which has no
/// line in the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Container {
    pub mode: u32,
    pub mtime: i64,
}

/// What reading a package to its end found.
#[derive(Debug)]
pub(crate) struct Package {
    pub container: Container,
    /// One entry per member below the top-level directory, in archive
    /// order, with the SHA-256 of every file computed from the stream.
    pub entries: Vec<ListingEntry>,
}

/// The listing entries of the members of `package`.
pub fn list(package: &Path) -> Result<Vec<ListingEntry>, PackError> {
    read_package(package).map(|read| read.entries)
}

pub(super) fn damaged(error: io::Error) -> PackError {
    PackError::Damaged(error.to_string())
}

/// `full` without the top-level directory's name and the `/` after it, or
/// `None` for a path that does not start with them.
fn within<'a>(container: &[u8], full: &'a [u8]) -> Option<&'a [u8]> {
    full.strip_prefix(container)?.strip_prefix(b"/")
}

/// The modification time of a member in whole seconds, 0 for one before
/// 1970. The `tar` crate reads the 12-byte field as an unsigned number
/// from its last 8 bytes, which turns the negative base-256 numbers that
/// GNU tar writes for such times into huge ones.
fn modification_time(header: &tar::Header) -> Result<i64, PackError> {
    let field = header.as_old().mtime;
    let too_large = || PackError::Damaged("a modification time does not fit 63 bits".into());
    if field[0] == 0xff {
        return Ok(0);
    }
    if field[0] & 0x80 != 0 {
        let mut value = 0u128;
        for (index, byte) in field.iter().enumerate() {
            let byte = if index == 0 { byte & 0x7f } else { *byte };
            value = (value << 8) | u128::from(byte);
        }
        return i64::try_from(value).map_err(|_| too_large());
    }
    let seconds = header.mtime().map_err(damaged)?;
    Ok(i64::try_from(seconds).expect("twelve octal digits are below 2^63"))
}

/// Counts the bytes that pass through, so the end of the tar stream can
/// be told from the end of the data that precedes it.
struct Counted<R> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R: io::Read> io::Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.count.set(self.count.get() + count as u64);
        Ok(count)
    }
}

/// The two zero blocks that end a tar archive.
const END_MARKER: u64 = 1024;

/// Reads `package` to its end.
///
/// The package must be one antiq wrote in this run or one that passed
/// the scan of `unpack`: the tar reader
/// loads a long-name or pax record whole, whatever size its header gives.
pub(crate) fn read_package(package: &Path) -> Result<Package, PackError> {
    let file = File::open(package).map_err(PackError::OpenPackage)?;
    read_package_from(file)
}

/// Reads the package `source` holds to its end.
///
/// A tar reader takes the end of its input on a header boundary for the
/// end of the archive, so a stream of several frames that was cut between
/// two frames would read as a smaller, complete archive. The end blocks
/// are what tells the two apart.
///
/// The precondition of [`read_package`] holds here too.
pub(crate) fn read_package_from<R: io::Read>(source: R) -> Result<Package, PackError> {
    read_package_observing(source, None, &mut |_| Ok(()))
}

/// [`read_package_from`], adding the bytes of the content of every regular
/// member to `content` as they come out of the decompressor, and calling
/// `on_file` with the path of every regular member that is not a hard link as
/// soon as its content has been read. An error of `on_file` ends the read.
pub(crate) fn read_package_observing<R: io::Read>(
    source: R,
    content: Option<&Counter>,
    on_file: &mut dyn FnMut(&RelPath) -> Result<(), PackError>,
) -> Result<Package, PackError> {
    let decoder = zstd::decoder(source).map_err(damaged)?;
    let count = Rc::new(Cell::new(0));
    let mut archive = tar::Archive::new(Counted {
        inner: decoder,
        count: Rc::clone(&count),
    });
    // Where the data of the last member ends, padded to a whole block.
    let mut members_end = 0;
    let mut container = None;
    let mut entries = Vec::new();
    let mut seen: HashMap<RelPath, EntryType> = HashMap::new();
    // What the files read so far hold, for the hard links that name them.
    let mut hashes: HashMap<RelPath, String> = HashMap::new();
    for member in archive.entries().map_err(damaged)? {
        let mut member = member.map_err(damaged)?;
        let path = member.path_bytes().into_owned();
        let kind = member.header().entry_type();
        let mode = member.header().mode().map_err(damaged)? & 0o7777;
        let mtime = modification_time(member.header())?;
        let size = member.size();
        members_end = member.raw_file_position() + size.next_multiple_of(512);
        let link = member.link_name_bytes().map(|name| name.into_owned());
        let Some((name, _)) = container.as_ref() else {
            if kind != EntryType::Directory {
                return Err(PackError::Damaged(
                    "the first member is not a directory".into(),
                ));
            }
            let name = path.strip_suffix(b"/").unwrap_or(&path).to_vec();
            Name::from_bytes(name.clone()).map_err(|error| {
                PackError::Damaged(format!(
                    "the top-level directory has no usable name: {error}"
                ))
            })?;
            container = Some((name, Container { mode, mtime }));
            continue;
        };
        let shown = || Shown(&path[..]).to_string();
        let outside = || {
            PackError::Damaged(format!(
                "`{}` lies outside the top-level directory",
                shown()
            ))
        };
        let mut below = within(name, &path).ok_or_else(outside)?;
        if kind == EntryType::Directory {
            below = below.strip_suffix(b"/").unwrap_or(below);
        }
        let path = RelPath::from_bytes(below.to_vec())
            .map_err(|error| PackError::Damaged(format!("`{}`: {error}", shown())))?;
        if path.is_root() || seen.insert(path.clone(), kind).is_some() {
            return Err(PackError::Damaged(format!("`{}` occurs twice", shown())));
        }
        let (kind, link, sha256) = match kind {
            EntryType::Directory => (ListingType::Dir, None, None),
            EntryType::Regular => {
                let mut reader = HashingReader::new(Metered::new(&mut member, content));
                io::copy(&mut reader, &mut io::sink()).map_err(damaged)?;
                let sha256 = reader.sha256();
                on_file(&path)?;
                hashes.insert(path.clone(), sha256.clone());
                (ListingType::File, None, Some(sha256))
            }
            // The crate reports an empty link name as none, and macOS
            // allows a symlink with an empty target.
            EntryType::Symlink => (
                ListingType::Symlink,
                Some(LinkTarget::from_bytes(link.unwrap_or_default())),
                None,
            ),
            EntryType::Link => {
                let target = link
                    .as_deref()
                    .and_then(|target| within(name, target))
                    .and_then(|target| RelPath::from_bytes(target.to_vec()).ok())
                    .filter(|target| seen.get(target) == Some(&EntryType::Regular))
                    .ok_or_else(|| {
                        PackError::Damaged(format!(
                            "the hard link `{}` does not name a file that comes before it",
                            shown()
                        ))
                    })?;
                let sha256 = hashes.get(&target).cloned();
                (
                    ListingType::Hardlink,
                    Some(LinkTarget::from_bytes(target.as_bytes().to_vec())),
                    sha256,
                )
            }
            _ => {
                return Err(PackError::Damaged(format!(
                    "`{}` has a member type that is not file, directory, symlink or hard link",
                    shown()
                )));
            }
        };
        entries.push(ListingEntry {
            path,
            kind,
            size,
            mtime,
            mode,
            link,
            sha256,
        });
    }
    let (_, container) =
        container.ok_or_else(|| PackError::Damaged("the package is empty".into()))?;
    finish_stream(archive.into_inner())?;
    if count.get() < members_end + END_MARKER {
        return Err(PackError::Damaged(
            "the archive ends without its end-of-archive marker; the file may be cut".into(),
        ));
    }
    Ok(Package { container, entries })
}

/// Reads what is left of the stream after the tar reader stopped. The zstd
/// decoder checks the content checksum only when it reaches the end of the
/// frame, and the tar reader stops at its end marker, before the padding
/// and the checksum. Anything after the marker that is not padding is data
/// the members do not account for.
fn finish_stream(mut rest: impl io::Read) -> Result<(), PackError> {
    let mut buffer = [0u8; 8192];
    loop {
        let count = rest.read(&mut buffer).map_err(damaged)?;
        if count == 0 {
            return Ok(());
        }
        if buffer[..count].iter().any(|byte| *byte != 0) {
            return Err(PackError::Damaged(
                "the stream holds data after the end of the archive".into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::package::{RawMember, compress, on_disk, pack_piece, raw_tar};
    use crate::testing::tree::Tree;

    /// A piece with every kind of member and a 1969 file.
    fn mixed_piece() -> Tree {
        let mut tree = Tree::new();
        tree.file_with("proj/src/main.rs", "fn main() {}", 0o600, 1_000_000_000);
        tree.file("proj/notes.txt", "hello");
        tree.symlink("proj/src/link", "main.rs");
        tree.hard_link("proj/notes.txt", "proj/src/notes-copy.txt");
        tree.dir("proj/empty");
        let old = tree.file("proj/old", "x");
        filetime::set_file_mtime(&old, filetime::FileTime::from_unix_time(-5, 0))
            .expect("a time before 1970 can be set");
        tree
    }

    #[test]
    fn a_fresh_pack_lists_as_it_was_packed() {
        let packaged = pack_piece(&mixed_piece(), "proj");
        let (_dir, path) = on_disk(&packaged.bytes);
        let listed = list(&path).expect("the package lists");
        assert_eq!(listed, packaged.packed.listing);
    }

    #[test]
    fn a_listed_hard_link_carries_the_hash_of_the_file_it_links_to() {
        let packaged = pack_piece(&mixed_piece(), "proj");
        let (_dir, path) = on_disk(&packaged.bytes);
        let listed = list(&path).expect("the package lists");
        let hash = |name: &str| {
            listed
                .iter()
                .find(|entry| entry.path.to_string() == name)
                .and_then(|entry| entry.sha256.clone())
        };
        assert!(hash("notes.txt").is_some());
        assert_eq!(hash("src/notes-copy.txt"), hash("notes.txt"));
    }

    fn damaged_by(bytes: &[u8]) -> String {
        let (_dir, path) = on_disk(bytes);
        match list(&path).expect_err("the package is damaged") {
            PackError::Damaged(reason) => reason,
            other => panic!("expected Damaged, got {other:?}"),
        }
    }

    #[test]
    fn a_stream_cut_in_the_middle_is_damaged() {
        let packaged = pack_piece(&mixed_piece(), "proj");
        damaged_by(&packaged.bytes[..packaged.bytes.len() / 2]);
    }

    #[test]
    fn a_stream_cut_before_its_checksum_is_damaged() {
        // The tar layer has seen every member by then, so only reading the
        // frame to its end shows the cut.
        let packaged = pack_piece(&mixed_piece(), "proj");
        damaged_by(&packaged.bytes[..packaged.bytes.len() - 4]);
    }

    #[test]
    fn a_flipped_byte_inside_the_checksum_is_damaged() {
        let mut packaged = pack_piece(&mixed_piece(), "proj");
        *packaged.bytes.last_mut().expect("a stream") ^= 0xff;
        damaged_by(&packaged.bytes);
    }

    #[test]
    fn bytes_after_the_end_of_the_archive_are_damaged() {
        let mut tar = raw_tar(&[RawMember::dir(b"proj/")]);
        tar.extend_from_slice(&[b'x'; 512]);
        damaged_by(&compress(&tar));
    }

    /// The reason a hand-built tar is refused with.
    fn refused(members: &[RawMember<'_>]) -> String {
        damaged_by(&compress(&raw_tar(members)))
    }

    #[test]
    fn a_second_member_with_the_same_path_is_damaged() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::file(b"proj/a", b"1"),
            RawMember::file(b"proj/a", b"2"),
        ]);
        assert!(reason.contains("twice"), "{reason}");
    }

    #[test]
    fn a_member_outside_the_container_is_damaged() {
        let reason = refused(&[RawMember::dir(b"proj/"), RawMember::file(b"other/a", b"1")]);
        assert!(reason.contains("outside"), "{reason}");
    }

    #[test]
    fn a_member_name_is_shown_by_the_rule_for_names_in_the_reason() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::file(b"other/a\n100%\xff", b"1"),
        ]);
        assert!(reason.contains("`other/a%0A100%25%FF`"), "{reason:?}");
    }

    #[test]
    fn a_path_with_dot_dot_is_damaged() {
        refused(&[
            RawMember::dir(b"proj/"),
            RawMember::file(b"proj/../a", b"1"),
        ]);
    }

    #[test]
    fn a_first_member_that_is_not_a_directory_is_damaged() {
        let reason = refused(&[RawMember::file(b"proj", b"1")]);
        assert!(reason.contains("directory"), "{reason}");
    }

    #[test]
    fn a_hard_link_to_a_path_the_archive_does_not_hold_is_damaged() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::link(b"proj/b", b'1', b"proj/a"),
        ]);
        assert!(reason.contains("hard link"), "{reason}");
    }

    #[test]
    fn a_hard_link_that_comes_before_its_target_is_damaged() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::link(b"proj/a", b'1', b"proj/b"),
            RawMember::file(b"proj/b", b"1"),
        ]);
        assert!(reason.contains("hard link"), "{reason}");
    }

    #[test]
    fn a_hard_link_to_a_directory_is_damaged() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::dir(b"proj/d/"),
            RawMember::link(b"proj/b", b'1', b"proj/d"),
        ]);
        assert!(reason.contains("hard link"), "{reason}");
    }

    #[test]
    fn a_hard_link_to_a_path_outside_the_container_is_damaged() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::link(b"proj/b", b'1', b"other/a"),
        ]);
        assert!(reason.contains("hard link"), "{reason}");
    }

    #[test]
    fn a_fifo_member_is_damaged() {
        let reason = refused(&[
            RawMember::dir(b"proj/"),
            RawMember::link(b"proj/queue", b'6', b""),
        ]);
        assert!(reason.contains("type"), "{reason}");
    }

    #[test]
    fn a_symlink_with_an_empty_target_lists_with_that_target() {
        let (_dir, path) = on_disk(&compress(&raw_tar(&[
            RawMember::dir(b"proj/"),
            RawMember::link(b"proj/l", b'2', b""),
        ])));
        let listed = list(&path).expect("lists");
        assert_eq!(listed[0].link, Some(LinkTarget::from_bytes(Vec::new())));
    }

    #[test]
    fn a_missing_package_file_is_an_open_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let error = list(&dir.path().join("none")).expect_err("no such file");
        assert!(matches!(error, PackError::OpenPackage(_)), "{error:?}");
    }

    #[test]
    fn listed_modes_keep_only_permission_bits() {
        let mut file = RawMember::file(b"proj/a", b"1");
        file.mode = 0o100644;
        let (_dir, path) = on_disk(&compress(&raw_tar(&[RawMember::dir(b"proj/"), file])));
        assert_eq!(list(&path).expect("lists")[0].mode, 0o644);
    }

    #[test]
    fn a_negative_modification_time_is_listed_as_zero() {
        // GNU tar writes -5 as a base-256 number: 0xff in the first byte
        // and the two's complement over the whole field.
        let mut minus_five = [0xff; 12];
        minus_five[11] = 0xfb;
        let (_dir, path) = on_disk(&compress(&tar_with_time_field(minus_five)));
        assert_eq!(list(&path).expect("lists")[0].mtime, 0);
    }

    /// A tar of a directory and a file whose modification time field is
    /// `field`.
    fn tar_with_time_field(field: [u8; 12]) -> Vec<u8> {
        let mut tar = raw_tar(&[RawMember::dir(b"proj/"), RawMember::file(b"proj/a", b"1")]);
        tar[512 + 136..512 + 148].copy_from_slice(&field);
        // The checksum covers the field, so it is written again.
        let mut header = tar::Header::new_old();
        header.as_mut_bytes().copy_from_slice(&tar[512..1024]);
        header.set_cksum();
        tar[512..1024].copy_from_slice(header.as_bytes());
        tar
    }

    #[test]
    fn a_positive_base_256_modification_time_is_read() {
        let mut field = [0; 12];
        field[0] = 0x80;
        field[11] = 7;
        let (_dir, path) = on_disk(&compress(&tar_with_time_field(field)));
        assert_eq!(list(&path).expect("lists")[0].mtime, 7);
    }

    #[test]
    fn a_modification_time_with_bit_63_set_is_damaged() {
        let mut field = [0; 12];
        field[0] = 0x80;
        field[4] = 0x80;
        let reason = damaged_by(&compress(&tar_with_time_field(field)));
        assert!(reason.contains("63 bits"), "{reason}");
    }

    #[test]
    fn a_modification_time_beyond_63_bits_is_damaged() {
        let mut field = [0; 12];
        field[0] = 0x80;
        field[3] = 1;
        let reason = damaged_by(&compress(&tar_with_time_field(field)));
        assert!(reason.contains("63 bits"), "{reason}");
    }

    #[test]
    fn a_tar_without_members_is_damaged() {
        let reason = damaged_by(&compress(&raw_tar(&[])));
        assert!(reason.contains("empty"), "{reason}");
    }

    #[test]
    fn a_top_level_directory_without_a_name_is_damaged() {
        let reason = refused(&[RawMember::dir(b"/")]);
        assert!(reason.contains("name"), "{reason}");
    }

    #[test]
    fn a_file_that_is_not_zstd_is_damaged() {
        damaged_by(b"this is not a compressed stream");
    }

    /// The tar of a directory and a file, without its end blocks and with
    /// `zero_blocks` zero blocks in their place.
    fn tar_ending_in(zero_blocks: usize) -> Vec<u8> {
        let mut tar = raw_tar(&[RawMember::dir(b"proj/"), RawMember::file(b"proj/a", b"1")]);
        tar.truncate(tar.len() - 1024);
        tar.resize(tar.len() + 512 * zero_blocks, 0);
        tar
    }

    #[test]
    fn a_tar_without_its_end_blocks_is_damaged() {
        let reason = damaged_by(&compress(&tar_ending_in(0)));
        assert!(reason.contains("end-of-archive"), "{reason}");
    }

    #[test]
    fn a_tar_with_one_end_block_is_damaged() {
        let reason = damaged_by(&compress(&tar_ending_in(1)));
        assert!(reason.contains("end-of-archive"), "{reason}");
    }

    #[test]
    fn one_end_block_and_zeros_short_of_a_second_are_damaged() {
        let mut tar = tar_ending_in(1);
        tar.resize(tar.len() + 511, 0);
        let reason = damaged_by(&compress(&tar));
        assert!(reason.contains("end-of-archive"), "{reason}");
    }

    #[test]
    fn a_tar_with_two_end_blocks_is_read() {
        let (_dir, path) = on_disk(&compress(&tar_ending_in(2)));
        assert_eq!(list(&path).expect("lists").len(), 1);
    }

    #[test]
    fn a_two_frame_archive_cut_at_the_frame_boundary_is_damaged() {
        let mut tar = raw_tar(&[RawMember::dir(b"proj/"), RawMember::file(b"proj/a", b"1")]);
        tar.truncate(tar.len() - 1024);
        // The first frame ends where the first member ends.
        let (first, second) = tar.split_at(512);
        let both = [compress(first), compress(second)].concat();
        let reason = damaged_by(&both);
        assert!(reason.contains("end-of-archive"), "{reason}");
    }

    #[test]
    fn a_package_antiq_packs_ends_with_the_end_blocks() {
        use std::io::Read;
        let packaged = pack_piece(&mixed_piece(), "proj");
        let mut tar = Vec::new();
        crate::archive::zstd::decoder(&packaged.bytes[..])
            .expect("a decoder")
            .read_to_end(&mut tar)
            .expect("decodes");
        assert!(tar.len() >= 1024);
        assert!(
            tar[tar.len() - 1024..].iter().all(|byte| *byte == 0),
            "the last two blocks are zeros"
        );
    }

    #[test]
    fn zeros_after_the_end_of_the_archive_are_accepted() {
        let mut tar = raw_tar(&[RawMember::dir(b"proj/")]);
        tar.resize(10240, 0);
        let (_dir, path) = on_disk(&compress(&tar));
        assert_eq!(list(&path).expect("the package lists"), []);
    }
}
