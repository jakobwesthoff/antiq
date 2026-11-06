// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Collections for scenarios: a collection root with its `.antiq/` layout,
//! a config and pieces whose files have chosen ages.

use crate::support::tree::{COLLECTION_ID, Tree};
use filetime::FileTime;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One instant per scenario. Both worlds of a scenario are built from the
/// same clock, so every modification time in them is equal even though the
/// worlds are built seconds apart.
#[derive(Clone, Copy)]
pub struct Clock {
    base: i64,
}

impl Clock {
    /// The current second.
    pub fn now() -> Clock {
        let base = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_secs() as i64;
        Clock { base }
    }

    pub fn base(&self) -> i64 {
        self.base
    }

    /// Seconds since the epoch, `days` before the base.
    pub fn days_ago(&self, days: i64) -> i64 {
        self.base - days * 86_400
    }

    /// The time every directory of a built collection carries. A tar
    /// archive and a listing record directory times, and a directory time
    /// changes with each file created in it, so it is fixed to keep two
    /// worlds comparable.
    pub fn dir_time(&self) -> i64 {
        self.days_ago(400)
    }
}

enum Entry {
    File {
        relative: String,
        content: Vec<u8>,
        days: Option<i64>,
    },
    Dir(String),
    Symlink {
        relative: String,
        target: PathBuf,
    },
    HardLink {
        existing: String,
        relative: String,
    },
}

/// The text of an `.antiq-restored` marker written `days` before the
/// clock's base.
pub(super) fn restored_line(clock: &Clock, days: i64, version: Option<&str>) -> String {
    let time = jiff::Timestamp::from_second(clock.days_ago(days))
        .expect("the time is representable")
        .strftime("%Y-%m-%dT%H:%M:%SZ");
    match version {
        Some(stamp) => format!("restored {time} version {stamp}\n"),
        None => format!("restored {time}\n"),
    }
}

/// The content of one piece: files with ages, links, control files.
#[derive(Default)]
pub struct PieceBuilder {
    entries: Vec<Entry>,
    age: i64,
    pinned: bool,
    restored: Option<(i64, Option<String>)>,
}

impl PieceBuilder {
    /// The age in days of every file that does not name its own.
    pub fn age_days(&mut self, days: i64) -> &mut PieceBuilder {
        self.age = days;
        self
    }

    pub fn file(&mut self, relative: &str, content: impl AsRef<[u8]>) -> &mut PieceBuilder {
        self.entries.push(Entry::File {
            relative: relative.to_string(),
            content: content.as_ref().to_vec(),
            days: None,
        });
        self
    }

    pub fn file_aged(
        &mut self,
        relative: &str,
        content: impl AsRef<[u8]>,
        days: i64,
    ) -> &mut PieceBuilder {
        self.entries.push(Entry::File {
            relative: relative.to_string(),
            content: content.as_ref().to_vec(),
            days: Some(days),
        });
        self
    }

    pub fn dir(&mut self, relative: &str) -> &mut PieceBuilder {
        self.entries.push(Entry::Dir(relative.to_string()));
        self
    }

    pub fn symlink(&mut self, relative: &str, target: impl AsRef<Path>) -> &mut PieceBuilder {
        self.entries.push(Entry::Symlink {
            relative: relative.to_string(),
            target: target.as_ref().to_path_buf(),
        });
        self
    }

    /// A second name for the file `existing`, which an earlier call made.
    pub fn hard_link(&mut self, existing: &str, relative: &str) -> &mut PieceBuilder {
        self.entries.push(Entry::HardLink {
            existing: existing.to_string(),
            relative: relative.to_string(),
        });
        self
    }

    /// Lines of the piece's top `.antiqignore`.
    pub fn rule_ignore(&mut self, lines: &[&str]) -> &mut PieceBuilder {
        self.file(".antiqignore", lines.join("\n") + "\n")
    }

    /// Lines of the piece's top `.antiq-timeless`.
    pub fn rule_timeless(&mut self, lines: &[&str]) -> &mut PieceBuilder {
        self.file(".antiq-timeless", lines.join("\n") + "\n")
    }

    pub fn pinned(&mut self) -> &mut PieceBuilder {
        self.pinned = true;
        self
    }

    /// An `.antiq-restored` marker with the time `days` before the
    /// clock's base and optionally the version it names.
    pub fn restored(&mut self, days: i64, version: Option<&str>) -> &mut PieceBuilder {
        self.restored = Some((days, version.map(str::to_string)));
        self
    }

    pub(super) fn write(&self, clock: &Clock, tree: &mut Tree, piece: &str) {
        tree.dir(piece);
        let aged = |relative: &str, content: &[u8], days: i64, tree: &mut Tree| {
            let path = tree.file(&format!("{piece}/{relative}"), content);
            filetime::set_file_mtime(&path, FileTime::from_unix_time(clock.days_ago(days), 0))
                .expect("the time can be set");
        };
        for entry in &self.entries {
            match entry {
                Entry::File {
                    relative,
                    content,
                    days,
                } => aged(relative, content, days.unwrap_or(self.age), tree),
                Entry::Dir(relative) => {
                    tree.dir(&format!("{piece}/{relative}"));
                }
                Entry::Symlink { relative, target } => {
                    tree.symlink(&format!("{piece}/{relative}"), target);
                }
                Entry::HardLink { existing, relative } => {
                    tree.hard_link(
                        &format!("{piece}/{existing}"),
                        &format!("{piece}/{relative}"),
                    );
                }
            }
        }
        if self.pinned {
            aged(".antiq-pin", b"", self.age, tree);
        }
        if let Some((days, version)) = &self.restored {
            let line = restored_line(clock, *days, version.as_deref());
            aged(".antiq-restored", line.as_bytes(), self.age, tree);
        }
    }
}

/// The config keys a scenario sets. A key left unset is not written, so
/// its default applies.
#[derive(Default)]
struct ConfigKeys {
    id: Option<String>,
    patterns: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    ignore: Option<Vec<String>>,
    timeless: Option<Vec<String>>,
    idle_days: Option<u32>,
    soon_days: Option<u32>,
}

/// A TOML string list; a JSON string is a valid TOML basic string.
fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|item| serde_json::to_string(item).expect("a string serialises"))
        .collect();
    format!("[{}]", quoted.join(", "))
}

impl ConfigKeys {
    fn render(&self) -> String {
        let id = self.id.as_deref().unwrap_or(COLLECTION_ID);
        let mut text = format!(
            "[collection]\nid = {}\n",
            serde_json::to_string(id).expect("a string serialises")
        );
        let lists = [
            ("patterns", &self.patterns),
            ("exclude", &self.exclude),
            ("ignore", &self.ignore),
            ("timeless", &self.timeless),
        ];
        for (key, value) in lists {
            if let Some(items) = value {
                text.push_str(&format!("{key} = {}\n", toml_list(items)));
            }
        }
        for (key, value) in [("idle_days", self.idle_days), ("soon_days", self.soon_days)] {
            if let Some(days) = value {
                text.push_str(&format!("{key} = {days}\n"));
            }
        }
        text
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// The `(path, content)` files of a stored version.
pub type StoredFiles = Vec<(String, Vec<u8>)>;

pub struct CollectionBuilder {
    clock: Clock,
    keys: ConfigKeys,
    pieces: Vec<(String, PieceBuilder)>,
    outside: Vec<(String, Vec<u8>)>,
    versions: Vec<(String, String, StoredFiles)>,
    initialised: bool,
}

impl CollectionBuilder {
    /// The collection ID; the default is `tree::COLLECTION_ID`, so two
    /// worlds of a scenario agree on it.
    pub fn id(&mut self, id: &str) -> &mut CollectionBuilder {
        self.keys.id = Some(id.to_string());
        self
    }

    pub fn patterns(&mut self, patterns: &[&str]) -> &mut CollectionBuilder {
        self.keys.patterns = Some(strings(patterns));
        self
    }

    pub fn exclude(&mut self, paths: &[&str]) -> &mut CollectionBuilder {
        self.keys.exclude = Some(strings(paths));
        self
    }

    pub fn ignore(&mut self, lines: &[&str]) -> &mut CollectionBuilder {
        self.keys.ignore = Some(strings(lines));
        self
    }

    pub fn timeless(&mut self, lines: &[&str]) -> &mut CollectionBuilder {
        self.keys.timeless = Some(strings(lines));
        self
    }

    pub fn idle_days(&mut self, days: u32) -> &mut CollectionBuilder {
        self.keys.idle_days = Some(days);
        self
    }

    pub fn soon_days(&mut self, days: u32) -> &mut CollectionBuilder {
        self.keys.soon_days = Some(days);
        self
    }

    pub fn piece(
        &mut self,
        path: &str,
        build: impl FnOnce(&mut PieceBuilder),
    ) -> &mut CollectionBuilder {
        let mut piece = PieceBuilder::default();
        build(&mut piece);
        self.pieces.push((path.to_string(), piece));
        self
    }

    /// A file in the collection root that belongs to no piece's folder.
    pub fn outside(&mut self, relative: &str, content: impl AsRef<[u8]>) -> &mut CollectionBuilder {
        self.outside
            .push((relative.to_string(), content.as_ref().to_vec()));
        self
    }

    /// A stored version of `piece` with the stamp `stamp` and the given
    /// `(path, content)` files, written to the store.
    pub fn version(
        &mut self,
        piece: &str,
        stamp: &str,
        files: &[(&str, &str)],
    ) -> &mut CollectionBuilder {
        let files = files
            .iter()
            .map(|(path, content)| (path.to_string(), content.as_bytes().to_vec()))
            .collect();
        self.versions
            .push((piece.to_string(), stamp.to_string(), files));
        self
    }

    /// Leaves out `.antiq/` and the config: the folders of a directory
    /// that `init` has not made a collection yet.
    pub fn uninitialised(&mut self) -> &mut CollectionBuilder {
        self.initialised = false;
        self
    }

    pub fn finish(&mut self) -> Collection {
        let mut tree = Tree::new();
        if self.initialised {
            for directory in ["tmp", "trash", "doomed", "recovered", "pieces"] {
                tree.dir(&format!(".antiq/{directory}"));
            }
            tree.file(".antiq/config.toml", self.keys.render());
        }
        for (path, piece) in &self.pieces {
            piece.write(&self.clock, &mut tree, path);
        }
        for (relative, content) in &self.outside {
            tree.file(relative, content);
        }
        let collection = Collection {
            tree,
            clock: self.clock,
        };
        for (piece, stamp, files) in &self.versions {
            collection.add_version(piece, stamp, files);
        }
        collection.settle();
        collection
    }
}

pub struct Collection {
    tree: Tree,
    clock: Clock,
}

impl Collection {
    pub fn build(clock: &Clock) -> CollectionBuilder {
        CollectionBuilder {
            clock: *clock,
            keys: ConfigKeys::default(),
            pieces: Vec::new(),
            outside: Vec::new(),
            versions: Vec::new(),
            initialised: true,
        }
    }

    /// An empty directory, for a scenario that starts with `init`.
    pub fn bare(clock: &Clock) -> Collection {
        Collection {
            tree: Tree::new(),
            clock: *clock,
        }
    }

    /// The canonical root, the spelling antiq prints.
    pub fn root(&self) -> PathBuf {
        fs::canonicalize(self.tree.root()).expect("the root exists")
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.tree.path(relative)
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Gives every directory below the root, and the root, the clock's
    /// directory time again.
    pub fn settle(&self) {
        fn walk(dir: &Path, time: FileTime) {
            for item in fs::read_dir(dir).expect("the directory lists") {
                let path = item.expect("an entry").path();
                let is_dir = fs::symlink_metadata(&path)
                    .expect("the entry has metadata")
                    .is_dir();
                if is_dir {
                    walk(&path, time);
                }
            }
            filetime::set_file_mtime(dir, time).expect("the time can be set");
        }
        walk(
            self.tree.root(),
            FileTime::from_unix_time(self.clock.dir_time(), 0),
        );
    }
}

impl Collection {
    /// The collection around a tree that already holds one, such as a copy
    /// back from a target.
    pub(super) fn around(tree: Tree, clock: Clock) -> Collection {
        Collection { tree, clock }
    }

    /// Adds \`[target] url\` for the path target to the config.
    pub fn use_target(&self, url: &Path) {
        let path = self.path(".antiq/config.toml");
        let mut text = fs::read_to_string(&path).expect("the config exists");
        let url = serde_json::to_string(url.to_str().expect("the path is UTF-8"))
            .expect("a string serialises");
        text.push_str(&format!("\n[target]\nurl = {url}\n"));
        fs::write(&path, text).expect("the config can be written");
    }

    /// The ID in the config.
    pub fn id(&self) -> String {
        antiq::config::Config::read(&self.path(".antiq/config.toml"))
            .expect("the config can be read")
            .collection
            .id
            .to_string()
    }
}
