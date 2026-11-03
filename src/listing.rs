// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archive listings: one JSON line per entry of an archive.

use crate::relpath::{RelPath, escape, unescape};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The type of a listed entry. Special files are not archived, so they
/// have no type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListingType {
    File,
    Dir,
    Symlink,
    Hardlink,
}

/// What a link points to: any bytes, in the escaped form as text. A
/// symlink target is not a path inside the piece, so it is not a
/// [`RelPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget(Vec<u8>);

impl LinkTarget {
    pub fn from_bytes(bytes: Vec<u8>) -> LinkTarget {
        LinkTarget(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for LinkTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&escape(&self.0))
    }
}

impl<'de> Deserialize<'de> for LinkTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        unescape(&text)
            .map(LinkTarget)
            .map_err(serde::de::Error::custom)
    }
}

/// One entry line of a listing, keys in this order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingEntry {
    /// Relative to the archive's top-level directory.
    pub path: RelPath,
    #[serde(rename = "type")]
    pub kind: ListingType,
    pub size: u64,
    /// Modification time in whole seconds since the epoch.
    pub mtime: i64,
    /// Permission bits.
    pub mode: u32,
    /// The target, for symlinks and hard links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkTarget>,
    /// SHA-256 of the content as lower-case hex, for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Errors of archive listings.
#[derive(Debug, thiserror::Error)]
pub enum ListingError {}

impl crate::error::HasKind for ListingError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relpath::RelPath;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn line(entry: &ListingEntry) -> String {
        serde_json::to_string(entry).expect("serialises")
    }

    #[test]
    fn a_file_entry_has_its_keys_in_order_and_a_checksum() {
        let entry = ListingEntry {
            path: path("src/main.rs"),
            kind: ListingType::File,
            size: 120,
            mtime: 1_600_000_000,
            mode: 0o644,
            link: None,
            sha256: Some("ab".repeat(32)),
        };
        assert_eq!(
            line(&entry),
            format!(
                r#"{{"path":"src/main.rs","type":"file","size":120,"mtime":1600000000,"mode":420,"sha256":"{}"}}"#,
                "ab".repeat(32)
            )
        );
    }

    #[test]
    fn a_directory_entry_has_neither_link_nor_checksum() {
        let entry = ListingEntry {
            path: path("src"),
            kind: ListingType::Dir,
            size: 0,
            mtime: 5,
            mode: 0o755,
            link: None,
            sha256: None,
        };
        assert_eq!(
            line(&entry),
            r#"{"path":"src","type":"dir","size":0,"mtime":5,"mode":493}"#
        );
    }

    #[test]
    fn a_symlink_entry_carries_its_target_in_the_escaped_form() {
        let entry = ListingEntry {
            path: path("link"),
            kind: ListingType::Symlink,
            size: 0,
            mtime: 5,
            mode: 0o777,
            link: Some(LinkTarget::from_bytes(b"../a%\xff".to_vec())),
            sha256: None,
        };
        assert_eq!(
            line(&entry),
            r#"{"path":"link","type":"symlink","size":0,"mtime":5,"mode":511,"link":"../a%25%FF"}"#
        );
    }

    #[test]
    fn a_hard_link_entry_names_the_path_it_links_to() {
        let entry = ListingEntry {
            path: path("copy"),
            kind: ListingType::Hardlink,
            size: 0,
            mtime: 5,
            mode: 0o644,
            link: Some(LinkTarget::from_bytes(b"original".to_vec())),
            sha256: None,
        };
        assert_eq!(
            line(&entry),
            r#"{"path":"copy","type":"hardlink","size":0,"mtime":5,"mode":420,"link":"original"}"#
        );
    }

    #[test]
    fn every_line_reads_back_as_the_entry_it_was_written_from() {
        let lines = [
            r#"{"path":"a/%FF","type":"file","size":1,"mtime":-3,"mode":420,"sha256":"00"}"#,
            r#"{"path":"d","type":"dir","size":0,"mtime":0,"mode":493}"#,
            r#"{"path":"l","type":"symlink","size":0,"mtime":0,"mode":511,"link":"x%FF"}"#,
            r#"{"path":"h","type":"hardlink","size":0,"mtime":0,"mode":420,"link":"a/b"}"#,
        ];
        for text in lines {
            let entry: ListingEntry = serde_json::from_str(text).expect("a valid line");
            assert_eq!(line(&entry), text);
        }
    }

    #[test]
    fn a_link_target_is_any_bytes() {
        let target = LinkTarget::from_bytes(b"/abs/../x\xc3".to_vec());
        assert_eq!(target.as_bytes(), b"/abs/../x\xc3");
        let text = serde_json::to_string(&target).expect("serialises");
        assert_eq!(text, r#""/abs/../x%C3""#);
        let back: LinkTarget = serde_json::from_str(&text).expect("deserialises");
        assert_eq!(back, target);
        assert!(serde_json::from_str::<LinkTarget>(r#""%41""#).is_err());
    }
}
