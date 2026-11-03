// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packing, verifying and unpacking archives.

pub mod extract;
pub mod frames;
pub mod hashing;
pub mod live;
pub mod manifest;
pub mod names;
pub mod pool;
pub mod read;
pub mod tar_write;
pub mod verify;
pub mod zstd;

use crate::fsx::tree::TreeRoot;
use crate::listing::ListingEntry;
use crate::relpath::RelPath;
use crate::tree::{EntryKind, PieceTree, Problem};
use std::fmt;
use std::io::Write;
use std::path::Path;
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
pub struct PackSettings {
    pub zstd: ZstdSettings,
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

/// Rules for comparing a package with a tree. Filled in by the strict
/// verify.
#[derive(Debug, Default)]
pub struct WalkRules;

/// Rules for the target of an extraction. Filled in by the extraction.
#[derive(Debug, Default)]
pub struct TargetRules;

/// What a strict verify records. Filled in by the strict verify.
#[derive(Debug)]
pub struct Verified;

/// What an extraction reports. Filled in by the extraction.
#[derive(Debug)]
pub struct Unpacked;

/// Packs, verifies, lists and unpacks one package format.
pub trait Packager {
    fn format(&self) -> PackageFormat;

    /// Packs the piece below `root` into `out`. `tree` is the walk of that
    /// piece and must have no problems; `pack` refuses one that has.
    fn pack(
        &self,
        root: &TreeRoot,
        tree: &PieceTree,
        settings: &PackSettings,
        out: &mut dyn Write,
    ) -> Result<Packed, PackError>;

    fn verify_strict(
        &self,
        package: &Path,
        root: &TreeRoot,
        rules: &WalkRules,
    ) -> Result<Verified, PackError>;

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
        _root: &TreeRoot,
        _tree: &PieceTree,
        _settings: &PackSettings,
        _out: &mut dyn Write,
    ) -> Result<Packed, PackError> {
        Err(PackError::NotImplemented("packing a piece"))
    }

    fn verify_strict(
        &self,
        package: &Path,
        root: &TreeRoot,
        rules: &WalkRules,
    ) -> Result<Verified, PackError> {
        verify::verify_strict(package, root, rules)
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
#[derive(Debug, thiserror::Error)]
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
    #[error("the piece cannot be packed: {0:?}")]
    TreeHasProblems(Problem),
    #[error("{0} is not implemented")]
    NotImplemented(&'static str),
}

impl crate::error::HasKind for PackError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::tree::TreeRoot;
    use crate::tree::PieceTree;

    fn empty_root() -> (tempfile::TempDir, TreeRoot) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = TreeRoot::open(dir.path()).expect("the dir opens");
        (dir, root)
    }

    #[test]
    fn the_format_id_is_tar_zstd_1() {
        assert_eq!(TarZstd.format().as_str(), "tar+zstd/1");
        assert_eq!(TarZstd.format().to_string(), "tar+zstd/1");
    }

    #[test]
    fn the_unfinished_operations_answer_not_implemented() {
        let (dir, root) = empty_root();
        let tree = PieceTree::new(RelPath::parse("p").expect("valid"), vec![], vec![]);
        let settings = PackSettings {
            zstd: ZstdSettings::live(1),
        };
        let mut sink = Vec::new();
        let errors = [
            TarZstd
                .pack(&root, &tree, &settings, &mut sink)
                .expect_err("packing is not there yet")
                .to_string(),
            TarZstd
                .verify_strict(dir.path(), &root, &WalkRules)
                .expect_err("not implemented")
                .to_string(),
            TarZstd
                .list(dir.path())
                .expect_err("not implemented")
                .to_string(),
            TarZstd
                .unpack(dir.path(), &root, &TargetRules)
                .expect_err("not implemented")
                .to_string(),
        ];
        assert_eq!(
            errors,
            [
                "packing a piece is not implemented",
                "the strict verify is not implemented",
                "listing a package is not implemented",
                "unpacking a package is not implemented",
            ]
        );
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
