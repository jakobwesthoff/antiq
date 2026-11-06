// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The output of the observed mirror: `-ii --info=progress2` with the
//! format `%i %l %n%L`, cut into segments (see `Call::run_observed`).
//!
//! rsync lists every entry it looks at, with its length in front of the
//! name, and writes progress updates between the lines. The report wants
//! the plain `--itemize-changes` listing; the progress card wants the
//! entries, the lengths and the progress totals.

use super::summary::split_item;

/// What one segment of the observed output is.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Segment {
    /// A progress update. `total` is the `T` of `to-chk=R/T` or
    /// `ir-chk=R/T`, when the update has one.
    Progress { total: Option<u64> },
    /// A line that names an entry.
    Entry(Entry),
    /// Any other line of rsync's output.
    Other,
}

/// A line that names an entry, as `--itemize-changes` would print it
/// when the entry changed.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Entry {
    /// The entry is as it was; `-ii` lists it, `-i` does not.
    pub unchanged: bool,
    /// A `*deleting` line.
    pub deleting: bool,
    /// The length of a file that rsync sends, 0 for every other entry.
    pub sent_bytes: u64,
    /// The line as `--itemize-changes` prints it: without the length.
    pub plain: Vec<u8>,
}

/// The total of a progress update such as `... (xfr#3, to-chk=4/7)`.
fn progress_total(update: &[u8]) -> Option<u64> {
    let marker = b"chk=";
    let at = update.windows(marker.len()).position(|w| w == marker)? + marker.len();
    let rest = &update[at..];
    let slash = rest.iter().position(|byte| *byte == b'/')?;
    let digits: Vec<u8> = rest[slash + 1..]
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

/// Reads one segment of the observed output.
pub(super) fn segment(bytes: &[u8]) -> Segment {
    // rsync starts every progress update with a carriage return.
    if bytes.starts_with(b"\r") {
        return Segment::Progress {
            total: progress_total(bytes),
        };
    }
    let (line, newline) = match bytes.strip_suffix(b"\n") {
        Some(line) => (line, &b"\n"[..]),
        None => (bytes, &b""[..]),
    };
    let Some((flags, field)) = split_item(line) else {
        return Segment::Other;
    };
    // The length is the first word of the field. A line without one is
    // kept as it is.
    let length = field
        .iter()
        .position(|byte| *byte == b' ')
        .filter(|space| *space > 0 && field[..*space].iter().all(u8::is_ascii_digit));
    let Some(space) = length else {
        return Segment::Other;
    };
    let digits = std::str::from_utf8(&field[..space]).expect("ASCII digits are UTF-8");
    let length: u64 = digits.parse().unwrap_or(0);
    let deleting = flags == b"*deleting";
    let mut plain = line[..line.len() - field.len()].to_vec();
    plain.extend_from_slice(&field[space + 1..]);
    plain.extend_from_slice(newline);
    Segment::Entry(Entry {
        unchanged: !deleting && flags[2..].iter().all(|byte| *byte == b' '),
        deleting,
        sent_bytes: if matches!(flags[0], b'<' | b'>') && flags[1] == b'f' {
            length
        } else {
            0
        },
        plain,
    })
}

/// What the report keeps of a segment of the observed output: the plain
/// `--itemize-changes` listing of what changed, and every line that is not
/// about an entry. Progress updates and unchanged entries are dropped.
pub(super) fn plain_listing(bytes: &[u8]) -> Option<Vec<u8>> {
    match segment(bytes) {
        Segment::Progress { .. } => None,
        Segment::Entry(entry) if entry.unchanged => None,
        Segment::Entry(entry) => Some(entry.plain),
        Segment::Other => Some(bytes.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(text: &str) -> Option<String> {
        plain_listing(text.as_bytes()).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    #[test]
    fn changes_lose_their_length_and_keep_everything_else() {
        assert_eq!(
            kept(">f+++++++++ 5 a.txt\n").as_deref(),
            Some(">f+++++++++ a.txt\n")
        );
        assert_eq!(
            kept(">f.st...... 120 d/sp ace.txt\n").as_deref(),
            Some(">f.st...... d/sp ace.txt\n")
        );
        assert_eq!(
            kept("cL+++++++++ 5 link -> a.txt\n").as_deref(),
            Some("cL+++++++++ link -> a.txt\n")
        );
        assert_eq!(
            kept("hf+++++++++ 5 b.txt => a.txt\n").as_deref(),
            Some("hf+++++++++ b.txt => a.txt\n")
        );
        assert_eq!(
            kept("<f.st...... 7 a.txt\n").as_deref(),
            Some("<f.st...... a.txt\n")
        );
    }

    #[test]
    fn deletions_directories_and_attribute_changes_are_kept() {
        assert_eq!(
            kept("*deleting   0 gone/f\n").as_deref(),
            Some("*deleting   gone/f\n")
        );
        assert_eq!(
            kept("cd+++++++++ 96 sub/\n").as_deref(),
            Some("cd+++++++++ sub/\n")
        );
        assert_eq!(
            kept(".d..t...... 96 sub/\n").as_deref(),
            Some(".d..t...... sub/\n")
        );
        assert_eq!(
            kept(".f...p..... 5 a.txt\n").as_deref(),
            Some(".f...p..... a.txt\n")
        );
    }

    #[test]
    fn unchanged_entries_of_every_type_are_dropped() {
        for line in [
            ".d          224 ./\n",
            ".f          5 a.txt\n",
            ".L          5 link -> a.txt\n",
            "hf          5 hard.txt\n",
            ".D          0 dev\n",
            ".S          0 sock\n",
        ] {
            assert_eq!(kept(line), None, "{line:?}");
        }
    }

    #[test]
    fn progress_updates_are_dropped_whether_or_not_they_end_a_line() {
        assert_eq!(
            kept("\r              7  28%    0.00kB/s    0:00:00  "),
            None
        );
        assert_eq!(
            kept("\r              7  28%    0.00kB/s    0:00:00 (xfr#1, to-chk=5/7)\n"),
            None
        );
    }

    #[test]
    fn other_lines_are_kept_as_they_are() {
        for line in [
            "sending incremental file list\n",
            "IO error encountered -- skipping file deletion\n",
            "\n",
            "rsync: [sender] opendir \"/c/x\" failed: Permission denied (13)\n",
        ] {
            assert_eq!(kept(line).as_deref(), Some(line));
        }
    }

    #[test]
    fn a_line_without_a_length_is_kept_as_it_is() {
        for line in [
            ">f+++++++++ projects/a/x\n",
            ">f+++++++++ my notes/a.md\n",
            ">f+++++++++ 12\n",
            ">f+++++++++ many a.txt\n",
        ] {
            assert_eq!(kept(line).as_deref(), Some(line), "{line:?}");
        }
    }

    #[test]
    fn bytes_that_are_not_utf8_pass_through() {
        let line = b">f+++++++++ 5 caf\xe9.txt\n";
        assert_eq!(
            plain_listing(line).as_deref(),
            Some(&b">f+++++++++ caf\xe9.txt\n"[..])
        );
    }

    #[test]
    fn a_segment_without_a_newline_at_the_end_of_the_output_is_read_too() {
        assert_eq!(
            kept(">f+++++++++ 5 a.txt").as_deref(),
            Some(">f+++++++++ a.txt")
        );
    }

    fn entry(text: &str) -> Entry {
        let found = segment(text.as_bytes());
        match found {
            Segment::Entry(entry) => Some(entry),
            _ => None,
        }
        .expect("an entry")
    }

    #[test]
    fn only_files_that_rsync_sends_have_sent_bytes() {
        assert_eq!(entry(">f+++++++++ 40 a\n").sent_bytes, 40);
        assert_eq!(entry(">f..t...... 40 a\n").sent_bytes, 40);
        assert_eq!(entry("<f+++++++++ 40 a\n").sent_bytes, 40);
        assert_eq!(entry("<f.st...... 40 a\n").sent_bytes, 40);
        for line in [
            "hf+++++++++ 40 b => a\n",
            "hf.st...... 40 b => a\n",
            "cd+++++++++ 96 d/\n",
            ".d..t...... 96 d/\n",
            "cL+++++++++ 5 l -> a\n",
            ".f          40 a\n",
            "*deleting   0 gone\n",
        ] {
            assert_eq!(entry(line).sent_bytes, 0, "{line:?}");
        }
    }

    #[test]
    fn deleting_lines_are_told_apart() {
        assert!(entry("*deleting   0 gone/\n").deleting);
        assert!(!entry("*deleting   0 gone/\n").unchanged);
        assert!(!entry(">f+++++++++ 5 a\n").deleting);
    }

    #[test]
    fn a_progress_update_gives_its_total() {
        let total = |text: &str| {
            let found = segment(text.as_bytes());
            match found {
                Segment::Progress { total } => Some(total),
                _ => None,
            }
            .expect("progress")
        };
        assert_eq!(
            total("\r  7  28%  0.00kB/s  0:00:00 (xfr#1, to-chk=5/7)\n"),
            Some(7)
        );
        assert_eq!(
            total("\r  7  28%  0.00kB/s  0:00:00 (xfr#1, ir-chk=15/19)"),
            Some(19)
        );
        assert_eq!(total("\r  7  28%  0.00kB/s  0:00:00  "), None);
        assert_eq!(total("\r  7  28%  (xfr#1, to-chk=5/)"), None);
        assert_eq!(total("\r  7  28%  (xfr#1, to-chk=5)"), None);
    }

    // =========================================================
    // Real rsync
    // =========================================================

    use crate::target::rsync::{Options, Rsync};
    use crate::target::url::TargetUrl;
    use std::path::Path;

    fn real_rsync() -> Rsync {
        let path_var = std::env::var_os("PATH").expect("PATH is set");
        Rsync::resolve(None, &path_var).expect(
            "these tests need rsync 3.2.4 or newer on PATH or in the usual install directories",
        )
    }

    /// The tree the mirror tests send: a space in a name, a symbolic link, a
    /// hard link and a directory.
    fn collection(root: &Path) {
        std::fs::create_dir_all(root.join("d")).expect("dirs");
        std::fs::write(root.join("a.txt"), "hello").expect("file");
        std::fs::write(root.join("d/sp ace.txt"), "xx yy").expect("file");
        std::os::unix::fs::symlink("a.txt", root.join("lnk")).expect("link");
        std::fs::hard_link(root.join("a.txt"), root.join("hard.txt")).expect("hard link");
    }

    fn plain_options(rsync_args: &[&str]) -> Options {
        Options {
            fsync: false,
            rsync_path: None,
            rsync_args: rsync_args.iter().map(ToString::to_string).collect(),
        }
    }

    /// What the observed mirror keeps for the report, and the sums
    /// `Progress` would see.
    fn observed_run(rsync: &Rsync, from: &Path, to: &Path, rsync_args: &[&str]) -> (Vec<u8>, u64) {
        let url = TargetUrl::Path {
            root: to.to_path_buf(),
        };
        let mut sent = 0;
        let outcome = rsync
            .mirror_observed(&plain_options(rsync_args), from, &url)
            .run_observed(&mut |bytes| {
                if let Segment::Entry(entry) = segment(bytes) {
                    sent += entry.sent_bytes;
                }
                plain_listing(bytes).map(Into::into)
            })
            .expect("rsync runs");
        assert_eq!(outcome.code, Some(0));
        (outcome.stdout, sent)
    }

    #[test]
    fn an_observed_mirror_leaves_the_listing_of_a_plain_one() {
        let rsync = real_rsync();
        let temp = tempfile::tempdir().expect("a temp dir");
        let from = temp.path().join("from");
        collection(&from);
        let (plain_to, observed_to) = (temp.path().join("plain"), temp.path().join("observed"));
        for to in [&plain_to, &observed_to] {
            std::fs::create_dir_all(to.join("gone")).expect("dirs");
            std::fs::write(to.join("gone/f"), "old").expect("file");
        }
        let url = TargetUrl::Path {
            root: plain_to.clone(),
        };
        let plain = rsync
            .mirror(&plain_options(&[]), &from, &url, false)
            .run()
            .expect("rsync runs");

        // A format of the user's, which the observed run must not follow.
        let (listing, sent) = observed_run(&rsync, &from, &observed_to, &["--out-format=%i %n%L"]);

        assert_eq!(
            super::super::summary::parse_itemized(&listing),
            super::super::summary::parse_itemized(&plain.stdout)
        );
        assert!(super::super::summary::has_deletion(&listing));
        assert_eq!(sent, 5 + 5, "a.txt and the space file, not the links");
    }

    #[test]
    fn an_observed_mirror_that_changes_nothing_keeps_nothing() {
        let rsync = real_rsync();
        let temp = tempfile::tempdir().expect("a temp dir");
        let (from, to) = (temp.path().join("from"), temp.path().join("to"));
        collection(&from);
        observed_run(&rsync, &from, &to, &[]);

        let (listing, sent) = observed_run(&rsync, &from, &to, &[]);

        assert_eq!(String::from_utf8_lossy(&listing), "");
        assert_eq!(sent, 0);
    }
}
