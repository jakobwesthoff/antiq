// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Packages for the tests of reading and verifying them.

use crate::archive::zstd::{ZstdSettings, encoder};
use crate::archive::{PackSettings, Packager, Packed, TarZstd};
use crate::filesystem::tree::TreeRoot;
use crate::relpath::RelPath;
use crate::rules::PieceRules;
use crate::select::Rules;
use crate::testing::tree::Tree;
use crate::walk::walk_piece;
use std::io::Write;

/// A package and what packing it reported.
pub struct Packaged {
    pub bytes: Vec<u8>,
    pub packed: Packed,
}

/// Packs the directory `piece` of `tree`, walked the way the real walk
/// does, with no ignore or timeless rules.
pub fn pack_piece(tree: &Tree, piece: &str) -> Packaged {
    pack_piece_with(tree, piece, &PieceRules::none())
}

/// Like [`pack_piece`], walking with `piece_rules`.
pub fn pack_piece_with(tree: &Tree, piece: &str, piece_rules: &PieceRules) -> Packaged {
    let collection = TreeRoot::open(tree.root()).expect("the tree opens");
    let top = RelPath::parse(piece).expect("a valid piece path");
    let rules = Rules::new(&[], &[]).expect("no patterns are valid");
    let walked = walk_piece(&collection, &top, &rules, piece_rules).expect("the walk succeeds");
    let root = collection.sub(&top).expect("the piece opens");
    let settings = PackSettings {
        zstd: ZstdSettings::fast(),
        counter: None,
    };
    let mut bytes = Vec::new();
    let packed = TarZstd
        .pack(&root, &walked, &settings, &mut bytes)
        .expect("the piece packs");
    Packaged { bytes, packed }
}

/// Writes `bytes` to a file in a fresh temp directory, which lives as long
/// as the returned guard.
pub fn on_disk(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("package.tar.zst");
    std::fs::write(&path, bytes).expect("the package is written");
    (dir, path)
}

/// `tar` compressed the way packages are, for archives a test builds by
/// hand.
pub fn compress(tar: &[u8]) -> Vec<u8> {
    let mut encoder = encoder(Vec::new(), &ZstdSettings::fast()).expect("an encoder");
    encoder.write_all(tar).expect("a vec accepts bytes");
    encoder.finish().expect("the stream ends")
}

/// One member of a hand-built tar: raw header fields, so a test can write
/// what the tar builder refuses.
pub struct RawMember<'a> {
    pub path: &'a [u8],
    pub kind: u8,
    pub mode: u32,
    pub mtime: u64,
    pub link: &'a [u8],
    pub data: &'a [u8],
}

impl<'a> RawMember<'a> {
    pub fn dir(path: &'a [u8]) -> RawMember<'a> {
        RawMember {
            path,
            kind: b'5',
            mode: 0o755,
            mtime: 1_000_000_000,
            link: b"",
            data: b"",
        }
    }

    pub fn file(path: &'a [u8], data: &'a [u8]) -> RawMember<'a> {
        RawMember {
            path,
            kind: b'0',
            mode: 0o644,
            mtime: 1_000_000_000,
            link: b"",
            data,
        }
    }

    pub fn link(path: &'a [u8], kind: u8, target: &'a [u8]) -> RawMember<'a> {
        RawMember {
            path,
            kind,
            mode: 0o644,
            mtime: 1_000_000_000,
            link: target,
            data: b"",
        }
    }
}

/// The bytes of a tar holding `members` in order, ended by two zero blocks.
pub fn raw_tar(members: &[RawMember<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for member in members {
        let mut header = tar::Header::new_gnu();
        let old = header.as_old_mut();
        old.name[..member.path.len()].copy_from_slice(member.path);
        old.linkname[..member.link.len()].copy_from_slice(member.link);
        header.set_entry_type(tar::EntryType::new(member.kind));
        header.set_mode(member.mode);
        header.set_mtime(member.mtime);
        header.set_size(member.data.len() as u64);
        header.set_cksum();
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(member.data);
        out.resize(out.len().next_multiple_of(512), 0);
    }
    out.extend_from_slice(&[0; 1024]);
    out
}

/// The listing in the code block of the spec's "Listing format" section,
/// which the writer must reproduce byte for byte.
pub fn spec_listing_example() -> String {
    let spec = include_str!("../../docs/specs/spec-v1.md");
    let section = spec
        .split("\n## Listing format\n")
        .nth(1)
        .expect("the spec has a Listing format section");
    let block = section
        .split("```text\n")
        .nth(1)
        .expect("the section opens with an example");
    block
        .split("```")
        .next()
        .expect("the example is closed")
        .to_string()
}
