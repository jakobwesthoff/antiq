// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The names of the files of a version in the store (spec "Store layout").
//!
//! `<name>--<version stamp><suffix>`: the part before the `--` is for
//! people only and nothing reads it back, so a name is parsed for its
//! stamp and its suffix alone.

use super::VersionsError;
use super::stamp::{MAX_COUNTER, Stamp};
use crate::relpath::RelPath;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

/// The longest file name a store file may get, in bytes.
const MAX_FILE_NAME: usize = 200;

/// `YYYY-MM-DDTHHMMSS`.
const STAMP_LEN: usize = 17;

/// The room a counter takes in the longest stamp: `-` and the digits of
/// [`MAX_COUNTER`].
const COUNTER_LEN: usize = 1 + MAX_COUNTER.ilog10() as usize + 1;

/// The longest suffix.
const LONGEST_SUFFIX: &str = ".tar.zst.sha256";

/// The bytes `<name>` may take: what is left of [`MAX_FILE_NAME`] after
/// `--`, the longest stamp and the longest suffix. It is the same for every
/// version of a piece, so every version of a piece has the same `<name>`.
pub const NAME_BUDGET: usize = MAX_FILE_NAME - 2 - STAMP_LEN - COUNTER_LEN - LONGEST_SUFFIX.len();

/// The three files of a version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VersionFile {
    /// The `.tar.zst`.
    Archive,
    /// The `.sha256.zst`.
    Listing,
    /// The `.tar.zst.sha256`.
    Checksum,
}

impl VersionFile {
    /// All three, the archive first.
    pub const ALL: [VersionFile; 3] = [
        VersionFile::Archive,
        VersionFile::Listing,
        VersionFile::Checksum,
    ];

    /// What follows the stamp in the file name.
    pub fn suffix(self) -> &'static str {
        match self {
            VersionFile::Archive => ".tar.zst",
            VersionFile::Listing => ".sha256.zst",
            VersionFile::Checksum => LONGEST_SUFFIX,
        }
    }
}

/// `<name>` for a piece: every character outside `[A-Za-z0-9._]` of the
/// piece path becomes `-`, repeated `-` collapse, and a result longer than
/// [`NAME_BUDGET`] loses its front. The cut falls after a `-`, so whole
/// components stay; a single component that is too long by itself is cut
/// inside. What is left is ASCII, so the cut is always at a character
/// boundary.
pub fn stem(piece: &RelPath) -> String {
    let mut name = String::new();
    for character in String::from_utf8_lossy(piece.as_bytes()).chars() {
        let kept = character.is_ascii_alphanumeric() || character == '.' || character == '_';
        if kept {
            name.push(character);
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    if name.len() <= NAME_BUDGET {
        return name;
    }
    let bytes = name.as_bytes();
    let start = (1..name.len())
        .filter(|start| bytes[start - 1] == b'-')
        .find(|start| name.len() - start <= NAME_BUDGET)
        .unwrap_or(name.len() - NAME_BUDGET);
    name[start..].to_string()
}

/// The file name of `kind` for the version `stamp` of `piece`.
pub fn file_name(piece: &RelPath, stamp: &Stamp, kind: VersionFile) -> String {
    format!("{}--{stamp}{}", stem(piece), kind.suffix())
}

/// The names of the three files of a version, as packing writes them and
/// the store moves them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionFileNames {
    pub archive: String,
    pub listing: String,
    pub checksum: String,
}

impl VersionFileNames {
    pub fn new(piece: &RelPath, stamp: &Stamp) -> VersionFileNames {
        VersionFileNames {
            archive: file_name(piece, stamp, VersionFile::Archive),
            listing: file_name(piece, stamp, VersionFile::Listing),
            checksum: file_name(piece, stamp, VersionFile::Checksum),
        }
    }

    /// The name of `kind`.
    pub fn of(&self, kind: VersionFile) -> &str {
        match kind {
            VersionFile::Archive => &self.archive,
            VersionFile::Listing => &self.listing,
            VersionFile::Checksum => &self.checksum,
        }
    }
}

/// The stamp and the kind of a store file name, or `None` when the name is
/// not one of a version's files. Only the suffix and the stamp are read:
/// the name is split at its last `--`, and whatever precedes that, bytes
/// that are no text included, is ignored.
pub fn parse_file_name(name: &OsStr) -> Option<(Stamp, VersionFile)> {
    let bytes = name.as_bytes();
    // `.tar.zst.sha256` first: no suffix is the ending of another.
    let (kind, rest) = [
        VersionFile::Checksum,
        VersionFile::Archive,
        VersionFile::Listing,
    ]
    .into_iter()
    .find_map(|kind| {
        bytes
            .strip_suffix(kind.suffix().as_bytes())
            .map(|rest| (kind, rest))
    })?;
    let split = rest.windows(2).rposition(|pair| pair == b"--")?;
    let stamp = std::str::from_utf8(&rest[split + 2..]).ok()?;
    match Stamp::parse(stamp) {
        Ok(stamp) => Some((stamp, kind)),
        Err(VersionsError::InvalidStamp(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn piece(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn stamp(text: &str) -> Stamp {
        Stamp::parse(text).expect("a valid stamp")
    }

    fn parsed(name: &str) -> Option<(Stamp, VersionFile)> {
        parse_file_name(OsStr::new(name))
    }

    #[test]
    fn the_budget_leaves_room_for_the_longest_file_name() {
        assert_eq!(NAME_BUDGET, 161);
        assert_eq!(COUNTER_LEN, 5);
    }

    #[test]
    fn a_path_becomes_its_components_joined_by_a_dash() {
        assert_eq!(stem(&piece("projects/scanner")), "projects-scanner");
    }

    #[test]
    fn characters_outside_the_set_become_a_dash_and_runs_collapse() {
        assert_eq!(stem(&piece("a  b/c")), "a-b-c");
        assert_eq!(stem(&piece("a/\u{e4}/b")), "a-b");
        assert_eq!(stem(&piece("a/?!/b")), "a-b");
        assert_eq!(stem(&piece("x--y/z")), "x-y-z");
    }

    #[test]
    fn dots_and_underscores_stay() {
        assert_eq!(stem(&piece(".hidden_dir/x.y")), ".hidden_dir-x.y");
    }

    #[test]
    fn bytes_that_are_no_text_become_a_dash() {
        let path = RelPath::from_bytes(b"a\xff\xfeb".to_vec()).expect("valid");
        assert_eq!(stem(&path), "a-b");
    }

    #[test]
    fn a_path_ending_in_a_replaced_character_ends_in_a_dash() {
        assert_eq!(stem(&piece("foo?")), "foo-");
    }

    #[test]
    fn a_long_path_is_cut_from_the_front_after_a_dash() {
        let components: Vec<String> = (0..100).map(|index| format!("c{index:02}")).collect();
        let path = piece(&components.join("/"));
        let cut = stem(&path);
        assert!(cut.len() <= NAME_BUDGET, "{}", cut.len());
        assert!(
            cut.len() > NAME_BUDGET - 4,
            "whole components fit: {}",
            cut.len()
        );
        assert!(cut.ends_with("c99"), "{cut}");
        assert!(
            cut.starts_with('c'),
            "the cut leaves no dash in front: {cut}"
        );
        assert_eq!(cut.split('-').next().expect("a component").len(), 3);
    }

    #[test]
    fn a_single_long_component_is_cut_inside() {
        let long = format!("{}{}", "a".repeat(250), "b".repeat(50));
        let cut = stem(&piece(&long));
        assert_eq!(cut.len(), NAME_BUDGET);
        assert!(cut.ends_with(&"b".repeat(50)));
        assert!(cut.starts_with('a'));
    }

    #[test]
    fn a_long_last_component_after_short_ones_is_cut_inside_too() {
        let path = piece(&format!("a/b/{}", "z".repeat(300)));
        assert_eq!(stem(&path), "z".repeat(NAME_BUDGET));
    }

    #[test]
    fn a_path_just_at_the_budget_is_kept_whole() {
        let exact = "q".repeat(NAME_BUDGET);
        assert_eq!(stem(&piece(&exact)), exact);
        let over = "q".repeat(NAME_BUDGET + 1);
        assert_eq!(stem(&piece(&over)), "q".repeat(NAME_BUDGET));
    }

    #[test]
    fn a_name_ending_in_a_dash_that_is_too_long_still_has_content() {
        let path = piece(&format!("{}?", "w".repeat(300)));
        let cut = stem(&path);
        assert_eq!(cut.len(), NAME_BUDGET);
        assert!(cut.ends_with("w-"));
    }

    #[test]
    fn the_longest_file_name_stays_within_200_bytes_for_every_version() {
        let path = piece(&"x".repeat(400));
        let worst = stamp("2026-11-04T090211").with_counter(MAX_COUNTER);
        for stamp in [stamp("2026-11-04T090211"), worst] {
            for kind in VersionFile::ALL {
                assert!(file_name(&path, &stamp, kind).len() <= 200);
            }
        }
        assert_eq!(file_name(&path, &worst, VersionFile::Checksum).len(), 200);
    }

    #[test]
    fn every_version_of_a_piece_has_the_same_name() {
        let path = piece(&"x".repeat(400));
        let first = file_name(&path, &stamp("2026-11-04T090211"), VersionFile::Archive);
        let later = file_name(&path, &stamp("2026-11-04T090211-12"), VersionFile::Archive);
        assert_eq!(
            first.split_once("--").expect("a split").0,
            later.split_once("--").expect("a split").0
        );
    }

    #[test]
    fn the_files_of_a_version_are_named_alike() {
        let names = VersionFileNames::new(&piece("old/client-site"), &stamp("2026-09-03T101500"));
        assert_eq!(names.archive, "old-client-site--2026-09-03T101500.tar.zst");
        assert_eq!(
            names.listing,
            "old-client-site--2026-09-03T101500.sha256.zst"
        );
        assert_eq!(
            names.checksum,
            "old-client-site--2026-09-03T101500.tar.zst.sha256"
        );
        for kind in VersionFile::ALL {
            assert_eq!(
                names.of(kind),
                file_name(&piece("old/client-site"), &stamp("2026-09-03T101500"), kind)
            );
        }
    }

    #[test]
    fn the_three_suffixes_are_read() {
        let expected = stamp("2026-09-03T101500");
        assert_eq!(
            parsed("a--2026-09-03T101500.tar.zst"),
            Some((expected, VersionFile::Archive))
        );
        assert_eq!(
            parsed("a--2026-09-03T101500.sha256.zst"),
            Some((expected, VersionFile::Listing))
        );
        assert_eq!(
            parsed("a--2026-09-03T101500.tar.zst.sha256"),
            Some((expected, VersionFile::Checksum))
        );
    }

    #[test]
    fn a_counter_is_part_of_the_stamp() {
        let (read, kind) = parsed("x--2026-11-04T090211-2.tar.zst").expect("parses");
        assert_eq!(read.counter(), 2);
        assert_eq!(kind, VersionFile::Archive);
    }

    #[test]
    fn the_name_is_split_at_the_last_double_dash() {
        let (read, _) = parsed("a--b--2026-11-04T090211.tar.zst").expect("parses");
        assert_eq!(read, stamp("2026-11-04T090211"));
    }

    #[test]
    fn a_name_ending_in_a_dash_is_split_after_all_its_dashes_but_two() {
        let (read, _) = parsed("a---2026-11-04T090211.tar.zst").expect("parses");
        assert_eq!(read, stamp("2026-11-04T090211"));
    }

    #[test]
    fn a_name_that_ends_in_a_dash_round_trips() {
        let stamp = stamp("2026-11-04T090211-3");
        for kind in VersionFile::ALL {
            let name = file_name(&piece("foo?"), &stamp, kind);
            assert!(name.starts_with("foo---"), "{name}");
            assert_eq!(parsed(&name), Some((stamp, kind)));
        }
    }

    #[test]
    fn an_empty_name_part_parses() {
        assert!(parsed("--2026-11-04T090211.tar.zst").is_some());
    }

    #[test]
    fn a_name_part_that_is_no_text_parses() {
        let mut name = OsString::from("caf");
        name.push(OsStr::from_bytes(b"\xff"));
        name.push("--2026-11-04T090211.tar.zst");
        assert!(parse_file_name(&name).is_some());
    }

    #[test]
    fn a_stamp_part_that_is_no_text_does_not_parse() {
        let mut name = OsString::from("a--2026-11-04T09021");
        name.push(OsStr::from_bytes(b"\xff"));
        name.push(".tar.zst");
        assert_eq!(parse_file_name(&name), None);
    }

    #[test]
    fn other_names_do_not_parse() {
        for name in [
            "a--2026-11-04T090211.tar.zst.sha256.bak",
            "a--2026-11-04T090211.tar.gz",
            "a--2026-11-04T090211",
            "a--2026-11-04T090211.zst",
            "a--stamp.tar.zst",
            "a--2026-11-04T090211-1.tar.zst",
            "a2026-11-04T090211.tar.zst",
            ".tar.zst",
            "notes.txt",
            "",
        ] {
            assert_eq!(parsed(name), None, "{name}");
        }
    }

    #[test]
    fn a_name_ending_in_a_dash_before_a_bad_stamp_does_not_parse() {
        assert_eq!(parsed("a---stamp.tar.zst"), None);
    }
}
