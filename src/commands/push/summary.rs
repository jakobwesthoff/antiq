// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a mirror run changed on the target, from rsync's
//! `--itemize-changes` output, grouped by piece.

use crate::relpath::RelPath;
use serde::Serialize;
use std::collections::HashMap;

/// How one path changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    Created,
    Updated,
    Deleted,
}

/// One itemized change of a file, link or other non-directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub change: Change,
    /// The path relative to the collection root, as bytes.
    pub path: Vec<u8>,
}

/// The changes of one group of paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub created: u64,
    pub updated: u64,
    pub deleted: u64,
}

/// The counts of one piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PieceChanges {
    pub piece: RelPath,
    #[serde(flatten)]
    pub counts: Counts,
}

/// The changes of a run: the pieces that changed in path order, then
/// everything outside every piece.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub pieces: Vec<PieceChanges>,
    pub other: Counts,
}

impl Changes {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty() && self.other == Counts::default()
    }
}

/// Splits an itemized line into its flags and the path field. A line is
/// itemized when it has rsync's eleven character change field, a space and
/// a path, or is a `*deleting` line.
fn split_item(line: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(path) = line.strip_prefix(b"*deleting   ") {
        return Some((b"*deleting", path));
    }
    let shaped = line.len() > 12
        && line[11] == b' '
        && b"<>ch.*".contains(&line[0])
        && b"fdLDS".contains(&line[1]);
    shaped.then(|| (&line[..11], &line[12..]))
}

/// Without `-8`, rsync prints every byte outside ASCII as `\#ooo` in octal.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut at = 0;
    while at < field.len() {
        let octal = field.get(at..at + 5).filter(|chunk| {
            chunk.starts_with(b"\\#") && chunk[2..].iter().all(|d| (b'0'..=b'7').contains(d))
        });
        match octal {
            Some(chunk) => {
                let value = chunk[2..]
                    .iter()
                    .fold(0u32, |acc, digit| acc * 8 + u32::from(digit - b'0'));
                out.push(u8::try_from(value).unwrap_or(b'?'));
                at += 5;
            }
            None => {
                out.push(field[at]);
                at += 1;
            }
        }
    }
    out
}

/// The path of an itemized path field, without the target of a link.
fn path_of(field: &[u8]) -> Vec<u8> {
    let end = [&b" -> "[..], &b" => "[..]]
        .iter()
        .filter_map(|arrow| field.windows(arrow.len()).position(|w| w == *arrow))
        .min()
        .unwrap_or(field.len());
    unescape(&field[..end])
}

/// The changes of non-directories in `stdout`; every other line is
/// ignored.
pub(super) fn parse_itemized(stdout: &[u8]) -> Vec<Item> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter_map(item_of_line)
        .collect()
}

/// The change an itemized line describes, if it is one of a non-directory.
pub(super) fn item_of_line(line: &[u8]) -> Option<Item> {
    let (flags, field) = split_item(line)?;
    item_of(flags, field)
}

fn item_of(flags: &[u8], field: &[u8]) -> Option<Item> {
    if flags == b"*deleting" {
        // A directory is listed with a trailing slash and counts through
        // the files that left with it.
        return (!field.ends_with(b"/")).then(|| Item {
            change: Change::Deleted,
            path: path_of(field),
        });
    }
    if flags[1] == b'd' {
        return None;
    }
    let created = flags[2..].iter().all(|byte| *byte == b'+');
    Some(Item {
        change: if created {
            Change::Created
        } else {
            Change::Updated
        },
        path: path_of(field),
    })
}

/// A change with the size rsync lists for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SizedItem {
    pub item: Item,
    /// The bytes of the file; 0 for a deletion.
    pub bytes: u64,
}

/// The changes of non-directories in the output of the counted dry run
/// (`%i %l %n%L`). A line whose size is not a number is left out.
pub(super) fn parse_sized(stdout: &[u8]) -> Vec<SizedItem> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let (flags, field) = split_item(line)?;
            // The size is the first word of the path field.
            let space = field.iter().position(|byte| *byte == b' ')?;
            let digits = &field[..space];
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let bytes = std::str::from_utf8(digits).ok()?.parse().ok()?;
            Some(SizedItem {
                item: item_of(flags, &field[space + 1..])?,
                bytes,
            })
        })
        .collect()
}

/// Whether any line of `stdout` is an itemized change, of a directory too.
pub(super) fn has_itemized_line(stdout: &[u8]) -> bool {
    stdout
        .split(|byte| *byte == b'\n')
        .any(|line| split_item(line).is_some())
}

/// Whether `stdout` has a line that deletes something.
pub(super) fn has_deletion(stdout: &[u8]) -> bool {
    stdout
        .split(|byte| *byte == b'\n')
        .any(|line| line.starts_with(b"*deleting"))
}

/// The folder a path belongs to.
pub(super) struct Owner {
    /// The index in the folders given.
    pub index: usize,
    /// The path lies in the store, not in the live folder.
    pub in_store: bool,
}

/// The longest of `folders` that `path` is, or lies below. A path in
/// `.antiq/pieces/<folder>/...` belongs to `<folder>` and is in the store.
pub(super) fn owner(path: &[u8], folders: &[RelPath]) -> Option<Owner> {
    let stored = path.strip_prefix(b".antiq/pieces/");
    let path = stored.unwrap_or(path);
    folders
        .iter()
        .enumerate()
        .filter(|(_, folder)| {
            let name = folder.as_bytes();
            path == name || (path.starts_with(name) && path[name.len()..].starts_with(b"/"))
        })
        .max_by_key(|(_, folder)| folder.as_bytes().len())
        .map(|(index, _)| Owner {
            index,
            in_store: stored.is_some(),
        })
}

/// Counts `items` per piece. `folders` are the live pieces and the store
/// folders; a path belongs to the longest of them that it is, or lies
/// below, and `.antiq/pieces/<folder>/...` belongs to `<folder>`.
pub(super) fn group(items: &[Item], folders: &[RelPath]) -> Changes {
    let mut per_folder: HashMap<usize, Counts> = HashMap::new();
    let mut changes = Changes::default();
    for item in items {
        let counts = match owner(&item.path, folders) {
            Some(owner) => per_folder.entry(owner.index).or_default(),
            None => &mut changes.other,
        };
        match item.change {
            Change::Created => counts.created += 1,
            Change::Updated => counts.updated += 1,
            Change::Deleted => counts.deleted += 1,
        }
    }
    let mut pieces: Vec<PieceChanges> = per_folder
        .into_iter()
        .map(|(index, counts)| PieceChanges {
            piece: folders[index].clone(),
            counts,
        })
        .collect();
    pieces.sort_by(|a, b| a.piece.as_bytes().cmp(b.piece.as_bytes()));
    changes.pieces = pieces;
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(text: &str) -> Vec<(Change, String)> {
        parse_itemized(text.as_bytes())
            .into_iter()
            .map(|item| {
                (
                    item.change,
                    String::from_utf8_lossy(&item.path).into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn created_updated_and_deleted_files_are_told_apart() {
        let text = ">f+++++++++ a/new.txt\n\
                    >f.st...... a/changed.txt\n\
                    .f..t...... a/touched.txt\n\
                    *deleting   a/gone.txt\n\
                    cL+++++++++ a/link -> target\n\
                    hf+++++++++ a/hard => a/new.txt\n";
        assert_eq!(
            items(text),
            [
                (Change::Created, "a/new.txt".into()),
                (Change::Updated, "a/changed.txt".into()),
                (Change::Updated, "a/touched.txt".into()),
                (Change::Deleted, "a/gone.txt".into()),
                (Change::Created, "a/link".into()),
                (Change::Created, "a/hard".into()),
            ]
        );
    }

    fn sized(text: &str) -> Vec<(Change, String, u64)> {
        parse_sized(text.as_bytes())
            .into_iter()
            .map(|sized| {
                (
                    sized.item.change,
                    String::from_utf8_lossy(&sized.item.path).into_owned(),
                    sized.bytes,
                )
            })
            .collect()
    }

    #[test]
    fn a_sized_listing_gives_every_change_its_bytes() {
        let text = ">f+++++++++ 5 a.txt\n\
                    >f.st...... 120 a/changed.txt\n\
                    cL+++++++++ 5 lnk -> a.txt\n\
                    >f+++++++++ 5 d/sp ace.txt\n\
                    *deleting   0 gone/f\n\
                    *deleting   0 stale\n";
        assert_eq!(
            sized(text),
            [
                (Change::Created, "a.txt".into(), 5),
                (Change::Updated, "a/changed.txt".into(), 120),
                (Change::Created, "lnk".into(), 5),
                (Change::Created, "d/sp ace.txt".into(), 5),
                (Change::Deleted, "gone/f".into(), 0),
                (Change::Deleted, "stale".into(), 0),
            ]
        );
    }

    #[test]
    fn directories_and_lines_without_a_size_are_not_sized_items() {
        let text = "sending incremental file list\n\
                    cd+++++++++ 96 d/\n\
                    *deleting   0 gone/\n\
                    >f+++++++++ many a.txt\n\
                    >f+++++++++ a.txt\n\
                    >f+++++++++ 12\n";
        assert_eq!(sized(text), []);
    }

    #[test]
    fn a_real_rsync_lists_what_the_parser_reads() {
        let path_var = std::env::var_os("PATH").expect("PATH is set");
        let rsync = crate::target::rsync::Rsync::resolve(None, &path_var).expect(
            "these tests need rsync 3.2.4 or newer on PATH or in the usual install directories",
        );
        let temp = tempfile::tempdir().expect("a temp dir");
        let (from, to) = (temp.path().join("from"), temp.path().join("to"));
        std::fs::create_dir_all(from.join("d")).expect("dirs");
        std::fs::create_dir_all(to.join("gone")).expect("dirs");
        std::fs::write(from.join("a.txt"), "hello").expect("file");
        std::fs::write(from.join("d/sp ace.txt"), "xx yy").expect("file");
        std::os::unix::fs::symlink("a.txt", from.join("lnk")).expect("link");
        std::fs::write(to.join("gone/f"), "old").expect("file");
        let options = crate::target::rsync::Options {
            fsync: false,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let url = crate::target::url::TargetUrl::Path { root: to.clone() };

        let outcome = rsync
            .mirror_counted(&options, &from, &url)
            .run()
            .expect("rsync runs");

        assert_eq!(outcome.code, Some(0));
        assert_eq!(
            sized(&String::from_utf8_lossy(&outcome.stdout)),
            [
                (Change::Created, "a.txt".into(), 5),
                (Change::Created, "lnk".into(), 5),
                (Change::Created, "d/sp ace.txt".into(), 5),
                (Change::Deleted, "gone/f".into(), 0),
            ]
        );
        assert!(!to.join("a.txt").exists(), "a dry run sends nothing");
    }

    #[test]
    fn directories_and_other_lines_are_not_items() {
        let text = "sending incremental file list\n\
                    created directory /srv/antiq\n\
                    cd+++++++++ a/\n\
                    .d..t...... a/\n\
                    *deleting   a/old/\n\
                    IO error encountered -- skipping file deletion\n\
                    \n\
                    >f short\n\
                    sent 1 bytes  received 2 bytes\n";
        assert_eq!(items(text), []);
    }

    #[test]
    fn escaped_bytes_in_a_path_are_decoded() {
        let found = parse_itemized(b">f+++++++++ caf\\#303\\#251.txt\n");
        assert_eq!(found[0].path, "caf\u{e9}.txt".as_bytes());
        let literal = parse_itemized(b">f+++++++++ odd\\#name\n");
        assert_eq!(literal[0].path, b"odd\\#name");
    }

    #[test]
    fn itemized_lines_of_directories_count_for_the_second_pass() {
        assert!(has_itemized_line(b"cd+++++++++ a/\n"));
        assert!(has_itemized_line(b">f+++++++++ a\n"));
        assert!(has_itemized_line(b"*deleting   a\n"));
        assert!(!has_itemized_line(
            b"sending incremental file list\ncreated directory x\n"
        ));
        assert!(!has_itemized_line(b""));
    }

    #[test]
    fn only_a_deleting_line_is_a_deletion() {
        assert!(has_deletion(b">f+++++++++ a\n*deleting   b\n"));
        assert!(!has_deletion(b">f+++++++++ a\nthe *deleting word\n"));
        assert!(!has_deletion(b""));
    }

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    fn item(change: Change, path: &str) -> Item {
        Item {
            change,
            path: path.as_bytes().to_vec(),
        }
    }

    fn counts(created: u64, updated: u64, deleted: u64) -> Counts {
        Counts {
            created,
            updated,
            deleted,
        }
    }

    #[test]
    fn paths_are_counted_for_the_longest_piece_they_lie_in() {
        let folders = [path("work"), path("work/deep"), path("other")];
        let changes = group(
            &[
                item(Change::Created, "work/a.txt"),
                item(Change::Updated, "work/deep/b.txt"),
                item(Change::Deleted, "work/deep/sub/c.txt"),
                item(Change::Created, "other"),
                item(Change::Created, "workshop/x"),
                item(Change::Updated, "loose.txt"),
            ],
            &folders,
        );
        assert_eq!(
            changes,
            Changes {
                pieces: vec![
                    PieceChanges {
                        piece: path("other"),
                        counts: counts(1, 0, 0)
                    },
                    PieceChanges {
                        piece: path("work"),
                        counts: counts(1, 0, 0)
                    },
                    PieceChanges {
                        piece: path("work/deep"),
                        counts: counts(0, 1, 1)
                    },
                ],
                other: counts(1, 1, 0),
            }
        );
    }

    #[test]
    fn the_store_of_a_folder_counts_for_the_folder() {
        let folders = [path("work")];
        let changes = group(
            &[
                item(
                    Change::Created,
                    ".antiq/pieces/work/2024-06-30T081245--work.tar.zst",
                ),
                item(Change::Created, ".antiq/config.toml"),
                item(Change::Created, ".antiq/pieces/unknown/x"),
            ],
            &folders,
        );
        assert_eq!(changes.pieces.len(), 1);
        assert_eq!(changes.pieces[0].piece, path("work"));
        assert_eq!(changes.pieces[0].counts, counts(1, 0, 0));
        assert_eq!(changes.other, counts(2, 0, 0));
    }

    #[test]
    fn no_items_is_an_empty_summary() {
        assert!(group(&[], &[path("work")]).is_empty());
        assert!(!group(&[item(Change::Created, "x")], &[]).is_empty());
    }
}
