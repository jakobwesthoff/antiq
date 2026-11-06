// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Extracting a package into staging.

//! A package is read with the `tar` crate, but every write is antiq's
//! own and goes through a [`TreeRoot`]: the directory chain of each call
//! is opened without following symlinks, so no member is written through
//! one, and paths past the platform's limit work.

use super::names::check_paths;
use super::{PackError, TargetRules, Unpacked, zstd};
use crate::fsx::tree::TreeRoot;
use crate::relpath::{RelPath, escape};
use crate::walk::MAX_DEPTH;
use std::collections::HashMap;
use std::fmt;
use std::fs::{File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tar::{Archive, Entry, PaxExtensions};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// The permission bits of a mode, including set-user-id, set-group-id and
/// sticky.
const PERMISSION_BITS: u32 = 0o7777;

/// What the owner needs to create entries in a directory and to enter it
/// again. Directories keep it until the final pass, so a stored mode that
/// takes it away cannot block the members below.
const OWNER_ACCESS: u32 = 0o700;

const READ_CHUNK: usize = 64 * 1024;

/// Largest long name, long link or pax record accepted. The `tar` crate
/// reads such a record whole into memory, whatever size its header claims.
const MAX_RECORD: u64 = 64 * 1024;

/// Why a member of a package is not extracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The path is empty, or is not a path below a directory.
    InvalidPath,
    /// The first member is not the directory that holds the rest.
    NoContainer,
    /// A member lies outside the one top-level directory.
    OtherTopLevel,
    /// A kind of member that archives do not hold; the tar type byte.
    UnsupportedType(u8),
    /// The path starts at the root of the file system.
    Absolute,
    /// The path has a `..` component.
    ParentComponent,
    /// An earlier member has the same path.
    Duplicate,
    /// The directory the member belongs in was not extracted before it.
    ParentMissing,
    /// The entry the member belongs in is a file or a symlink.
    ParentNotDirectory,
    /// A hard link whose target is missing, outside the container or not
    /// a regular file extracted before it.
    LinkTarget,
    /// A symlink without a target, or with a NUL in it.
    InvalidLink,
    /// A header field that does not parse or is out of range.
    Malformed(&'static str),
    /// More directory levels below the container than the walk enters.
    TooDeep,
    /// A long name, long link or pax record above the size cap.
    OversizedRecord,
    /// A pax size record that disagrees with the member's header.
    PaxSizeMismatch,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::InvalidPath => f.write_str("its path is empty or not below a directory"),
            Refusal::NoContainer => {
                f.write_str("the first member is not the directory that holds the others")
            }
            Refusal::OtherTopLevel => {
                f.write_str("it lies outside the archive's top-level directory")
            }
            Refusal::UnsupportedType(byte) => write!(
                f,
                "it is not a file, directory or link (tar type {})",
                char::from(*byte)
            ),
            Refusal::Absolute => f.write_str("its path is absolute"),
            Refusal::ParentComponent => f.write_str("its path has a `..` component"),
            Refusal::Duplicate => f.write_str("an earlier member has the same path"),
            Refusal::ParentMissing => {
                f.write_str("the directory it belongs in was not extracted before it")
            }
            Refusal::ParentNotDirectory => {
                f.write_str("the entry it belongs in is not a directory")
            }
            Refusal::LinkTarget => f.write_str("its link target is not a file extracted before it"),
            Refusal::InvalidLink => {
                f.write_str("its symlink target is missing, empty or holds a NUL byte")
            }
            Refusal::Malformed(field) => write!(f, "its {field} field is not valid"),
            Refusal::TooDeep => write!(
                f,
                "it lies more than {MAX_DEPTH} levels below the top-level directory"
            ),
            Refusal::OversizedRecord => write!(
                f,
                "a record that describes it is larger than {MAX_RECORD} bytes"
            ),
            Refusal::PaxSizeMismatch => {
                f.write_str("a pax size record disagrees with the size in its header")
            }
        }
    }
}

// =========================================================
// Members
// =========================================================

/// What a member is, with what extraction needs to create it.
enum Kind {
    Directory,
    File,
    Symlink(Vec<u8>),
    /// The path of the file it is another name for, below the container.
    HardLink(RelPath),
}

/// A member of the package as extraction sees it.
struct Member {
    /// The path as the archive spells it, in escaped form.
    shown: String,
    /// Below the container; the root path for the container itself.
    path: RelPath,
    kind: Kind,
    mode: u32,
    mtime: u64,
}

/// The path of a member below the archive's one top-level directory, the
/// container. The first member names the container and must be a
/// directory; a path may start with `./`.
fn locate(
    raw: &[u8],
    container: &mut Option<Vec<u8>>,
    is_directory: bool,
) -> Result<RelPath, Refusal> {
    if raw.starts_with(b"/") {
        return Err(Refusal::Absolute);
    }
    let mut trimmed = raw;
    while let Some(rest) = trimmed.strip_prefix(b"./") {
        trimmed = rest;
    }
    let trimmed = trimmed.strip_suffix(b"/").unwrap_or(trimmed);
    if trimmed
        .split(|byte| *byte == b'/')
        .any(|name| name == b"..")
    {
        return Err(Refusal::ParentComponent);
    }
    let full = RelPath::from_bytes(trimmed.to_vec()).map_err(|_| Refusal::InvalidPath)?;
    let mut names = full.components();
    let top = names.next().ok_or(Refusal::InvalidPath)?;
    match container.as_deref() {
        None => {
            if !is_directory || names.next().is_some() {
                return Err(Refusal::NoContainer);
            }
            *container = Some(top.to_vec());
            Ok(RelPath::root())
        }
        Some(name) if name != top => Err(Refusal::OtherTopLevel),
        Some(_) => {
            let below = &full.as_bytes()[top.len()..];
            if below.is_empty() {
                // The container again.
                return Err(Refusal::InvalidPath);
            }
            RelPath::from_bytes(below[1..].to_vec()).map_err(|_| Refusal::InvalidPath)
        }
    }
}

impl Member {
    fn refused(&self, refusal: Refusal) -> PackError {
        PackError::Refused {
            member: self.shown.clone(),
            refusal,
        }
    }
}

/// Turns a header into a [`Member`], or says why the member is refused.
fn classify<R: Read>(
    entry: &Entry<'_, R>,
    container: &mut Option<Vec<u8>>,
) -> Result<Member, PackError> {
    // `Entry::path_bytes` and `link_name_bytes` give the name a long-name
    // or pax record carries; the header's own field holds only a prefix.
    let raw = entry.path_bytes();
    let shown = escape(&raw);
    let refuse = |refusal| PackError::Refused {
        member: shown.clone(),
        refusal,
    };
    let entry_type = entry.header().entry_type();
    let path = locate(&raw, container, entry_type.is_dir()).map_err(refuse)?;
    let kind = if entry_type.is_dir() {
        Kind::Directory
    } else if entry_type.is_file() {
        Kind::File
    } else if entry_type.is_symlink() {
        let target = entry.link_name_bytes().unwrap_or_default();
        if target.is_empty() || target.contains(&0) {
            return Err(refuse(Refusal::InvalidLink));
        }
        Kind::Symlink(target.into_owned())
    } else if entry_type.is_hard_link() {
        let target = entry.link_name_bytes().unwrap_or_default();
        let target = locate(&target, container, false).map_err(|_| refuse(Refusal::LinkTarget))?;
        Kind::HardLink(target)
    } else {
        return Err(refuse(Refusal::UnsupportedType(entry_type.as_byte())));
    };
    let header = entry.header();
    let mode = header
        .mode()
        .map_err(|_| refuse(Refusal::Malformed("mode")))?;
    let mtime = header
        .mtime()
        .ok()
        .filter(|seconds| i64::try_from(*seconds).is_ok())
        .ok_or_else(|| refuse(Refusal::Malformed("mtime")))?;
    Ok(Member {
        shown: shown.clone(),
        path,
        kind,
        mode: mode & PERMISSION_BITS,
        mtime,
    })
}

/// What an extracted member is, as far as later members can build on it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placed {
    Directory,
    File,
    Symlink,
}

/// The members seen so far, by the exact bytes of their paths. Whether a
/// member can be created, and whether a hard link has a target, is
/// decided here and not by asking the volume, which may find a different
/// spelling (`a` for `A`) than the archive means.
struct Placement {
    placed: HashMap<RelPath, Placed>,
}

impl Placement {
    fn new() -> Placement {
        Placement {
            placed: HashMap::new(),
        }
    }

    fn admit(&mut self, member: &Member) -> Result<(), Refusal> {
        if member.path.is_root() {
            return Ok(());
        }
        // The walk, the strict verify and the removal of staging go this
        // deep and no deeper.
        if member.path.components().count() > MAX_DEPTH {
            return Err(Refusal::TooDeep);
        }
        if self.placed.contains_key(&member.path) {
            return Err(Refusal::Duplicate);
        }
        let (parent, _) = split(&member.path);
        if !parent.is_root() {
            match self.placed.get(&parent) {
                None => return Err(Refusal::ParentMissing),
                Some(Placed::Directory) => {}
                Some(_) => return Err(Refusal::ParentNotDirectory),
            }
        }
        let placed = match &member.kind {
            Kind::Directory => Placed::Directory,
            Kind::File => Placed::File,
            Kind::Symlink(_) => Placed::Symlink,
            Kind::HardLink(target) => {
                if self.placed.get(target) != Some(&Placed::File) {
                    return Err(Refusal::LinkTarget);
                }
                Placed::File
            }
        };
        self.placed.insert(member.path.clone(), placed);
        Ok(())
    }
}

// =========================================================
// Extraction
// =========================================================

/// A directory whose stored mode and time are set once all its members
/// exist.
struct DirectoryRecord {
    path: RelPath,
    mode: u32,
    mtime: u64,
}

struct Extraction<'a> {
    staging: &'a TreeRoot,
    container: Option<Vec<u8>>,
    /// Mode and time the archive stores for the container.
    top: Option<(u32, u64)>,
    directories: Vec<DirectoryRecord>,
    /// The directory the previous member was created in. Entries come in
    /// path order, so most members find their parent open here.
    parent: Option<(RelPath, TreeRoot)>,
    members: usize,
    chunk: Vec<u8>,
    placement: Placement,
}

/// The open directory `parent` of a member, kept for the next member.
fn parent_of<'a>(
    staging: &'a TreeRoot,
    cache: &'a mut Option<(RelPath, TreeRoot)>,
    parent: &RelPath,
) -> io::Result<&'a TreeRoot> {
    if parent.is_root() {
        return Ok(staging);
    }
    if cache.as_ref().is_none_or(|(path, _)| path != parent) {
        *cache = Some((parent.clone(), staging.sub(parent)?));
    }
    Ok(&cache.as_ref().expect("the cache was filled above").1)
}

/// The parent directory and the name of `path`, which is not the root.
fn split(path: &RelPath) -> (RelPath, RelPath) {
    let bytes = path.as_bytes();
    let valid = "a part of a valid path is valid";
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (
            RelPath::from_bytes(bytes[..slash].to_vec()).expect(valid),
            RelPath::from_bytes(bytes[slash + 1..].to_vec()).expect(valid),
        ),
        None => (RelPath::root(), path.clone()),
    }
}

/// Turns a failed filesystem call on `path` into the error of the
/// extraction.
fn failed_at(path: &RelPath) -> impl Fn(io::Error) -> PackError + Copy + '_ {
    move |source| PackError::Extract {
        path: path.clone(),
        source,
    }
}

/// A symlink's stored mode is restored where the platform can change it.
/// Linux cannot, and every symlink there has the same mode, so that
/// refusal is not an error.
fn restored_where_possible(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(()),
        other => other,
    }
}

fn nanoseconds(seconds: u64) -> i128 {
    i128::from(seconds) * NANOS_PER_SECOND
}

impl<'a> Extraction<'a> {
    fn new(staging: &'a TreeRoot) -> Extraction<'a> {
        Extraction {
            staging,
            container: None,
            top: None,
            directories: Vec::new(),
            parent: None,
            members: 0,
            chunk: vec![0; READ_CHUNK],
            placement: Placement::new(),
        }
    }

    fn extract<R: Read>(
        &mut self,
        member: Member,
        entry: &mut Entry<'_, R>,
    ) -> Result<(), PackError> {
        self.placement
            .admit(&member)
            .map_err(|refusal| member.refused(refusal))?;
        let Member {
            path,
            kind,
            mode,
            mtime,
            ..
        } = member;
        if path.is_root() {
            self.top = Some((mode, mtime));
            return Ok(());
        }
        let at = failed_at(&path);
        let (parent_path, leaf) = split(&path);
        let parent = parent_of(self.staging, &mut self.parent, &parent_path).map_err(at)?;
        match kind {
            Kind::Directory => {
                parent.create_dir(&leaf, OWNER_ACCESS).map_err(at)?;
                // `mkdir` applies the umask, which can take owner bits away.
                parent.set_mode(&leaf, OWNER_ACCESS).map_err(at)?;
                self.directories.push(DirectoryRecord {
                    path: path.clone(),
                    mode,
                    mtime,
                });
            }
            Kind::File => {
                let mut file = parent.create_file(&leaf, 0o600).map_err(at)?;
                loop {
                    let count = entry.read(&mut self.chunk).map_err(PackError::Archive)?;
                    if count == 0 {
                        break;
                    }
                    file.write_all(&self.chunk[..count]).map_err(at)?;
                }
                file.set_permissions(Permissions::from_mode(mode))
                    .map_err(at)?;
                drop(file);
                parent.set_times(&leaf, nanoseconds(mtime)).map_err(at)?;
            }
            Kind::Symlink(target) => {
                parent.create_symlink(&leaf, &target).map_err(at)?;
                parent.set_times(&leaf, nanoseconds(mtime)).map_err(at)?;
                restored_where_possible(parent.set_mode(&leaf, mode)).map_err(at)?;
            }
            Kind::HardLink(target) => {
                self.staging.hard_link(&target, &path).map_err(at)?;
            }
        }
        self.members += 1;
        Ok(())
    }

    /// Sets the stored modes and times of the directories, the deepest
    /// first: a directory's time changes whenever an entry is created in
    /// it, and its mode may keep the owner from entering it.
    fn finish(mut self) -> Result<Unpacked, PackError> {
        let (top_mode, top_mtime) = self.top.ok_or_else(|| PackError::Refused {
            member: String::new(),
            refusal: Refusal::NoContainer,
        })?;
        self.directories.sort_by(|a, b| b.path.cmp(&a.path));
        for record in &self.directories {
            let at = failed_at(&record.path);
            self.staging
                .set_times(&record.path, nanoseconds(record.mtime))
                .map_err(at)?;
            self.staging
                .set_mode(&record.path, record.mode)
                .map_err(at)?;
        }
        let root = RelPath::root();
        self.staging
            .set_times(&root, nanoseconds(top_mtime))
            .map_err(failed_at(&root))?;
        Ok(Unpacked {
            top_mode,
            top_mtime,
            members: self.members,
        })
    }
}

/// A sink that takes zeros only.
struct ZerosOnly;

impl Write for ZerosOnly {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.iter().any(|byte| *byte != 0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the package holds data after the end of its tar stream",
            ));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Unpacks `package` below `staging`.
///
/// What was written stays in staging when a member is refused or a
/// write fails; the caller empties staging.
pub fn unpack(
    package: &Path,
    staging: &TreeRoot,
    rules: &TargetRules,
) -> Result<Unpacked, PackError> {
    // Two passes write nothing, so a package that is refused for what its
    // headers say, or for names the volume cannot hold, leaves staging as
    // it was.
    scan(package)?;
    let names = gather(package)?;
    let problems = check_paths(&names, rules);
    if !problems.is_empty() {
        return Err(PackError::UnholdableNames(problems));
    }

    let mut archive = open(package)?;
    let mut extraction = Extraction::new(staging);
    for entry in archive.entries().map_err(PackError::Archive)? {
        let mut entry = entry.map_err(PackError::Archive)?;
        let member = classify(&entry, &mut extraction.container)?;
        extraction.extract(member, &mut entry)?;
    }
    drain(archive)?;
    extraction.finish()
}

fn open(package: &Path) -> Result<Archive<impl Read>, PackError> {
    let file = File::open(package).map_err(PackError::Archive)?;
    let decoder = zstd::decoder(file).map_err(PackError::Archive)?;
    Ok(Archive::new(decoder))
}

/// The tar iterator stops at the first zero block. Reading on checks the
/// frame's content checksum and that nothing but zeros follows.
fn drain<R: Read>(archive: Archive<R>) -> Result<(), PackError> {
    io::copy(&mut archive.into_inner(), &mut ZerosOnly).map_err(PackError::Archive)?;
    Ok(())
}

/// The `size` a pax record sets for the member after it, if it has one.
fn pax_size<R: Read>(entry: &mut Entry<'_, R>) -> Result<Option<Option<u64>>, io::Error> {
    let mut data = Vec::new();
    entry.read_to_end(&mut data)?;
    for record in PaxExtensions::new(&data) {
        let record = record?;
        if record.key() == Ok("size") {
            return Ok(Some(record.value().ok().and_then(|text| text.parse().ok())));
        }
    }
    Ok(None)
}

/// The first pass looks at the records the `tar` crate would read whole
/// into memory, and at nothing else, in raw mode: raw mode does not read
/// them. It refuses a record larger than [`MAX_RECORD`], a sparse member,
/// whose extra headers the crate also reads before antiq sees the member,
/// and a pax `size` that differs from the header's. With that size
/// applied the crate would find the records that follow at other places
/// than this pass does.
fn scan(package: &Path) -> Result<(), PackError> {
    let mut archive = open(package)?;
    // The size a pax record set for the next member; `Some(None)` when
    // the record's value is not a number.
    let mut pending: Option<Option<u64>> = None;
    for entry in archive.entries().map_err(PackError::Archive)?.raw(true) {
        let mut entry = entry.map_err(PackError::Archive)?;
        let entry_type = entry.header().entry_type();
        let shown = escape(&entry.path_bytes());
        let refuse = |refusal| PackError::Refused {
            member: shown.clone(),
            refusal,
        };
        let is_pax = entry_type.is_pax_local_extensions();
        if entry_type.is_gnu_longname() || entry_type.is_gnu_longlink() || is_pax {
            if entry.size() > MAX_RECORD {
                return Err(refuse(Refusal::OversizedRecord));
            }
            if is_pax {
                pending = pax_size(&mut entry)
                    .map_err(PackError::Archive)?
                    .or(pending);
            }
            continue;
        }
        if entry_type.is_gnu_sparse() {
            return Err(refuse(Refusal::UnsupportedType(entry_type.as_byte())));
        }
        if let Some(size) = pending.take()
            && size != Some(entry.size())
        {
            return Err(refuse(Refusal::PaxSizeMismatch));
        }
    }
    drain(archive)
}

/// The second pass classifies and places every member and returns their
/// paths below the container, in archive order, for the name check.
fn gather(package: &Path) -> Result<Vec<RelPath>, PackError> {
    let mut archive = open(package)?;
    let mut container = None;
    let mut placement = Placement::new();
    let mut paths = Vec::new();
    for entry in archive.entries().map_err(PackError::Archive)? {
        let entry = entry.map_err(PackError::Archive)?;
        let member = classify(&entry, &mut container)?;
        placement
            .admit(&member)
            .map_err(|refusal| member.refused(refusal))?;
        if !member.path.is_root() {
            paths.push(member.path);
        }
    }
    drain(archive)?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::names;
    use crate::pack::zstd::{ZstdSettings, encoder};
    use crate::pack::{PackSettings, Packager, TarZstd};
    use crate::relpath::RelPath;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use crate::tree::{EntryKind, PieceTree, TreeEntry};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tar::EntryType;

    const NANOS_PER_SECOND: i128 = 1_000_000_000;

    // =========================================================
    // Fixtures
    // =========================================================

    fn live() -> PackSettings {
        PackSettings {
            zstd: ZstdSettings::live(1),
        }
    }

    /// Every entry below `root`, the root first, in path order.
    fn walk(root: &TreeRoot) -> Vec<TreeEntry> {
        fn collect(root: &TreeRoot, directory: &RelPath, entries: &mut Vec<TreeEntry>) {
            for child in root.read_dir(directory).expect("the directory lists") {
                let entry = root.lstat(&child.path).expect("the entry exists");
                let is_directory = entry.kind == EntryKind::Directory;
                entries.push(entry);
                if is_directory {
                    collect(root, &child.path, entries);
                }
            }
        }
        let mut entries = vec![root.lstat(&RelPath::root()).expect("the root exists")];
        collect(root, &RelPath::root(), &mut entries);
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries
    }

    /// The package of the tree below `tree`, as bytes.
    fn pack(tree: &Tree, container: &str) -> Vec<u8> {
        let root = TreeRoot::open(tree.root()).expect("the tree opens");
        let entries = walk(&root);
        let walked = PieceTree::new(RelPath::parse(container).expect("valid"), entries, vec![]);
        let mut out = Vec::new();
        TarZstd
            .pack(&root, &walked, &live(), &mut out)
            .expect("the piece packs");
        out
    }

    /// A package file in a temp directory of its own.
    struct PackageFile {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    fn package_file(bytes: &[u8]) -> PackageFile {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("package.tar.zst");
        std::fs::write(&path, bytes).expect("the package is written");
        PackageFile { _dir: dir, path }
    }

    fn staging() -> (tempfile::TempDir, TreeRoot) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = TreeRoot::open(dir.path()).expect("the dir opens");
        (dir, root)
    }

    fn unpack_bytes(bytes: &[u8], staging: &TreeRoot) -> Result<Unpacked, PackError> {
        let package = package_file(bytes);
        unpack(&package.path, staging, &TargetRules::linux())
    }

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    /// Builds a tar stream member by member with the names exactly as
    /// given, which the `tar` crate's own builder would not allow for
    /// absolute paths and `..`.
    struct RawArchive {
        bytes: Vec<u8>,
    }

    impl RawArchive {
        fn new() -> RawArchive {
            RawArchive { bytes: Vec::new() }
        }

        fn member(
            &mut self,
            name: &[u8],
            type_byte: u8,
            mode: u32,
            mtime: u64,
            link: &[u8],
            data: &[u8],
        ) -> &mut RawArchive {
            assert!(name.len() <= 100 && link.len() <= 100);
            let mut header = tar::Header::new_gnu();
            {
                let old = header.as_old_mut();
                old.name[..name.len()].copy_from_slice(name);
                old.linkname[..link.len()].copy_from_slice(link);
            }
            header.set_mode(mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(mtime);
            header.set_size(data.len() as u64);
            header.set_entry_type(EntryType::new(type_byte));
            header.set_cksum();
            self.bytes.extend_from_slice(header.as_bytes());
            self.bytes.extend_from_slice(data);
            let padding = (512 - data.len() % 512) % 512;
            self.bytes.resize(self.bytes.len() + padding, 0);
            self
        }

        /// A member whose header `tweak` changes before its checksum is
        /// computed.
        fn member_with(
            &mut self,
            name: &[u8],
            type_byte: u8,
            tweak: impl FnOnce(&mut tar::Header),
        ) -> &mut RawArchive {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_size(0);
            header.set_mtime(1_000_000_000);
            header.set_entry_type(EntryType::new(type_byte));
            tweak(&mut header);
            header.set_cksum();
            self.bytes.extend_from_slice(header.as_bytes());
            self
        }

        /// A record that describes the next member: a GNU long name
        /// (`L`), a GNU long link (`K`) or a pax header (`x`).
        fn record(&mut self, type_byte: u8, data: &[u8]) -> &mut RawArchive {
            let size = data.len() as u64;
            self.member_with(b"././@LongLink", type_byte, |header| header.set_size(size));
            self.bytes.extend_from_slice(data);
            let padding = (512 - data.len() % 512) % 512;
            self.bytes.resize(self.bytes.len() + padding, 0);
            self
        }

        /// A GNU long name for the next member. The next header carries a
        /// harmless name of its own.
        fn long_name(&mut self, path: &[u8]) -> &mut RawArchive {
            self.record(b'L', &[path, b"\0"].concat())
        }

        fn long_link(&mut self, target: &[u8]) -> &mut RawArchive {
            self.record(b'K', &[target, b"\0"].concat())
        }

        /// A pax header with the given records for the next member.
        fn pax(&mut self, records: &[(&str, &str)]) -> &mut RawArchive {
            let mut data = Vec::new();
            for (key, value) in records {
                // Each record starts with its own length, digits included.
                let body = format!(" {key}={value}\n");
                let mut length = body.len() + 1;
                while format!("{length}{body}").len() != length {
                    length += 1;
                }
                data.extend_from_slice(format!("{length}{body}").as_bytes());
            }
            self.record(b'x', &data)
        }

        /// Bytes appended to the stream as they are.
        fn raw(&mut self, bytes: &[u8]) -> &mut RawArchive {
            self.bytes.extend_from_slice(bytes);
            self
        }

        fn dir(&mut self, name: &str, mode: u32, mtime: u64) -> &mut RawArchive {
            self.member(name.as_bytes(), b'5', mode, mtime, b"", b"")
        }

        fn file(&mut self, name: &str, mode: u32, mtime: u64, data: &[u8]) -> &mut RawArchive {
            self.member(name.as_bytes(), b'0', mode, mtime, b"", data)
        }

        fn symlink(&mut self, name: &str, target: &str, mode: u32, mtime: u64) -> &mut RawArchive {
            self.member(name.as_bytes(), b'2', mode, mtime, target.as_bytes(), b"")
        }

        /// The compressed package, ended with the two zero blocks of a
        /// tar stream followed by `after`.
        fn finish_with(&self, after: &[u8]) -> Vec<u8> {
            let mut stream = self.bytes.clone();
            stream.extend_from_slice(&[0; 1024]);
            stream.extend_from_slice(after);
            let mut encoder = encoder(Vec::new(), &ZstdSettings::live(1)).expect("an encoder");
            encoder
                .write_all(&stream)
                .expect("a vec accepts the stream");
            encoder.finish().expect("the frame ends")
        }

        fn finish(&self) -> Vec<u8> {
            self.finish_with(b"")
        }
    }

    /// Restores owner access to directories a test made unreadable, so
    /// the temp directory can be removed.
    struct RestoreModes(Vec<PathBuf>);

    impl Drop for RestoreModes {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            }
        }
    }

    fn mode_of(root: &TreeRoot, path: &str) -> u32 {
        root.lstat(&rel(path)).expect("the entry exists").mode & 0o7777
    }

    fn seconds_of(root: &TreeRoot, path: &RelPath) -> i128 {
        root.lstat(path)
            .expect("the entry exists")
            .mtime_ns
            .div_euclid(NANOS_PER_SECOND)
    }

    fn read(root: &TreeRoot, path: &str) -> Vec<u8> {
        use std::io::Read;
        let mut content = Vec::new();
        root.open_read(&rel(path))
            .expect("the file opens")
            .read_to_end(&mut content)
            .expect("the file reads");
        content
    }

    // =========================================================
    // Well-formed archives
    // =========================================================

    #[test]
    fn a_packed_tree_comes_back_with_its_kinds_modes_times_sizes_and_links() {
        let mut tree = Tree::new();
        tree.file_with("a/b/secret.txt", "secret", 0o600, 1_000_000_001);
        tree.file_with("a/readable.txt", "readable", 0o644, 1_000_000_002);
        tree.file_with("a/run.sh", "#!/bin/sh\n", 0o755, 1_000_000_003);
        tree.dir("a/empty");
        tree.symlink("a/relative", "readable.txt");
        tree.symlink("a/dangling", "nowhere");
        tree.hard_link("a/readable.txt", "a/readable-again.txt");
        std::fs::set_permissions(tree.path("a"), std::fs::Permissions::from_mode(0o750))
            .expect("the mode can be set");
        std::fs::set_permissions(tree.path("a/b"), std::fs::Permissions::from_mode(0o700))
            .expect("the mode can be set");
        let package = pack(&tree, "projects/proj");
        let source = TreeRoot::open(tree.root()).expect("opens");

        let (_dir, staged) = staging();
        let unpacked = unpack_bytes(&package, &staged).expect("the package unpacks");

        let expected = walk(&source);
        let found = walk(&staged);
        assert_eq!(unpacked.members, expected.len() - 1);
        let paths = |entries: &[TreeEntry]| -> Vec<RelPath> {
            entries.iter().map(|entry| entry.path.clone()).collect()
        };
        assert_eq!(paths(&found), paths(&expected));
        for (want, got) in expected.iter().zip(&found) {
            let name = want.path.to_string();
            assert_eq!(got.kind, want.kind, "kind of {name}");
            assert_eq!(
                got.mtime_ns.div_euclid(NANOS_PER_SECOND),
                want.mtime_ns.div_euclid(NANOS_PER_SECOND),
                "modification time of {name}"
            );
            assert_eq!(got.link_target, want.link_target, "link target of {name}");
            if want.kind == EntryKind::File {
                assert_eq!(got.size, want.size, "size of {name}");
                assert_eq!(got.nlink, want.nlink, "links of {name}");
            }
            // The staging root keeps the mode its owner gave it, and a
            // symlink's mode is the platform's.
            if !want.path.is_root() && want.kind != EntryKind::Symlink {
                assert_eq!(got.mode & 0o7777, want.mode & 0o7777, "mode of {name}");
            }
        }
        assert_eq!(read(&staged, "a/b/secret.txt"), b"secret");
        assert_eq!(read(&staged, "a/readable-again.txt"), b"readable");
    }

    #[test]
    fn the_contents_of_the_container_land_at_the_staging_root_whatever_its_name() {
        let mut archive = RawArchive::new();
        archive.dir("whatever-name/", 0o751, 1_234_567_890).file(
            "whatever-name/note",
            0o644,
            1_000_000_000,
            b"hi",
        );
        let (_dir, staged) = staging();
        let unpacked = unpack_bytes(&archive.finish(), &staged).expect("unpacks");
        assert_eq!(read(&staged, "note"), b"hi");
        assert_eq!(
            unpacked,
            Unpacked {
                top_mode: 0o751,
                top_mtime: 1_234_567_890,
                members: 1,
            }
        );
        assert_eq!(seconds_of(&staged, &RelPath::root()), 1_234_567_890);
    }

    #[test]
    fn directories_with_modes_that_block_their_children_are_extracted_and_end_with_their_stored_mode()
     {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .dir("proj/readonly/", 0o555, 1_000_000_000)
            .file("proj/readonly/child", 0o644, 1_000_000_000, b"r")
            .dir("proj/nosearch/", 0o644, 1_000_000_000)
            .file("proj/nosearch/child", 0o644, 1_000_000_000, b"n")
            .dir("proj/locked/", 0o000, 1_000_000_000)
            .file("proj/locked/child", 0o644, 1_000_000_000, b"l");
        let (dir, staged) = staging();
        let _restore = RestoreModes(vec![
            dir.path().join("readonly"),
            dir.path().join("nosearch"),
            dir.path().join("locked"),
        ]);
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(mode_of(&staged, "readonly"), 0o555);
        assert_eq!(mode_of(&staged, "nosearch"), 0o644);
        assert_eq!(mode_of(&staged, "locked"), 0o000);

        // The children can be seen once the owner may enter again.
        for name in ["readonly", "nosearch", "locked"] {
            std::fs::set_permissions(
                dir.path().join(name),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("the mode can be set");
        }
        assert_eq!(read(&staged, "readonly/child"), b"r");
        assert_eq!(read(&staged, "nosearch/child"), b"n");
        assert_eq!(read(&staged, "locked/child"), b"l");
    }

    #[test]
    fn directory_times_are_the_stored_ones_after_their_children_were_written() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_100_000_000)
            .dir("proj/outer/", 0o755, 1_200_000_000)
            .dir("proj/outer/inner/", 0o755, 1_300_000_000)
            .file("proj/outer/inner/file", 0o644, 1_400_000_000, b"x")
            .file("proj/outer/sibling", 0o644, 1_500_000_000, b"y");
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(seconds_of(&staged, &RelPath::root()), 1_100_000_000);
        assert_eq!(seconds_of(&staged, &rel("outer")), 1_200_000_000);
        assert_eq!(seconds_of(&staged, &rel("outer/inner")), 1_300_000_000);
        assert_eq!(seconds_of(&staged, &rel("outer/inner/file")), 1_400_000_000);
        assert_eq!(seconds_of(&staged, &rel("outer/sibling")), 1_500_000_000);
    }

    #[test]
    fn a_path_longer_than_the_platform_limit_is_extracted() {
        let source = Tree::new();
        let root = TreeRoot::open(source.root()).expect("opens");
        let deep = deep_path(6);
        for level in 1..=6 {
            let directory: Vec<&[u8]> = deep.components().take(level).collect();
            root.create_dir(
                &RelPath::from_bytes(directory.join(&b'/')).expect("valid"),
                0o755,
            )
            .expect("the directory is created");
        }
        let file = deep.concat(&rel("data.txt"));
        root.create_file(&file, 0o644)
            .expect("the file is created")
            .write_all(b"deep")
            .expect("written");
        let package = pack(&source, "proj");

        let (_dir, staged) = staging();
        unpack_bytes(&package, &staged).expect("the package unpacks");
        assert!(file.as_bytes().len() > 1024);
        assert_eq!(read(&staged, &file.to_string()), b"deep");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_symlink_gets_its_stored_mode_on_macos() {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000).symlink(
            "proj/link",
            "target",
            0o700,
            1_000_000_000,
        );
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(mode_of(&staged, "link"), 0o700);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlink_keeps_the_mode_linux_gives_it_and_extraction_goes_on() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .symlink("proj/link", "target", 0o700, 1_000_000_000)
            .file("proj/after", 0o644, 1_000_000_000, b"x");
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(mode_of(&staged, "link"), 0o777);
        assert_eq!(read(&staged, "after"), b"x");
    }

    #[test]
    fn a_name_that_already_exists_in_staging_fails_the_extraction() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .file("proj/taken", 0o644, 1_000_000_000, b"new");
        let (dir, staged) = staging();
        std::fs::write(dir.path().join("taken"), b"old").expect("a file");
        let error = unpack_bytes(&archive.finish(), &staged).expect_err("the name is taken");
        assert!(
            matches!(&error, PackError::Extract { path, source }
                if path.to_string() == "taken"
                    && source.kind() == std::io::ErrorKind::AlreadyExists),
            "{error:?}"
        );
        assert_eq!(read(&staged, "taken"), b"old");
    }

    // =========================================================
    // Packages that cannot be read
    // =========================================================

    fn an_archive_error(bytes: &[u8]) -> PackError {
        let (_dir, staged) = staging();
        unpack_bytes(bytes, &staged).expect_err("the package cannot be read")
    }

    #[test]
    fn a_package_that_is_not_there_is_an_archive_error() {
        let (dir, staged) = staging();
        let error = unpack(&dir.path().join("missing"), &staged, &TargetRules::linux())
            .expect_err("no such file");
        assert!(matches!(error, PackError::Archive(_)), "{error:?}");
    }

    #[test]
    fn a_truncated_package_is_an_archive_error() {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000).file(
            "proj/data",
            0o644,
            1_000_000_000,
            &[7; 5000],
        );
        let package = archive.finish();
        let error = an_archive_error(&package[..package.len() / 2]);
        assert!(matches!(error, PackError::Archive(_)), "{error:?}");
    }

    #[test]
    fn a_package_with_a_flipped_checksum_is_an_archive_error() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .file("proj/data", 0o644, 1_000_000_000, b"data");
        let mut package = archive.finish();
        let last = package.len() - 1;
        package[last] ^= 0xff;
        let error = an_archive_error(&package);
        assert!(matches!(error, PackError::Archive(_)), "{error:?}");
    }

    #[test]
    fn a_byte_after_the_end_of_the_tar_stream_is_an_archive_error() {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000);
        let error = an_archive_error(&archive.finish_with(b"x"));
        assert!(matches!(error, PackError::Archive(_)), "{error:?}");
    }

    #[test]
    fn zero_padding_after_the_end_of_the_tar_stream_is_fine() {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000);
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish_with(&[0; 8192]), &staged).expect("the padding is allowed");
    }

    #[test]
    fn a_frame_with_a_window_above_128_mib_is_an_archive_error() {
        // An empty frame with a window of 2^28 bytes.
        let frame = [0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x90, 0x01, 0x00, 0x00];
        let error = an_archive_error(&frame);
        assert!(matches!(error, PackError::Archive(_)), "{error:?}");
    }

    // =========================================================
    // Members that are refused
    // =========================================================

    fn refusal_of(archive: &RawArchive) -> Refusal {
        let (_dir, staged) = staging();
        let error = unpack_bytes(&archive.finish(), &staged).expect_err("a member is refused");
        let PackError::Refused { refusal, .. } = &error else {
            panic!("{error:?}")
        };
        refusal.clone()
    }

    #[test]
    fn a_symlink_mode_the_platform_cannot_change_is_not_an_error() {
        let unsupported = io::Error::from(io::ErrorKind::Unsupported);
        assert!(restored_where_possible(Err(unsupported)).is_ok());
        assert!(restored_where_possible(Ok(())).is_ok());
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(restored_where_possible(Err(denied)).is_err());
    }

    #[test]
    fn the_sink_after_the_tar_stream_takes_zeros_and_nothing_else() {
        let mut sink = ZerosOnly;
        assert_eq!(sink.write(&[0; 16]).expect("zeros are taken"), 16);
        sink.flush().expect("a flush is nothing");
        let error = sink.write(&[0, 1]).expect_err("data is refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_path_with_an_empty_component_is_refused() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .file("proj//x", 0o644, 1_000_000_000, b"x");
        assert_eq!(refusal_of(&archive), Refusal::InvalidPath);
    }

    #[test]
    fn the_container_twice_is_refused() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .dir("proj/", 0o755, 1_000_000_000);
        assert_eq!(refusal_of(&archive), Refusal::InvalidPath);
    }

    #[test]
    fn an_archive_that_does_not_start_with_its_directory_is_refused() {
        let mut first_a_file = RawArchive::new();
        first_a_file.file("proj", 0o644, 1_000_000_000, b"x");
        assert_eq!(refusal_of(&first_a_file), Refusal::NoContainer);

        let mut nested = RawArchive::new();
        nested.dir("a/b/", 0o755, 1_000_000_000);
        assert_eq!(refusal_of(&nested), Refusal::NoContainer);
    }

    #[test]
    fn an_archive_without_members_is_refused() {
        assert_eq!(refusal_of(&RawArchive::new()), Refusal::NoContainer);
    }

    #[test]
    fn a_member_outside_the_one_top_level_directory_is_refused() {
        let mut archive = RawArchive::new();
        archive
            .dir("proj/", 0o755, 1_000_000_000)
            .file("other/x", 0o644, 1_000_000_000, b"x");
        assert_eq!(refusal_of(&archive), Refusal::OtherTopLevel);
    }

    #[test]
    fn a_fifo_member_is_refused() {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000).member(
            b"proj/queue",
            b'6',
            0o644,
            1_000_000_000,
            b"",
            b"",
        );
        assert_eq!(refusal_of(&archive), Refusal::UnsupportedType(b'6'));
    }

    #[test]
    fn a_leading_dot_slash_is_the_same_member() {
        let mut archive = RawArchive::new();
        archive
            .dir("./proj/", 0o755, 1_000_000_000)
            .file("./proj/a", 0o644, 1_000_000_000, b"a");
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(read(&staged, "a"), b"a");
    }

    #[test]
    fn refusals_print_with_the_member() {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000).member(
            b"proj/queue",
            b'6',
            0o644,
            1_000_000_000,
            b"",
            b"",
        );
        let (_dir, staged) = staging();
        let error = unpack_bytes(&archive.finish(), &staged).expect_err("refused");
        assert_eq!(
            error.to_string(),
            "the package holds a member that cannot be extracted: `proj/queue`: \
             it is not a file, directory or link (tar type 6)"
        );
        let others = [
            Refusal::InvalidPath.to_string(),
            Refusal::NoContainer.to_string(),
            Refusal::OtherTopLevel.to_string(),
            Refusal::Absolute.to_string(),
            Refusal::ParentComponent.to_string(),
            Refusal::Duplicate.to_string(),
            Refusal::ParentMissing.to_string(),
            Refusal::ParentNotDirectory.to_string(),
            Refusal::LinkTarget.to_string(),
            Refusal::InvalidLink.to_string(),
            Refusal::Malformed("mode").to_string(),
            Refusal::TooDeep.to_string(),
            Refusal::OversizedRecord.to_string(),
            Refusal::PaxSizeMismatch.to_string(),
        ];
        assert_eq!(
            others,
            [
                "its path is empty or not below a directory",
                "the first member is not the directory that holds the others",
                "it lies outside the archive's top-level directory",
                "its path is absolute",
                "its path has a `..` component",
                "an earlier member has the same path",
                "the directory it belongs in was not extracted before it",
                "the entry it belongs in is not a directory",
                "its link target is not a file extracted before it",
                "its symlink target is missing, empty or holds a NUL byte",
                "its mode field is not valid",
                "it lies more than 512 levels below the top-level directory",
                "a record that describes it is larger than 65536 bytes",
                "a pax size record disagrees with the size in its header",
            ]
        );
    }

    // =========================================================
    // Hostile archives
    // =========================================================

    /// A staging root next to a directory the archive must never reach.
    struct Sandbox {
        _dir: tempfile::TempDir,
        staging: TreeRoot,
        staging_path: PathBuf,
        outside: PathBuf,
    }

    fn sandbox() -> Sandbox {
        let dir = tempfile::tempdir().expect("a temp dir");
        let staging_path = dir.path().join("staging");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&staging_path).expect("staging is created");
        std::fs::create_dir(&outside).expect("outside is created");
        std::fs::write(outside.join("secret"), b"secret").expect("a secret");
        let staging = TreeRoot::open(&staging_path).expect("staging opens");
        Sandbox {
            _dir: dir,
            staging,
            staging_path,
            outside,
        }
    }

    impl Sandbox {
        /// Unpacks `archive` and checks it was refused for `refusal`, that
        /// nothing was written to staging and that outside is as it was.
        fn assert_refused_before_any_write(&self, archive: &RawArchive, refusal: Refusal) {
            let error = unpack_bytes(&archive.finish(), &self.staging).expect_err("refused");
            assert!(
                matches!(&error, PackError::Refused { refusal: found, .. } if *found == refusal),
                "{error:?}"
            );
            let written = std::fs::read_dir(&self.staging_path)
                .expect("staging lists")
                .count();
            assert_eq!(written, 0, "staging is empty");
            let outside: Vec<_> = std::fs::read_dir(&self.outside)
                .expect("outside lists")
                .map(|entry| entry.expect("an entry").file_name())
                .collect();
            assert_eq!(outside, ["secret"], "outside is untouched");
            assert_eq!(
                std::fs::read(self.outside.join("secret")).expect("the secret reads"),
                b"secret"
            );
        }
    }

    fn container() -> RawArchive {
        let mut archive = RawArchive::new();
        archive.dir("proj/", 0o755, 1_000_000_000);
        archive
    }

    #[test]
    fn an_absolute_member_is_refused_before_anything_is_written() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.file("/tmp/x", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::Absolute);
    }

    #[test]
    fn a_member_with_a_parent_component_is_refused_before_anything_is_written() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.file("proj/ok", 0o644, 1_000_000_000, b"ok").file(
            "proj/../escaped.txt",
            0o644,
            1_000_000_000,
            b"x",
        );
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentComponent);
    }

    #[test]
    fn a_child_of_a_symlink_to_a_directory_outside_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .symlink("proj/a", "../outside", 0o777, 1_000_000_000)
            .file("proj/a/pwn.txt", 0o644, 1_000_000_000, b"pwned");
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentNotDirectory);
    }

    #[test]
    fn a_child_of_a_symlink_to_an_absolute_path_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .symlink("proj/a", "/etc", 0o777, 1_000_000_000)
            .file("proj/a/pwn.txt", 0o644, 1_000_000_000, b"pwned");
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentNotDirectory);
    }

    #[test]
    fn a_directory_that_replaces_a_symlink_of_the_same_name_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .symlink("proj/a", "../outside", 0o777, 1_000_000_000)
            .dir("proj/a/", 0o755, 1_000_000_000);
        sandbox.assert_refused_before_any_write(&archive, Refusal::Duplicate);
    }

    #[test]
    fn the_same_path_twice_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.file("proj/a", 0o644, 1_000_000_000, b"1").file(
            "proj/a",
            0o644,
            1_000_000_000,
            b"2",
        );
        sandbox.assert_refused_before_any_write(&archive, Refusal::Duplicate);
    }

    #[test]
    fn a_hard_link_to_a_file_outside_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .member(
                b"proj/h",
                b'1',
                0o644,
                1_000_000_000,
                b"../outside/secret",
                b"",
            )
            .file("proj/h2", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_hard_link_to_an_absolute_path_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.member(b"proj/h", b'1', 0o644, 1_000_000_000, b"/etc/passwd", b"");
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_hard_link_to_a_symlink_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .symlink("proj/s", "../outside/secret", 0o777, 1_000_000_000)
            .member(b"proj/h", b'1', 0o644, 1_000_000_000, b"proj/s", b"");
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_hard_link_to_a_directory_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.dir("proj/d/", 0o755, 1_000_000_000).member(
            b"proj/h",
            b'1',
            0o644,
            1_000_000_000,
            b"proj/d",
            b"",
        );
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_hard_link_to_a_member_that_comes_later_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .member(b"proj/h", b'1', 0o644, 1_000_000_000, b"proj/later", b"")
            .file("proj/later", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_hard_link_without_a_target_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.member(b"proj/h", b'1', 0o644, 1_000_000_000, b"", b"");
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_hard_link_to_an_earlier_file_is_extracted() {
        let mut archive = container();
        archive
            .file("proj/first", 0o644, 1_000_000_000, b"shared")
            .member(
                b"proj/second",
                b'1',
                0o644,
                1_000_000_000,
                b"proj/first",
                b"",
            );
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(read(&staged, "second"), b"shared");
        assert_eq!(
            staged.lstat(&rel("first")).expect("exists").inode,
            staged.lstat(&rel("second")).expect("exists").inode
        );
    }

    #[test]
    fn a_symlink_without_a_target_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.member(b"proj/s", b'2', 0o777, 1_000_000_000, b"", b"");
        sandbox.assert_refused_before_any_write(&archive, Refusal::InvalidLink);
    }

    #[test]
    fn a_member_whose_parent_was_not_extracted_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.file("proj/missing/x", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentMissing);
    }

    #[test]
    fn a_member_below_a_file_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.file("proj/f", 0o644, 1_000_000_000, b"x").file(
            "proj/f/x",
            0o644,
            1_000_000_000,
            b"x",
        );
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentNotDirectory);
    }

    #[test]
    fn a_parent_is_found_by_its_exact_spelling_and_not_by_what_the_volume_makes_of_it() {
        // On a volume that ignores case, `a` finds the directory `A`.
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .dir("proj/A/", 0o755, 1_000_000_000)
            .file("proj/a/x", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentMissing);
    }

    #[test]
    fn a_hostile_path_in_a_gnu_long_name_record_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .long_name(b"proj/../../escaped")
            .file("proj/ok", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::ParentComponent);
    }

    #[test]
    fn a_hostile_target_in_a_gnu_long_link_record_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .file("proj/f", 0o644, 1_000_000_000, b"x")
            .long_link(b"../outside/secret")
            .member(b"proj/h", b'1', 0o644, 1_000_000_000, b"proj/f", b"");
        sandbox.assert_refused_before_any_write(&archive, Refusal::LinkTarget);
    }

    #[test]
    fn a_nul_in_a_symlink_target_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .long_link(b"a\0b")
            .member(b"proj/s", b'2', 0o777, 1_000_000_000, b"harmless", b"");
        sandbox.assert_refused_before_any_write(&archive, Refusal::InvalidLink);
    }

    #[test]
    fn a_hostile_path_in_a_pax_header_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .pax(&[("path", "/tmp/x")])
            .file("proj/ok", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::Absolute);
    }

    #[test]
    fn a_mode_that_is_not_octal_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.member_with(b"proj/f", b'0', |header| {
            header.as_old_mut().mode = *b"zzzzzzz\0";
        });
        sandbox.assert_refused_before_any_write(&archive, Refusal::Malformed("mode"));
    }

    #[test]
    fn a_modification_time_beyond_the_range_of_i64_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.member_with(b"proj/f", b'0', |header| {
            // Base-256 with the largest value of 64 bits.
            let mut field = [0xff; 12];
            field[..4].copy_from_slice(&[0x80, 0, 0, 0]);
            header.as_old_mut().mtime = field;
        });
        sandbox.assert_refused_before_any_write(&archive, Refusal::Malformed("mtime"));
    }

    #[test]
    fn a_long_name_record_above_the_cap_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .record(b'L', &vec![b'a'; 70_000])
            .file("proj/ok", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::OversizedRecord);
    }

    #[test]
    fn a_pax_record_above_the_cap_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .record(b'x', &vec![b' '; 70_000])
            .file("proj/ok", 0o644, 1_000_000_000, b"x");
        sandbox.assert_refused_before_any_write(&archive, Refusal::OversizedRecord);
    }

    #[test]
    fn a_sparse_member_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive.member_with(b"proj/sparse", b'S', |_| {});
        sandbox.assert_refused_before_any_write(&archive, Refusal::UnsupportedType(b'S'));
    }

    #[test]
    fn a_pax_size_that_differs_from_the_header_size_is_refused_before_anything_hides_behind_it() {
        // Read by header sizes alone, the zero block ends the stream before
        // the oversized record that follows. Applying the pax size, the
        // record is the next thing the reader would buffer.
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .pax(&[("size", "1024")])
            .member(b"proj/f", b'0', 0o644, 1_000_000_000, b"", b"")
            .raw(&[0; 1024])
            .record(b'L', &vec![b'a'; 70_000]);
        sandbox.assert_refused_before_any_write(&archive, Refusal::PaxSizeMismatch);
    }

    #[test]
    fn a_pax_size_that_equals_the_header_size_is_fine() {
        let mut archive = container();
        archive
            .pax(&[("size", "2"), ("mtime", "1")])
            .file("proj/f", 0o644, 1_000_000_000, b"ok");
        let (_dir, staged) = staging();
        unpack_bytes(&archive.finish(), &staged).expect("the package unpacks");
        assert_eq!(read(&staged, "f"), b"ok");
    }

    #[test]
    fn a_pax_size_that_is_not_a_number_is_refused() {
        let sandbox = sandbox();
        let mut archive = container();
        archive
            .pax(&[("size", "many")])
            .file("proj/f", 0o644, 1_000_000_000, b"ok");
        sandbox.assert_refused_before_any_write(&archive, Refusal::PaxSizeMismatch);
    }

    // =========================================================
    // Names the volume cannot hold
    // =========================================================

    fn case_insensitive() -> TargetRules {
        TargetRules {
            case_insensitive: true,
            ..TargetRules::linux()
        }
    }

    fn normalization_insensitive() -> TargetRules {
        TargetRules {
            normalization_insensitive: true,
            ..TargetRules::linux()
        }
    }

    /// Unpacks into an empty staging root and returns the problems the
    /// name check found, checking that nothing was written.
    fn name_problems(archive: &RawArchive, rules: &TargetRules) -> Vec<names::NameProblem> {
        let (dir, staged) = staging();
        let package = package_file(&archive.finish());
        let error = unpack(&package.path, &staged, rules).expect_err("the names are refused");
        let PackError::UnholdableNames(problems) = error else {
            panic!("{error:?}")
        };
        let written = std::fs::read_dir(dir.path())
            .expect("staging lists")
            .count();
        assert_eq!(written, 0, "staging is empty");
        problems
    }

    fn collision(spellings: &[&str]) -> names::NameProblem {
        names::NameProblem::Collision {
            paths: spellings.iter().map(|name| rel(name)).collect(),
        }
    }

    #[test]
    fn names_that_differ_in_case_are_refused_on_a_volume_that_ignores_case() {
        let mut archive = container();
        archive
            .file("proj/README", 0o644, 1_000_000_000, b"1")
            .file("proj/Readme", 0o644, 1_000_000_000, b"2");
        assert_eq!(
            name_problems(&archive, &case_insensitive()),
            [collision(&["README", "Readme"])]
        );
    }

    #[test]
    fn names_that_differ_in_normalisation_are_refused_on_a_volume_that_ignores_it() {
        let mut archive = container();
        archive
            .file("proj/caf\u{e9}", 0o644, 1_000_000_000, b"1")
            .file("proj/cafe\u{301}", 0o644, 1_000_000_000, b"2");
        assert_eq!(
            name_problems(&archive, &normalization_insensitive()),
            [collision(&["caf\u{e9}", "cafe\u{301}"])]
        );
    }

    #[test]
    fn a_name_longer_than_the_volume_allows_is_refused() {
        let name = "\u{e9}".repeat(128);
        let mut archive = container();
        archive.long_name(format!("proj/{name}").as_bytes()).file(
            "proj/x",
            0o644,
            1_000_000_000,
            b"x",
        );
        let problems = name_problems(&archive, &TargetRules::linux());
        assert_eq!(
            problems,
            [names::NameProblem::TooLong {
                path: rel(&name),
                length: 256,
                limit: TargetRules::linux().name_limit,
            }]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_name_of_256_bytes_that_macos_counts_as_128_units_is_extracted() {
        let name = "\u{e9}".repeat(128);
        let mut archive = container();
        archive.long_name(format!("proj/{name}").as_bytes()).file(
            "proj/x",
            0o644,
            1_000_000_000,
            b"x",
        );
        let (_dir, staged) = staging();
        let package = package_file(&archive.finish());
        unpack(&package.path, &staged, &TargetRules::macos()).expect("the name is fine");
        assert_eq!(read(&staged, &name), b"x");
    }

    fn non_utf8() -> RawArchive {
        let mut archive = container();
        archive.member(b"proj/a\xff", b'0', 0o644, 1_000_000_000, b"", b"x");
        archive
    }

    #[test]
    fn a_name_that_is_not_utf8_is_refused_where_the_volume_requires_utf8() {
        let problems = name_problems(&non_utf8(), &TargetRules::macos());
        let odd = RelPath::from_bytes(vec![b'a', 0xff]).expect("a valid path");
        assert_eq!(problems, [names::NameProblem::NotUtf8 { path: odd }]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_is_extracted_where_the_volume_takes_it() {
        let (_dir, staged) = staging();
        let package = package_file(&non_utf8().finish());
        unpack(&package.path, &staged, &TargetRules::linux()).expect("the name is fine");
        assert_eq!(walk(&staged).len(), 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_name_that_is_not_utf8_fails_on_apfs_when_the_rules_do_not_say_so() {
        let (_dir, staged) = staging();
        let package = package_file(&non_utf8().finish());
        let error = unpack(&package.path, &staged, &TargetRules::linux()).expect_err("fails");
        assert!(
            matches!(&error, PackError::Extract { source, .. }
                if source.raw_os_error() == Some(libc::EILSEQ)),
            "{error:?}"
        );
    }

    #[test]
    fn the_refusal_of_names_lists_one_line_per_problem() {
        let mut archive = container();
        archive
            .file("proj/README", 0o644, 1_000_000_000, b"1")
            .file("proj/Readme", 0o644, 1_000_000_000, b"2");
        let (_dir, staged) = staging();
        let package = package_file(&archive.finish());
        let error = unpack(&package.path, &staged, &case_insensitive()).expect_err("refused");
        assert_eq!(
            error.to_string(),
            "the package holds names that the target volume cannot hold:\n\
             `README` and `Readme` become one name on this volume"
        );
    }

    /// An archive of directories nested `depth` levels, one name each.
    fn nested(depth: usize) -> RawArchive {
        let mut archive = container();
        for level in 1..=depth {
            let path = format!("proj/{}", "d/".repeat(level));
            if path.len() > 100 {
                archive.long_name(path.as_bytes());
                archive.member(b"d", b'5', 0o755, 1_000_000_000, b"", b"");
            } else {
                archive.dir(&path, 0o755, 1_000_000_000);
            }
        }
        archive
    }

    #[test]
    fn a_member_more_than_512_levels_below_the_container_is_refused() {
        let sandbox = sandbox();
        sandbox.assert_refused_before_any_write(&nested(513), Refusal::TooDeep);
    }

    #[test]
    fn a_member_512_levels_below_the_container_is_extracted() {
        let (_dir, staged) = staging();
        let unpacked = unpack_bytes(&nested(512).finish(), &staged).expect("unpacks");
        assert_eq!(unpacked.members, 512);
    }
}
