// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The version fixture: a store of versions written with antiq's own
//! packer and listing writer, and helpers that damage them.
//!
//! This file is shared: the binary tests include it through
//! `support::versions`, the library tests through `testing::versions`. It
//! reaches the library only through `antiq::` paths. The scenario harness
//! in `scenario/versions.rs` writes versions by hand instead, so that a
//! scenario checks antiq against the spec; the two share no code.

use super::tree::Tree;
use antiq::archive::check::ChecksumFile;
use antiq::archive::zstd::ZstdSettings;
use antiq::archive::{PackSettings, Packager, TarZstd};
use antiq::filesystem::tree::TreeRoot;
use antiq::listing::{self, Header};
use antiq::relpath::RelPath;
use antiq::rules::PieceRules;
use antiq::select::Rules;
use antiq::versions::damage::Places;
use antiq::versions::names::{VersionFile, VersionFileNames};
use antiq::versions::stamp::Stamp;
use antiq::versions::store::{StoreScan, scan};
use antiq::walk::walk_piece;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

/// A store: `pieces/` for the versions and `tmp/pack/` for archives that
/// are staged on their way in.
pub struct Store {
    root: Tree,
    pieces: PathBuf,
    tmp_pack: PathBuf,
}

impl Store {
    pub fn new() -> Store {
        let mut root = Tree::new();
        let pieces = root.dir("pieces");
        let tmp_pack = root.dir("tmp/pack");
        Store {
            root,
            pieces,
            tmp_pack,
        }
    }

    pub fn pieces_dir(&self) -> PathBuf {
        self.pieces.clone()
    }

    pub fn tmp_pack_dir(&self) -> PathBuf {
        self.tmp_pack.clone()
    }

    /// The two places `damage::inspect` looks in.
    pub fn places(&self) -> Places<'_> {
        Places {
            pieces_dir: &self.pieces,
            tmp_pack_dir: &self.tmp_pack,
        }
    }

    /// What `versions::store::scan` finds in the store now.
    pub fn scan(&self) -> StoreScan {
        scan(&self.pieces_dir()).expect("the store scans")
    }

    /// Packs the directory `piece` of `source` and stores it as the
    /// version `stamp`, with a listing and a checksum file as an
    /// `archive` writes them.
    pub fn add_version(&mut self, source: &Tree, piece: &str, stamp: &str) -> StoredVersion {
        let piece = RelPath::parse(piece).expect("a valid piece path");
        let stamp: Stamp = stamp.parse().expect("a valid stamp");
        let names = VersionFileNames::new(&piece, &stamp);

        let collection = TreeRoot::open(source.root()).expect("the source opens");
        let rules = Rules::new(&[], &[]).expect("no patterns are valid");
        let walked = walk_piece(&collection, &piece, &rules, &PieceRules::none())
            .expect("the walk succeeds");
        let root = collection.sub(&piece).expect("the piece opens");
        let settings = PackSettings {
            zstd: ZstdSettings::fast(),
        };
        let mut archive = Vec::new();
        let packed = TarZstd
            .pack(&root, &walked, &settings, &mut archive)
            .expect("the piece packs");

        let folder = self.root.dir(&format!("pieces/{piece}"));
        fs::write(folder.join(&names.archive), &archive).expect("the archive is written");
        let header = Header {
            piece: piece.clone(),
            archived: stamp.time(),
            archive: names.archive.clone(),
            archive_bytes: packed.size,
            imported: None,
            rebuilt: None,
        };
        let listing = File::create(folder.join(&names.listing)).expect("the listing is created");
        listing::write(&header, &packed.listing, listing).expect("the listing is written");
        let checksum = ChecksumFile::new(packed.sha256, names.archive.clone());
        fs::write(folder.join(&names.checksum), checksum.to_bytes())
            .expect("the checksum file is written");

        StoredVersion {
            piece,
            stamp,
            folder,
            names,
        }
    }
}

/// The three files of one version in a [`Store`], with helpers that damage
/// them. A helper changes the file it names and no other.
pub struct StoredVersion {
    pub piece: RelPath,
    pub stamp: Stamp,
    pub folder: PathBuf,
    pub names: VersionFileNames,
}

impl StoredVersion {
    pub fn path(&self, kind: VersionFile) -> PathBuf {
        self.folder.join(self.names.of(kind))
    }

    pub fn remove(&self, kind: VersionFile) {
        fs::remove_file(self.path(kind)).expect("the file exists");
    }

    /// Keeps the first `len` bytes of the file.
    pub fn truncate(&self, kind: VersionFile, len: u64) {
        File::options()
            .write(true)
            .open(self.path(kind))
            .and_then(|file| file.set_len(len))
            .expect("the file can be cut");
    }

    pub fn overwrite(&self, kind: VersionFile, bytes: &[u8]) {
        fs::write(self.path(kind), bytes).expect("the file is written");
    }

    /// Replaces the listing with `text`, compressed the way listings are.
    pub fn listing_from_text(&self, text: &[u8]) {
        let bytes = encode_listing(&[text]).0;
        fs::write(self.path(VersionFile::Listing), bytes).expect("the listing is written");
    }

    /// Replaces the listing with `change` applied to its text.
    pub fn edit_listing(&self, change: impl FnOnce(String) -> String) {
        let text = String::from_utf8(self.listing_text()).expect("the listing is UTF-8");
        self.listing_from_text(change(text).as_bytes());
    }

    /// The listing without the head line `# <key> ...`, compressed anew.
    pub fn listing_without(&self, key: &str) {
        let text = self.listing_text();
        let prefix = format!("# {key} ");
        let kept: Vec<u8> = text
            .split_inclusive(|byte| *byte == b'\n')
            .filter(|line| !line.starts_with(prefix.as_bytes()))
            .flatten()
            .copied()
            .collect();
        assert!(kept.len() < text.len(), "the listing has no `{key}` line");
        self.listing_from_text(&kept);
    }

    /// Cuts the listing inside the entries and leaves the head readable:
    /// the head ends a zstd block of its own, and the file stops a few
    /// bytes into the next one. A head-only read still finds the head; a
    /// read in full fails.
    pub fn cut_listing(&self) {
        let text = self.listing_text();
        let end_of_head = text
            .windows(3)
            .position(|window| window == b"\n#\n")
            .expect("the listing has a closed head")
            + 3;
        let (bytes, head_len) = encode_listing(&[&text[..end_of_head], &text[end_of_head..]]);
        let cut = head_len + 3;
        assert!(cut < bytes.len(), "the entries need a block of their own");
        fs::write(self.path(VersionFile::Listing), &bytes[..cut]).expect("the listing is cut");
    }

    fn listing_text(&self) -> Vec<u8> {
        let bytes = fs::read(self.path(VersionFile::Listing)).expect("the listing exists");
        zstd::stream::decode_all(&bytes[..]).expect("the listing decodes")
    }

    /// Appends a second line to the checksum file.
    pub fn checksum_with_two_lines(&self) {
        let mut bytes = fs::read(self.path(VersionFile::Checksum)).expect("the file exists");
        bytes.extend_from_within(..);
        fs::write(self.path(VersionFile::Checksum), bytes).expect("the file is written");
    }

    /// Copies the file of `kind` under the same stamp and suffix with
    /// `<name>` replaced by `other_name`, so the slot of `kind` holds two
    /// files. Returns the path of the copy.
    pub fn add_twin(&self, kind: VersionFile, other_name: &str) -> PathBuf {
        let twin = self
            .folder
            .join(format!("{other_name}--{}{}", self.stamp, kind.suffix()));
        fs::copy(self.path(kind), &twin).expect("the file is copied");
        twin
    }

    /// Copies the archive to where a running `archive` stages it, in
    /// `tmp/pack/<stamp>/<piece>/`. Returns the path of the copy.
    pub fn stage_archive(&self, store: &Store) -> PathBuf {
        let staged = store
            .tmp_pack_dir()
            .join(self.stamp.to_string())
            .join(self.piece.to_string());
        fs::create_dir_all(&staged).expect("the staging folder is created");
        let copy = staged.join(&self.names.archive);
        fs::copy(self.path(VersionFile::Archive), &copy).expect("the archive is copied");
        copy
    }
}

/// `parts` as one zstd frame at level 3 with the content checksum, each
/// part ending a block of its own. Returns the frame and the bytes up to
/// the end of the first part's block.
fn encode_listing(parts: &[&[u8]]) -> (Vec<u8>, usize) {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).expect("the encoder starts");
    encoder
        .include_checksum(true)
        .expect("the checksum is switched on");
    let mut first = 0;
    for (index, part) in parts.iter().enumerate() {
        encoder.write_all(part).expect("the part is written");
        encoder.flush().expect("the block ends");
        if index == 0 {
            first = encoder.get_ref().len();
        }
    }
    (encoder.finish().expect("the frame ends"), first)
}
