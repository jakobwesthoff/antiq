// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packing, verifying and unpacking archives.

pub mod check;
pub mod extract;
pub mod hashing;
pub mod names;
pub mod pool;
pub mod read;
pub mod tar_write;
pub mod verify;
pub mod zstd;

use crate::filesystem::tree::TreeRoot;
use crate::listing::ListingEntry;
use crate::progress::Counter;
use crate::relpath::{Name, RelPath};
use crate::rules::PieceRules;
use crate::tree::{EntryKind, PieceTree, Problem, TreeEntry};
use hashing::HashingWriter;
use std::fmt;
use std::io::Write;
use std::path::Path;
use tar_write::{DirectoryStack, TarWriter};
use zstd::ZstdSettings;

/// The id of a package format, as the metadata stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageFormat(&'static str);

impl PackageFormat {
    /// A tar stream compressed with zstd and its content checksum.
    pub const TAR_ZSTD_1: PackageFormat = PackageFormat("tar+zstd/1");

    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for PackageFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// How a piece is packed.
#[derive(Debug, Clone, Copy)]
pub struct PackSettings<'a> {
    pub zstd: ZstdSettings,
    /// Counts the files and symlinks as they are packed, and gets their
    /// number as its total.
    pub counter: Option<&'a Counter>,
}

/// What packing found worth telling the user. Paths start with the name
/// of the archive's top-level directory, so the directory itself is the
/// path of that name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackNote {
    /// A fifo, socket or device file, which archives do not hold.
    SpecialFileSkipped { path: RelPath, kind: EntryKind },
    /// A modification time before 1970, stored as 1970.
    TimeClamped { path: RelPath },
    /// A file that ended before the size the walk recorded.
    FileChanged { path: RelPath },
}

impl fmt::Display for PackNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackNote::SpecialFileSkipped { path, kind } => {
                let kind = match kind {
                    EntryKind::Fifo => "a fifo",
                    EntryKind::Socket => "a socket",
                    EntryKind::CharDevice => "a character device",
                    EntryKind::BlockDevice => "a block device",
                    _ => "a file of unknown type",
                };
                write!(f, "`{path}` is {kind} and is not archived")
            }
            PackNote::TimeClamped { path } => write!(
                f,
                "the modification time of `{path}` is before 1970 and is stored as 1970"
            ),
            PackNote::FileChanged { path } => {
                write!(
                    f,
                    "`{path}` was shorter than recorded and is padded with zeros"
                )
            }
        }
    }
}

/// The result of packing a piece.
#[derive(Debug)]
pub struct Packed {
    /// Bytes of the compressed stream.
    pub size: u64,
    /// SHA-256 of the compressed stream, lower-case hex.
    pub sha256: String,
    /// One entry per member below the top-level directory, in the order of
    /// the archive.
    pub listing: Vec<ListingEntry>,
    pub notes: Vec<PackNote>,
}

/// Rules for comparing a package or a listing with a tree.
#[derive(Debug, Default)]
pub struct WalkRules {
    /// The ignore and timeless rules the walk of a user's folder applies,
    /// so that the folder is compared with what packing it would archive.
    /// The default holds none, which is right for a staged tree.
    pub rules: PieceRules,
    /// The tree is antiq's own staging, where the extraction's final pass
    /// may have left a directory whose mode keeps its owner out. Only then
    /// does the strict verify give such a directory owner access for the
    /// read and put its mode back, and only then do directory times count.
    pub staged: bool,
    /// The folder stays where it is, so what only stops a removal is no
    /// problem for it: locked entries, and the folder being a mount root.
    pub kept: bool,
}

/// What the volume that receives an extraction can hold. Extraction
/// takes the rules as a value, so tests choose them and a real volume is
/// probed for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRules {
    /// `README` and `Readme` are one name.
    pub case_insensitive: bool,
    /// `café` spelled with one code point and with two are one name.
    pub normalization_insensitive: bool,
    /// The volume stores a name in its decomposed form (HFS+), so the
    /// name it holds is longer than the one that was written.
    pub decomposes: bool,
    /// A name that is not valid UTF-8 cannot be created.
    pub requires_utf8: bool,
    /// A second name for a file can be created.
    pub hard_links: bool,
    /// A symlink can be created.
    pub symlinks: bool,
    /// The longest symlink target in bytes, as stored.
    pub symlink_target_limit: usize,
    /// The longest name one directory entry can have.
    pub name_limit: names::NameLimit,
}

/// The proof that a folder equals a package or a listing. Only the
/// comparisons in [`verify`] make one.
///
/// ```compile_fail
/// // The fields are private, so a value cannot be made without a
/// // comparison that found no difference.
/// let _ = antiq::archive::Verified { record: vec![], listing: vec![] };
/// ```
#[derive(Debug)]
pub struct Verified {
    record: Vec<verify::LstatRecord>,
    listing: Vec<ListingEntry>,
}

impl Verified {
    /// The `lstat` of every entry of the folder, in path order.
    pub fn record(&self) -> &[verify::LstatRecord] {
        &self.record
    }

    /// What the folder was compared with: the members of the package in
    /// archive order, or the listing given.
    pub fn listing(&self) -> &[ListingEntry] {
        &self.listing
    }
}

/// What an extraction reports.
#[derive(Debug, PartialEq, Eq)]
pub struct Unpacked {
    /// Permission bits the archive stores for its top-level directory.
    /// The staging root keeps the mode its owner gave it; the caller sets
    /// this one before it moves the tree into place.
    pub top_mode: u32,
    /// Modification time of that directory, in seconds since the epoch.
    /// Extraction sets it on the staging root.
    pub top_mtime: u64,
    /// Members extracted below the top-level directory.
    pub members: usize,
    /// Regular files among them: symlinks, hard links and directories do
    /// not count, as in the `files` of a listing.
    pub files: u64,
}

/// Packs, verifies, lists and unpacks one package format.
pub trait Packager {
    fn format(&self) -> PackageFormat;

    /// Packs the piece below `root` into `out`. `tree` is the walk of that
    /// piece and must have no problems; `pack` refuses one that has.
    fn pack(
        &self,
        root: &TreeRoot,
        tree: &PieceTree,
        settings: &PackSettings<'_>,
        out: &mut dyn Write,
    ) -> Result<Packed, PackError>;

    fn list(&self, package: &Path) -> Result<Vec<ListingEntry>, PackError>;

    fn unpack(
        &self,
        package: &Path,
        staging: &TreeRoot,
        rules: &TargetRules,
    ) -> Result<Unpacked, PackError>;
}

/// The one package format of version 1: tar compressed with zstd.
#[derive(Debug, Default, Clone, Copy)]
pub struct TarZstd;

impl Packager for TarZstd {
    fn format(&self) -> PackageFormat {
        PackageFormat::TAR_ZSTD_1
    }

    fn pack(
        &self,
        root: &TreeRoot,
        tree: &PieceTree,
        settings: &PackSettings<'_>,
        out: &mut dyn Write,
    ) -> Result<Packed, PackError> {
        // An incomplete walk would give an archive that silently lacks
        // files, so nothing is written for it.
        if let Some(problem) = tree.problems().first() {
            return Err(PackError::TreeHasProblems(problem.clone()));
        }
        if tree.entries().first().is_none_or(|top| !top.path.is_root()) {
            return Err(PackError::NoTopDirectory);
        }
        let folder = tree
            .top()
            .components()
            .last()
            .ok_or(PackError::NoContainerName)?;
        let container =
            Name::from_bytes(folder.to_vec()).expect("a component of a valid path is a valid name");

        // The bar follows the content bytes of the files among the tar
        // stream bytes the compressor has consumed. Where the content lies
        // in the stream is known before the first byte.
        let compressed = HashingWriter::new(out);
        let encoder = match settings.counter {
            Some(counter) => {
                let layout = tar_write::layout(tree.packed_entries(), &container);
                let clock = tar_write::ContentClock::new(layout.spans);
                counter.set_total(clock.total());
                zstd::encoder_clocked(compressed, &settings.zstd, counter, clock)
            }
            None => zstd::encoder(compressed, &settings.zstd),
        }
        .map_err(PackError::Write)?;
        let mut writer = TarWriter::new(encoder, container);
        let mut directories = DirectoryStack::new(root);
        let mut content = |entry: &TreeEntry| directories.open(entry);
        for entry in tree.packed_entries() {
            writer.add(entry, &mut content)?;
        }
        let (encoder, listing, notes) = writer.finish()?;
        let mut compressed = encoder.finish().map_err(PackError::Write)?;
        // The encoder does not flush the writer under it.
        compressed.flush().map_err(PackError::Write)?;
        Ok(Packed {
            size: compressed.written(),
            sha256: compressed.sha256(),
            listing,
            notes,
        })
    }

    fn list(&self, package: &Path) -> Result<Vec<ListingEntry>, PackError> {
        read::list(package)
    }

    fn unpack(
        &self,
        package: &Path,
        staging: &TreeRoot,
        rules: &TargetRules,
    ) -> Result<Unpacked, PackError> {
        extract::unpack(package, staging, rules)
    }
}

/// Errors of packing and unpacking.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum PackError {
    #[error("cannot read `{path}`")]
    Read {
        path: RelPath,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write the package")]
    Write(#[source] std::io::Error),
    #[error("the walk of the piece has no entry for its top directory")]
    NoTopDirectory,
    #[error("the piece has no folder name to name the archive's top-level directory after")]
    NoContainerName,
    #[error("the walk of the piece found a problem: {0:?}")]
    TreeHasProblems(Problem),
    #[error("cannot walk the piece")]
    Walk(
        #[from]
        #[diagnostic_source]
        crate::walk::WalkError,
    ),
    #[error("cannot open the package")]
    OpenPackage(#[source] std::io::Error),
    #[error("the package is damaged: {0}")]
    Damaged(String),
    #[error("cannot put the mode of `{path}` back to {mode:o}, so it keeps owner read and search")]
    RestoreMode {
        path: RelPath,
        mode: u32,
        #[source]
        source: std::io::Error,
    },
    #[error("the folder differs from the package: {}", verify::describe(.0))]
    Differs(Vec<verify::Difference>),
    #[error("cannot read the package")]
    Archive(#[source] std::io::Error),
    #[error("cannot read `{path}`")]
    ReadFile {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the archive file `{file}` is not the one that was stored: {what}")]
    ArchiveDiffers { file: String, what: String },
    #[error("the checksum file cannot be used: {0}")]
    ChecksumFile(String),
    #[error("the archive differs from its listing: {}", verify::describe_against_listing(.0))]
    ListingDiffers(Vec<verify::Difference>),
    #[error("the package holds no members")]
    NoMembers,
    #[error("the package holds a member that cannot be extracted: `{member}`: {refusal}")]
    Refused {
        /// The member's path as the archive spells it, in escaped form.
        member: String,
        refusal: extract::Refusal,
    },
    #[error("the package holds names that the target volume cannot hold:\n{}", lines(.0))]
    UnholdableNames(Vec<names::NameProblem>),
    #[error("cannot extract `{path}`")]
    Extract {
        path: RelPath,
        #[source]
        source: std::io::Error,
    },
}

/// One line per problem.
fn lines(problems: &[names::NameProblem]) -> String {
    let lines: Vec<String> = problems.iter().map(ToString::to_string).collect();
    lines.join("\n")
}

impl crate::error::HasKind for PackError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_format_id_is_tar_zstd_1() {
        assert_eq!(TarZstd.format().as_str(), "tar+zstd/1");
        assert_eq!(TarZstd.format().to_string(), "tar+zstd/1");
    }

    #[test]
    fn notes_print_as_sentences() {
        let path = RelPath::parse("proj/a").expect("valid");
        let skipped = |kind| PackNote::SpecialFileSkipped {
            path: path.clone(),
            kind,
        };
        let lines = [
            skipped(EntryKind::Fifo),
            skipped(EntryKind::Socket),
            skipped(EntryKind::CharDevice),
            skipped(EntryKind::BlockDevice),
            skipped(EntryKind::Unknown),
            PackNote::TimeClamped { path: path.clone() },
            PackNote::FileChanged { path },
        ]
        .map(|note| note.to_string());
        assert_eq!(
            lines,
            [
                "`proj/a` is a fifo and is not archived",
                "`proj/a` is a socket and is not archived",
                "`proj/a` is a character device and is not archived",
                "`proj/a` is a block device and is not archived",
                "`proj/a` is a file of unknown type and is not archived",
                "the modification time of `proj/a` is before 1970 and is stored as 1970",
                "`proj/a` was shorter than recorded and is padded with zeros",
            ]
        );
    }
}

/// Packing real trees through a `TreeRoot`.
#[cfg(test)]
mod pack_tests {
    use super::*;
    use crate::testing::deep_path;
    use crate::testing::tree::Tree;
    use crate::tree::PieceTree;
    use hashing::HashingReader;
    use std::io::{BufWriter, Read};
    use std::process::Command;

    /// A walk of the tree below `root`, as the piece walk of a later item
    /// will report it.
    fn walk(root: &TreeRoot, top: &str) -> PieceTree {
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
        PieceTree::new(RelPath::parse(top).expect("valid"), entries, vec![])
    }

    fn fast() -> PackSettings<'static> {
        PackSettings {
            zstd: ZstdSettings::fast(),
            counter: None,
        }
    }

    fn pack(tree: &Tree, top: &str) -> (Vec<u8>, Packed) {
        let root = TreeRoot::open(tree.root()).expect("the tree opens");
        let walked = walk(&root, top);
        let mut out = Vec::new();
        let packed = TarZstd
            .pack(&root, &walked, &fast(), &mut out)
            .expect("the piece packs");
        (out, packed)
    }

    #[test]
    fn ignored_entries_and_the_restored_marker_do_not_reach_the_archive() {
        use crate::testing::package::pack_piece_with;
        let mut tree = Tree::new();
        tree.file("proj/keep", "k");
        tree.file("proj/node_modules/dep/index.js", "x");
        tree.file("proj/.antiq-restored", "r");
        tree.file("proj/.antiqignore", "# nothing\n");
        let rules = PieceRules::new(&["node_modules/".to_string()], &[]).expect("valid rules");

        let packaged = pack_piece_with(&tree, "proj", &rules);

        let listed: Vec<String> = packaged
            .packed
            .listing
            .iter()
            .map(|line| line.path.to_string())
            .collect();
        assert_eq!(listed, [".antiqignore", "keep"]);
        let names: Vec<String> = members(&packaged.bytes)
            .into_iter()
            .map(|member| member.0)
            .collect();
        assert_eq!(names, ["proj/", "proj/.antiqignore", "proj/keep"]);
    }

    /// (path, type, link name, data) of every member of a package.
    fn members(package: &[u8]) -> Vec<(String, tar::EntryType, Option<String>, Vec<u8>)> {
        let decoder = zstd::decoder(package).expect("a zstd stream");
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .expect("a tar stream")
            .map(|entry| {
                let mut entry = entry.expect("a member");
                let mut data = Vec::new();
                entry.read_to_end(&mut data).expect("member data");
                (
                    String::from_utf8_lossy(&entry.path_bytes()).into_owned(),
                    entry.header().entry_type(),
                    entry
                        .link_name_bytes()
                        .map(|name| String::from_utf8_lossy(&name).into_owned()),
                    data,
                )
            })
            .collect()
    }

    fn sha256(bytes: &[u8]) -> String {
        let mut reader = HashingReader::new(bytes);
        std::io::copy(&mut reader, &mut std::io::sink()).expect("hashes");
        reader.sha256()
    }

    #[test]
    fn packing_counts_the_content_bytes_of_the_files_and_ends_at_their_sum() {
        let mut tree = Tree::new();
        tree.file("proj/a.txt", "alpha");
        tree.file("proj/src/big.bin", "0123456789".repeat(5_000));
        tree.symlink("proj/link", "a.txt");
        tree.hard_link("proj/a.txt", "proj/src/a-copy.txt");
        let root = TreeRoot::open(tree.root()).expect("the tree opens");
        let walked = walk(&root, "proj");
        let counter = Counter::new();
        let settings = PackSettings {
            zstd: ZstdSettings::fast(),
            counter: Some(&counter),
        };
        let mut out = Vec::new();
        TarZstd
            .pack(&root, &walked, &settings, &mut out)
            .expect("the piece packs");

        // The 5 bytes of a.txt once, whatever its second name, and the
        // 50 000 of big.bin.
        assert_eq!((counter.done(), counter.total()), (50_005, 50_005));
    }

    #[test]
    fn a_piece_packs_into_members_below_a_container_named_after_the_folder() {
        let mut tree = Tree::new();
        tree.file_with("src/main.rs", "fn main() {}", 0o600, 1_000_000_000);
        tree.file("notes.txt", "hello");
        tree.symlink("src/link", "main.rs");
        tree.hard_link("notes.txt", "src/notes-copy.txt");
        let (package, packed) = pack(&tree, "projects/proj");
        let found = members(&package);
        let paths: Vec<_> = found.iter().map(|m| m.0.as_str()).collect();
        assert_eq!(
            paths,
            [
                "proj/",
                "proj/notes.txt",
                "proj/src/",
                "proj/src/link",
                "proj/src/main.rs",
                "proj/src/notes-copy.txt",
            ]
        );
        assert_eq!(found[1].3, b"hello");
        assert_eq!(found[3].1, tar::EntryType::Symlink);
        assert_eq!(found[3].2.as_deref(), Some("main.rs"));
        assert_eq!(found[5].1, tar::EntryType::Link);
        assert_eq!(found[5].2.as_deref(), Some("proj/notes.txt"));
        let listed: Vec<_> = packed.listing.iter().map(|e| e.path.to_string()).collect();
        assert_eq!(
            listed,
            [
                "notes.txt",
                "src",
                "src/link",
                "src/main.rs",
                "src/notes-copy.txt"
            ]
        );
        assert!(packed.notes.is_empty());
    }

    #[test]
    fn files_in_sibling_and_nested_directories_are_read_from_their_own_directory() {
        let mut tree = Tree::new();
        tree.file("a/one", "1");
        tree.file("a/sub/two", "2");
        tree.file("a/three", "3");
        tree.file("b/four", "4");
        tree.file("b/sub/five", "5");
        let (package, _) = pack(&tree, "proj");
        let contents: Vec<_> = members(&package)
            .into_iter()
            .filter(|member| member.1 == tar::EntryType::Regular)
            .map(|member| (member.0, member.3))
            .collect();
        let expected: Vec<(String, Vec<u8>)> = [
            ("proj/a/one", "1"),
            ("proj/a/sub/two", "2"),
            ("proj/a/three", "3"),
            ("proj/b/four", "4"),
            ("proj/b/sub/five", "5"),
        ]
        .iter()
        .map(|(path, data)| (path.to_string(), data.as_bytes().to_vec()))
        .collect();
        assert_eq!(contents, expected);
    }

    #[test]
    fn the_size_and_digest_describe_the_compressed_stream_once_it_is_flushed() {
        let mut tree = Tree::new();
        tree.file("a", "some content");
        let root = TreeRoot::open(tree.root()).expect("opens");
        let walked = walk(&root, "proj");
        let mut sink = Vec::new();
        let packed = {
            let mut buffered = BufWriter::new(&mut sink);
            TarZstd
                .pack(&root, &walked, &fast(), &mut buffered)
                .expect("packs")
        };
        assert_eq!(packed.size, sink.len() as u64);
        assert_eq!(packed.sha256, sha256(&sink));

        // The pack itself flushed: a buffered sink holds everything
        // before it is dropped.
        let mut buffered_sink = Vec::new();
        let mut buffered = BufWriter::new(&mut buffered_sink);
        let again = TarZstd
            .pack(&root, &walked, &fast(), &mut buffered)
            .expect("packs");
        assert_eq!(buffered.buffer().len(), 0, "nothing is left unwritten");
        drop(buffered);
        assert_eq!(again.size, buffered_sink.len() as u64);
    }

    #[test]
    fn packing_the_same_tree_twice_gives_the_same_bytes_and_a_touched_file_changes_them() {
        let mut tree = Tree::new();
        let file = tree.file_with("a", "same", 0o644, 1_000_000_000);
        let (first, _) = pack(&tree, "proj");
        let (second, _) = pack(&tree, "proj");
        assert_eq!(first, second);
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1_000_000_001, 0))
            .expect("the time can be set");
        let (touched, _) = pack(&tree, "proj");
        assert_ne!(first, touched);
    }

    #[test]
    fn a_fifo_is_skipped_and_reported() {
        let mut tree = Tree::new();
        tree.file("a", "x");
        tree.fifo("queue");
        let (package, packed) = pack(&tree, "proj");
        assert!(members(&package).iter().all(|m| !m.0.contains("queue")));
        let notes: Vec<_> = packed.notes.iter().map(PackNote::to_string).collect();
        assert_eq!(notes, ["`proj/queue` is a fifo and is not archived"]);
    }

    #[test]
    fn a_1969_modification_time_is_stored_as_zero_and_reported() {
        let mut tree = Tree::new();
        let path = tree.file("old", "x");
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(-5, 0))
            .expect("a time before 1970 can be set");
        let (_, packed) = pack(&tree, "proj");
        assert_eq!(packed.listing[0].mtime, 0);
        let notes: Vec<_> = packed.notes.iter().map(PackNote::to_string).collect();
        assert_eq!(
            notes,
            ["the modification time of `proj/old` is before 1970 and is stored as 1970"]
        );
    }

    #[test]
    fn a_file_that_shrank_after_the_walk_is_padded_and_reported() {
        let mut tree = Tree::new();
        let path = tree.file("log", "0123456789");
        let root = TreeRoot::open(tree.root()).expect("opens");
        let walked = walk(&root, "proj");
        std::fs::write(&path, "0123").expect("the file shrinks");
        let mut out = Vec::new();
        let packed = TarZstd
            .pack(&root, &walked, &fast(), &mut out)
            .expect("packs");
        let found = members(&out);
        assert_eq!(found[1].3, b"0123\0\0\0\0\0\0");
        let notes: Vec<_> = packed.notes.iter().map(PackNote::to_string).collect();
        assert_eq!(
            notes,
            ["`proj/log` was shorter than recorded and is padded with zeros"]
        );
    }

    #[test]
    fn a_file_that_vanished_after_the_walk_fails_the_pack_naming_it() {
        let mut tree = Tree::new();
        let path = tree.file("dir/gone", "x");
        let root = TreeRoot::open(tree.root()).expect("opens");
        let walked = walk(&root, "proj");
        std::fs::remove_file(path).expect("the file is removed");
        let error = TarZstd
            .pack(&root, &walked, &fast(), &mut Vec::new())
            .expect_err("the file is gone");
        assert!(
            matches!(error, PackError::Read { ref path, .. } if path.to_string() == "dir/gone"),
            "{error:?}"
        );
    }

    #[test]
    fn a_file_replaced_by_a_fifo_after_the_walk_fails_the_pack_naming_it() {
        let mut tree = Tree::new();
        let path = tree.file("swapped", "x");
        let root = TreeRoot::open(tree.root()).expect("opens");
        let walked = walk(&root, "proj");
        std::fs::remove_file(path).expect("the file is removed");
        tree.fifo("swapped");
        let error = TarZstd
            .pack(&root, &walked, &fast(), &mut Vec::new())
            .expect_err("a fifo is not read");
        assert!(
            matches!(error, PackError::Read { ref path, .. } if path.to_string() == "swapped"),
            "{error:?}"
        );
    }

    #[test]
    fn a_file_below_a_path_longer_than_the_platform_limit_packs() {
        let tree = Tree::new();
        let root = TreeRoot::open(tree.root()).expect("opens");
        let deep = deep_path(6);
        for level in 1..=6 {
            let directory: Vec<&[u8]> = deep.components().take(level).collect();
            root.create_dir(
                &RelPath::from_bytes(directory.join(&b'/')).expect("valid"),
                0o755,
            )
            .expect("the directory is created");
        }
        let file = deep.concat(&RelPath::parse("data.txt").expect("valid"));
        {
            use std::io::Write;
            let mut created = root.create_file(&file, 0o644).expect("the file is created");
            created.write_all(b"deep").expect("written");
        }
        let walked = walk(&root, "proj");
        let mut out = Vec::new();
        let packed = TarZstd
            .pack(&root, &walked, &fast(), &mut out)
            .expect("the piece packs");
        let found = members(&out);
        let last = found.last().expect("members");
        assert_eq!(last.0, format!("proj/{file}"));
        assert!(last.0.len() > 1024);
        assert_eq!(last.3, b"deep");
        assert_eq!(packed.listing.len(), 7);
    }

    #[test]
    fn a_tree_with_problems_or_without_its_top_is_refused() {
        let tree = Tree::new();
        let root = TreeRoot::open(tree.root()).expect("opens");
        let walked = walk(&root, "proj");
        let top = RelPath::parse("proj").expect("valid");
        let entries = walked.entries().to_vec();

        for problem in [
            Problem::Unreadable {
                first: RelPath::parse("locked").expect("valid"),
            },
            Problem::MountPoint {
                other: RelPath::parse("mnt").expect("valid"),
            },
            Problem::Excluded {
                path: RelPath::parse("skip").expect("valid"),
            },
            Problem::NestedCollection {
                root: RelPath::parse("inner").expect("valid"),
            },
        ] {
            let flawed = PieceTree::new(top.clone(), entries.clone(), vec![problem.clone()]);
            let mut out = Vec::new();
            let error = TarZstd
                .pack(&root, &flawed, &fast(), &mut out)
                .expect_err("a flawed walk is refused");
            assert!(matches!(&error, PackError::TreeHasProblems(p) if *p == problem));
            assert!(out.is_empty(), "nothing was written");
        }

        let headless = PieceTree::new(top, vec![], vec![]);
        let error = TarZstd
            .pack(&root, &headless, &fast(), &mut Vec::new())
            .expect_err("no top entry");
        assert!(matches!(error, PackError::NoTopDirectory));

        let nameless = PieceTree::new(RelPath::root(), entries, vec![]);
        let error = TarZstd
            .pack(&root, &nameless, &fast(), &mut Vec::new())
            .expect_err("no folder name");
        assert!(matches!(error, PackError::NoContainerName));
    }

    #[test]
    fn a_sink_that_fails_makes_the_pack_fail_with_a_write_error() {
        // A slice of length zero accepts no byte, which `write_all` reports
        // as an error.
        let mut full: &mut [u8] = &mut [];
        let mut tree = Tree::new();
        tree.file("a", "x");
        let root = TreeRoot::open(tree.root()).expect("opens");
        let walked = walk(&root, "proj");
        let error = TarZstd
            .pack(&root, &walked, &fast(), &mut full)
            .expect_err("nothing can be written");
        assert!(matches!(error, PackError::Write(_)), "{error:?}");
    }

    /// The tools that set and read a user extended attribute, with their
    /// arguments before the path.
    #[cfg(target_os = "macos")]
    const XATTR_TOOLS: (&str, &[&str], &str, &[&str]) = (
        "xattr",
        &["-w", "user.antiq", "1"],
        "xattr",
        &["-p", "user.antiq"],
    );
    #[cfg(not(target_os = "macos"))]
    const XATTR_TOOLS: (&str, &[&str], &str, &[&str]) = (
        "setfattr",
        &["-n", "user.antiq", "-v", "1"],
        "getfattr",
        &["-n", "user.antiq"],
    );

    /// Sets a user extended attribute with the platform's tool and reads
    /// it back, so that the test fails naming the tool when the file
    /// system refuses it.
    fn set_xattr(path: &Path) {
        let (set_tool, set, get_tool, get) = XATTR_TOOLS;
        let status = Command::new(set_tool)
            .args(set)
            .arg(path)
            .status()
            .unwrap_or_else(|error| panic!("{set_tool} could not run: {error}"));
        assert!(status.success(), "{set_tool} failed for {}", path.display());
        let status = Command::new(get_tool)
            .args(get)
            .arg(path)
            .output()
            .unwrap_or_else(|error| panic!("{get_tool} could not run: {error}"))
            .status;
        assert!(
            status.success(),
            "{get_tool} cannot read the attribute back"
        );
    }

    #[test]
    fn an_archive_has_only_plain_member_types() {
        let mut tree = Tree::new();
        let marked = tree.file("marked", "x");
        set_xattr(&marked);
        tree.dir("dir");
        tree.symlink("link", "marked");
        tree.hard_link("marked", "again");
        let (package, _) = pack(&tree, "proj");
        let allowed = [
            tar::EntryType::Regular,
            tar::EntryType::Directory,
            tar::EntryType::Symlink,
            tar::EntryType::Link,
        ];
        let found = members(&package);
        assert_eq!(found.len(), 5);
        for member in found {
            assert!(
                allowed.contains(&member.1),
                "{} is a {:?}",
                member.0,
                member.1
            );
        }
    }
}
