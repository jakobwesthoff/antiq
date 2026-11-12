// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The markers antiq writes into a collection.

use crate::error::HasKind;
use crate::filesystem::atomic::{Durable, RealDurable, flush_dir_of, named};
use crate::filesystem::tree::TreeRoot;
use crate::filesystem::{FilesystemError, failure};
use crate::pin::PIN_FILE;
use crate::relpath::{Name, RelPath};
use crate::shown::Shown;
use crate::tree::EntryKind;
use crate::versions::stamp::Stamp;
use jiff::Timestamp;
use rustix::io::Errno;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

/// The marker at the top of a piece that records its unearthing. It is
/// antiq's own bookkeeping and does not go into the archive.
pub const RESTORED_FILE: &str = ".antiq-restored";

/// Why a marker operation failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum MarkerError {
    #[error("{0}")]
    Filesystem(
        #[from]
        #[diagnostic_source]
        FilesystemError,
    ),
    #[error("{path}: {reason}", path = Shown(path))]
    Invalid { path: RelPath, reason: String },
    #[error("cannot write a marker into {path}: mount point", path = Shown(path))]
    MountPoint { path: RelPath },
}

impl HasKind for MarkerError {}

/// What `.antiq-restored` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    pub time: Timestamp,
    pub version: Option<Stamp>,
}

impl Restored {
    /// Reads the one line `restored <time>[ version <stamp>]`, with or
    /// without its line end. The time is `YYYY-MM-DDTHH:MM:SSZ` and
    /// nothing else; the reason a text is refused is the error.
    pub fn parse(bytes: &[u8]) -> Result<Restored, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "is not text".to_string())?;
        let line = text.strip_suffix('\n').unwrap_or(text);
        let words: Vec<&str> = line.split(' ').collect();
        let (time, version) = match words.as_slice() {
            ["restored", time] => (*time, None),
            ["restored", time, "version", stamp] => (*time, Some(*stamp)),
            _ => return Err("is not `restored <time>[ version <stamp>]`".to_string()),
        };
        let parsed: Timestamp = time
            .parse()
            .map_err(|_| format!("holds {time:?}, which is no time"))?;
        if parsed.strftime(TIME_FORMAT).to_string() != time {
            return Err(format!("holds {time:?}, which is not {TIME_FORMAT}"));
        }
        let version = version
            .map(|stamp| Stamp::parse(stamp).map_err(|_| format!("holds {stamp:?}, no stamp")))
            .transpose()?;
        Ok(Restored {
            time: parsed,
            version,
        })
    }

    /// The line to write, cut to the second, with its line end.
    pub fn render(&self) -> String {
        let second = Timestamp::from_second(self.time.as_second())
            .expect("a whole second of a valid timestamp is valid");
        let mut line = format!("restored {}", second.strftime(TIME_FORMAT));
        if let Some(version) = &self.version {
            line.push_str(&format!(" version {version}"));
        }
        line.push('\n');
        line
    }
}

const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%SZ";

/// More than any `restored` line holds.
const RESTORED_LIMIT: u64 = 256;

/// Where markers are written before they are renamed into place, relative
/// to the collection root.
const TMP: &str = ".antiq/tmp";

fn marker_path(top: &RelPath, name: &str) -> RelPath {
    top.join(&Name::from_bytes(name.as_bytes().to_vec()).expect("a marker name is a valid name"))
}

/// Reads `.antiq-restored` at `top`. A piece without one has none; a
/// marker that is no regular file, cannot be read or does not parse is an
/// error naming the file, so the idle check never calls the piece due by
/// mistake.
pub fn read_restored(root: &TreeRoot, top: &RelPath) -> Result<Option<Restored>, MarkerError> {
    let marker = marker_path(top, RESTORED_FILE);
    let shown = root.prefix().concat(&marker);
    let invalid = |reason: String| MarkerError::Invalid {
        path: shown.clone(),
        reason,
    };
    let entry = match root.lstat(&marker) {
        Ok(entry) => entry,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failure("stat", root, &marker)(error).into()),
    };
    if entry.kind != EntryKind::File {
        return Err(invalid("is not a regular file".to_string()));
    }
    let file = root
        .open_read(&marker)
        .map_err(failure("open", root, &marker))?;
    let mut bytes = Vec::new();
    file.take(RESTORED_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(failure("read", root, &marker))?;
    if bytes.len() as u64 > RESTORED_LIMIT {
        return Err(invalid("is longer than a `restored` line".to_string()));
    }
    Restored::parse(&bytes).map(Some).map_err(invalid)
}

/// The places a marker write can be made to fail, and the flushing it
/// uses.
pub(crate) trait Site {
    fn durable(&self) -> &dyn Durable;
    fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
        Ok(root.lstat(path)?.device)
    }
    fn before_rename(&self) -> io::Result<()> {
        Ok(())
    }
    fn rename(&self, root: &TreeRoot, from: &RelPath, to: &RelPath) -> io::Result<()> {
        root.rename_replace(from, root, to)
    }
    fn unlink(&self, root: &TreeRoot, path: &RelPath) -> io::Result<()> {
        root.remove_file(path)
    }
    fn set_mode(&self, root: &TreeRoot, path: &RelPath, mode: u32) -> io::Result<()> {
        root.set_mode(path, mode)
    }
}

pub(crate) struct RealSite;

impl Site for RealSite {
    fn durable(&self) -> &dyn Durable {
        &RealDurable
    }
}

/// Whether `kind` is what a marker called `name` may be: a regular file,
/// and for the pin also a symlink.
fn is_marker(name: &str, kind: EntryKind) -> bool {
    kind == EntryKind::File || (kind == EntryKind::Symlink && name == PIN_FILE)
}

/// What is at the marker's place: `None` for nothing, an error for an
/// entry that is no marker, so it is never replaced or removed.
fn existing(
    root: &TreeRoot,
    marker: &RelPath,
    name: &str,
) -> Result<Option<EntryKind>, MarkerError> {
    match root.lstat(marker) {
        Ok(entry) if is_marker(name, entry.kind) => Ok(Some(entry.kind)),
        Ok(_) => Err(MarkerError::Invalid {
            path: root.prefix().concat(marker),
            reason: format!("is not a file, so it stays as it is ({name})"),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failure("stat", root, marker)(error).into()),
    }
}

/// Runs `operation` on the top's entries. A top without owner write gets
/// it first and its recorded permission bits back right after, whatever
/// `operation` did; when both fail, the error of `operation` is the one
/// returned.
///
/// A crash between the grant and the restore leaves the top with owner
/// write.
fn with_owner_write(
    root: &TreeRoot,
    top: &RelPath,
    site: &dyn Site,
    operation: impl FnOnce() -> Result<(), MarkerError>,
) -> Result<(), MarkerError> {
    let recorded = root.lstat(top).map_err(failure("stat", root, top))?.mode & 0o7777;
    let granted = recorded & 0o200 == 0;
    if granted {
        site.set_mode(root, top, recorded | 0o200).map_err(failure(
            "grant owner write on",
            root,
            top,
        ))?;
    }
    let done = operation();
    let restored = if granted {
        site.set_mode(root, top, recorded)
            .map_err(failure("restore the mode of", root, top))
    } else {
        Ok(())
    };
    done?;
    Ok(restored?)
}

/// Refuses a top on another device than `.antiq/tmp/` as a mount point.
/// A marker is written in `tmp/` and renamed into place, which does not
/// work across devices; `put` asks this before it writes anything, and a
/// dry run asks it to refuse the same top. A dry run can ask before the
/// run has created a missing `tmp/`: the device of `.antiq/` is the one
/// the new folder lies on.
pub(crate) fn ensure_same_device(
    root: &TreeRoot,
    top: &RelPath,
    site: &dyn Site,
) -> Result<(), MarkerError> {
    let tmp = RelPath::parse(TMP).expect("the temporary folder is a valid path");
    let device_of = |path: &RelPath| site.device(root, path).map_err(failure("stat", root, path));
    let antiq = RelPath::parse(".antiq").expect("the layout folder is a valid path");
    let tmp_device = match site.device(root, &tmp) {
        Ok(device) => device,
        Err(error) if error.kind() == io::ErrorKind::NotFound => site
            .device(root, &antiq)
            .map_err(|_| failure("stat", root, &tmp)(error))?,
        Err(error) => return Err(failure("stat", root, &tmp)(error).into()),
    };
    if device_of(top)? != tmp_device {
        return Err(MarkerError::MountPoint {
            path: root.prefix().concat(top),
        });
    }
    Ok(())
}

/// Writes the marker `name` with `content` at `top`.
///
/// The content is written and flushed in `.antiq/tmp/`, then renamed over
/// the marker's place, then the top is flushed. A failure up to and including
/// the rename removes the temp file. A top on another device than `tmp/`
/// is refused before anything is written, and so is an entry at the
/// marker's place that is no file.
pub(crate) fn put(
    root: &TreeRoot,
    top: &RelPath,
    name: &str,
    content: &[u8],
    site: &dyn Site,
) -> Result<(), MarkerError> {
    let marker = marker_path(top, name);
    existing(root, &marker, name)?;

    ensure_same_device(root, top, site)?;

    let tmp = RelPath::parse(TMP).expect("the temporary folder is a valid path");
    let (temp, mut file) = create_temp(root, &tmp, name)?;
    let placed = (|| {
        file.write_all(content)
            .map_err(failure("write", root, &temp))?;
        site.durable().file(&file, &named(root, &temp))?;
        drop(file);
        site.before_rename()
            .map_err(failure("rename", root, &marker))?;

        with_owner_write(root, top, site, || {
            site.rename(root, &temp, &marker).map_err(|error| {
                if error.raw_os_error() == Some(Errno::XDEV.raw_os_error()) {
                    MarkerError::MountPoint {
                        path: root.prefix().concat(top),
                    }
                } else {
                    failure("rename", root, &marker)(error).into()
                }
            })
        })
    })();
    if let Err(error) = placed {
        // The failure being reported matters more than a temp file that
        // cannot be removed either; the next run empties `tmp/`.
        let _ = root.remove_file(&temp);
        return Err(error);
    }
    Ok(flush_dir_of(site.durable(), root, top)?)
}

/// Removes the marker `name` at `top`. A marker that is not there is left
/// as it is and nothing is flushed; an entry that is no file stays.
pub(crate) fn unlink(
    root: &TreeRoot,
    top: &RelPath,
    name: &str,
    site: &dyn Site,
) -> Result<(), MarkerError> {
    let marker = marker_path(top, name);
    if existing(root, &marker, name)?.is_none() {
        return Ok(());
    }
    with_owner_write(root, top, site, || {
        site.unlink(root, &marker)
            .map_err(|error| failure("remove", root, &marker)(error).into())
    })?;
    Ok(flush_dir_of(site.durable(), root, top)?)
}

/// Creates a temp file for the marker `name` in `tmp`. The name carries
/// the process id and a counter; one that exists already, left by a
/// process whose id was used again, is skipped.
fn create_temp(
    root: &TreeRoot,
    tmp: &RelPath,
    name: &str,
) -> Result<(RelPath, std::fs::File), MarkerError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let number = COUNTER.fetch_add(1, Ordering::Relaxed);
        let leaf = format!("{name}.{}.{number}", std::process::id());
        let temp = marker_path(tmp, &leaf);
        match root.create_file(&temp, 0o644) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(failure("create", root, &temp)(error).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::RestoreModes;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use crate::testing::tree::Tree;
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn time(text: &str) -> Timestamp {
        text.parse().expect("a valid time")
    }

    fn stamp(text: &str) -> Stamp {
        Stamp::parse(text).expect("a valid stamp")
    }

    // =========================================================
    // The line of .antiq-restored
    // =========================================================

    #[test]
    fn a_time_alone_parses_with_or_without_its_line_end() {
        let expected = Restored {
            time: time("2026-11-04T09:02:11Z"),
            version: None,
        };

        assert_eq!(
            Restored::parse(b"restored 2026-11-04T09:02:11Z\n"),
            Ok(expected)
        );
        assert_eq!(
            Restored::parse(b"restored 2026-11-04T09:02:11Z"),
            Ok(expected)
        );
    }

    #[test]
    fn a_time_and_a_stamp_parse() {
        let parsed =
            Restored::parse(b"restored 2026-11-04T09:02:11Z version 2026-10-30T174402-2\n");

        let expected = Restored {
            time: time("2026-11-04T09:02:11Z"),
            version: Some(stamp("2026-10-30T174402-2")),
        };
        assert_eq!(parsed, Ok(expected));
    }

    macro_rules! refused {
        ($($name:ident: $line:expr,)*) => {$(
            #[test]
            fn $name() {
                let line: &[u8] = $line;
                assert!(Restored::parse(line).is_err(), "{:?}", String::from_utf8_lossy(line));
            }
        )*};
    }

    refused! {
        an_empty_file_is_refused: b"",
        a_line_without_a_time_is_refused: b"restored\n",
        a_fraction_is_refused: b"restored 2026-11-04T09:02:11.5Z\n",
        an_offset_is_refused: b"restored 2026-11-04T09:02:11+01:00\n",
        a_lower_case_zone_is_refused: b"restored 2026-11-04T09:02:11z\n",
        a_second_space_is_refused: b"restored  2026-11-04T09:02:11Z\n",
        a_carriage_return_is_refused: b"restored 2026-11-04T09:02:11Z\r\n",
        a_second_line_is_refused: b"restored 2026-11-04T09:02:11Z\nrestored 2026-11-04T09:02:11Z\n",
        an_empty_second_line_is_refused: b"restored 2026-11-04T09:02:11Z\n\n",
        another_first_word_is_refused: b"Restored 2026-11-04T09:02:11Z\n",
        an_impossible_date_is_refused: b"restored 2026-02-30T00:00:00Z\n",
        hour_24_is_refused: b"restored 2026-11-04T24:00:00Z\n",
        a_stamp_that_is_none_is_refused: b"restored 2026-11-04T09:02:11Z version nonsense\n",
        a_missing_stamp_is_refused: b"restored 2026-11-04T09:02:11Z version \n",
        trailing_words_are_refused: b"restored 2026-11-04T09:02:11Z version 2026-10-30T174402 more\n",
        text_that_is_not_utf8_is_refused: b"restored \xff\n",
    }

    #[test]
    fn rendering_gives_the_line_that_parses_back() {
        let restored = Restored {
            time: time("2026-11-04T09:02:11Z"),
            version: Some(stamp("2026-10-30T174402")),
        };

        assert_eq!(
            restored.render(),
            "restored 2026-11-04T09:02:11Z version 2026-10-30T174402\n"
        );
        assert_eq!(Restored::parse(restored.render().as_bytes()), Ok(restored));
    }

    #[test]
    fn rendering_cuts_the_time_to_the_second() {
        let restored = Restored {
            time: time("2026-11-04T09:02:11.75Z"),
            version: None,
        };

        assert_eq!(restored.render(), "restored 2026-11-04T09:02:11Z\n");
    }

    // =========================================================
    // Reading the marker
    // =========================================================

    fn open(tree: &Tree) -> TreeRoot {
        TreeRoot::open(tree.root()).expect("the root opens")
    }

    #[test]
    fn a_piece_without_a_marker_has_no_restored_time() {
        let mut tree = Tree::new();
        tree.dir("p");

        assert_eq!(
            read_restored(&open(&tree), &path("p")).expect("reads"),
            None
        );
    }

    #[test]
    fn a_marker_is_read() {
        let mut tree = Tree::new();
        tree.file("p/.antiq-restored", "restored 2026-11-04T09:02:11Z\n");

        let read = read_restored(&open(&tree), &path("p")).expect("reads");

        assert_eq!(
            read.map(|restored| restored.time),
            Some(time("2026-11-04T09:02:11Z"))
        );
    }

    fn read_failure(tree: &Tree) -> String {
        read_restored(&open(tree), &path("p"))
            .expect_err("the marker is refused")
            .to_string()
    }

    #[test]
    fn a_symlink_marker_is_refused_naming_the_file() {
        let mut tree = Tree::new();
        tree.file("p/real", "restored 2026-11-04T09:02:11Z\n");
        tree.symlink("p/.antiq-restored", "real");

        assert!(read_failure(&tree).contains("p/.antiq-restored"));
    }

    #[test]
    fn a_directory_marker_is_refused_naming_the_file() {
        let mut tree = Tree::new();
        tree.dir("p/.antiq-restored");

        assert!(read_failure(&tree).contains("p/.antiq-restored"));
    }

    #[test]
    fn an_unparsable_marker_is_refused_naming_the_file() {
        let mut tree = Tree::new();
        tree.file("p/.antiq-restored", "yesterday\n");

        assert!(read_failure(&tree).contains("p/.antiq-restored"));
    }

    #[test]
    fn a_marker_longer_than_any_line_is_refused() {
        let mut tree = Tree::new();
        tree.file("p/.antiq-restored", "restored ".repeat(100));

        assert!(read_failure(&tree).contains("p/.antiq-restored"));
    }

    #[test]
    fn a_top_that_cannot_be_searched_is_reported_naming_the_marker() {
        let mut tree = Tree::new();
        let locked = tree.unreadable_dir("p");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        assert!(read_failure(&tree).contains("p/.antiq-restored"));
    }

    // =========================================================
    // Writing a marker
    // =========================================================

    /// A site whose steps a test can fail or watch.
    #[derive(Default)]
    struct Probe {
        recording: Recording,
        other_device: bool,
        crash_before_rename: bool,
        fail_before_rename: bool,
        rename_errno: Cell<Option<i32>>,
        unlink_fails: bool,
        /// Which calls of `set_mode` fail, counted from 0.
        failing_set_modes: Vec<usize>,
        set_modes: RefCell<Vec<u32>>,
        mode_seen_by_rename: Cell<Option<u32>>,
        top: RefCell<Option<std::path::PathBuf>>,
    }

    impl Site for Probe {
        fn durable(&self) -> &dyn Durable {
            &self.recording
        }

        fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
            let device = root.lstat(path)?.device;
            if self.other_device && path.to_string() != ".antiq/tmp" {
                return Ok(device + 1);
            }
            Ok(device)
        }

        fn before_rename(&self) -> io::Result<()> {
            if self.crash_before_rename {
                panic!("the process died");
            }
            if self.fail_before_rename {
                return Err(io::Error::other("the volume is full"));
            }
            Ok(())
        }

        fn rename(&self, root: &TreeRoot, from: &RelPath, to: &RelPath) -> io::Result<()> {
            if let Some(top) = &*self.top.borrow() {
                let mode = fs::metadata(top)
                    .expect("the top exists")
                    .permissions()
                    .mode();
                self.mode_seen_by_rename.set(Some(mode & 0o7777));
            }
            if let Some(errno) = self.rename_errno.get() {
                return Err(io::Error::from_raw_os_error(errno));
            }
            root.rename_replace(from, root, to)
        }

        fn unlink(&self, root: &TreeRoot, path: &RelPath) -> io::Result<()> {
            if self.unlink_fails {
                return Err(io::Error::from_raw_os_error(5));
            }
            root.remove_file(path)
        }

        fn set_mode(&self, root: &TreeRoot, path: &RelPath, mode: u32) -> io::Result<()> {
            let call = self.set_modes.borrow().len();
            self.set_modes.borrow_mut().push(mode);
            if self.failing_set_modes.contains(&call) {
                return Err(io::Error::from_raw_os_error(1));
            }
            root.set_mode(path, mode)
        }
    }

    /// A collection with a piece `p` and the temporary folder.
    fn collection() -> Tree {
        let mut tree = Tree::new();
        tree.dir("p");
        tree.dir(".antiq/tmp");
        tree
    }

    fn temp_leftovers(tree: &Tree) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(tree.path(".antiq/tmp"))
            .expect("tmp reads")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn top_entries(tree: &Tree) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(tree.path("p"))
            .expect("the top reads")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn put_real(tree: &Tree, content: &[u8]) -> Result<(), MarkerError> {
        put(
            &open(tree),
            &path("p"),
            ".antiq-restored",
            content,
            &RealSite,
        )
    }

    /// Makes `p` read-only for its owner, with the precondition checked.
    fn read_only_top(tree: &Tree) -> RestoreModes {
        let guard = RestoreModes(vec![tree.path("p")]);
        fs::set_permissions(tree.path("p"), fs::Permissions::from_mode(0o555))
            .expect("the mode is set");
        assert!(
            fs::write(tree.path("p/probe"), "").is_err(),
            "the test needs a user that a read-only directory stops"
        );
        guard
    }

    #[test]
    fn a_marker_is_written_with_its_content_and_leaves_no_temp_file() {
        let tree = collection();

        put_real(&tree, b"restored 2026-11-04T09:02:11Z\n").expect("written");

        assert_eq!(top_entries(&tree), [".antiq-restored"]);
        assert_eq!(
            fs::read(tree.path("p/.antiq-restored")).expect("read"),
            b"restored 2026-11-04T09:02:11Z\n"
        );
        let mode = fs::metadata(tree.path("p/.antiq-restored"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644 & !current_umask());
        assert!(temp_leftovers(&tree).is_empty());
    }

    #[allow(
        clippy::useless_conversion,
        reason = "the mode bits are a u16 on macOS and already a u32 on Linux"
    )]
    fn current_umask() -> u32 {
        let mask = rustix::process::umask(rustix::fs::Mode::empty());
        rustix::process::umask(mask);
        u32::from(mask.bits())
    }

    #[test]
    fn a_marker_replaces_an_existing_one() {
        let mut tree = collection();
        tree.file("p/.antiq-restored", "old\n");

        put_real(&tree, b"new\n").expect("written");

        assert_eq!(
            fs::read(tree.path("p/.antiq-restored")).expect("read"),
            b"new\n"
        );
        assert!(temp_leftovers(&tree).is_empty());
    }

    #[test]
    fn a_read_only_top_is_written_to_and_gets_its_mode_back() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        let probe = Probe::default();
        *probe.top.borrow_mut() = Some(tree.path("p"));

        put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect("written");

        assert_eq!(probe.mode_seen_by_rename.get(), Some(0o755));
        let mode = fs::metadata(tree.path("p"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o555);
        assert!(tree.path("p/.antiq-restored").exists());
    }

    #[test]
    fn a_top_with_owner_write_is_not_touched_by_the_mode_steps() {
        let tree = collection();
        let probe = Probe::default();

        put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect("written");

        assert!(probe.set_modes.borrow().is_empty());
    }

    #[test]
    fn special_mode_bits_of_the_top_come_back() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        // The system drops the setgid bit of a directory whose group the
        // user is not in, and a directory takes the group of the folder it
        // is made in, which can be one of another group.
        std::os::unix::fs::chown(
            tree.path("p"),
            None,
            Some(rustix::process::getegid().as_raw()),
        )
        .expect("the directory goes to the group of the user");
        fs::set_permissions(tree.path("p"), fs::Permissions::from_mode(0o2555)).expect("setgid");
        let probe = Probe::default();

        put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect("written");

        assert_eq!(*probe.set_modes.borrow(), [0o2755, 0o2555]);
    }

    #[test]
    fn a_failed_rename_still_puts_the_mode_back_and_removes_the_temp() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        let probe = Probe::default();
        probe.rename_errno.set(Some(5));

        let error = put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
            .expect_err("the rename fails");

        assert!(matches!(error, MarkerError::Filesystem(_)), "{error}");
        let mode = fs::metadata(tree.path("p"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o555);
        assert!(temp_leftovers(&tree).is_empty());
        assert!(!tree.path("p/.antiq-restored").exists());
        assert_eq!(
            probe.recording.flushes().len(),
            1,
            "only the temp file was flushed"
        );
    }

    #[test]
    fn the_rename_error_wins_when_the_mode_cannot_be_restored_either() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        let probe = Probe {
            failing_set_modes: vec![1],
            ..Probe::default()
        };
        probe.rename_errno.set(Some(5));

        let error = put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
            .expect_err("both fail");

        assert!(error.to_string().contains("rename"), "{error}");
    }

    #[test]
    fn a_mode_that_cannot_be_restored_is_reported() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        let probe = Probe {
            failing_set_modes: vec![1],
            ..Probe::default()
        };

        let error = put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
            .expect_err("the restore fails");

        assert!(error.to_string().contains("mode"), "{error}");
    }

    #[test]
    fn owner_write_that_cannot_be_granted_is_reported_and_nothing_is_renamed() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        let probe = Probe {
            failing_set_modes: vec![0],
            ..Probe::default()
        };

        let error = put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
            .expect_err("the grant fails");

        assert!(error.to_string().contains("grant owner write"), "{error}");
        assert!(!tree.path("p/.antiq-restored").exists());
    }

    #[test]
    fn a_crash_before_the_rename_leaves_the_temp_in_tmp_and_the_top_alone() {
        let tree = collection();
        let probe = Probe {
            crash_before_rename: true,
            ..Probe::default()
        };

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
        }));

        assert!(outcome.is_err(), "the process dies");
        assert_eq!(temp_leftovers(&tree).len(), 1);
        assert!(top_entries(&tree).is_empty());
    }

    #[test]
    fn a_failure_before_the_rename_removes_the_temp_and_leaves_the_top_alone() {
        let tree = collection();
        let probe = Probe {
            fail_before_rename: true,
            ..Probe::default()
        };

        let error = put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
            .expect_err("the volume is full");

        assert!(error.to_string().contains("rename"), "{error}");
        assert!(temp_leftovers(&tree).is_empty());
        assert!(top_entries(&tree).is_empty());
    }

    #[test]
    fn a_top_on_another_device_is_refused_as_a_mount_point_before_any_write() {
        let tree = collection();
        let probe = Probe {
            other_device: true,
            ..Probe::default()
        };

        let error =
            put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect_err("refused");

        assert!(matches!(error, MarkerError::MountPoint { .. }), "{error}");
        assert!(error.to_string().contains("mount point"));
        assert!(temp_leftovers(&tree).is_empty());
        assert!(top_entries(&tree).is_empty());
    }

    #[test]
    fn the_device_check_passes_on_one_device_and_refuses_two() {
        let tree = collection();
        let root = open(&tree);

        ensure_same_device(&root, &path("p"), &Probe::default()).expect("one device");
        let probe = Probe {
            other_device: true,
            ..Probe::default()
        };
        let error = ensure_same_device(&root, &path("p"), &probe).expect_err("two devices");

        assert!(matches!(error, MarkerError::MountPoint { .. }), "{error}");
        assert!(temp_leftovers(&tree).is_empty());
    }

    #[test]
    fn a_missing_tmp_is_measured_by_the_device_of_antiq() {
        let tree = collection();
        fs::remove_dir(tree.path(".antiq/tmp")).expect("tmp is empty");
        let root = open(&tree);

        ensure_same_device(&root, &path("p"), &Probe::default()).expect("one device");
    }

    #[test]
    fn a_tmp_that_cannot_be_looked_at_is_reported_naming_it() {
        let tree = collection();
        let root = open(&tree);
        let _restore = RestoreModes(vec![tree.path(".antiq")]);
        fs::set_permissions(tree.path(".antiq"), fs::Permissions::from_mode(0o000)).expect("chmod");
        assert!(
            fs::read_dir(tree.path(".antiq")).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        let error = ensure_same_device(&root, &path("p"), &Probe::default()).expect_err("denied");

        assert!(error.to_string().contains(".antiq/tmp"), "{error}");
    }

    #[test]
    fn a_rename_across_devices_is_a_mount_point_too() {
        let tree = collection();
        let probe = Probe::default();
        probe
            .rename_errno
            .set(Some(rustix::io::Errno::XDEV.raw_os_error()));

        let error = put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe)
            .expect_err("the rename crosses devices");

        assert!(matches!(error, MarkerError::MountPoint { .. }), "{error}");
        assert!(temp_leftovers(&tree).is_empty());
    }

    #[test]
    fn a_missing_tmp_is_reported_naming_it_and_the_top_stays_untouched() {
        let mut tree = Tree::new();
        tree.dir("p");

        let error = put_real(&tree, b"x\n").expect_err("no tmp");

        assert!(error.to_string().contains(".antiq/tmp"), "{error}");
        assert!(top_entries(&tree).is_empty());
    }

    #[test]
    fn a_top_that_cannot_be_searched_is_reported_and_nothing_is_written() {
        let mut tree = collection();
        let locked = tree.unreadable_dir("q");
        assert!(
            fs::read_dir(&locked).is_err(),
            "the test needs a user that mode 000 locks out"
        );

        let error = put(&open(&tree), &path("q"), ".antiq-pin", b"", &RealSite)
            .expect_err("the top cannot be searched");

        assert!(error.to_string().contains("q/.antiq-pin"), "{error}");
        assert!(temp_leftovers(&tree).is_empty());
    }

    #[test]
    fn a_tmp_that_cannot_be_written_is_reported_and_the_top_stays_untouched() {
        let tree = collection();
        let _restore = read_only_top(&tree);
        let _restore_tmp = RestoreModes(vec![tree.path(".antiq/tmp")]);
        fs::set_permissions(tree.path(".antiq/tmp"), fs::Permissions::from_mode(0o555))
            .expect("the mode is set");
        assert!(
            fs::write(tree.path(".antiq/tmp/probe"), "").is_err(),
            "the test needs a user that a read-only directory stops"
        );

        let error = put_real(&tree, b"x\n").expect_err("no temp file can be made");

        assert!(error.to_string().contains("create"), "{error}");
        assert_eq!(fs::read_dir(tree.path("p")).expect("reads").count(), 0);
    }

    #[test]
    fn a_stale_temp_of_the_next_name_is_skipped() {
        let tree = collection();
        let pid = std::process::id();
        // The counter is shared by the whole test binary; stale files for a
        // range of numbers cover whichever one comes next.
        for n in 0..4096 {
            fs::write(
                tree.path(&format!(".antiq/tmp/.antiq-restored.{pid}.{n}")),
                "",
            )
            .expect("stale");
        }

        put_real(&tree, b"x\n").expect("written despite the stale files");

        assert!(tree.path("p/.antiq-restored").exists());
    }

    #[test]
    fn a_marker_over_a_fifo_a_symlink_or_a_directory_is_refused_and_left() {
        for kind in ["fifo", "symlink", "directory"] {
            let mut tree = collection();
            match kind {
                "fifo" => drop(tree.fifo("p/.antiq-restored")),
                "symlink" => drop(tree.symlink("p/.antiq-restored", "elsewhere")),
                _ => drop(tree.dir("p/.antiq-restored")),
            }

            let error = put_real(&tree, b"x\n").expect_err(kind);

            assert!(
                matches!(error, MarkerError::Invalid { .. }),
                "{kind}: {error}"
            );
            assert!(error.to_string().contains("p/.antiq-restored"), "{kind}");
            assert!(temp_leftovers(&tree).is_empty(), "{kind}");
            assert!(
                fs::symlink_metadata(tree.path("p/.antiq-restored")).is_ok(),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_pin_may_replace_a_symlink_but_not_a_fifo() {
        let mut tree = collection();
        tree.symlink("p/.antiq-pin", "elsewhere");
        put(&open(&tree), &path("p"), ".antiq-pin", b"", &RealSite).expect("a symlink pin goes");
        assert!(
            fs::symlink_metadata(tree.path("p/.antiq-pin"))
                .expect("meta")
                .is_file()
        );

        let mut tree = collection();
        tree.fifo("p/.antiq-pin");
        put(&open(&tree), &path("p"), ".antiq-pin", b"", &RealSite).expect_err("a fifo stays");
    }

    #[test]
    fn the_file_is_flushed_before_the_rename_and_the_top_after_it() {
        let tree = collection();
        let probe = Probe::default();
        let marker = tree.path("p/.antiq-restored");
        probe.recording.observe(move |flush, _| {
            if matches!(flush, Flush::Dir(_)) {
                assert!(marker.exists(), "the top is flushed after the rename");
            }
        });

        put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect("written");

        let flushes = probe.recording.flushes();
        assert!(
            matches!(&flushes[0], Flush::File(file) if file.to_string_lossy().starts_with(".antiq/tmp/"))
        );
        assert_eq!(flushes[1], Flush::Dir(std::path::PathBuf::from("p")));
        assert_eq!(flushes.len(), 2);
    }

    #[test]
    fn a_temp_file_that_cannot_be_flushed_is_reported_and_nothing_is_renamed() {
        let tree = collection();
        let probe = Probe::default();
        probe.recording.fail_files_from(0);

        put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect_err("flush fails");

        assert!(top_entries(&tree).is_empty());
    }

    #[test]
    fn a_top_that_cannot_be_flushed_is_reported_after_the_marker_is_in_place() {
        let tree = collection();
        let probe = Probe::default();
        probe.recording.fail_dirs_from(0);

        put(&open(&tree), &path("p"), ".antiq-restored", b"x\n", &probe).expect_err("flush fails");

        assert!(tree.path("p/.antiq-restored").exists());
    }

    // =========================================================
    // Removing a marker
    // =========================================================

    #[test]
    fn a_marker_is_removed_and_the_top_flushed() {
        let mut tree = collection();
        tree.file("p/.antiq-pin", "");
        let probe = Probe::default();

        unlink(&open(&tree), &path("p"), ".antiq-pin", &probe).expect("removed");

        assert!(top_entries(&tree).is_empty());
        assert_eq!(probe.recording.flushes(), [Flush::Dir("p".into())]);
    }

    #[test]
    fn a_marker_that_is_not_there_is_left_alone_and_nothing_is_flushed() {
        let tree = collection();
        let probe = Probe::default();

        unlink(&open(&tree), &path("p"), ".antiq-pin", &probe).expect("nothing to do");

        assert!(probe.recording.flushes().is_empty());
    }

    #[test]
    fn a_symlink_pin_is_removed_without_touching_its_target() {
        let mut tree = collection();
        tree.file("target", "keep");
        tree.symlink("p/.antiq-pin", "../target");

        unlink(&open(&tree), &path("p"), ".antiq-pin", &RealSite).expect("removed");

        assert!(top_entries(&tree).is_empty());
        assert_eq!(fs::read(tree.path("target")).expect("target"), b"keep");
    }

    #[test]
    fn removing_from_a_read_only_top_gets_its_mode_back() {
        let mut tree = collection();
        tree.file("p/.antiq-pin", "");
        let _restore = read_only_top(&tree);

        unlink(&open(&tree), &path("p"), ".antiq-pin", &RealSite).expect("removed");

        let mode = fs::metadata(tree.path("p"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o555);
        assert!(!tree.path("p/.antiq-pin").exists());
    }

    #[test]
    fn a_fifo_a_directory_or_a_symlink_restored_marker_is_not_removed() {
        let mut tree = collection();
        tree.fifo("p/.antiq-pin");
        unlink(&open(&tree), &path("p"), ".antiq-pin", &RealSite).expect_err("fifo");
        assert!(tree.path("p/.antiq-pin").exists());

        let mut tree = collection();
        tree.symlink("p/.antiq-restored", "x");
        unlink(&open(&tree), &path("p"), ".antiq-restored", &RealSite).expect_err("symlink");
        assert!(fs::symlink_metadata(tree.path("p/.antiq-restored")).is_ok());
    }

    #[test]
    fn an_unlink_that_fails_is_reported_and_the_mode_comes_back() {
        let mut tree = collection();
        tree.file("p/.antiq-pin", "");
        let _restore = read_only_top(&tree);
        let probe = Probe {
            unlink_fails: true,
            ..Probe::default()
        };

        let error = unlink(&open(&tree), &path("p"), ".antiq-pin", &probe).expect_err("fails");

        assert!(error.to_string().contains(".antiq-pin"), "{error}");
        assert_eq!(*probe.set_modes.borrow(), [0o755, 0o555]);
    }
}
