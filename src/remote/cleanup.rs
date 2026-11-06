// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cleanup of remote leftovers (spec "Committing a version", Cleanup).
//!
//! [`plan`] looks at every object directory and says what could go;
//! [`apply`] looks again at each directory it is about to delete in, and
//! deletes only what both looks agree on. Whatever a commit or a claim
//! added in between is therefore never taken for a leftover.
//!
//! A caller runs [`apply`] after the run's uploads and commits are done
//! and lists the temp names of uploads that stay pending in
//! [`Settings::keep`]. A version that a commit reported as overtaken is
//! removed by cleanup once it is older than the grace period, and its
//! payloads one run later; the caller never drops its local archive
//! because of such a version.

use super::commit::{claim_name, claim_number, read_document, read_listed, version_name};
use super::{Remote, RemoteError, RemoteFile};
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta::{self, Metadata};
use crate::plan::PlanAction;
use crate::relpath::machine_part;
use crate::store::version_number;
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// What cleanup needs to know about this machine and the collection.
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    /// The machine's name, which its temp names carry.
    pub machine: &'a str,
    /// The collection that runs the cleanup.
    pub collection: ObjectId,
    /// How old a file has to be before cleanup may remove it.
    pub grace: SignedDuration,
    /// Temp names of uploads that are still pending on this machine, by
    /// object.
    pub keep: &'a BTreeSet<(ObjectId, String)>,
}

/// Whether the cleanup is a real run, which knows the time and holds the
/// run lock, or a dry run, which reads nothing about time.
pub enum Mode<'a> {
    Run {
        lock: &'a RunLock,
        /// The remote's clock, as [`Remote::clock`] gave it.
        now: Timestamp,
    },
    DryRun,
}

/// What a cleanup would do.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Cleanup {
    pub steps: Vec<Step>,
    /// Directories that cleanup does not touch, with the reason.
    pub left_alone: Vec<LeftAlone>,
}

/// The kind of file a step removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    Claim,
    OldMetadata,
    Payload,
    Listing,
    TempFile,
    PartialFile,
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            What::Claim => "claim",
            What::OldMetadata => "old metadata",
            What::Payload => "payload",
            What::Listing => "listing",
            What::TempFile => "temp file",
            What::PartialFile => "partial file",
        })
    }
}

/// One removal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Step {
    Remove {
        id: ObjectId,
        name: String,
        what: What,
    },
    /// A removal that depends on an age a dry run cannot read.
    AgeUnknown {
        id: ObjectId,
        name: String,
        what: What,
    },
}

impl PlanAction for Step {
    fn describe(&self) -> String {
        match self {
            Step::Remove { id, name, what } => format!("remove {what} {name} of {id}"),
            Step::AgeUnknown { id, name, what } => {
                format!("remove {what} {name} of {id} (age unknown in a dry run)")
            }
        }
    }
}

/// An object whose directory cleanup does not touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftAlone {
    pub id: ObjectId,
    pub reason: String,
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub removed: usize,
    /// Steps of the plan that the second look did not confirm.
    pub skipped: usize,
}

/// Works out the steps for every object on the remote. A real run
/// ([`Mode::Run`]) measures ages against the `now` it carries; a dry run
/// reads no clock and names every step that depends on an age
/// [`Step::AgeUnknown`].
pub fn plan(remote: &dyn Remote, settings: &Settings, mode: &Mode) -> Result<Cleanup, RemoteError> {
    let now = match mode {
        Mode::Run { now, .. } => Some(*now),
        Mode::DryRun => None,
    };
    let mut cleanup = Cleanup::default();
    for id in remote.list_objects()? {
        let examined = examine(remote, settings, now, id)?;
        cleanup.steps.extend(examined.steps);
        cleanup.left_alone.extend(examined.left_alone);
    }
    Ok(cleanup)
}

/// Removes the steps of `cleanup` that a second look at each directory
/// confirms.
///
/// Directly before the first payload or listing of an object is deleted,
/// the directory is listed once more, and none of its payloads or
/// listings is deleted when a version, a claim or a metadata temp name
/// has appeared that the second look did not see. Another machine's
/// commit that started in between is not closable without locks on the
/// remote; the grace period is what makes it rare.
pub fn apply(
    remote: &dyn Remote,
    settings: &Settings,
    _lock: &RunLock,
    now: Timestamp,
    cleanup: &Cleanup,
) -> Result<Applied, RemoteError> {
    let mut applied = Applied {
        removed: 0,
        skipped: 0,
    };
    let mut planned: BTreeMap<ObjectId, Vec<(&str, What)>> = BTreeMap::new();
    for step in &cleanup.steps {
        if let Step::Remove { id, name, what } = step {
            planned.entry(*id).or_default().push((name, *what));
        }
    }
    for (id, steps) in planned {
        let fresh = examine(remote, settings, Some(now), id)?;
        let confirmed: BTreeSet<(&str, What)> = fresh
            .steps
            .iter()
            .map(|step| match step {
                // The second look has a time, so it holds no age-unknown step.
                Step::Remove { name, what, .. } | Step::AgeUnknown { name, what, .. } => {
                    (name.as_str(), *what)
                }
            })
            .collect();
        let mut guarded: Option<bool> = None;
        for (name, what) in steps {
            if !confirmed.contains(&(name, what)) {
                applied.skipped += 1;
                continue;
            }
            if matches!(what, What::Payload | What::Listing) {
                let held_back = match guarded {
                    Some(held_back) => held_back,
                    None => {
                        let held_back = appeared_since(remote, id, &fresh.seen)?;
                        guarded = Some(held_back);
                        held_back
                    }
                };
                if held_back {
                    applied.skipped += 1;
                    continue;
                }
            }
            match what {
                What::PartialFile => remote.delete_partial(id, name)?,
                _ => remote.delete(id, name)?,
            }
            applied.removed += 1;
        }
    }
    Ok(applied)
}

/// Whether the directory now holds a version, a claim or a metadata temp
/// name that `seen` does not.
fn appeared_since(
    remote: &dyn Remote,
    id: ObjectId,
    seen: &BTreeSet<String>,
) -> Result<bool, RemoteError> {
    Ok(remote
        .list_dir(id)?
        .iter()
        .any(|file| concerns_commits(&file.name) && !seen.contains(&file.name)))
}

/// A name that fits the temp name of an upload or of a metadata version.
struct TempName {
    /// The temp name was made by this machine and by nobody else's name.
    own: bool,
    /// The file the temp name stands for: a payload, a listing or
    /// `meta.<N>`.
    target: String,
}

/// Reads `.<target>.<machine part>.part`, optionally followed by `.` and
/// six characters of `[A-Za-z0-9]`, the file rsync leaves when it is
/// killed hard. A target may contain dots, and so may a machine part, so
/// a name can split into target and machine part in more than one way. It
/// is this machine's own only when it splits in exactly one way and that
/// way names this machine; a name that splits two ways falls under the
/// grace period like another machine's.
fn classify_temp(name: &str, ours: &str) -> Option<TempName> {
    let without_leftover = match name.len().checked_sub(7).and_then(|at| name.get(..at)) {
        Some(head)
            if name.as_bytes()[name.len() - 7] == b'.'
                && name.as_bytes()[name.len() - 6..]
                    .iter()
                    .all(u8::is_ascii_alphanumeric) =>
        {
            head
        }
        _ => name,
    };
    let inner = without_leftover.strip_prefix('.')?.strip_suffix(".part")?;
    let splits: Vec<(&str, &str)> = inner
        .match_indices('.')
        .map(|(at, _)| (&inner[..at], &inner[at + 1..]))
        .filter(|(target, machine)| is_target(target) && is_machine_part(machine))
        .collect();
    let (target, machine) = *splits.first()?;
    Some(TempName {
        own: splits.len() == 1 && machine == ours,
        target: target.to_string(),
    })
}

fn is_target(name: &str) -> bool {
    meta::is_payload_file_name(name)
        || meta::is_listing_file_name(name)
        || (version_number(name).is_some())
}

fn is_machine_part(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'%'))
}

/// Whether the name belongs to the commit protocol: a version, a claim, or
/// a metadata temp name.
fn concerns_commits(name: &str) -> bool {
    version_number(name).is_some() || claim_number(name).is_some() || name.starts_with(".meta.")
}

// =========================================================
// Looking at one object
// =========================================================

/// How often a directory is looked at again when a version disappears
/// while it is read.
const ATTEMPTS: usize = 3;

/// What one look at an object's directory found.
struct Examined {
    steps: Vec<Step>,
    left_alone: Option<LeftAlone>,
    /// The names of the commit protocol that the listing held.
    seen: BTreeSet<String>,
}

impl Examined {
    fn left_alone(id: ObjectId, reason: String) -> Examined {
        Examined {
            steps: Vec::new(),
            left_alone: Some(LeftAlone { id, reason }),
            seen: BTreeSet::new(),
        }
    }
}

fn examine(
    remote: &dyn Remote,
    settings: &Settings<'_>,
    now: Option<Timestamp>,
    id: ObjectId,
) -> Result<Examined, RemoteError> {
    for _ in 0..ATTEMPTS {
        let files = remote.list_dir(id)?;
        if let Some(directory) = files
            .iter()
            .find(|file| file.is_dir && concerns_commits(&file.name))
        {
            return Ok(Examined::left_alone(
                id,
                format!("{} is a directory", directory.name),
            ));
        }
        match read_versions(remote, id, &files)? {
            // A cleanup of another machine removed a version while it was
            // read; the listing is stale, so it is made again.
            Read::Vanished => continue,
            Read::Unusable(reason) => return Ok(Examined::left_alone(id, reason)),
            Read::Versions(versions) => {
                let object = Object {
                    id,
                    files,
                    partial: remote.list_partial(id)?,
                    versions,
                    now,
                    settings: *settings,
                };
                return Ok(object.decide());
            }
        }
    }
    Ok(Examined::left_alone(
        id,
        "a metadata version kept disappearing while it was read".to_string(),
    ))
}

enum Read {
    Versions(BTreeMap<u32, Metadata>),
    Vanished,
    /// The reason cleanup does not touch the directory.
    Unusable(String),
}

/// Reads every `meta.<N>` of the listing. Cleanup runs in a directory only
/// when every one of them parses with the format this version knows and
/// belongs to the object.
fn read_versions(
    remote: &dyn Remote,
    id: ObjectId,
    files: &[RemoteFile],
) -> Result<Read, RemoteError> {
    let mut versions = BTreeMap::new();
    for file in files.iter().filter(|file| !file.is_dir) {
        let Some(number) = version_number(&file.name) else {
            continue;
        };
        let Some(bytes) = read_listed(remote, id, &file.name)? else {
            return Ok(Read::Vanished);
        };
        match read_document(id, &bytes) {
            Ok(metadata) => {
                versions.insert(number, metadata);
            }
            Err(reason) => {
                return Ok(Read::Unusable(format!(
                    "{} cannot be used: {reason}",
                    file.name
                )));
            }
        }
    }
    Ok(Read::Versions(versions))
}

/// A yes or no that a dry run may not be able to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    No,
    Maybe,
    Yes,
}

impl Tri {
    fn from_bool(value: bool) -> Tri {
        if value { Tri::Yes } else { Tri::No }
    }

    fn and(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::No, _) | (_, Tri::No) => Tri::No,
            (Tri::Maybe, _) | (_, Tri::Maybe) => Tri::Maybe,
            (Tri::Yes, Tri::Yes) => Tri::Yes,
        }
    }

    fn or(self, other: Tri) -> Tri {
        match (self, other) {
            (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
            (Tri::Maybe, _) | (_, Tri::Maybe) => Tri::Maybe,
            (Tri::No, Tri::No) => Tri::No,
        }
    }

    fn not(self) -> Tri {
        match self {
            Tri::Yes => Tri::No,
            Tri::No => Tri::Yes,
            Tri::Maybe => Tri::Maybe,
        }
    }
}

/// An object directory whose versions are all readable.
struct Object<'a> {
    id: ObjectId,
    files: Vec<RemoteFile>,
    /// The entries of the `.antiq-partial` directory.
    partial: Vec<RemoteFile>,
    versions: BTreeMap<u32, Metadata>,
    /// The remote's time; none in a dry run.
    now: Option<Timestamp>,
    settings: Settings<'a>,
}

impl Object<'_> {
    fn highest(&self) -> u32 {
        self.versions.keys().next_back().copied().unwrap_or(0)
    }

    fn regular(&self) -> impl Iterator<Item = &RemoteFile> {
        self.files.iter().filter(|file| !file.is_dir)
    }

    fn older_than_grace(&self, file: &RemoteFile) -> Tri {
        match self.now {
            Some(now) => Tri::from_bool(now.duration_since(file.modified) > self.settings.grace),
            None => Tri::Maybe,
        }
    }

    fn file(&self, name: &str) -> Option<&RemoteFile> {
        self.regular().find(|file| file.name == name)
    }

    /// `meta.<N>` is retained for now only when it is the highest
    /// version; older ones go once they are older than the grace period.
    fn version_goes(&self, number: u32) -> Tri {
        match self.file(&version_name(number)) {
            Some(file) if number < self.highest() => self.older_than_grace(file),
            _ => Tri::No,
        }
    }

    /// Whether a claim without metadata is still being worked on.
    fn any_open_claim(&self) -> Tri {
        self.regular()
            .filter_map(|file| Some((claim_number(&file.name)?, file)))
            .filter(|(number, _)| !self.versions.contains_key(number))
            .fold(Tri::No, |open, (_, file)| {
                open.or(self.older_than_grace(file).not())
            })
    }

    fn referenced(&self) -> BTreeSet<&str> {
        let mut names = BTreeSet::new();
        for metadata in self.versions.values() {
            for payload in &metadata.payloads {
                names.insert(payload.file.as_str());
                if let Some(listing) = &payload.listing {
                    names.insert(listing.as_str());
                }
            }
        }
        names
    }

    /// Whether the committed metadata names this collection as the owner.
    fn owned(&self) -> bool {
        self.versions
            .get(&self.highest())
            .is_some_and(|metadata| metadata.collection == self.settings.collection)
    }

    /// Whether the claim for a metadata temp name's version is still to be
    /// decided: the temp name is the evidence that the claim is this
    /// machine's own interrupted attempt.
    fn proves_own_claim(&self, target: &str) -> bool {
        version_number(target).is_some_and(|number| {
            number > self.highest() && self.file(&claim_name(number)).is_some()
        })
    }

    /// Temp names and partial files. This machine's own go at once in a
    /// directory its collection owns, unless the upload is still pending
    /// or the name proves a claim. Every other one goes once it is older
    /// than the grace period. The early removal does not depend on an age,
    /// so a dry run lists it as a removal.
    fn push_leftovers(&self, push: &mut impl FnMut(Tri, &str, What)) {
        let ours = machine_part(self.settings.machine);
        let listings = [
            (self.regular().collect::<Vec<_>>(), What::TempFile),
            (
                self.partial.iter().filter(|file| !file.is_dir).collect(),
                What::PartialFile,
            ),
        ];
        for (files, what) in listings {
            for file in files {
                let Some(temp) = classify_temp(&file.name, &ours) else {
                    continue;
                };
                if self.settings.keep.contains(&(self.id, file.name.clone())) {
                    continue;
                }
                let early = temp.own && self.owned() && !self.proves_own_claim(&temp.target);
                let goes = if early {
                    Tri::Yes
                } else {
                    self.older_than_grace(file)
                };
                push(goes, &file.name, what);
            }
        }
    }

    fn decide(&self) -> Examined {
        let mut steps = Vec::new();
        let mut push = |tri: Tri, name: &str, what: What| {
            let (id, name) = (self.id, name.to_string());
            match tri {
                Tri::No => {}
                Tri::Maybe => steps.push(Step::AgeUnknown { id, name, what }),
                Tri::Yes => steps.push(Step::Remove { id, name, what }),
            }
        };

        for &number in self.versions.keys() {
            push(
                self.version_goes(number),
                &version_name(number),
                What::OldMetadata,
            );
        }

        let mut claims: Vec<(u32, &RemoteFile)> = self
            .regular()
            .filter_map(|file| Some((claim_number(&file.name)?, file)))
            .collect();
        claims.sort_by_key(|(number, _)| *number);
        for (number, file) in claims {
            if number >= self.highest() {
                continue;
            }
            // A claim goes once it is abandoned, and with its version if
            // that goes; a version that stays keeps its claim.
            let version = if self.versions.contains_key(&number) {
                self.version_goes(number)
            } else {
                Tri::Yes
            };
            push(
                self.older_than_grace(file).and(version),
                &file.name,
                What::Claim,
            );
        }

        let open = self.any_open_claim();
        let referenced = self.referenced();
        let mut payloads: Vec<&RemoteFile> = Vec::new();
        let mut listings: Vec<&RemoteFile> = Vec::new();
        for file in self.regular() {
            if meta::is_payload_file_name(&file.name) {
                payloads.push(file);
            } else if meta::is_listing_file_name(&file.name) {
                listings.push(file);
            }
        }
        for (files, what) in [(payloads, What::Payload), (listings, What::Listing)] {
            for file in files {
                if referenced.contains(file.name.as_str()) {
                    continue;
                }
                push(
                    self.older_than_grace(file).and(open.not()),
                    &file.name,
                    what,
                );
            }
        }

        self.push_leftovers(&mut push);

        Examined {
            steps,
            left_alone: None,
            seen: self
                .regular()
                .map(|file| file.name.clone())
                .filter(|name| concerns_commits(name))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::error::{ErrorKind, HasKind};
    use crate::lock;
    use crate::remote::testing::{
        COLLECTION_ID, Call, Fault, FaultRemote, Op, metadata_bytes, object, path_remote,
        remote_root,
    };
    use serde_json::json;
    use std::path::PathBuf;

    const PAYLOAD_A: &str = "0123456789abcdef-a.tar.zst";
    const LISTING_A: &str = "0123456789abcdef.listing.zst";
    const PAYLOAD_B: &str = "fedcba9876543210-b.tar.zst";
    const LISTING_B: &str = "fedcba9876543210.listing.zst";
    const HOUR: i64 = 3600;

    fn now() -> Timestamp {
        "2026-11-04T12:00:00Z".parse().expect("a time")
    }

    fn id(number: u8) -> ObjectId {
        object(number)
    }

    struct World {
        root: tempfile::TempDir,
        remote: FaultRemote,
        lock: lock::RunLock,
        keep: BTreeSet<(ObjectId, String)>,
        /// The machine name cleanup runs as.
        machine: &'static str,
        _local: tempfile::TempDir,
    }

    impl World {
        fn new() -> World {
            let root = remote_root();
            let remote = FaultRemote::new(path_remote(root.path(), "laptop"));
            remote.set_clock(now());
            let local = tempfile::tempdir().expect("temp dir");
            let lock = lock::acquire(&local.path().join("lock"), "laptop", &SystemClock)
                .expect("the lock is free");
            World {
                root,
                remote,
                lock,
                keep: BTreeSet::new(),
                machine: "laptop",
                _local: local,
            }
        }

        fn settings(&self) -> Settings<'_> {
            Settings {
                machine: self.machine,
                collection: COLLECTION_ID.parse().expect("an ID"),
                grace: SignedDuration::from_hours(24),
                keep: &self.keep,
            }
        }

        fn dir(&self, number: u8) -> PathBuf {
            self.root
                .path()
                .join("objects")
                .join(id(number).to_string())
        }

        /// Writes a file of object `number` that is `hours` hours old.
        fn put(&self, number: u8, name: &str, bytes: &[u8], hours: i64) {
            std::fs::create_dir_all(self.dir(number)).expect("the object directory");
            std::fs::write(self.dir(number).join(name), bytes).expect("a remote file");
            self.age(number, name, hours * HOUR);
        }

        /// Writes a file in the `.antiq-partial` directory of object
        /// `number` that is `hours` hours old.
        fn partial(&self, number: u8, name: &str, hours: i64) {
            let directory = self.dir(number).join(".antiq-partial");
            std::fs::create_dir_all(&directory).expect("the partial directory");
            std::fs::write(directory.join(name), b"half").expect("a partial file");
            let time = filetime::FileTime::from_unix_time(now().as_second() - hours * HOUR, 0);
            filetime::set_file_mtime(directory.join(name), time).expect("time is set");
        }

        fn age(&self, number: u8, name: &str, seconds: i64) {
            let time = filetime::FileTime::from_unix_time(now().as_second() - seconds, 0);
            filetime::set_file_mtime(self.dir(number).join(name), time).expect("time is set");
        }

        /// A version of object `number` that references `payloads`.
        fn version(&self, number: u8, version: u32, payloads: &[(&str, &str)], hours: i64) {
            let bytes = metadata_bytes(id(number), COLLECTION_ID, "a", payloads);
            self.put(number, &format!("meta.{version}"), &bytes, hours);
        }

        fn names(&self, number: u8) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.dir(number))
                .expect("lists")
                .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
                .collect();
            names.sort();
            names
        }

        fn mode(&self) -> Mode<'_> {
            Mode::Run {
                lock: &self.lock,
                now: now(),
            }
        }

        fn plan(&self) -> Cleanup {
            plan(&self.remote, &self.settings(), &self.mode()).expect("the plan is made")
        }

        fn dry_plan(&self) -> Cleanup {
            plan(&self.remote, &self.settings(), &Mode::DryRun).expect("the plan is made")
        }

        fn apply(&self, cleanup: &Cleanup) -> Applied {
            apply(&self.remote, &self.settings(), &self.lock, now(), cleanup)
                .expect("the cleanup is applied")
        }
    }

    fn remove(number: u8, name: &str, what: What) -> Step {
        Step::Remove {
            id: id(number),
            name: name.into(),
            what,
        }
    }

    fn unknown(number: u8, name: &str, what: What) -> Step {
        Step::AgeUnknown {
            id: id(number),
            name: name.into(),
            what,
        }
    }

    // =====================================================
    // Payloads and listings
    // =====================================================

    #[test]
    fn an_unreferenced_payload_older_than_the_grace_period_is_removed() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 25);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, PAYLOAD_A, What::Payload)]);
        assert_eq!(
            world.apply(&cleanup),
            Applied {
                removed: 1,
                skipped: 0
            }
        );
        assert_eq!(world.names(1), ["meta.1"]);
    }

    #[test]
    fn a_payload_exactly_a_grace_period_old_stays() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 24);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_younger_payload_stays() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 23);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_referenced_payload_stays() {
        let world = World::new();
        world.version(1, 1, &[(PAYLOAD_A, LISTING_A)], 100);
        world.put(1, PAYLOAD_A, b"x", 48);
        world.put(1, LISTING_A, b"x", 48);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_superseded_payload_stays_as_the_metadata_still_names_it() {
        let world = World::new();
        world.version(1, 1, &[(PAYLOAD_A, LISTING_A), (PAYLOAD_B, LISTING_B)], 100);
        for name in [PAYLOAD_A, LISTING_A, PAYLOAD_B, LISTING_B] {
            world.put(1, name, b"x", 48);
        }
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_payload_named_by_an_older_present_version_stays() {
        let world = World::new();
        world.version(1, 1, &[(PAYLOAD_A, LISTING_A)], 30);
        world.version(1, 2, &[(PAYLOAD_B, LISTING_B)], 1);
        world.put(1, PAYLOAD_A, b"x", 30);
        world.put(1, LISTING_A, b"x", 30);
        world.put(1, PAYLOAD_B, b"x", 1);
        world.put(1, LISTING_B, b"x", 1);
        assert_eq!(
            world.plan().steps,
            [remove(1, "meta.1", What::OldMetadata)],
            "the version goes, its payloads one run later"
        );
    }

    #[test]
    fn its_listing_goes_with_it() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 25);
        world.put(1, LISTING_A, b"x", 25);
        assert_eq!(
            world.plan().steps,
            [
                remove(1, PAYLOAD_A, What::Payload),
                remove(1, LISTING_A, What::Listing)
            ]
        );
    }

    #[test]
    fn an_open_claim_protects_an_old_unreferenced_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, "meta.2.claim", b"", 1);
        world.put(1, PAYLOAD_A, b"x", 48);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_young_claim_beside_its_version_is_not_open() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, "meta.1.claim", b"", 1);
        world.put(1, PAYLOAD_A, b"x", 48);
        assert_eq!(
            world.plan().steps,
            [remove(1, PAYLOAD_A, What::Payload)],
            "the claim has been used for the version, nobody is writing"
        );
    }

    #[test]
    fn an_abandoned_claim_does_not() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, "meta.2.claim", b"", 30);
        world.put(1, PAYLOAD_A, b"x", 48);
        assert_eq!(
            world.plan().steps,
            [remove(1, PAYLOAD_A, What::Payload)],
            "the claim itself stays, as nothing higher is committed"
        );
    }

    #[test]
    fn an_abandoned_first_upload_without_metadata_is_removed_after_the_grace_period() {
        let world = World::new();
        world.put(1, PAYLOAD_A, b"x", 25);
        world.put(1, LISTING_A, b"x", 1);
        assert_eq!(
            world.plan().steps,
            [remove(1, PAYLOAD_A, What::Payload)],
            "the young listing stays"
        );
    }

    // =====================================================
    // Metadata and claims
    // =====================================================

    #[test]
    fn an_old_version_is_removed_once_a_higher_one_exists() {
        let world = World::new();
        world.version(1, 1, &[], 25);
        world.version(1, 2, &[], 1);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, "meta.1", What::OldMetadata)]);
        world.apply(&cleanup);
        assert_eq!(world.names(1), ["meta.2"]);
    }

    #[test]
    fn the_highest_version_stays_however_old() {
        let world = World::new();
        world.version(1, 1, &[], 1000);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn an_old_version_younger_than_the_grace_period_stays() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.version(1, 2, &[], 1);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn an_abandoned_claim_below_the_highest_is_removed() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.version(1, 3, &[], 1);
        world.put(1, "meta.2.claim", b"", 25);
        assert_eq!(world.plan().steps, [remove(1, "meta.2.claim", What::Claim)]);
    }

    #[test]
    fn a_young_claim_below_the_highest_stays() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.version(1, 3, &[], 1);
        world.put(1, "meta.2.claim", b"", 1);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_claim_above_the_highest_stays() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.version(1, 2, &[], 1);
        world.put(1, "meta.3.claim", b"", 1000);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_claim_of_a_removed_version_goes_with_it() {
        let world = World::new();
        world.version(1, 1, &[], 25);
        world.put(1, "meta.1.claim", b"", 25);
        world.version(1, 2, &[], 1);
        assert_eq!(
            world.plan().steps,
            [
                remove(1, "meta.1", What::OldMetadata),
                remove(1, "meta.1.claim", What::Claim)
            ]
        );
    }

    #[test]
    fn the_claim_of_a_version_that_stays_stays() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.put(1, "meta.1.claim", b"", 25);
        world.version(1, 2, &[], 1);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn the_claim_of_the_highest_stays() {
        let world = World::new();
        world.version(1, 1, &[], 25);
        world.version(1, 2, &[], 100);
        world.put(1, "meta.2.claim", b"", 100);
        assert_eq!(world.plan().steps, [remove(1, "meta.1", What::OldMetadata)]);
    }

    #[test]
    fn steps_of_several_objects_come_in_id_order_and_in_the_order_of_the_kinds() {
        let world = World::new();
        world.put(2, PAYLOAD_A, b"x", 25);
        world.version(1, 1, &[], 25);
        world.version(1, 2, &[], 1);
        world.put(1, "meta.1.claim", b"", 25);
        world.put(1, PAYLOAD_B, b"x", 25);
        world.put(1, LISTING_B, b"x", 25);
        assert_eq!(
            world.plan().steps,
            [
                remove(1, "meta.1", What::OldMetadata),
                remove(1, "meta.1.claim", What::Claim),
                remove(1, PAYLOAD_B, What::Payload),
                remove(1, LISTING_B, What::Listing),
                remove(2, PAYLOAD_A, What::Payload),
            ]
        );
    }

    // =====================================================
    // Directories that are left alone
    // =====================================================

    fn assert_left_alone(world: &World, reason_part: &str) {
        let cleanup = world.plan();
        assert_eq!(
            cleanup.steps,
            [remove(2, PAYLOAD_A, What::Payload)],
            "the other object is still cleaned"
        );
        assert_eq!(cleanup.left_alone.len(), 1, "{cleanup:?}");
        assert_eq!(cleanup.left_alone[0].id, id(1));
        assert!(
            cleanup.left_alone[0].reason.contains(reason_part),
            "{}",
            cleanup.left_alone[0].reason
        );
    }

    #[test]
    fn a_directory_with_a_format_2_file_is_left_alone_and_reported() {
        let world = World::new();
        let newer = String::from_utf8(metadata_bytes(id(1), COLLECTION_ID, "a", &[]))
            .expect("UTF-8")
            .replacen("\"format\":1", "\"format\":2", 1);
        world.put(1, "meta.1", newer.as_bytes(), 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(2, PAYLOAD_A, b"x", 100);
        assert_left_alone(&world, "format 2");
    }

    #[test]
    fn an_unparsable_version_leaves_the_directory_alone() {
        let world = World::new();
        world.put(1, "meta.1", b"not json", 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(2, PAYLOAD_A, b"x", 100);
        assert_left_alone(&world, "meta.1");
    }

    #[test]
    fn a_version_of_another_id_leaves_the_directory_alone() {
        let world = World::new();
        let foreign = metadata_bytes(id(3), COLLECTION_ID, "a", &[]);
        world.put(1, "meta.1", &foreign, 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(2, PAYLOAD_A, b"x", 100);
        assert_left_alone(&world, &id(3).to_string());
    }

    #[test]
    fn nothing_in_such_a_directory_is_removed() {
        let world = World::new();
        world.put(1, "meta.1", b"not json", 100);
        world.version(1, 2, &[], 100);
        world.put(1, "meta.3", b"not json", 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(1, "meta.2.claim", b"", 100);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, []);
        assert_eq!(cleanup.left_alone.len(), 1);
        world.apply(&cleanup);
        assert_eq!(world.names(1).len(), 5);
    }

    #[test]
    fn a_directory_named_like_a_version_leaves_the_object_alone() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        std::fs::create_dir(world.dir(1).join("meta.2")).expect("a directory");
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(2, PAYLOAD_A, b"x", 100);
        assert_left_alone(&world, "meta.2");
    }

    #[test]
    fn a_directory_named_like_a_payload_is_ignored() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        std::fs::create_dir(world.dir(1).join(PAYLOAD_A)).expect("a directory");
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, []);
        assert_eq!(cleanup.left_alone, []);
    }

    #[test]
    fn a_version_removed_while_it_is_read_is_examined_again() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.version(1, 2, &[], 1);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.remote.before(Op::ReadFile, 1, |inner| {
            inner.delete(id(1), "meta.1").expect("removed by a cleanup");
        });
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, PAYLOAD_A, What::Payload)]);
        assert_eq!(cleanup.left_alone, []);
    }

    #[test]
    fn a_version_that_keeps_vanishing_leaves_the_object_alone() {
        let world = World::new();
        for version in 1..=4 {
            world.version(1, version, &[], 100);
        }
        for nth in 1..=3 {
            world.remote.before(Op::ReadFile, nth, move |inner| {
                inner
                    .delete(id(1), &format!("meta.{nth}"))
                    .expect("removed by a cleanup");
            });
        }
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, []);
        assert_eq!(cleanup.left_alone.len(), 1, "{cleanup:?}");
        assert!(
            cleanup.left_alone[0].reason.contains("disappear"),
            "{}",
            cleanup.left_alone[0].reason
        );
    }

    #[test]
    fn a_version_that_cannot_be_read_while_listed_is_an_error() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.remote.fail(Op::ReadFile, 1, Fault::Io);
        let error =
            plan(&world.remote, &world.settings(), &world.mode()).expect_err("the read fails");
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    // =====================================================
    // Dry runs
    // =====================================================

    #[test]
    fn a_dry_run_reports_age_dependent_steps_as_age_unknown() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.version(1, 2, &[], 1);
        world.version(1, 4, &[], 1);
        world.put(1, "meta.3.claim", b"", 1);
        world.put(1, PAYLOAD_A, b"x", 1);
        world.put(1, LISTING_A, b"x", 1);
        assert_eq!(
            world.dry_plan().steps,
            [
                unknown(1, "meta.1", What::OldMetadata),
                unknown(1, "meta.2", What::OldMetadata),
                unknown(1, "meta.3.claim", What::Claim),
                unknown(1, PAYLOAD_A, What::Payload),
                unknown(1, LISTING_A, What::Listing),
            ]
        );
    }

    #[test]
    fn a_dry_run_reports_a_payload_beside_a_claim_as_age_unknown() {
        let world = World::new();
        world.version(1, 1, &[], 1);
        world.put(1, "meta.2.claim", b"", 1);
        world.put(1, PAYLOAD_A, b"x", 100);
        assert_eq!(
            world.dry_plan().steps,
            [unknown(1, PAYLOAD_A, What::Payload)],
            "the claim may be abandoned by the time of the real run"
        );
    }

    #[test]
    fn a_dry_run_lists_only_what_could_be_removed() {
        let world = World::new();
        world.version(1, 1, &[(PAYLOAD_A, LISTING_A)], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(1, LISTING_A, b"x", 100);
        world.put(1, "meta.2.claim", b"", 100);
        assert_eq!(world.dry_plan().steps, []);
    }

    #[test]
    fn a_dry_run_never_reads_the_clock_and_writes_nothing() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.version(1, 2, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let before = world.names(1);
        world.dry_plan();
        assert_eq!(world.names(1), before);
        let writes = [
            Op::Clock,
            Op::CreateExclusive,
            Op::WriteTemp,
            Op::RenameIntoPlace,
            Op::PutPayload,
            Op::Delete,
            Op::DeletePartial,
        ];
        for call in world.remote.calls() {
            assert!(!writes.contains(&call.op), "{call:?}");
        }
        assert!(!world.root.path().join(".antiq-clock.laptop").exists());
    }

    // =====================================================
    // Applying
    // =====================================================

    #[test]
    fn a_version_committed_after_the_plan_keeps_the_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, PAYLOAD_A, What::Payload)]);
        world.version(1, 2, &[(PAYLOAD_A, LISTING_A)], 0);
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 1
            }
        );
        assert!(world.names(1).contains(&PAYLOAD_A.to_string()));
    }

    #[test]
    fn a_claim_made_after_the_plan_keeps_the_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        world.put(1, "meta.2.claim", b"", 0);
        world.apply(&cleanup);
        assert!(world.names(1).contains(&PAYLOAD_A.to_string()));
    }

    #[test]
    fn a_claim_made_while_the_plan_is_checked_again_keeps_the_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        let reads = world.remote.count(Op::ReadFile);
        world.remote.after(Op::ReadFile, reads + 1, |inner| {
            inner
                .create_exclusive(id(1), "meta.2.claim")
                .expect("another machine claims");
        });
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 1
            }
        );
        assert!(world.names(1).contains(&PAYLOAD_A.to_string()));
    }

    #[test]
    fn a_metadata_temp_name_made_while_the_plan_is_checked_again_keeps_the_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        let reads = world.remote.count(Op::ReadFile);
        world.remote.after(Op::ReadFile, reads + 1, |inner| {
            inner
                .write_temp(id(1), ".meta.2.studio.part", b"half")
                .expect("another machine writes");
        });
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 1
            }
        );
    }

    #[test]
    fn a_version_made_while_the_plan_is_checked_again_keeps_the_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        let reads = world.remote.count(Op::ReadFile);
        world.remote.after(Op::ReadFile, reads + 1, |inner| {
            inner
                .write_temp(
                    id(1),
                    "meta.2",
                    &metadata_bytes(id(1), COLLECTION_ID, "a", &[]),
                )
                .expect("another machine commits");
        });
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 1
            }
        );
    }

    #[test]
    fn a_listing_is_held_back_like_a_payload() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, LISTING_A, b"x", 100);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, LISTING_A, What::Listing)]);
        let reads = world.remote.count(Op::ReadFile);
        world.remote.after(Op::ReadFile, reads + 1, |inner| {
            inner
                .create_exclusive(id(1), "meta.2.claim")
                .expect("another machine claims");
        });
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 1
            }
        );
    }

    #[test]
    fn steps_run_in_the_documented_order() {
        let world = World::new();
        world.version(1, 1, &[], 25);
        world.put(1, "meta.1.claim", b"", 25);
        world.version(1, 2, &[], 1);
        world.put(1, PAYLOAD_A, b"x", 25);
        world.put(1, LISTING_A, b"x", 25);
        let cleanup = world.plan();
        world.apply(&cleanup);
        let deletes: Vec<Call> = world
            .remote
            .calls()
            .into_iter()
            .filter(|call| call.op == Op::Delete)
            .collect();
        assert_eq!(
            deletes,
            [
                Call::new(Op::Delete, "meta.1"),
                Call::new(Op::Delete, "meta.1.claim"),
                Call::new(Op::Delete, PAYLOAD_A),
                Call::new(Op::Delete, LISTING_A),
            ]
        );
        assert_eq!(world.names(1), ["meta.2"]);
    }

    #[test]
    fn a_remote_that_goes_away_stops_the_run() {
        let world = World::new();
        world.version(1, 1, &[], 25);
        world.version(1, 2, &[], 1);
        world.put(1, PAYLOAD_A, b"x", 25);
        let cleanup = world.plan();
        world.remote.fail(Op::Delete, 1, Fault::Away);
        let error = apply(
            &world.remote,
            &world.settings(),
            &world.lock,
            now(),
            &cleanup,
        )
        .expect_err("the remote is away");
        assert_eq!(error.kind(), ErrorKind::RemoteAway);
        assert_eq!(world.remote.count(Op::Delete), 1, "nothing else is tried");
    }

    #[test]
    fn a_step_that_is_gone_is_skipped() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        world.put(1, PAYLOAD_B, b"x", 100);
        let cleanup = world.plan();
        std::fs::remove_file(world.dir(1).join(PAYLOAD_A)).expect("another machine removes it");
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 1,
                skipped: 1
            }
        );
    }

    #[test]
    fn a_missing_file_counts_as_removed() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        world.remote.before(Op::Delete, 1, |inner| {
            inner.delete(id(1), PAYLOAD_A).expect("removed meanwhile");
        });
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 1,
                skipped: 0
            }
        );
    }

    #[test]
    fn an_empty_cleanup_asks_nothing_of_the_remote() {
        let world = World::new();
        let applied = world.apply(&Cleanup::default());
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 0
            }
        );
        assert!(world.remote.calls().is_empty());
    }

    #[test]
    fn an_object_left_alone_by_the_second_look_skips_its_steps() {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world.put(1, PAYLOAD_A, b"x", 100);
        let cleanup = world.plan();
        world.put(1, "meta.2", b"not json", 100);
        let applied = world.apply(&cleanup);
        assert_eq!(
            applied,
            Applied {
                removed: 0,
                skipped: 1
            }
        );
        assert!(world.names(1).contains(&PAYLOAD_A.to_string()));
    }

    // =====================================================
    // Temp names and partial files
    // =====================================================

    const OWN_TEMP: &str = ".0123456789abcdef-a.tar.zst.laptop.part";
    const FOREIGN_TEMP: &str = ".0123456789abcdef-a.tar.zst.studio.part";
    const OTHER_COLLECTION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    /// A world whose object 1 is owned by this collection.
    fn owned() -> World {
        let world = World::new();
        world.version(1, 1, &[], 100);
        world
    }

    #[test]
    fn an_own_temp_name_is_removed_early_in_an_owned_directory() {
        let world = owned();
        world.put(1, OWN_TEMP, b"half", 1);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, OWN_TEMP, What::TempFile)]);
        world.apply(&cleanup);
        assert_eq!(world.names(1), ["meta.1"]);
    }

    #[test]
    fn an_own_rsync_leftover_is_removed_early() {
        let world = owned();
        let leftover = format!("{OWN_TEMP}.Ab12Cd");
        world.put(1, &leftover, b"half", 1);
        assert_eq!(world.plan().steps, [remove(1, &leftover, What::TempFile)]);
    }

    #[test]
    fn another_machines_temp_name_younger_than_the_grace_period_stays() {
        let world = owned();
        world.put(1, FOREIGN_TEMP, b"half", 23);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn another_machines_temp_name_is_removed_after_the_grace_period() {
        let world = owned();
        world.put(1, FOREIGN_TEMP, b"half", 25);
        assert_eq!(
            world.plan().steps,
            [remove(1, FOREIGN_TEMP, What::TempFile)]
        );
    }

    #[test]
    fn another_machines_rsync_leftover_waits_for_the_grace_period() {
        let world = owned();
        let leftover = format!("{FOREIGN_TEMP}.Ab12Cd");
        world.put(1, &leftover, b"half", 23);
        assert_eq!(world.plan().steps, []);
        world.age(1, &leftover, 25 * HOUR);
        assert_eq!(world.plan().steps, [remove(1, &leftover, What::TempFile)]);
    }

    #[test]
    fn an_own_temp_name_in_a_directory_of_another_collection_waits_for_the_grace_period() {
        let world = World::new();
        let foreign = metadata_bytes(id(1), OTHER_COLLECTION, "a", &[]);
        world.put(1, "meta.1", &foreign, 100);
        world.put(1, OWN_TEMP, b"half", 1);
        assert_eq!(world.plan().steps, []);
        world.age(1, OWN_TEMP, 25 * HOUR);
        assert_eq!(world.plan().steps, [remove(1, OWN_TEMP, What::TempFile)]);
    }

    #[test]
    fn an_own_temp_name_in_a_directory_without_metadata_waits_for_the_grace_period() {
        let world = World::new();
        world.put(1, OWN_TEMP, b"half", 1);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_machine_name_that_ends_in_ours_is_not_ours() {
        let world = owned();
        world.put(1, ".0123456789abcdef-a.tar.zst.old.laptop.part", b"half", 1);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_temp_name_that_also_splits_as_ours_is_not_ours() {
        let world = owned();
        world.put(
            1,
            ".0123456789abcdef-a.tar.zst.tar.zst.laptop.part",
            b"half",
            1,
        );
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_temp_name_that_splits_two_ways_is_not_ours_even_when_the_first_split_is() {
        let mut world = owned();
        world.machine = "tar.zst.laptop";
        world.put(
            1,
            ".0123456789abcdef-a.tar.zst.tar.zst.laptop.part",
            b"half",
            1,
        );
        assert_eq!(
            world.plan().steps,
            [],
            "it is also the name of machine `laptop` for the payload `…tar.zst.tar.zst`"
        );
    }

    #[test]
    fn a_name_that_is_no_temp_name_is_never_removed() {
        let world = owned();
        for name in [
            "notes.txt",
            ".hidden",
            ".x.laptop.part",
            ".0123456789abcdef-a.tar.zst.laptop.part.toolong1",
            ".0123456789abcdef-a.tar.zst.laptop.part.Ab12C",
            ".0123456789abcdef-a.tar.zst..part",
            ".0123456789abcdef-a.tar.zst.laptop.part.Ab-2Cd",
            ".0123456789abcdef-a.tar.zst.laptop.part_Ab12Cd",
            ".0123456789abcdef-a.tar.zst.lap top.part",
            "0123456789abcdef-a.tar.zst.laptop.part",
            "meta.1.laptop.part",
        ] {
            world.put(1, name, b"x", 1000);
        }
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn an_own_metadata_temp_name_is_removed_early() {
        let world = owned();
        world.put(1, ".meta.2.laptop.part", b"half", 1);
        assert_eq!(
            world.plan().steps,
            [remove(1, ".meta.2.laptop.part", What::TempFile)]
        );
    }

    #[test]
    fn an_own_metadata_temp_name_beside_its_claim_stays() {
        let world = owned();
        world.put(1, ".meta.2.laptop.part", b"half", 1);
        world.put(1, "meta.2.claim", b"", 1);
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn an_own_metadata_temp_name_beside_a_claim_below_the_highest_is_removed() {
        let world = owned();
        world.version(1, 3, &[], 1);
        world.put(1, ".meta.2.laptop.part", b"half", 1);
        world.put(1, "meta.2.claim", b"", 1);
        assert_eq!(
            world.plan().steps,
            [
                remove(1, "meta.1", What::OldMetadata),
                remove(1, ".meta.2.laptop.part", What::TempFile)
            ]
        );
    }

    #[test]
    fn an_own_metadata_temp_name_beside_the_claim_of_the_highest_version_is_removed() {
        let world = owned();
        world.put(1, ".meta.1.laptop.part", b"half", 1);
        world.put(1, "meta.1.claim", b"", 1);
        assert_eq!(
            world.plan().steps,
            [remove(1, ".meta.1.laptop.part", What::TempFile)]
        );
    }

    #[test]
    fn a_metadata_temp_name_of_another_machine_waits_for_the_grace_period() {
        let world = owned();
        world.put(1, ".meta.2.studio.part", b"half", 1);
        assert_eq!(world.plan().steps, []);
        world.age(1, ".meta.2.studio.part", 25 * HOUR);
        assert_eq!(
            world.plan().steps,
            [remove(1, ".meta.2.studio.part", What::TempFile)]
        );
    }

    #[test]
    fn an_own_temp_name_of_a_pending_upload_is_kept() {
        let mut world = owned();
        world.put(1, OWN_TEMP, b"half", 100);
        world.keep.insert((id(1), OWN_TEMP.to_string()));
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_kept_name_of_another_object_protects_nothing_here() {
        let mut world = owned();
        world.put(1, OWN_TEMP, b"half", 1);
        world.keep.insert((id(2), OWN_TEMP.to_string()));
        assert_eq!(world.plan().steps, [remove(1, OWN_TEMP, What::TempFile)]);
    }

    #[test]
    fn an_own_partial_of_a_pending_upload_is_kept() {
        let mut world = owned();
        world.partial(1, OWN_TEMP, 100);
        world.keep.insert((id(1), OWN_TEMP.to_string()));
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn an_own_partial_file_is_removed_early() {
        let world = owned();
        world.partial(1, OWN_TEMP, 1);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, [remove(1, OWN_TEMP, What::PartialFile)]);
        world.apply(&cleanup);
        assert!(!world.dir(1).join(".antiq-partial").join(OWN_TEMP).exists());
    }

    #[test]
    fn a_foreign_partial_file_is_removed_after_the_grace_period() {
        let world = owned();
        world.partial(1, FOREIGN_TEMP, 23);
        assert_eq!(world.plan().steps, []);
        world.partial(1, FOREIGN_TEMP, 25);
        assert_eq!(
            world.plan().steps,
            [remove(1, FOREIGN_TEMP, What::PartialFile)]
        );
    }

    #[test]
    fn a_partial_name_that_is_no_temp_name_is_never_removed() {
        let world = owned();
        world.partial(1, "notes.txt", 1000);
        std::fs::create_dir(world.dir(1).join(".antiq-partial").join(OWN_TEMP))
            .expect("a directory");
        assert_eq!(world.plan().steps, []);
    }

    #[test]
    fn a_partial_file_is_removed_through_the_partial_directory() {
        let world = owned();
        world.partial(1, OWN_TEMP, 1);
        let cleanup = world.plan();
        world.apply(&cleanup);
        let deletes: Vec<Call> = world
            .remote
            .calls()
            .into_iter()
            .filter(|call| matches!(call.op, Op::Delete | Op::DeletePartial))
            .collect();
        assert_eq!(deletes, [Call::new(Op::DeletePartial, OWN_TEMP)]);
    }

    #[test]
    fn temp_files_and_partial_files_come_last_in_that_order() {
        let world = owned();
        world.put(1, PAYLOAD_A, b"x", 25);
        world.put(1, LISTING_A, b"x", 25);
        world.put(1, OWN_TEMP, b"half", 1);
        world.partial(1, FOREIGN_TEMP, 25);
        assert_eq!(
            world.plan().steps,
            [
                remove(1, PAYLOAD_A, What::Payload),
                remove(1, LISTING_A, What::Listing),
                remove(1, OWN_TEMP, What::TempFile),
                remove(1, FOREIGN_TEMP, What::PartialFile),
            ]
        );
    }

    #[test]
    fn a_dry_run_lists_early_removals() {
        let world = owned();
        world.put(1, OWN_TEMP, b"half", 1);
        world.partial(1, OWN_TEMP, 1);
        world.put(1, FOREIGN_TEMP, b"half", 1);
        assert_eq!(
            world.dry_plan().steps,
            [
                remove(1, OWN_TEMP, What::TempFile),
                unknown(1, FOREIGN_TEMP, What::TempFile),
                remove(1, OWN_TEMP, What::PartialFile),
            ]
        );
    }

    #[test]
    fn nothing_in_a_left_alone_directory_is_removed_not_even_temp_files() {
        let world = World::new();
        world.put(1, "meta.1", b"not json", 100);
        world.put(1, OWN_TEMP, b"half", 100);
        world.partial(1, OWN_TEMP, 100);
        let cleanup = world.plan();
        assert_eq!(cleanup.steps, []);
        assert_eq!(cleanup.left_alone.len(), 1);
        assert_eq!(world.remote.count(Op::ListPartial), 0);
    }

    // =====================================================
    // How steps read
    // =====================================================

    #[test]
    fn steps_describe_themselves() {
        assert_eq!(
            remove(1, PAYLOAD_A, What::Payload).describe(),
            format!("remove payload {PAYLOAD_A} of {}", id(1))
        );
        assert_eq!(
            unknown(1, "meta.2.claim", What::Claim).describe(),
            format!(
                "remove claim meta.2.claim of {} (age unknown in a dry run)",
                id(1)
            )
        );
        assert_eq!(
            remove(1, "meta.1", What::OldMetadata).describe(),
            format!("remove old metadata meta.1 of {}", id(1))
        );
        assert_eq!(
            remove(1, LISTING_A, What::Listing).describe(),
            format!("remove listing {LISTING_A} of {}", id(1))
        );
        assert_eq!(
            remove(1, OWN_TEMP, What::TempFile).describe(),
            format!("remove temp file {OWN_TEMP} of {}", id(1))
        );
        assert_eq!(
            remove(1, OWN_TEMP, What::PartialFile).describe(),
            format!("remove partial file {OWN_TEMP} of {}", id(1))
        );
    }

    #[test]
    fn steps_serialise_with_their_action() {
        assert_eq!(
            serde_json::to_value(remove(1, "meta.1", What::OldMetadata)).expect("serialises"),
            json!({"action": "remove", "id": id(1).to_string(), "name": "meta.1",
                   "what": "old_metadata"})
        );
        assert_eq!(
            serde_json::to_value(unknown(1, PAYLOAD_A, What::Payload)).expect("serialises"),
            json!({"action": "age_unknown", "id": id(1).to_string(), "name": PAYLOAD_A,
                   "what": "payload"})
        );
        assert_eq!(
            serde_json::to_value(remove(1, OWN_TEMP, What::PartialFile)).expect("serialises"),
            json!({"action": "remove", "id": id(1).to_string(), "name": OWN_TEMP,
                   "what": "partial_file"})
        );
        assert_eq!(
            serde_json::to_value(remove(1, OWN_TEMP, What::TempFile)).expect("serialises"),
            json!({"action": "remove", "id": id(1).to_string(), "name": OWN_TEMP,
                   "what": "temp_file"})
        );
    }
}
