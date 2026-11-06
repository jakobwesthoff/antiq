// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The end state of a world, to compare two worlds or one world before and
//! after a command.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Entry {
    Dir,
    Link(String),
    File {
        mode: u32,
        mtime: i64,
        content: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct State {
    entries: BTreeMap<String, Entry>,
}

/// The lock file is rewritten by every command that takes it.
const LOCK: &str = ".antiq/lock";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_tree(root: &Path) -> BTreeMap<String, Entry> {
    fn walk(base: &Path, dir: &Path, seen: &mut BTreeMap<String, Entry>) {
        for item in fs::read_dir(dir).expect("the directory lists") {
            let path = item.expect("an entry").path();
            let name = path
                .strip_prefix(base)
                .expect("below the root")
                .display()
                .to_string();
            if name == LOCK {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).expect("the entry has metadata");
            if metadata.is_symlink() {
                let target = fs::read_link(&path).expect("the link reads");
                seen.insert(name, Entry::Link(target.display().to_string()));
            } else if metadata.is_dir() {
                seen.insert(name, Entry::Dir);
                walk(base, &path, seen);
            } else {
                seen.insert(
                    name,
                    Entry::File {
                        mode: metadata.mode() & 0o7777,
                        mtime: metadata.mtime(),
                        content: fs::read(&path).expect("the file reads"),
                    },
                );
            }
        }
    }
    let mut seen = BTreeMap::new();
    walk(root, root, &mut seen);
    seen
}

/// The members of a zstd-compressed tar, one line each, so that archives
/// compare by what they hold and not by how they are compressed.
fn tar_members(compressed: &[u8]) -> Vec<u8> {
    let tar = zstd::stream::decode_all(compressed).expect("the archive decompresses");
    let mut archive = tar::Archive::new(&tar[..]);
    let mut lines = String::new();
    for entry in archive.entries().expect("the archive lists") {
        let mut entry = entry.expect("an entry");
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut content).expect("the member reads");
        let header = entry.header();
        lines.push_str(&format!(
            "{} {:?} {:o} {} {} {:?} {}\n",
            entry.path().expect("a path").display(),
            header.entry_type(),
            header.mode().expect("a mode"),
            header.size().expect("a size"),
            header.mtime().expect("a time"),
            header.link_name().expect("a link name"),
            hex(&Sha256::digest(&content)),
        ));
    }
    lines.into_bytes()
}

/// The ranges of `YYYY-MM-DDTHHMMSS[-N]` in `text`.
fn stamps_in(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let digits = |from: usize, count: usize| {
        bytes
            .get(from..from + count)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let mut found = Vec::new();
    let mut at = 0;
    while at + 17 <= bytes.len() {
        let shape = digits(at, 4)
            && bytes[at + 4] == b'-'
            && digits(at + 5, 2)
            && bytes[at + 7] == b'-'
            && digits(at + 8, 2)
            && bytes[at + 10] == b'T'
            && digits(at + 11, 6);
        if !shape {
            at += 1;
            continue;
        }
        let mut end = at + 17;
        if bytes.get(end) == Some(&b'-') && digits(end + 1, 1) {
            end += 1;
            while digits(end, 1) {
                end += 1;
            }
        }
        found.push((at, end));
        at = end;
    }
    found
}

fn replace_stamps(text: &str, labels: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut last = 0;
    for (start, end) in stamps_in(text) {
        out.push_str(&text[last..start]);
        out.push_str(&labels[&text[start..end]]);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

/// The sort key of a stamp: its time, then its counter as a number.
fn stamp_order(stamp: &str) -> (String, u64) {
    match stamp[17..].strip_prefix('-') {
        Some(counter) => (stamp[..17].to_string(), counter.parse().expect("digits")),
        None => (stamp[..17].to_string(), 1),
    }
}

/// The values of the keys that hold the real time of an action.
fn blank_times(text: &str) -> String {
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let kept = ["# archived ", "# imported ", "# rebuilt "]
            .iter()
            .find_map(|key| line.starts_with(key).then(|| format!("{key}<time>\n")));
        if let Some(kept) = kept {
            out.push_str(&kept);
        } else if let Some(rest) = line.strip_prefix("restored ") {
            let after = rest.split_once(' ').map_or("", |(_, tail)| tail);
            let tail = if after.is_empty() {
                "\n".to_string()
            } else {
                format!(" {after}")
            };
            out.push_str(&format!("restored <time>{tail}"));
        } else {
            out.push_str(line);
        }
    }
    out
}

fn blank_digests(text: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() == 64 {
            out.push_str("<sha256>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for character in text.chars() {
        if character.is_ascii_hexdigit() && !character.is_ascii_uppercase() {
            run.push(character);
        } else {
            flush(&mut run, &mut out);
            out.push(character);
        }
    }
    flush(&mut run, &mut out);
    out
}

impl State {
    /// Everything below `root`, exactly as it is: for a command that
    /// must change nothing.
    pub fn raw(root: &Path) -> State {
        State {
            entries: read_tree(root),
        }
    }

    /// Everything below `root` with what differs between two worlds
    /// replaced: the world's directories (`roots`, each with the label
    /// that stands for it), version stamps (by their order), the times of
    /// actions, digests of checksum files and file times. Archives and
    /// compressed listings are compared by their content.
    pub fn normalised(root: &Path, roots: &[(&Path, &str)]) -> State {
        let relabel = |text: String| {
            roots.iter().fold(text, |text, (path, label)| {
                text.replace(path.to_str().expect("a UTF-8 path"), label)
            })
        };
        let mut decoded: BTreeMap<String, Entry> = BTreeMap::new();
        for (name, entry) in read_tree(root) {
            let entry = match entry {
                Entry::Link(target) => Entry::Link(relabel(target)),
                Entry::File { mode, content, .. } => {
                    let content = if name.ends_with(".tar.zst") {
                        tar_members(&content)
                    } else if name.ends_with(".sha256.zst") {
                        zstd::stream::decode_all(&content[..]).expect("the listing decompresses")
                    } else {
                        content
                    };
                    let content = match String::from_utf8(content) {
                        Ok(text) => relabel(text).into_bytes(),
                        Err(error) => error.into_bytes(),
                    };
                    Entry::File {
                        mode,
                        mtime: 0,
                        content,
                    }
                }
                Entry::Dir => Entry::Dir,
            };
            decoded.insert(name, entry);
        }

        let mut stamps = std::collections::BTreeSet::new();
        for (name, entry) in &decoded {
            for (start, end) in stamps_in(name) {
                stamps.insert(name[start..end].to_string());
            }
            if let Entry::File { content, .. } = entry
                && let Ok(text) = std::str::from_utf8(content)
            {
                for (start, end) in stamps_in(text) {
                    stamps.insert(text[start..end].to_string());
                }
            }
        }
        let mut ordered: Vec<String> = stamps.into_iter().collect();
        ordered.sort_by_key(|stamp| stamp_order(stamp));
        let labels: BTreeMap<String, String> = ordered
            .into_iter()
            .enumerate()
            .map(|(index, stamp)| (stamp, format!("<v{}>", index + 1)))
            .collect();

        let entries = decoded
            .into_iter()
            .map(|(name, entry)| {
                let entry = match entry {
                    Entry::File {
                        mode,
                        mtime,
                        content,
                    } => match String::from_utf8(content) {
                        Ok(text) => {
                            let mut text = blank_times(&replace_stamps(&text, &labels));
                            if name.ends_with(".sha256") {
                                text = blank_digests(&text);
                            }
                            Entry::File {
                                mode,
                                mtime,
                                content: text.into_bytes(),
                            }
                        }
                        Err(error) => Entry::File {
                            mode,
                            mtime,
                            content: error.into_bytes(),
                        },
                    },
                    other => other,
                };
                (replace_stamps(&name, &labels), entry)
            })
            .collect();
        State { entries }
    }

    pub fn paths(&self) -> Vec<&str> {
        self.entries.keys().map(String::as_str).collect()
    }

    pub fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// What differs, one line per path, or `None` when nothing does.
    pub fn diff(&self, other: &State) -> Option<String> {
        let mut lines = Vec::new();
        for (path, entry) in &self.entries {
            match other.entries.get(path) {
                None => lines.push(format!("only in the first: {path}")),
                Some(theirs) if theirs != entry => lines.push(format!(
                    "changed: {path}\n  first:  {}\n  second: {}",
                    describe(entry),
                    describe(theirs)
                )),
                Some(_) => {}
            }
        }
        for path in other.entries.keys() {
            if !self.entries.contains_key(path) {
                lines.push(format!("only in the second: {path}"));
            }
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

fn describe(entry: &Entry) -> String {
    match entry {
        Entry::Dir => "directory".to_string(),
        Entry::Link(target) => format!("link to {target}"),
        Entry::File {
            mode,
            mtime,
            content,
        } => {
            let shown = String::from_utf8_lossy(&content[..content.len().min(200)]);
            format!("file mode {mode:o} mtime {mtime} {shown:?}")
        }
    }
}
