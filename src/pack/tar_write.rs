// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tar writer.
//!
//! Every header is built here instead of by the `tar` crate's helpers, so
//! that the archive holds permission bits and real modification times
//! (ADR 0050) and the listing can describe exactly what was written.

use super::hashing::HashingReader;
use super::{PackError, PackNote};
use crate::fsx::tree::TreeRoot;
use crate::listing::{LinkTarget, ListingEntry, ListingType};
use crate::relpath::{Name, RelPath};
use crate::tree::{EntryKind, TreeEntry};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use tar::{Builder, EntryType, Header};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// The permission bits of a mode, including set-user-id, set-group-id and
/// sticky.
const PERMISSION_BITS: u32 = 0o7777;

/// Bytes a link name takes in a tar header. Longer names go into a GNU
/// long link record before the header.
const LINK_NAME_FIELD: usize = 100;

/// What a member of the archive is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberKind {
    File,
    Dir,
    Symlink,
    Hardlink,
}

/// The modification time as the header stores it: whole seconds, rounded
/// down, and 0 for a time before 1970. The flag says the time was clamped.
fn stored_mtime(mtime_ns: i128) -> (u64, bool) {
    let seconds = mtime_ns.div_euclid(NANOS_PER_SECOND);
    if seconds < 0 {
        (0, true)
    } else {
        (u64::try_from(seconds).unwrap_or(u64::MAX), false)
    }
}

/// The header of a member, and whether its modification time was clamped.
/// The path and the link name are set by the caller.
fn header(entry: &TreeEntry, kind: MemberKind, size: u64) -> (Header, bool) {
    let (mtime, clamped) = stored_mtime(entry.mtime_ns);
    let mut header = Header::new_gnu();
    header.set_entry_type(match kind {
        MemberKind::File => EntryType::Regular,
        MemberKind::Dir => EntryType::Directory,
        MemberKind::Symlink => EntryType::Symlink,
        MemberKind::Hardlink => EntryType::Link,
    });
    header.set_mode(entry.mode & PERMISSION_BITS);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(mtime);
    header.set_size(size);
    (header, clamped)
}

/// The GNU record that carries a link name too long for the header. The
/// crate's own `append_link` would first run a shorter name through its
/// path normalisation, which rewrites `a//b` as `a/b`.
fn long_link_header(target_len: usize) -> Header {
    const NAME: &[u8] = b"././@LongLink";
    let mut header = Header::new_gnu();
    header.as_old_mut().name[..NAME.len()].copy_from_slice(NAME);
    header.set_mode(0o644);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_size(target_len as u64 + 1);
    header.set_entry_type(EntryType::new(b'K'));
    header.set_cksum();
    header
}

/// Yields exactly the number of bytes the header announces. `append_data`
/// copies whatever it is given and pads to a whole block, so a file that
/// shrank or grew since the walk would otherwise corrupt the stream.
struct ExactReader<R> {
    inner: R,
    remaining: u64,
    short: bool,
    failed: bool,
}

impl<R: Read> ExactReader<R> {
    fn new(inner: R, size: u64) -> ExactReader<R> {
        ExactReader {
            inner,
            remaining: size,
            short: false,
            failed: false,
        }
    }
}

impl<R: Read> Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let wanted = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        if wanted == 0 {
            return Ok(0);
        }
        let count = if self.short {
            0
        } else {
            match self.inner.read(&mut buf[..wanted]) {
                Ok(count) => count,
                Err(error) => {
                    // An interrupted read is retried by the caller and is
                    // not a failure of the file.
                    self.failed = error.kind() != io::ErrorKind::Interrupted;
                    return Err(error);
                }
            }
        };
        if count == 0 {
            // The file ended early: the rest of the member is zeros.
            self.short = true;
            buf[..wanted].fill(0);
            self.remaining -= wanted as u64;
            return Ok(wanted);
        }
        self.remaining -= count as u64;
        Ok(count)
    }
}

/// Opens the content of a file entry.
pub type ContentSource<'a> = dyn FnMut(&TreeEntry) -> io::Result<Box<dyn Read>> + 'a;

/// Writes the members of one archive and collects the listing and the
/// notes that go with them.
pub struct TarWriter<W: Write> {
    builder: Builder<W>,
    container: Name,
    listing: Vec<ListingEntry>,
    notes: Vec<PackNote>,
    /// The first path of each file with more than one link, by device and
    /// inode.
    first_link: HashMap<(u64, u64), RelPath>,
}

impl<W: Write> TarWriter<W> {
    /// `container` names the archive's top-level directory.
    pub fn new(out: W, container: Name) -> TarWriter<W> {
        TarWriter {
            builder: Builder::new(out),
            container,
            listing: Vec::new(),
            notes: Vec::new(),
            first_link: HashMap::new(),
        }
    }

    /// The path of an entry inside the archive: below the container.
    fn archive_path(&self, path: &RelPath) -> Vec<u8> {
        let mut bytes = self.container.as_bytes().to_vec();
        if !path.is_root() {
            bytes.push(b'/');
            bytes.extend_from_slice(path.as_bytes());
        }
        bytes
    }

    fn note_path(&self, path: &RelPath) -> RelPath {
        RelPath::from_bytes(self.archive_path(path))
            .expect("a container name and a path make a valid path")
    }

    /// Writes the next entry. Entries come in path order, parents first.
    /// `content` opens the files the entries name.
    pub fn add(
        &mut self,
        entry: &TreeEntry,
        content: &mut ContentSource<'_>,
    ) -> Result<(), PackError> {
        let mut member = self.archive_path(&entry.path);
        let kind = match entry.kind {
            EntryKind::Directory => {
                // The trailing slash is how tar spells a directory.
                member.push(b'/');
                MemberKind::Dir
            }
            EntryKind::File => {
                let first = (entry.nlink > 1)
                    .then(|| self.first_link.get(&(entry.device, entry.inode)).cloned())
                    .flatten();
                match first {
                    Some(first) => return self.add_hard_link(entry, &member, &first),
                    None => {
                        if entry.nlink > 1 {
                            self.first_link
                                .insert((entry.device, entry.inode), entry.path.clone());
                        }
                        return self.add_file(entry, &member, content);
                    }
                }
            }
            EntryKind::Symlink => return self.add_symlink(entry, &member),
            kind => {
                self.notes.push(PackNote::SpecialFileSkipped {
                    path: self.note_path(&entry.path),
                    kind,
                });
                return Ok(());
            }
        };
        let (mut header, clamped) = header(entry, kind, 0);
        self.note_clamp(entry, clamped);
        self.builder
            .append_data(&mut header, member_path(&member), io::empty())
            .map_err(PackError::Write)?;
        // The top-level directory has no line in the listing.
        if !entry.path.is_root() {
            self.record(entry, ListingType::Dir, 0, None, None);
        }
        Ok(())
    }

    fn note_clamp(&mut self, entry: &TreeEntry, clamped: bool) {
        if clamped {
            self.notes.push(PackNote::TimeClamped {
                path: self.note_path(&entry.path),
            });
        }
    }

    /// Appends the listing line of a member whose header was written with
    /// the entry's mode and modification time.
    fn record(
        &mut self,
        entry: &TreeEntry,
        kind: ListingType,
        size: u64,
        link: Option<LinkTarget>,
        sha256: Option<String>,
    ) {
        let (mtime, _) = stored_mtime(entry.mtime_ns);
        self.listing.push(ListingEntry {
            path: entry.path.clone(),
            kind,
            size,
            mtime: i64::try_from(mtime).unwrap_or(i64::MAX),
            mode: entry.mode & PERMISSION_BITS,
            link,
            sha256,
        });
    }

    fn add_file(
        &mut self,
        entry: &TreeEntry,
        member: &[u8],
        content: &mut ContentSource<'_>,
    ) -> Result<(), PackError> {
        let (mut header, clamped) = header(entry, MemberKind::File, entry.size);
        self.note_clamp(entry, clamped);
        let file = content(entry).map_err(|source| PackError::Read {
            path: entry.path.clone(),
            source,
        })?;
        let mut exact = ExactReader::new(file, entry.size);
        let mut hashing = HashingReader::new(&mut exact);
        let written = self
            .builder
            .append_data(&mut header, member_path(member), &mut hashing);
        let sha256 = hashing.sha256();
        if let Err(error) = written {
            return Err(if exact.failed {
                PackError::Read {
                    path: entry.path.clone(),
                    source: error,
                }
            } else {
                PackError::Write(error)
            });
        }
        if exact.short {
            self.notes.push(PackNote::FileChanged {
                path: self.note_path(&entry.path),
            });
        }
        self.record(entry, ListingType::File, entry.size, None, Some(sha256));
        Ok(())
    }

    fn add_hard_link(
        &mut self,
        entry: &TreeEntry,
        member: &[u8],
        first: &RelPath,
    ) -> Result<(), PackError> {
        let (mut header, clamped) = header(entry, MemberKind::Hardlink, 0);
        self.note_clamp(entry, clamped);
        // The target is a path the container and a `RelPath` made, which
        // the crate's path handling reproduces byte for byte.
        let target = self.archive_path(first);
        self.builder
            .append_link(&mut header, member_path(member), member_path(&target))
            .map_err(PackError::Write)?;
        let link = LinkTarget::from_bytes(first.as_bytes().to_vec());
        self.record(entry, ListingType::Hardlink, 0, Some(link), None);
        Ok(())
    }

    fn add_symlink(&mut self, entry: &TreeEntry, member: &[u8]) -> Result<(), PackError> {
        let target = entry.link_target.clone().ok_or_else(|| PackError::Read {
            path: entry.path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidData, "a symlink without a target"),
        })?;
        let (mut header, clamped) = header(entry, MemberKind::Symlink, 0);
        self.note_clamp(entry, clamped);
        self.write_symlink(&mut header, member, &target)
            .map_err(PackError::Write)?;
        self.record(
            entry,
            ListingType::Symlink,
            0,
            Some(LinkTarget::from_bytes(target)),
            None,
        );
        Ok(())
    }

    /// A symlink target is any bytes and is stored as they are. The
    /// crate's `append_link` would run a target below 100 bytes through
    /// its path normalisation (`a//b` becomes `a/b`, `/` becomes `//`).
    fn write_symlink(
        &mut self,
        header: &mut Header,
        member: &[u8],
        target: &[u8],
    ) -> io::Result<()> {
        if target.len() > LINK_NAME_FIELD {
            let data = target.chain(&[0u8][..]);
            self.builder.append(&long_link_header(target.len()), data)?;
            header.set_link_name_literal(&target[..LINK_NAME_FIELD])?;
        } else {
            header.set_link_name_literal(target)?;
        }
        self.builder
            .append_data(header, member_path(member), io::empty())
    }

    /// Ends the archive and returns the writer, the listing and the notes.
    pub fn finish(self) -> Result<(W, Vec<ListingEntry>, Vec<PackNote>), PackError> {
        let out = self.builder.into_inner().map_err(PackError::Write)?;
        Ok((out, self.listing, self.notes))
    }
}

/// Opens the files of a piece through the directory handles of the path
/// being walked. Entries come in path order, so the handles of the
/// directories above one file are mostly those of the file before it, and
/// no open has to walk down from the root again (which `TreeRoot` does for
/// every call made on a root).
pub struct DirectoryStack<'a> {
    root: &'a TreeRoot,
    /// The names of the directories held open, from the top.
    names: Vec<Vec<u8>>,
    /// The open directory for each name in `names`.
    handles: Vec<TreeRoot>,
}

impl<'a> DirectoryStack<'a> {
    pub fn new(root: &'a TreeRoot) -> DirectoryStack<'a> {
        DirectoryStack {
            root,
            names: Vec::new(),
            handles: Vec::new(),
        }
    }

    /// Opens the regular file the entry names.
    pub fn open(&mut self, entry: &TreeEntry) -> io::Result<Box<dyn Read>> {
        let components: Vec<&[u8]> = entry.path.components().collect();
        let (file_name, directories) = components
            .split_last()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the root is not a file"))?;

        // Keep the handles of the directories this path shares with the
        // one before it.
        let shared = self
            .names
            .iter()
            .zip(directories)
            .take_while(|(held, wanted)| held.as_slice() == **wanted)
            .count();
        self.names.truncate(shared);
        self.handles.truncate(shared);
        for directory in &directories[shared..] {
            let parent = self.handles.last().unwrap_or(self.root);
            let sub = parent.sub(&single(directory))?;
            self.names.push(directory.to_vec());
            self.handles.push(sub);
        }

        let parent = self.handles.last().unwrap_or(self.root);
        Ok(Box::new(parent.open_read(&single(file_name))?))
    }
}

/// The path of one component, which the path it came from already
/// proved valid.
fn single(component: &[u8]) -> RelPath {
    let name =
        Name::from_bytes(component.to_vec()).expect("a component of a valid path is a valid name");
    RelPath::root().join(&name)
}

fn member_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn entry(path_text: &str, kind: EntryKind) -> TreeEntry {
        TreeEntry {
            path: path(path_text),
            kind,
            size: 0,
            allocated: 0,
            mtime_ns: 1_700_000_000 * NANOS_PER_SECOND,
            ctime_ns: 0,
            inode: 1,
            device: 1,
            mode: match kind {
                EntryKind::Directory => 0o040755,
                EntryKind::Symlink => 0o120777,
                _ => 0o100644,
            },
            nlink: 1,
            link_target: None,
            birth_ns: None,
        }
    }

    fn file(path_text: &str, content: &[u8]) -> (TreeEntry, Vec<u8>) {
        let mut file = entry(path_text, EntryKind::File);
        file.size = content.len() as u64;
        (file, content.to_vec())
    }

    fn symlink(path_text: &str, target: &[u8]) -> TreeEntry {
        let mut link = entry(path_text, EntryKind::Symlink);
        link.link_target = Some(target.to_vec());
        link
    }

    fn container() -> Name {
        Name::from_bytes(b"proj".to_vec()).expect("a valid name")
    }

    /// What a test reads back from the tar of a writer.
    #[derive(Debug, PartialEq)]
    struct Member {
        path: Vec<u8>,
        kind: EntryType,
        size: u64,
        mode: u32,
        mtime: u64,
        uid: u64,
        gid: u64,
        user: Vec<u8>,
        group: Vec<u8>,
        link: Option<Vec<u8>>,
        data: Vec<u8>,
    }

    fn members(tar: &[u8]) -> Vec<Member> {
        let mut archive = tar::Archive::new(tar);
        archive
            .entries()
            .expect("a tar stream")
            .map(|entry| {
                let mut entry = entry.expect("a readable member");
                let mut data = Vec::new();
                entry.read_to_end(&mut data).expect("member data");
                let header = entry.header();
                Member {
                    path: entry.path_bytes().into_owned(),
                    kind: header.entry_type(),
                    size: header.size().expect("a size"),
                    mode: header.mode().expect("a mode"),
                    mtime: header.mtime().expect("a time"),
                    uid: header.uid().expect("a uid"),
                    gid: header.gid().expect("a gid"),
                    user: header.username_bytes().unwrap_or_default().to_vec(),
                    group: header.groupname_bytes().unwrap_or_default().to_vec(),
                    link: entry.link_name_bytes().map(|name| name.into_owned()),
                    data,
                }
            })
            .collect()
    }

    /// Adds the entries in order, with the content of files from `files`.
    fn pack(entries: &[(TreeEntry, Vec<u8>)]) -> (Vec<u8>, Vec<ListingEntry>, Vec<PackNote>) {
        let mut writer = TarWriter::new(Vec::new(), container());
        let mut content = |entry: &TreeEntry| -> io::Result<Box<dyn Read>> {
            let (_, data) = entries
                .iter()
                .find(|(candidate, _)| candidate.path == entry.path)
                .expect("the test lists the file");
            Ok(Box::new(Cursor::new(data.clone())))
        };
        for (entry, _) in entries {
            writer
                .add(entry, &mut content)
                .expect("the entry is written");
        }
        writer.finish().expect("the archive ends")
    }

    fn plain(entry: TreeEntry) -> (TreeEntry, Vec<u8>) {
        (entry, Vec::new())
    }

    #[test]
    fn a_header_keeps_permission_bits_and_drops_owners() {
        let mut regular = entry("a", EntryKind::File);
        regular.mode = 0o100600;
        let (header, _) = header(&regular, MemberKind::File, 5);
        assert_eq!(header.mode().expect("a mode"), 0o600);
        let mut special = entry("b", EntryKind::File);
        special.mode = 0o107755;
        let (header, _) = super::header(&special, MemberKind::File, 5);
        assert_eq!(header.mode().expect("a mode"), 0o7755);
        assert_eq!(
            (header.uid().expect("uid"), header.gid().expect("gid")),
            (0, 0)
        );
        assert_eq!(header.username_bytes(), Some(&b""[..]));
        assert_eq!(header.groupname_bytes(), Some(&b""[..]));
        assert!(header.as_gnu().is_some(), "the format is GNU");
        assert_eq!(header.size().expect("a size"), 5);
    }

    #[test]
    fn the_modification_time_is_the_floor_in_whole_seconds() {
        let mut touched = entry("a", EntryKind::File);
        touched.mtime_ns = 1_999_999_999;
        let (header, clamped) = header(&touched, MemberKind::File, 0);
        assert_eq!(header.mtime().expect("a time"), 1);
        assert!(!clamped);
    }

    #[test]
    fn a_time_before_1970_is_stored_as_zero_and_flagged() {
        for mtime_ns in [-1, -5 * NANOS_PER_SECOND] {
            let mut old = entry("a", EntryKind::File);
            old.mtime_ns = mtime_ns;
            let (header, clamped) = header(&old, MemberKind::File, 0);
            assert_eq!(header.mtime().expect("a time"), 0, "{mtime_ns} ns");
            assert!(clamped, "{mtime_ns} ns");
        }
        let mut epoch = entry("a", EntryKind::File);
        epoch.mtime_ns = 0;
        assert!(!header(&epoch, MemberKind::File, 0).1);
    }

    #[test]
    fn a_size_past_8_gib_is_stored_and_read_back() {
        let huge = 9 * 1024 * 1024 * 1024;
        let (header, _) = header(&entry("a", EntryKind::File), MemberKind::File, huge);
        assert_eq!(header.size().expect("a size"), huge);
    }

    #[test]
    fn the_exact_reader_pads_a_short_read_and_marks_it() {
        let mut reader = ExactReader::new(&b"abc"[..], 8);
        let mut data = Vec::new();
        reader.read_to_end(&mut data).expect("reads");
        assert_eq!(data, b"abc\0\0\0\0\0");
        assert!(reader.short);
        assert!(!reader.failed);
    }

    #[test]
    fn the_exact_reader_keeps_padding_across_small_reads() {
        let mut reader = ExactReader::new(&b"ab"[..], 6);
        let mut data = Vec::new();
        let mut chunk = [9u8; 2];
        loop {
            let count = reader.read(&mut chunk).expect("reads");
            if count == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..count]);
        }
        assert_eq!(data, b"ab\0\0\0\0");
    }

    #[test]
    fn the_exact_reader_stops_at_the_header_size() {
        let mut reader = ExactReader::new(&b"abcdef"[..], 4);
        let mut data = Vec::new();
        reader.read_to_end(&mut data).expect("reads");
        assert_eq!(data, b"abcd");
        assert!(!reader.short);
    }

    #[test]
    fn the_exact_reader_records_an_inner_error() {
        struct Broken(io::ErrorKind);
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(self.0))
            }
        }
        let mut reader = ExactReader::new(Broken(io::ErrorKind::PermissionDenied), 4);
        let error = reader
            .read(&mut [0; 4])
            .expect_err("the inner reader fails");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(reader.failed);

        let mut reader = ExactReader::new(Broken(io::ErrorKind::Interrupted), 4);
        reader
            .read(&mut [0; 4])
            .expect_err("the inner reader is interrupted");
        assert!(!reader.failed, "an interruption is retried, not a failure");
    }

    #[test]
    fn special_kinds_write_nothing_and_are_noted() {
        let kinds = [
            EntryKind::Fifo,
            EntryKind::Socket,
            EntryKind::CharDevice,
            EntryKind::BlockDevice,
            EntryKind::Unknown,
        ];
        let entries: Vec<_> = kinds
            .iter()
            .map(|kind| plain(entry("special", *kind)))
            .collect();
        let (tar, listing, notes) = pack(&entries);
        assert!(members(&tar).is_empty());
        assert!(listing.is_empty());
        let noted: Vec<_> = notes.iter().map(PackNote::to_string).collect();
        assert_eq!(
            noted,
            [
                "`proj/special` is a fifo and is not archived",
                "`proj/special` is a socket and is not archived",
                "`proj/special` is a character device and is not archived",
                "`proj/special` is a block device and is not archived",
                "`proj/special` is a file of unknown type and is not archived",
            ]
        );
    }

    #[test]
    fn a_file_keeps_its_mode_with_owner_zero() {
        let mut secret = entry("secret", EntryKind::File);
        secret.mode = 0o100600;
        secret.size = 3;
        let (tar, _, _) = pack(&[(secret, b"abc".to_vec())]);
        let [member] = <[Member; 1]>::try_from(members(&tar)).expect("one member");
        assert_eq!(member.path, b"proj/secret");
        assert_eq!(member.kind, EntryType::Regular);
        assert_eq!((member.mode, member.uid, member.gid), (0o600, 0, 0));
        assert!(member.user.is_empty() && member.group.is_empty());
        assert_eq!(member.data, b"abc");
        assert_eq!(member.mtime, 1_700_000_000);
    }

    #[test]
    fn the_container_comes_first_and_members_follow_path_bytes_without_the_slash() {
        let mut top = entry("", EntryKind::Directory);
        top.mode = 0o040700;
        top.mtime_ns = 5 * NANOS_PER_SECOND;
        let (tar, listing, _) = pack(&[
            plain(top),
            plain(entry("dir", EntryKind::Directory)),
            file("dir-y", b"y"),
            file("dir/x", b"x"),
        ]);
        let members = members(&tar);
        let paths: Vec<_> = members.iter().map(|m| m.path.clone()).collect();
        assert_eq!(
            paths,
            [&b"proj/"[..], b"proj/dir/", b"proj/dir-y", b"proj/dir/x"]
        );
        assert_eq!(members[0].kind, EntryType::Directory);
        assert_eq!((members[0].mode, members[0].mtime), (0o700, 5));
        let listed: Vec<_> = listing.iter().map(|e| e.path.to_string()).collect();
        assert_eq!(
            listed,
            ["dir", "dir-y", "dir/x"],
            "no line for the container"
        );
    }

    #[test]
    fn a_hard_link_reads_back_as_a_link_to_the_first_path() {
        let mut first = entry("a", EntryKind::File);
        first.nlink = 2;
        first.size = 3;
        let mut second = entry("b", EntryKind::File);
        second.nlink = 2;
        second.size = 3;
        let (tar, listing, _) = pack(&[(first, b"abc".to_vec()), (second, b"abc".to_vec())]);
        let members = members(&tar);
        assert_eq!(members[0].kind, EntryType::Regular);
        assert_eq!(members[1].kind, EntryType::Link);
        assert_eq!(members[1].link.as_deref(), Some(&b"proj/a"[..]));
        assert_eq!(members[1].size, 0);
        let line = serde_json::to_string(&listing[1]).expect("serialises");
        assert_eq!(
            line,
            r#"{"path":"b","type":"hardlink","size":0,"mtime":1700000000,"mode":420,"link":"a"}"#
        );
    }

    #[test]
    fn files_with_one_link_or_another_inode_are_not_linked() {
        let mut other_inode = entry("c", EntryKind::File);
        other_inode.nlink = 2;
        other_inode.inode = 9;
        let mut first = entry("a", EntryKind::File);
        first.nlink = 2;
        let (tar, _, _) = pack(&[
            plain(first),
            plain(other_inode),
            plain(entry("d", EntryKind::File)),
        ]);
        let kinds: Vec<_> = members(&tar).iter().map(|m| m.kind).collect();
        assert_eq!(kinds, [EntryType::Regular; 3]);
    }

    #[test]
    fn a_time_before_1970_is_stored_as_zero_and_reported_with_the_container() {
        let mut top = entry("", EntryKind::Directory);
        top.mtime_ns = -1;
        let mut old = entry("old", EntryKind::File);
        old.mtime_ns = -5 * NANOS_PER_SECOND;
        let (tar, listing, notes) = pack(&[plain(top), plain(old)]);
        let members = members(&tar);
        assert_eq!((members[0].mtime, members[1].mtime), (0, 0));
        assert_eq!(listing[0].mtime, 0, "the listing holds the stored value");
        let noted: Vec<_> = notes.iter().map(PackNote::to_string).collect();
        assert_eq!(
            noted,
            [
                "the modification time of `proj` is before 1970 and is stored as 1970",
                "the modification time of `proj/old` is before 1970 and is stored as 1970",
            ]
        );
    }

    #[test]
    fn a_file_that_shrank_is_padded_and_marked_changed_and_one_that_grew_is_cut() {
        let mut shrunk = entry("shrunk", EntryKind::File);
        shrunk.size = 6;
        let mut grown = entry("grown", EntryKind::File);
        grown.size = 2;
        let (tar, listing, notes) = pack(&[(shrunk, b"ab".to_vec()), (grown, b"abcdef".to_vec())]);
        let members = members(&tar);
        assert_eq!(members[0].data, b"ab\0\0\0\0");
        assert_eq!(members[1].data, b"ab");
        assert_eq!(
            notes,
            [PackNote::FileChanged {
                path: path("proj/shrunk")
            }]
        );
        // The checksum covers the bytes in the archive, padding included.
        let mut hasher = HashingReader::new(&b"ab\0\0\0\0"[..]);
        io::copy(&mut hasher, &mut io::sink()).expect("hashes");
        assert_eq!(listing[0].sha256.as_deref(), Some(hasher.sha256().as_str()));
    }

    #[test]
    fn listing_entries_describe_the_members() {
        let (note, data) = file("src/note.txt", b"hello");
        let (tar, listing, _) = pack(&[
            plain(entry("src", EntryKind::Directory)),
            (note, data),
            plain(symlink("src/link", b"note.txt")),
        ]);
        assert_eq!(members(&tar).len(), 3);
        let lines: Vec<_> = listing
            .iter()
            .map(|e| serde_json::to_string(e).expect("serialises"))
            .collect();
        assert_eq!(
            lines,
            [
                r#"{"path":"src","type":"dir","size":0,"mtime":1700000000,"mode":493}"#.to_string(),
                r#"{"path":"src/note.txt","type":"file","size":5,"mtime":1700000000,"mode":420,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}"#.to_string(),
                r#"{"path":"src/link","type":"symlink","size":0,"mtime":1700000000,"mode":511,"link":"note.txt"}"#.to_string(),
            ]
        );
    }

    #[test]
    fn link_names_are_stored_as_the_bytes_the_filesystem_holds() {
        let long_with_slashes = format!("{}//b", "a".repeat(98)).into_bytes();
        let targets: Vec<Vec<u8>> = vec![
            b"a//b".to_vec(),
            b"x/./y".to_vec(),
            b"/".to_vec(),
            b"dir/".to_vec(),
            b"a//b/./c".to_vec(),
            b"//a".to_vec(),
            vec![b'a'; 99],
            vec![b'a'; 100],
            long_with_slashes,
            format!("{}//{}", "b".repeat(70), "c".repeat(78)).into_bytes(),
        ];
        let entries: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(index, target)| plain(symlink(&format!("l{index}"), target)))
            .collect();
        let (tar, listing, _) = pack(&entries);
        let members = members(&tar);
        assert_eq!(members.len(), targets.len());
        for ((member, line), target) in members.iter().zip(&listing).zip(&targets) {
            let length = target.len();
            assert_eq!(member.link.as_deref(), Some(target.as_slice()), "{length}");
            assert_eq!(member.kind, EntryType::Symlink);
            assert_eq!(
                line.link.as_ref().map(LinkTarget::as_bytes),
                Some(target.as_slice())
            );
        }
    }

    #[test]
    fn a_symlink_without_a_target_is_an_error_naming_it() {
        let mut writer = TarWriter::new(Vec::new(), container());
        let mut content = |_: &TreeEntry| -> io::Result<Box<dyn Read>> { unreachable!() };
        let error = writer
            .add(&entry("bad", EntryKind::Symlink), &mut content)
            .expect_err("a symlink needs its target");
        assert!(matches!(error, PackError::Read { ref path, .. } if path.to_string() == "bad"));
    }

    #[test]
    fn a_long_member_path_is_stored_in_full() {
        let long = format!("{}/{}", "d".repeat(80), "f".repeat(80));
        let (long_file, data) = file(&long, b"x");
        let (tar, _, _) = pack(&[
            plain(entry(&"d".repeat(80), EntryKind::Directory)),
            (long_file, data),
        ]);
        let members = members(&tar);
        assert_eq!(members[1].path, format!("proj/{long}").into_bytes());
    }

    #[test]
    fn a_reader_that_fails_is_a_read_error_and_a_writer_that_fails_a_write_error() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
        let mut sized = entry("data", EntryKind::File);
        sized.size = 4;

        let mut writer = TarWriter::new(Vec::new(), container());
        let mut unreadable = |_: &TreeEntry| -> io::Result<Box<dyn Read>> { Ok(Box::new(Broken)) };
        let error = writer
            .add(&sized, &mut unreadable)
            .expect_err("the file fails");
        assert!(matches!(error, PackError::Read { ref path, .. } if path.to_string() == "data"));

        let mut writer = TarWriter::new(Full { budget: 600 }, container());
        let mut readable = |_: &TreeEntry| -> io::Result<Box<dyn Read>> {
            Ok(Box::new(Cursor::new(vec![7; 4096])))
        };
        sized.size = 4096;
        let error = writer
            .add(&sized, &mut readable)
            .expect_err("the sink fills up");
        assert!(matches!(error, PackError::Write(_)), "{error:?}");

        let mut writer = TarWriter::new(Vec::new(), container());
        let mut missing = |_: &TreeEntry| -> io::Result<Box<dyn Read>> {
            Err(io::Error::from(io::ErrorKind::NotFound))
        };
        let error = writer
            .add(&sized, &mut missing)
            .expect_err("the file is gone");
        assert!(matches!(error, PackError::Read { ref path, .. } if path.to_string() == "data"));
    }

    #[test]
    fn a_directory_that_cannot_be_written_is_a_write_error() {
        let mut writer = TarWriter::new(Full { budget: 0 }, container());
        let mut content = |_: &TreeEntry| -> io::Result<Box<dyn Read>> { unreachable!() };
        let error = writer
            .add(&entry("d", EntryKind::Directory), &mut content)
            .expect_err("nothing can be written");
        assert!(matches!(error, PackError::Write(_)));
        let link = symlink("l", b"t");
        let error = writer
            .add(&link, &mut content)
            .expect_err("nothing can be written");
        assert!(matches!(error, PackError::Write(_)));

        // The first of two links to one file fails to write, so the second
        // is written as a link to it and fails too.
        let mut empty = |_: &TreeEntry| -> io::Result<Box<dyn Read>> { Ok(Box::new(io::empty())) };
        let mut first = entry("a", EntryKind::File);
        first.nlink = 2;
        let mut second = entry("b", EntryKind::File);
        second.nlink = 2;
        writer
            .add(&first, &mut empty)
            .expect_err("nothing can be written");
        let error = writer
            .add(&second, &mut empty)
            .expect_err("nothing can be written");
        assert!(matches!(error, PackError::Write(_)));

        let error = writer
            .finish()
            .expect_err("the end marker cannot be written");
        assert!(matches!(error, PackError::Write(_)));
    }

    /// A sink that takes `budget` bytes and then reports a full disk.
    #[derive(Debug)]
    struct Full {
        budget: usize,
    }

    impl Write for Full {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.budget == 0 {
                return Err(io::Error::from(io::ErrorKind::StorageFull));
            }
            let taken = buf.len().min(self.budget);
            self.budget -= taken;
            Ok(taken)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_test_sink_flushes() {
        Full { budget: 0 }.flush().expect("flushing never fails");
    }
}
