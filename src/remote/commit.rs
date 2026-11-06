// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The commit protocol: how a writer gets a metadata version onto a remote
//! that other machines write to as well (spec "Committing a version",
//! ADR 0030).
//!
//! A version is committed once `meta.<N>` exists, so everything else the
//! protocol creates is there to make that rename safe: the payload goes
//! first under its final name, a claim `meta.<N>.claim` is created
//! exclusively, and the metadata waits under a temp name until the checks
//! after the claim have passed.

use super::{Created, Placement, Remote, RemoteError, RemoteFile, Upload};
use crate::id::ObjectId;
use crate::lock::RunLock;
use crate::meta::{self, FORMAT};
use crate::relpath::{Name, temp_name};
use crate::store::version_number;
use jiff::{SignedDuration, Timestamp};
use std::num::NonZeroU32;

/// What a commit needs to know about this machine and the collection.
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    /// The machine's name, which temp names carry.
    pub machine: &'a str,
    /// How old a claim without metadata has to be to count as abandoned.
    pub grace: SignedDuration,
}

impl<'a> Settings<'a> {
    pub fn new(machine: &'a str, grace_hours: u32) -> Settings<'a> {
        Settings {
            machine,
            grace: SignedDuration::from_hours(i64::from(grace_hours)),
        }
    }
}

/// One version to commit.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub id: ObjectId,
    /// The highest committed version the change was made on; 0 for a new
    /// object.
    pub based_on: u32,
    /// The bytes of the version, history already stamped. The caller keeps
    /// them until the commit is known to have happened, so a retry offers
    /// the same bytes and is recognised.
    pub metadata: &'a [u8],
    /// The payloads and listings of this object that the metadata names and
    /// the remote may not have yet.
    pub uploads: &'a [Upload<'a>],
    /// The caller has put `uploads` on the remote and flushed it, so the
    /// first step puts nothing and flushes nothing. A file that is gone
    /// when the metadata is checked is still put again from `uploads`.
    pub put_already: bool,
}

/// How a commit ended, when the remote itself did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `meta.<number>` exists and is the highest version.
    Committed { number: NonZeroU32 },
    /// `meta.<number>` exists, and a higher version was committed before
    /// the commit looked again. The caller treats it as a conflict and
    /// never drops its local archive because of this version: cleanup
    /// removes it once it is older than the grace period, and its payloads
    /// one run later.
    Overtaken { number: NonZeroU32, higher: u32 },
    /// `meta.<number>` exists, and another writer holds the claim `claim`
    /// above it, which no committed version has yet. The claim's writer
    /// either skipped this commit's claim as abandoned or is building on
    /// this version, and the commit cannot tell which. The version stays
    /// current if the claim is released, and a higher version that does
    /// not build on it replaces it. The caller keeps its local archive,
    /// as for `Overtaken`, and looks at the remote again at its next sync.
    Contested { number: NonZeroU32, claim: u32 },
    /// A call that retried an earlier attempt found `meta.<number>`, which
    /// that attempt committed, and a higher version that names every file
    /// this commit names. The higher version may build on this one, and the
    /// commit cannot tell, as the metadata records no base. The caller
    /// pulls the remote and judges the two versions, which is a conflict
    /// only if they differ in more than this commit's change, and keeps its
    /// local archive until then.
    Landed { number: NonZeroU32, highest: u32 },
    /// Nothing of this commit is committed.
    Refused(Refusal),
}

/// Why a commit did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The metadata does not parse, belongs to another object or has a
    /// format other than 1. Nothing was asked of the remote.
    InvalidMetadata(String),
    /// The highest version on the remote cannot be read; nothing is
    /// written on top of it.
    UnreadableHighest { number: u32, reason: String },
    /// The remote has a committed version above `based_on`.
    Behind { highest: u32 },
    /// The remote has less than `based_on`.
    MissingBase { based_on: u32, highest: u32 },
    /// Another writer holds a claim above the highest version that is not
    /// abandoned yet, or above the claim of this commit. The first claimer
    /// goes on; this commit is retried at the next sync.
    ClaimOpen { number: u32 },
    /// Another writer created the claim first.
    ClaimTaken { number: u32 },
    /// The claim was gone when the writer looked before the rename.
    ClaimLost { number: u32 },
    /// A version above the one this commit is based on appeared after the
    /// claim.
    NewerCommitted { highest: u32 },
    /// A file the metadata names is not on the remote and cannot be put
    /// again.
    PayloadMissing { name: String },
    /// The version was committed by another writer between the last check
    /// and the rename.
    TargetTaken { number: u32 },
    /// A claim or version already has the largest number.
    VersionLimit { number: u32 },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::InvalidMetadata(reason) => {
                write!(formatter, "the metadata cannot be committed: {reason}")
            }
            Refusal::UnreadableHighest { number, reason } => write!(
                formatter,
                "version {number} on the remote cannot be read: {reason}"
            ),
            Refusal::Behind { highest } => write!(
                formatter,
                "the remote has version {highest}, which this change does not build on"
            ),
            Refusal::MissingBase { based_on, highest } => write!(
                formatter,
                "the remote has version {highest}, below version {based_on} that this change builds on"
            ),
            Refusal::ClaimOpen { number } => {
                write!(
                    formatter,
                    "another writer holds the claim for version {number}"
                )
            }
            Refusal::ClaimTaken { number } => write!(
                formatter,
                "another writer created the claim for version {number} first"
            ),
            Refusal::ClaimLost { number } => write!(
                formatter,
                "the claim for version {number} was gone before the rename"
            ),
            Refusal::NewerCommitted { highest } => write!(
                formatter,
                "version {highest} was committed while this change was being committed"
            ),
            Refusal::PayloadMissing { name } => write!(
                formatter,
                "{name} is not on the remote and cannot be put again"
            ),
            Refusal::TargetTaken { number } => write!(
                formatter,
                "another writer committed version {number} before the rename"
            ),
            Refusal::VersionLimit { number } => write!(
                formatter,
                "version {number} is the largest number a version can have"
            ),
        }
    }
}

/// Commits `request.metadata` as the next version of the object.
///
/// Remote failures (away, broken, I/O, a payload name taken by other
/// bytes) are `Err`; every outcome of the protocol itself is an
/// [`Outcome`]. The caller acts on it as follows:
///
/// - `Committed`: the version is current. The caller may drop what the
///   commit made redundant locally.
/// - `Overtaken`: the version is committed, and a higher committed version
///   exists that is not known to build on it. The caller handles a
///   conflict (ADR 0017) and keeps its local archive.
/// - `Landed`: a retry finds its version committed by the earlier attempt,
///   and a higher version that names all its files. The caller pulls the
///   remote and judges the versions, keeping its local archive until then.
/// - `Contested`: the version is committed, and a claim above it is open.
///   The caller keeps its local archive, treats the version as not settled
///   and reads the remote again at the next sync: the claim becomes a
///   higher version (a conflict, or a change built on this version) or
///   goes away (this version is current).
/// - `Refused`: nothing of this commit is committed.
///
/// A commit that fails after the rename leaves the version
/// committed, and calling again with the same request finds it and returns
/// the outcome without writing.
///
/// A named file that is gone after the rename and cannot be put again
/// leaves the version committed and is an `Err`. When the put itself
/// fails, that error comes back unchanged, so away stays away and an I/O
/// error stays an I/O error. When the put succeeds and the file is still
/// not there, the error is [`RemoteError::Broken`], naming the file and
/// the version. The caller treats any `Err` as not committed and keeps its
/// local archive.
///
/// A refusal after the claim that is known before the rename
/// (`PayloadMissing`, `NewerCommitted`, `ClaimOpen` above the commit's own
/// claim) removes the claim and the temp file, unless the version number is
/// taken. `ClaimLost` removes the temp file. A failed temp write removes
/// both on a best-effort basis: the removal stops at its first error and
/// the write's error is the one returned. Every other stop leaves them: the
/// next commit recognises a claim with this machine's metadata temp beside
/// it as its own interrupted attempt, which the run lock proves is not in
/// flight, and skips it.
///
/// Known limits:
///
/// - A crash between creating the claim and writing the temp file leaves a
///   claim that cannot be told from another machine's. It blocks the
///   object with `ClaimOpen` for one grace period.
/// - Own-claim recognition rests on the run lock, which proves only that
///   this collection on this machine is not running. A collection copied
///   to another machine with the same name, or two collections of one
///   machine on one remote, can have a live claim of the other writer
///   skipped. That writer then gets `NewerCommitted`, `ClaimOpen`,
///   `Overtaken` or `Contested`, not a silent loss.
pub fn commit(
    remote: &dyn Remote,
    settings: &Settings,
    _lock: &RunLock,
    request: &Request,
) -> Result<Outcome, RemoteError> {
    match run(remote, settings, request) {
        Ok(outcome) => Ok(outcome),
        Err(Stop::Refused(refusal)) => Ok(Outcome::Refused(refusal)),
        Err(Stop::Failed(error)) => Err(error),
    }
}

/// Ends a commit early: with a refusal, or with a failure of the remote.
enum Stop {
    Refused(Refusal),
    Failed(RemoteError),
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Stop {
        Stop::Refused(refusal)
    }
}

impl From<RemoteError> for Stop {
    fn from(error: RemoteError) -> Stop {
        Stop::Failed(error)
    }
}

// =========================================================
// Names
// =========================================================

pub(super) fn version_name(number: u32) -> String {
    format!("meta.{number}")
}

pub(super) fn claim_name(number: u32) -> String {
    format!("meta.{number}.claim")
}

/// The number of a claim file's name, `meta.<N>.claim`.
pub(super) fn claim_number(name: &str) -> Option<u32> {
    version_number(name.strip_suffix(".claim")?)
}

/// The name this machine writes the metadata of version `number` under
/// until it is renamed into place.
pub(super) fn metadata_temp_name(machine: &str, number: u32) -> String {
    let final_name =
        Name::from_bytes(version_name(number).into_bytes()).expect("`meta.<N>` is a valid name");
    String::from_utf8(temp_name(&final_name, machine).as_bytes().to_vec())
        .expect("a temp name is built from ASCII")
}

// =========================================================
// What the object's directory holds
// =========================================================

/// One listing of an object's directory.
struct Directory(Vec<RemoteFile>);

impl Directory {
    fn load(remote: &dyn Remote, id: ObjectId) -> Result<Directory, RemoteError> {
        Ok(Directory(remote.list_dir(id)?))
    }

    /// A regular file by name.
    fn file(&self, name: &str) -> Option<&RemoteFile> {
        self.0.iter().find(|file| !file.is_dir && file.name == name)
    }

    /// The numbers of the committed versions, ascending.
    fn versions(&self) -> Vec<u32> {
        let mut numbers: Vec<u32> = self
            .0
            .iter()
            .filter(|file| !file.is_dir)
            .filter_map(|file| version_number(&file.name))
            .collect();
        numbers.sort_unstable();
        numbers
    }

    /// The highest committed version; 0 when there is none.
    fn highest(&self) -> u32 {
        self.versions().last().copied().unwrap_or(0)
    }

    /// The claims above `highest`, ascending, with their modification times.
    fn claims_above(&self, highest: u32) -> Vec<(u32, Timestamp)> {
        let mut claims: Vec<(u32, Timestamp)> = self
            .0
            .iter()
            .filter(|file| !file.is_dir)
            .filter_map(|file| Some((claim_number(&file.name)?, file.modified)))
            .filter(|(number, _)| *number > highest)
            .collect();
        claims.sort_unstable();
        claims
    }

    /// The files of `named` that are absent or have another size.
    fn missing(&self, named: &[Named]) -> Vec<String> {
        named
            .iter()
            .filter(|wanted| {
                self.file(&wanted.file)
                    .is_none_or(|found| wanted.size.is_some_and(|size| size != found.size))
            })
            .map(|wanted| wanted.file.clone())
            .collect()
    }
}

/// A file the metadata names, with the size it records where it records
/// one.
struct Named {
    file: String,
    size: Option<u64>,
}

/// The metadata of `bytes`, if this version reads it and it belongs to
/// `id`; else the reason it cannot be used.
pub(super) fn read_document(id: ObjectId, bytes: &[u8]) -> Result<meta::Metadata, String> {
    let metadata = meta::parse(bytes).map_err(|error| error.to_string())?;
    if metadata.id != id {
        return Err(format!("it belongs to {}", metadata.id));
    }
    if metadata.format != FORMAT {
        return Err(format!(
            "it has format {}, and this version reads format {FORMAT}",
            metadata.format
        ));
    }
    Ok(metadata)
}

fn named_files(request: &Request) -> Result<Vec<Named>, Refusal> {
    let metadata = read_document(request.id, request.metadata).map_err(Refusal::InvalidMetadata)?;
    Ok(files_of(&metadata))
}

/// The payloads and listings `metadata` names.
fn files_of(metadata: &meta::Metadata) -> Vec<Named> {
    let mut named = Vec::new();
    for payload in &metadata.payloads {
        named.push(Named {
            file: payload.file.as_str().to_string(),
            size: Some(payload.size),
        });
        if let Some(listing) = &payload.listing {
            named.push(Named {
                file: listing.as_str().to_string(),
                size: None,
            });
        }
    }
    named
}

/// A file of the object read in full. A read that fails because a
/// cleanup removed the file in the meantime is told from one that fails
/// for another reason by listing the directory again: the trait cannot
/// say it. `None` is a file that is no longer there.
pub(super) fn read_listed(
    remote: &dyn Remote,
    id: ObjectId,
    name: &str,
) -> Result<Option<Vec<u8>>, RemoteError> {
    match remote.read_file(id, name) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) => {
            if Directory::load(remote, id)?.file(name).is_some() {
                Err(error)
            } else {
                Ok(None)
            }
        }
    }
}

// =========================================================
// The protocol
// =========================================================

/// The directory as the commit finds it before it writes.
struct Inspected {
    directory: Directory,
    highest: u32,
    /// The bytes of the highest version; none while there is no version.
    highest_bytes: Option<Vec<u8>>,
}

fn run(remote: &dyn Remote, settings: &Settings, request: &Request) -> Result<Outcome, Stop> {
    let named = named_files(request)?;

    // Before an upload that takes minutes, so that a commit that cannot
    // happen is refused first. A retry after a failure past the rename
    // finds its own version here.
    let found = inspect(remote, request.id)?;
    if found.highest > request.based_on
        && let Some(number) = own_version(remote, request, &found)?
    {
        return confirm(remote, request, &named, number, true);
    }
    require_base(request.based_on, found.highest)?;

    // The payloads are durable under their final names before a claim for
    // metadata that names them exists.
    if !request.put_already {
        for upload in request.uploads {
            remote.put_payload(upload)?;
        }
        if !request.uploads.is_empty() {
            remote.flush()?;
        }
    }

    // State may have changed during the upload.
    let found = inspect(remote, request.id)?;
    require_base(request.based_on, found.highest)?;
    let number = next_number(remote, settings, &found)?;
    match remote.create_exclusive(request.id, &claim_name(number))? {
        Created::Yes => {}
        Created::AlreadyExists => return Err(Refusal::ClaimTaken { number }.into()),
    }

    // Everything that could still make the rename wrong is checked after
    // the claim, against a listing taken after the temp file is durable.
    let temp = metadata_temp_name(settings.machine, number);
    if let Err(error) = remote.write_temp(request.id, &temp, request.metadata) {
        // Without this the claim blocks the object for a grace period. The
        // write failed, so the remote may fail again here; the error to
        // report is the write's.
        let _ = release(remote, request.id, number, &temp, &found.directory);
        return Err(error.into());
    }
    remote.flush()?;
    let listed = Directory::load(remote, request.id)?;
    if listed.highest() > found.highest {
        let refusal = Refusal::NewerCommitted {
            highest: listed.highest(),
        };
        release(remote, request.id, number, &temp, &listed)?;
        return Err(refusal.into());
    }
    if listed.file(&claim_name(number)).is_none() {
        // Only the temp file is ours to take back. Left behind, it would
        // pass for this machine's interrupted attempt under the claim
        // another writer makes for this number.
        remote.delete(request.id, &temp)?;
        remote.flush()?;
        return Err(Refusal::ClaimLost { number }.into());
    }
    // A writer whose number is above ours skipped our claim as abandoned.
    // Its version would not build on ours, so ours must not be committed.
    if let Some(&(claim, _)) = listed.claims_above(number).first() {
        release(remote, request.id, number, &temp, &listed)?;
        return Err(Refusal::ClaimOpen { number: claim }.into());
    }
    let restored = restore_missing(remote, request, &named, listed)?;
    // The puts of a repair take time, in which a version can be committed.
    if restored.directory.highest() > found.highest {
        let refusal = Refusal::NewerCommitted {
            highest: restored.directory.highest(),
        };
        release(remote, request.id, number, &temp, &restored.directory)?;
        return Err(refusal.into());
    }
    if let Some(name) = restored.missing {
        release(remote, request.id, number, &temp, &restored.directory)?;
        return Err(Refusal::PayloadMissing { name }.into());
    }

    match remote.rename_into_place(request.id, &temp, &version_name(number))? {
        Placement::Placed => {}
        Placement::TargetExists => return Err(Refusal::TargetTaken { number }.into()),
    }
    remote.flush()?;
    confirm(remote, request, &named, number, false)
}

/// Lists the directory and reads the highest version, which has to be
/// usable for a new one to be written on top of it.
fn inspect(remote: &dyn Remote, id: ObjectId) -> Result<Inspected, Stop> {
    let directory = Directory::load(remote, id)?;
    let highest = directory.highest();
    let mut highest_bytes = None;
    if highest > 0 {
        let bytes = remote.read_file(id, &version_name(highest))?;
        read_document(id, &bytes).map_err(|reason| Refusal::UnreadableHighest {
            number: highest,
            reason,
        })?;
        highest_bytes = Some(bytes);
    }
    Ok(Inspected {
        directory,
        highest,
        highest_bytes,
    })
}

fn require_base(based_on: u32, highest: u32) -> Result<(), Refusal> {
    if highest > based_on {
        Err(Refusal::Behind { highest })
    } else if highest < based_on {
        Err(Refusal::MissingBase { based_on, highest })
    } else {
        Ok(())
    }
}

/// The version above `based_on` that holds exactly the bytes of this
/// request, if an earlier attempt of it got past the rename.
fn own_version(
    remote: &dyn Remote,
    request: &Request,
    found: &Inspected,
) -> Result<Option<u32>, RemoteError> {
    for number in found.directory.versions() {
        if number <= request.based_on {
            continue;
        }
        let bytes = if number == found.highest {
            found.highest_bytes.clone()
        } else {
            read_listed(remote, request.id, &version_name(number))?
        };
        if bytes.as_deref() == Some(request.metadata) {
            return Ok(Some(number));
        }
    }
    Ok(None)
}

/// The number for the new version: one above the highest committed
/// version or the highest claim that is skipped. A claim is skipped when
/// it is abandoned, which is when it is older than the grace period, or
/// when it is this machine's own interrupted attempt. Any other claim is a
/// writer at work, and this commit gives way.
fn next_number(remote: &dyn Remote, settings: &Settings, found: &Inspected) -> Result<u32, Stop> {
    let mut taken = found.highest;
    let mut now: Option<Timestamp> = None;
    for (number, modified) in found.directory.claims_above(found.highest) {
        let own = found
            .directory
            .file(&metadata_temp_name(settings.machine, number))
            .is_some();
        if !own {
            let now = match now {
                Some(now) => now,
                None => *now.insert(remote.clock()?),
            };
            if now.duration_since(modified) <= settings.grace {
                return Err(Refusal::ClaimOpen { number }.into());
            }
        }
        taken = taken.max(number);
    }
    taken
        .checked_add(1)
        .ok_or_else(|| Refusal::VersionLimit { number: taken }.into())
}

/// Takes back the temp file and the claim of a commit that is refused
/// before the rename. The temp file goes first, and a flush separates it
/// from the claim: a stop between the two leaves a claim without a temp
/// file, which blocks safely, and never a temp file without a claim, which
/// would pass for the interrupted attempt of this machine under a claim
/// that another writer made. Nothing is taken back where the version
/// exists, as its number is then in use.
fn release(
    remote: &dyn Remote,
    id: ObjectId,
    number: u32,
    temp: &str,
    directory: &Directory,
) -> Result<(), RemoteError> {
    if directory.file(&version_name(number)).is_some() {
        return Ok(());
    }
    remote.delete(id, temp)?;
    remote.flush()?;
    remote.delete(id, &claim_name(number))
}

/// The directory after payloads that were missing were put again.
struct Restored {
    directory: Directory,
    /// A file the metadata names that is still not there.
    missing: Option<String>,
}

/// Puts a missing or wrong-sized file of the metadata again, when this
/// commit has it to upload. Another machine's cleanup may have taken an
/// old unreferenced payload that an upload found in place and did not
/// rewrite.
fn restore_missing(
    remote: &dyn Remote,
    request: &Request,
    named: &[Named],
    directory: Directory,
) -> Result<Restored, RemoteError> {
    let missing = directory.missing(named);
    let uploads: Vec<Option<&Upload>> = missing
        .iter()
        .map(|name| {
            request
                .uploads
                .iter()
                .find(|upload| upload.final_name == name)
        })
        .collect();
    if let Some(position) = uploads.iter().position(Option::is_none) {
        return Ok(Restored {
            directory,
            missing: Some(missing[position].clone()),
        });
    }
    if missing.is_empty() {
        return Ok(Restored {
            directory,
            missing: None,
        });
    }
    for upload in uploads.into_iter().flatten() {
        remote.put_payload(upload)?;
    }
    remote.flush()?;
    let directory = Directory::load(remote, request.id)?;
    let missing = directory.missing(named).into_iter().next();
    Ok(Restored { directory, missing })
}

/// Looks at the directory once the version is committed: the files it
/// names are checked once more, a higher version makes it overtaken and a
/// claim above it makes it contested.
fn confirm(
    remote: &dyn Remote,
    request: &Request,
    named: &[Named],
    number: u32,
    retried: bool,
) -> Result<Outcome, Stop> {
    let listed = Directory::load(remote, request.id)?;
    let restored = restore_missing(remote, request, named, listed)?;
    if let Some(name) = restored.missing {
        return Err(RemoteError::Broken(format!(
            "{name} is gone from the remote although {} names it",
            version_name(number)
        ))
        .into());
    }
    let highest = restored.directory.highest();
    let number = NonZeroU32::new(number).expect("a version number is above zero");
    Ok(if highest > number.get() {
        if retried && builds_on(remote, request, named, highest)? {
            Outcome::Landed { number, highest }
        } else {
            Outcome::Overtaken {
                number,
                higher: highest,
            }
        }
    } else if let Some(&(claim, _)) = restored.directory.claims_above(number.get()).first() {
        Outcome::Contested { number, claim }
    } else {
        Outcome::Committed { number }
    })
}

/// Whether the committed version `highest` names every file `named` does.
/// The metadata records no base version, so this is the only sign that a
/// version was written on top of one that holds these files.
fn builds_on(
    remote: &dyn Remote,
    request: &Request,
    named: &[Named],
    highest: u32,
) -> Result<bool, RemoteError> {
    let Some(bytes) = read_listed(remote, request.id, &version_name(highest))? else {
        return Ok(false);
    };
    let Ok(metadata) = read_document(request.id, &bytes) else {
        return Ok(false);
    };
    let theirs = files_of(&metadata);
    Ok(named
        .iter()
        .all(|ours| theirs.iter().any(|their| their.file == ours.file)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::error::{ErrorKind, HasKind};
    use crate::lock;
    use crate::meta::{
        self, Checksum, CreatedSource, FileName, Lifecycle, Metadata, Payload, PayloadKind,
    };
    use crate::relpath::{Name, RelPath};
    use crate::remote::path::PathRemote;
    use crate::remote::testing::{Call, Fault, FaultRemote, Op, object, path_remote, remote_root};
    use jiff::Timestamp;

    #[test]
    fn every_refusal_reads_as_one_sentence() {
        let cases = [
            (
                Refusal::InvalidMetadata("it does not parse".to_string()),
                "the metadata cannot be committed: it does not parse",
            ),
            (
                Refusal::UnreadableHighest {
                    number: 3,
                    reason: "it is cut off".to_string(),
                },
                "version 3 on the remote cannot be read: it is cut off",
            ),
            (
                Refusal::Behind { highest: 4 },
                "the remote has version 4, which this change does not build on",
            ),
            (
                Refusal::MissingBase {
                    based_on: 5,
                    highest: 2,
                },
                "the remote has version 2, below version 5 that this change builds on",
            ),
            (
                Refusal::ClaimOpen { number: 6 },
                "another writer holds the claim for version 6",
            ),
            (
                Refusal::ClaimTaken { number: 7 },
                "another writer created the claim for version 7 first",
            ),
            (
                Refusal::ClaimLost { number: 8 },
                "the claim for version 8 was gone before the rename",
            ),
            (
                Refusal::NewerCommitted { highest: 9 },
                "version 9 was committed while this change was being committed",
            ),
            (
                Refusal::PayloadMissing {
                    name: "a.tar.zst".to_string(),
                },
                "a.tar.zst is not on the remote and cannot be put again",
            ),
            (
                Refusal::TargetTaken { number: 10 },
                "another writer committed version 10 before the rename",
            ),
            (
                Refusal::VersionLimit { number: 11 },
                "version 11 is the largest number a version can have",
            ),
        ];
        for (refusal, text) in cases {
            assert_eq!(refusal.to_string(), text);
        }
    }
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    const PAYLOAD: &str = "0123456789abcdef-a.tar.zst";
    const LISTING: &str = "0123456789abcdef.listing.zst";
    const CLAIM: &str = "meta.1.claim";
    const OWN_TEMP: &str = ".meta.1.laptop.part";

    fn id() -> ObjectId {
        object(1)
    }

    fn now() -> Timestamp {
        "2026-11-04T12:00:00Z".parse().expect("a time")
    }

    /// A file to upload: its bytes, where they live locally and their hash.
    struct Parcel {
        name: &'static str,
        temp: String,
        source: PathBuf,
        sha256: String,
    }

    impl Parcel {
        fn upload(&self) -> Upload<'_> {
            Upload::new(id(), &self.temp, self.name, &self.source, &self.sha256)
        }
    }

    /// A remote shared by two machines and the commit's inputs.
    struct World {
        root: tempfile::TempDir,
        remote: FaultRemote,
        parcels: Vec<Parcel>,
        lock: lock::RunLock,
        _local: tempfile::TempDir,
    }

    impl World {
        fn new() -> World {
            let root = remote_root();
            let remote = FaultRemote::new(path_remote(root.path(), "laptop"));
            remote.set_clock(now());
            let local = tempfile::tempdir().expect("temp dir");
            let parcels = [(PAYLOAD, &b"payload bytes"[..]), (LISTING, &b"listing"[..])]
                .into_iter()
                .map(|(name, bytes)| {
                    let source = local.path().join(name);
                    std::fs::write(&source, bytes).expect("a local file");
                    Parcel {
                        name,
                        temp: format!(".{name}.laptop.part"),
                        source,
                        sha256: crate::pack::hashing::hex(&Sha256::digest(bytes)),
                    }
                })
                .collect();
            let lock = lock::acquire(&local.path().join("lock"), "laptop", &SystemClock)
                .expect("the lock is free");
            World {
                root,
                remote,
                parcels,
                lock,
                _local: local,
            }
        }

        fn settings(&self) -> Settings<'static> {
            Settings::new("laptop", 24)
        }

        fn dir(&self) -> PathBuf {
            self.root.path().join("objects").join(id().to_string())
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = match std::fs::read_dir(self.dir()) {
                Ok(entries) => entries
                    .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
                    .collect(),
                Err(_) => Vec::new(),
            };
            names.sort();
            names
        }

        fn put(&self, name: &str, bytes: &[u8]) {
            std::fs::create_dir_all(self.dir()).expect("the object directory");
            std::fs::write(self.dir().join(name), bytes).expect("a remote file");
        }

        /// Sets the age of a remote file against the remote's clock.
        fn age(&self, name: &str, seconds: i64) {
            let time = filetime::FileTime::from_unix_time(now().as_second() - seconds, 0);
            filetime::set_file_mtime(self.dir().join(name), time).expect("the time is set");
        }

        fn read(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.dir().join(name)).expect("the file exists")
        }

        /// A committed version of the object, written straight to the
        /// directory.
        fn put_version(&self, number: u32, path: &str) {
            self.put(&format!("meta.{number}"), &metadata(path, false));
        }

        fn uploads(&self) -> Vec<Upload<'_>> {
            self.parcels.iter().map(Parcel::upload).collect()
        }

        fn commit(&self, based_on: u32, metadata: &[u8]) -> Result<Outcome, RemoteError> {
            let uploads = self.uploads();
            self.commit_with(based_on, metadata, &uploads)
        }

        fn commit_with(
            &self,
            based_on: u32,
            metadata: &[u8],
            uploads: &[Upload<'_>],
        ) -> Result<Outcome, RemoteError> {
            commit(
                &self.remote,
                &self.settings(),
                &self.lock,
                &Request {
                    id: id(),
                    based_on,
                    metadata,
                    uploads,
                    put_already: false,
                },
            )
        }

        /// A commit of a caller that put the uploads itself.
        fn commit_put_already(&self, metadata: &[u8]) -> Outcome {
            let uploads = self.uploads();
            for upload in &uploads {
                self.remote.put_payload(upload).expect("the caller puts");
            }
            self.remote.flush().expect("the caller flushes");
            commit(
                &self.remote,
                &self.settings(),
                &self.lock,
                &Request {
                    id: id(),
                    based_on: 0,
                    metadata,
                    uploads: &uploads,
                    put_already: true,
                },
            )
            .expect("the commit runs")
        }

        fn committed(&self, based_on: u32, metadata: &[u8]) -> Outcome {
            self.commit(based_on, metadata).expect("the commit runs")
        }
    }

    fn payload_entry(file: &str, listing: Option<&str>, size: u64) -> Payload {
        Payload {
            kind: PayloadKind::Archive,
            version: 1,
            file: FileName::new(file).expect("a payload name"),
            package_format: "tar+zstd/1".into(),
            size,
            checksum: Checksum::sha256("00"),
            listing: listing.map(|name| FileName::new(name).expect("a listing name")),
            machine: "laptop".into(),
            created: now(),
            verified: None,
            superseded: false,
            encoder: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Metadata of the object at `path`; with payloads, it names the
    /// archive and listing the world uploads.
    fn metadata(path: &str, with_payloads: bool) -> Vec<u8> {
        let payloads = if with_payloads {
            vec![payload_entry(PAYLOAD, Some(LISTING), 13)]
        } else {
            Vec::new()
        };
        let path = RelPath::parse(path).expect("a valid path");
        let name = Name::from_bytes(path.components().last().expect("a name").to_vec())
            .expect("a valid name");
        meta::to_bytes(&Metadata {
            format: 1,
            id: id(),
            collection: "01BX5ZZKBKACTAV9WEVGEMMVRZ".parse().expect("an ID"),
            path,
            name,
            created: now(),
            created_source: CreatedSource::Birth,
            lifecycle: Lifecycle::Live,
            payloads,
            history: Vec::new(),
            extra: serde_json::Map::new(),
        })
        .expect("metadata serialises")
    }

    fn number(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("not zero")
    }

    fn committed(value: u32) -> Outcome {
        Outcome::Committed {
            number: number(value),
        }
    }

    fn refused(refusal: Refusal) -> Outcome {
        Outcome::Refused(refusal)
    }

    /// What another machine does to commit version `number`.
    fn commit_as_studio(inner: &PathRemote, number: u32, bytes: &[u8]) {
        let temp = format!(".meta.{number}.studio.part");
        inner.write_temp(id(), &temp, bytes).expect("writes");
        inner
            .rename_into_place(id(), &temp, &format!("meta.{number}"))
            .expect("renames");
    }

    fn calls_after(world: &World, skipped: usize) -> Vec<Call> {
        world.remote.calls().split_off(skipped)
    }

    // =====================================================
    // The steps of a commit
    // =====================================================

    #[test]
    fn a_first_commit_writes_meta_1() {
        let world = World::new();
        let bytes = metadata("a", true);
        assert_eq!(world.committed(0, &bytes), committed(1));
        assert_eq!(world.read("meta.1"), bytes);
        assert_eq!(
            world.names(),
            [PAYLOAD, LISTING, "meta.1", CLAIM],
            "the temp names are renamed away"
        );
    }

    #[test]
    fn a_commit_on_version_2_writes_meta_3() {
        let world = World::new();
        world.put_version(1, "a");
        world.put_version(2, "a");
        assert_eq!(world.committed(2, &metadata("b", true)), committed(3));
        assert!(world.names().contains(&"meta.3".to_string()));
        assert!(world.names().contains(&"meta.3.claim".to_string()));
    }

    #[test]
    fn a_metadata_only_commit_has_no_uploads() {
        let world = World::new();
        let outcome = world
            .commit_with(0, &metadata("a", false), &[])
            .expect("the commit runs");
        assert_eq!(outcome, committed(1));
        assert_eq!(world.remote.count(Op::PutPayload), 0);
        assert_eq!(
            world.remote.count(Op::Flush),
            2,
            "no flush for uploads that are not there"
        );
    }

    #[test]
    fn a_commit_of_files_the_caller_put_makes_no_put_and_no_first_flush() {
        let world = World::new();
        let before = 3;
        let outcome = world.commit_put_already(&metadata("a", true));
        assert_eq!(outcome, committed(1));
        // The caller's two puts and its flush come first.
        assert_eq!(
            calls_after(&world, before),
            [
                Call::new(Op::ListDir, ""),
                Call::new(Op::ListDir, ""),
                Call::new(Op::CreateExclusive, CLAIM),
                Call::new(Op::WriteTemp, OWN_TEMP),
                Call::new(Op::Flush, ""),
                Call::new(Op::ListDir, ""),
                Call::new(Op::RenameIntoPlace, "meta.1"),
                Call::new(Op::Flush, ""),
                Call::new(Op::ListDir, ""),
            ]
        );
    }

    #[test]
    fn a_file_that_vanished_after_the_caller_put_it_is_put_again() {
        let world = World::new();
        // The caller's puts and flush are three calls; the commit's own
        // listings follow.
        world.remote.before(Op::ListDir, 4, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted");
        });
        let outcome = world.commit_put_already(&metadata("a", true));
        assert_eq!(outcome, committed(1));
        assert_eq!(
            world.remote.count(Op::PutPayload),
            3,
            "two by the caller, one again"
        );
        assert!(world.names().contains(&PAYLOAD.to_string()));
    }

    #[test]
    fn the_steps_run_in_the_order_of_the_adr() {
        let world = World::new();
        world.committed(0, &metadata("a", true));
        assert_eq!(
            world.remote.calls(),
            [
                Call::new(Op::ListDir, ""),
                Call::new(Op::PutPayload, PAYLOAD),
                Call::new(Op::PutPayload, LISTING),
                Call::new(Op::Flush, ""),
                Call::new(Op::ListDir, ""),
                Call::new(Op::CreateExclusive, CLAIM),
                Call::new(Op::WriteTemp, OWN_TEMP),
                Call::new(Op::Flush, ""),
                Call::new(Op::ListDir, ""),
                Call::new(Op::RenameIntoPlace, "meta.1"),
                Call::new(Op::Flush, ""),
                Call::new(Op::ListDir, ""),
            ]
        );
    }

    #[test]
    fn a_failing_flush_before_the_rename_leaves_no_new_version() {
        let world = World::new();
        world.remote.fail(Op::Flush, 2, Fault::Io);
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the flush fails");
        assert!(matches!(error, RemoteError::Io { .. }), "{error:?}");
        assert!(!world.names().contains(&"meta.1".to_string()));
        assert_eq!(world.remote.count(Op::RenameIntoPlace), 0);
    }

    #[test]
    fn a_failing_flush_after_the_rename_is_an_error_with_the_version_committed() {
        let world = World::new();
        world.remote.fail(Op::Flush, 3, Fault::Io);
        world
            .commit(0, &metadata("a", true))
            .expect_err("the flush fails");
        assert!(world.names().contains(&"meta.1".to_string()));
    }

    #[test]
    fn a_failure_in_the_last_listing_does_not_hide_the_committed_version() {
        let world = World::new();
        world.remote.fail(Op::ListDir, 4, Fault::Away);
        world
            .commit(0, &metadata("a", true))
            .expect_err("the listing fails");
        assert!(world.names().contains(&"meta.1".to_string()));
    }

    #[test]
    fn a_failure_at_any_call_before_the_rename_leaves_no_new_version() {
        let clean = World::new();
        clean.committed(0, &metadata("a", true));
        let log = clean.remote.calls();
        let position = |op: Op| log.iter().position(|call| call.op == op).expect("a call");
        let (claimed, written, renamed) = (
            position(Op::CreateExclusive),
            position(Op::WriteTemp),
            position(Op::RenameIntoPlace),
        );
        for index in 0..renamed {
            let world = World::new();
            let nth = log[..=index]
                .iter()
                .filter(|call| call.op == log[index].op)
                .count();
            world.remote.fail(log[index].op, nth, Fault::Away);
            world
                .commit(0, &metadata("a", true))
                .expect_err("the call fails");
            let names = world.names();
            assert!(
                !names.contains(&"meta.1".to_string()),
                "call {index} ({:?}) left a version",
                log[index]
            );
            assert_eq!(
                names.contains(&CLAIM.to_string()),
                index > claimed && index != written,
                "the claim after a failure at call {index}"
            );
            assert_eq!(
                names.contains(&OWN_TEMP.to_string()),
                index > written,
                "the temp name after a failure at call {index}"
            );
        }
    }

    // =====================================================
    // What the metadata and the remote must look like
    // =====================================================

    #[test]
    fn a_remote_ahead_of_the_base_is_behind_and_nothing_is_written() {
        let world = World::new();
        world.put_version(1, "a");
        world.put_version(2, "a");
        let outcome = world.committed(1, &metadata("b", true));
        assert_eq!(outcome, refused(Refusal::Behind { highest: 2 }));
        assert_eq!(world.names(), ["meta.1", "meta.2"]);
        assert_eq!(world.remote.count(Op::PutPayload), 0);
    }

    #[test]
    fn a_remote_below_the_base_is_refused() {
        let world = World::new();
        world.put_version(1, "a");
        let outcome = world.committed(3, &metadata("b", true));
        assert_eq!(
            outcome,
            refused(Refusal::MissingBase {
                based_on: 3,
                highest: 1
            })
        );
        assert_eq!(world.names(), ["meta.1"]);
    }

    fn assert_unreadable_highest(outcome: Outcome, world: &World) {
        assert!(
            matches!(
                outcome,
                Outcome::Refused(Refusal::UnreadableHighest { number: 1, .. })
            ),
            "{outcome:?}"
        );
        assert_eq!(world.names(), ["meta.1"], "nothing is written on top");
    }

    #[test]
    fn an_unreadable_highest_version_is_refused() {
        let world = World::new();
        world.put("meta.1", b"not json");
        assert_unreadable_highest(world.committed(1, &metadata("b", true)), &world);
    }

    #[test]
    fn a_highest_version_of_format_2_is_refused() {
        let world = World::new();
        let newer = String::from_utf8(metadata("a", false))
            .expect("UTF-8")
            .replacen("\"format\":1", "\"format\":2", 1);
        world.put("meta.1", newer.as_bytes());
        assert_unreadable_highest(world.committed(1, &metadata("b", true)), &world);
    }

    #[test]
    fn a_highest_version_of_another_object_is_refused() {
        let world = World::new();
        let foreign = String::from_utf8(metadata("a", false))
            .expect("UTF-8")
            .replacen(&id().to_string(), &object(2).to_string(), 1);
        world.put("meta.1", foreign.as_bytes());
        assert_unreadable_highest(world.committed(1, &metadata("b", true)), &world);
    }

    fn assert_invalid_metadata(bytes: &[u8]) {
        let world = World::new();
        let outcome = world.committed(0, bytes);
        assert!(
            matches!(outcome, Outcome::Refused(Refusal::InvalidMetadata(_))),
            "{outcome:?}"
        );
        assert!(
            world.remote.calls().is_empty(),
            "nothing was asked of the remote"
        );
    }

    #[test]
    fn metadata_that_does_not_parse_is_refused_before_anything_is_uploaded() {
        assert_invalid_metadata(b"not json");
    }

    #[test]
    fn metadata_of_another_object_is_refused_before_anything_is_uploaded() {
        let foreign = String::from_utf8(metadata("a", true))
            .expect("UTF-8")
            .replacen(&id().to_string(), &object(2).to_string(), 1);
        assert_invalid_metadata(foreign.as_bytes());
    }

    #[test]
    fn metadata_of_format_2_is_refused_before_anything_is_uploaded() {
        let newer = String::from_utf8(metadata("a", true))
            .expect("UTF-8")
            .replacen("\"format\":1", "\"format\":2", 1);
        assert_invalid_metadata(newer.as_bytes());
    }

    // =====================================================
    // Claims
    // =====================================================

    #[test]
    fn an_open_claim_above_the_highest_is_a_conflict() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.age(CLAIM, 60);
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimOpen { number: 1 }));
        assert_eq!(world.remote.count(Op::CreateExclusive), 0);
        assert!(!world.names().contains(&"meta.1".to_string()));
    }

    #[test]
    fn an_abandoned_claim_is_skipped_and_the_next_number_is_used() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.age(CLAIM, 25 * 3600);
        assert_eq!(world.committed(0, &metadata("a", true)), committed(2));
        let names = world.names();
        assert!(names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(names.contains(&"meta.2".to_string()), "{names:?}");
        assert!(!names.contains(&"meta.1".to_string()), "{names:?}");
    }

    #[test]
    fn a_claim_exactly_a_grace_period_old_is_still_open() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.age(CLAIM, 24 * 3600);
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimOpen { number: 1 }));
    }

    #[test]
    fn the_clock_is_not_read_when_no_claim_is_in_the_way() {
        let world = World::new();
        world.committed(0, &metadata("a", true));
        assert_eq!(world.remote.count(Op::Clock), 0);
    }

    #[test]
    fn the_clock_is_read_once_for_the_claims_in_the_way() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.put("meta.2.claim", b"");
        world.age(CLAIM, 25 * 3600);
        world.age("meta.2.claim", 25 * 3600);
        assert_eq!(world.committed(0, &metadata("a", true)), committed(3));
        assert_eq!(world.remote.count(Op::Clock), 1);
    }

    #[test]
    fn a_claim_that_appears_between_listing_and_creating_is_taken() {
        let world = World::new();
        world.remote.before(Op::CreateExclusive, 1, |inner| {
            inner
                .create_exclusive(id(), CLAIM)
                .expect("the other machine claims");
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimTaken { number: 1 }));
        assert_eq!(world.remote.count(Op::WriteTemp), 0);
        assert!(!world.names().contains(&OWN_TEMP.to_string()));
    }

    #[test]
    fn a_claim_that_vanishes_before_the_rename_is_lost() {
        let world = World::new();
        world.remote.after(Op::CreateExclusive, 1, |inner| {
            inner.delete(id(), CLAIM).expect("the claim is removed");
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimLost { number: 1 }));
        let names = world.names();
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
        assert!(!names.contains(&"meta.1".to_string()));
    }

    #[test]
    fn a_lost_claim_takes_back_the_temp_and_flushes() {
        let world = World::new();
        world.remote.after(Op::CreateExclusive, 1, |inner| {
            inner.delete(id(), CLAIM).expect("the claim is removed");
        });
        world.committed(0, &metadata("a", true));
        let log = world.remote.calls();
        assert_eq!(
            log[log.len() - 2..],
            [Call::new(Op::Delete, OWN_TEMP), Call::new(Op::Flush, "")]
        );
    }

    /// The temp file of a write that failed after it began.
    fn half_written_temp(world: &World) {
        world.remote.before(Op::WriteTemp, 1, |inner| {
            inner
                .write_temp(id(), OWN_TEMP, b"half")
                .expect("the half write");
        });
    }

    #[test]
    fn a_failing_temp_write_takes_back_the_claim_and_the_temp() {
        let world = World::new();
        half_written_temp(&world);
        world.remote.fail(Op::WriteTemp, 1, Fault::Away);
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the write fails");
        assert_eq!(error.kind(), ErrorKind::RemoteAway, "{error}");
        let names = world.names();
        assert!(!names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
    }

    #[test]
    fn a_temp_write_that_fails_beside_a_failing_delete_of_the_temp_leaves_the_claim() {
        let world = World::new();
        half_written_temp(&world);
        world.remote.fail(Op::WriteTemp, 1, Fault::Away);
        world.remote.fail(Op::Delete, 1, Fault::Io);
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the write fails");
        assert_eq!(error.kind(), ErrorKind::RemoteAway, "the first error wins");
        let names = world.names();
        assert!(names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(names.contains(&OWN_TEMP.to_string()), "{names:?}");
    }

    #[test]
    fn a_temp_write_that_fails_beside_a_failing_delete_of_the_claim_never_leaves_the_temp_alone() {
        let world = World::new();
        half_written_temp(&world);
        world.remote.fail(Op::WriteTemp, 1, Fault::Away);
        world.remote.fail(Op::Delete, 2, Fault::Io);
        world
            .commit(0, &metadata("a", true))
            .expect_err("the write fails");
        let names = world.names();
        assert!(names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
    }

    #[test]
    fn an_own_crashed_claim_does_not_block_the_next_commit() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.age(CLAIM, 60);
        world.put(OWN_TEMP, b"half");
        assert_eq!(world.committed(0, &metadata("a", true)), committed(2));
    }

    #[test]
    fn a_claim_beside_our_temp_of_another_number_still_blocks() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.age(CLAIM, 60);
        world.put(".meta.2.laptop.part", b"half");
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimOpen { number: 1 }));
    }

    #[test]
    fn another_machines_claim_beside_its_temp_still_blocks() {
        let world = World::new();
        world.put(CLAIM, b"");
        world.age(CLAIM, 60);
        world.put(".meta.1.studio.part", b"half");
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimOpen { number: 1 }));
    }

    #[test]
    fn a_claim_at_the_largest_version_is_refused() {
        let world = World::new();
        world.put("meta.4294967295.claim", b"");
        world.age("meta.4294967295.claim", 25 * 3600);
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::VersionLimit { number: u32::MAX }));
        assert_eq!(world.remote.count(Op::CreateExclusive), 0);
    }

    // =====================================================
    // Conflicts after the claim
    // =====================================================

    #[test]
    fn a_version_that_appears_after_the_claim_is_a_conflict() {
        let world = World::new();
        world.remote.after(Op::WriteTemp, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", false));
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::NewerCommitted { highest: 2 }));
        assert_eq!(world.remote.count(Op::RenameIntoPlace), 0);
    }

    #[test]
    fn an_abandoned_writer_that_commits_below_our_claim_is_a_conflict() {
        let world = World::new();
        world.put_version(1, "a");
        world.put_version(2, "a");
        world.put("meta.3.claim", b"");
        world.age("meta.3.claim", 25 * 3600);
        world.remote.after(Op::WriteTemp, 1, |inner| {
            commit_as_studio(inner, 3, &metadata("b", false));
        });
        let outcome = world.committed(2, &metadata("c", true));
        assert_eq!(outcome, refused(Refusal::NewerCommitted { highest: 3 }));
        assert!(!world.names().contains(&"meta.4".to_string()));
    }

    #[test]
    fn a_target_that_appears_before_the_rename_is_taken() {
        let world = World::new();
        world.remote.before(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 1, &metadata("b", false));
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::TargetTaken { number: 1 }));
        let names = world.names();
        assert!(names.contains(&CLAIM.to_string()), "the claim stays");
        assert!(names.contains(&OWN_TEMP.to_string()), "the temp stays");
        assert_eq!(world.read("meta.1"), metadata("b", false));
    }

    #[test]
    fn a_higher_version_after_the_rename_overtakes() {
        let world = World::new();
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", false));
        });
        let bytes = metadata("a", true);
        let outcome = world.committed(0, &bytes);
        assert_eq!(
            outcome,
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
        assert_eq!(world.read("meta.1"), bytes, "our version stays");
    }

    #[test]
    fn a_claim_above_ours_after_the_rename_contests() {
        let world = World::new();
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            inner
                .create_exclusive(id(), "meta.2.claim")
                .expect("the other machine claims");
        });
        let bytes = metadata("a", true);
        let outcome = world.committed(0, &bytes);
        assert_eq!(
            outcome,
            Outcome::Contested {
                number: number(1),
                claim: 2
            }
        );
        assert_eq!(world.read("meta.1"), bytes, "our version stays");
    }

    #[test]
    fn a_higher_version_beside_a_higher_claim_overtakes() {
        let world = World::new();
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            inner
                .create_exclusive(id(), "meta.3.claim")
                .expect("the other machine claims");
            commit_as_studio(inner, 2, &metadata("b", false));
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(
            outcome,
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
    }

    #[test]
    fn a_claim_above_ours_before_the_rename_is_refused_and_released() {
        let world = World::new();
        world.remote.after(Op::WriteTemp, 1, |inner| {
            inner
                .create_exclusive(id(), "meta.2.claim")
                .expect("the other machine claims");
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::ClaimOpen { number: 2 }));
        assert_eq!(world.remote.count(Op::RenameIntoPlace), 0);
        let names = world.names();
        assert!(!names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
        assert!(names.contains(&"meta.2.claim".to_string()), "{names:?}");
    }

    #[test]
    fn a_version_that_appears_while_a_payload_is_put_again_is_a_conflict() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.remote.after(Op::PutPayload, 3, |inner| {
            commit_as_studio(inner, 2, &metadata("b", false));
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::NewerCommitted { highest: 2 }));
        assert_eq!(world.remote.count(Op::RenameIntoPlace), 0);
        let names = world.names();
        assert!(!names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
    }

    // =====================================================
    // Refusals release what was made
    // =====================================================

    #[test]
    fn a_refusal_by_a_newer_version_leaves_no_claim_and_no_temp() {
        let world = World::new();
        world.remote.after(Op::WriteTemp, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", false));
        });
        world.committed(0, &metadata("a", true));
        let names = world.names();
        assert!(!names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
    }

    #[test]
    fn a_version_under_our_own_number_keeps_the_claim_and_the_temp() {
        let world = World::new();
        world.remote.after(Op::WriteTemp, 1, |inner| {
            commit_as_studio(inner, 1, &metadata("b", false));
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, refused(Refusal::NewerCommitted { highest: 1 }));
        let names = world.names();
        assert!(names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(names.contains(&OWN_TEMP.to_string()), "{names:?}");
    }

    #[test]
    fn a_release_deletes_the_temp_then_flushes_then_deletes_the_claim() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.remote.after(Op::PutPayload, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.committed(0, &metadata("a", true));
        let log = world.remote.calls();
        let tail = &log[log.len() - 3..];
        assert_eq!(
            tail,
            [
                Call::new(Op::Delete, OWN_TEMP),
                Call::new(Op::Flush, ""),
                Call::new(Op::Delete, CLAIM),
            ]
        );
    }

    // =====================================================
    // Payloads
    // =====================================================

    #[test]
    fn a_payload_taken_before_the_check_is_put_again() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, committed(1));
        assert_eq!(world.read(PAYLOAD), b"payload bytes");
        assert_eq!(world.remote.count(Op::PutPayload), 3);
    }

    #[test]
    fn a_repair_put_is_followed_by_a_flush_before_the_rename() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.committed(0, &metadata("a", true));
        let log = world.remote.calls();
        let repair = log
            .iter()
            .rposition(|call| call.op == Op::PutPayload)
            .expect("a put");
        assert_eq!(log[repair + 1], Call::new(Op::Flush, ""));
        assert_eq!(log[repair + 2], Call::new(Op::ListDir, ""));
        let rename = log
            .iter()
            .position(|call| call.op == Op::RenameIntoPlace)
            .expect("a rename");
        assert!(repair < rename);
    }

    #[test]
    fn a_payload_taken_again_is_refused() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.remote.after(Op::PutPayload, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(
            outcome,
            refused(Refusal::PayloadMissing {
                name: PAYLOAD.into()
            })
        );
        let names = world.names();
        assert!(!names.contains(&CLAIM.to_string()), "{names:?}");
        assert!(!names.contains(&OWN_TEMP.to_string()), "{names:?}");
        assert!(!names.contains(&"meta.1".to_string()), "{names:?}");
    }

    #[test]
    fn a_payload_taken_after_the_rename_is_put_again() {
        let world = World::new();
        world.remote.before(Op::ListDir, 4, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(outcome, committed(1));
        assert_eq!(world.read(PAYLOAD), b"payload bytes");
        let log = world.remote.calls();
        let repair = log
            .iter()
            .rposition(|call| call.op == Op::PutPayload)
            .expect("a put");
        assert_eq!(
            log[repair + 1..],
            [Call::new(Op::Flush, ""), Call::new(Op::ListDir, "")],
            "the repair is flushed and looked at again before returning"
        );
    }

    #[test]
    fn a_payload_taken_after_the_rename_and_again_is_an_error_with_the_version_committed() {
        let world = World::new();
        world.remote.before(Op::ListDir, 4, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.remote.after(Op::PutPayload, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the payload is gone");
        assert!(matches!(error, RemoteError::Broken(_)), "{error:?}");
        assert!(error.to_string().contains(PAYLOAD), "{error}");
        assert!(error.to_string().contains("meta.1"), "{error}");
        assert!(world.names().contains(&"meta.1".to_string()));
    }

    #[test]
    fn a_failing_repair_put_after_the_rename_returns_its_own_error() {
        let world = World::new();
        world.remote.before(Op::ListDir, 4, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted")
        });
        world.remote.fail(Op::PutPayload, 3, Fault::Away);
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the repair put fails");
        assert_eq!(error.kind(), ErrorKind::RemoteAway, "{error}");
        assert!(world.names().contains(&"meta.1".to_string()));
    }

    #[test]
    fn a_payload_with_other_bytes_is_an_error() {
        let world = World::new();
        world.put(PAYLOAD, b"other bytes");
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the name is taken by other bytes");
        assert!(
            matches!(error, RemoteError::PayloadConflict { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_payload_replaced_by_other_bytes_is_an_error() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted");
            inner
                .write_temp(id(), PAYLOAD, b"other size of bytes")
                .expect("replaced");
        });
        let error = world
            .commit(0, &metadata("a", true))
            .expect_err("the put finds other bytes");
        assert!(
            matches!(error, RemoteError::PayloadConflict { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_named_payload_that_is_not_uploaded_and_is_gone_is_refused() {
        let world = World::new();
        let outcome = world
            .commit_with(0, &metadata("a", true), &[])
            .expect("the commit runs");
        assert_eq!(
            outcome,
            refused(Refusal::PayloadMissing {
                name: PAYLOAD.into()
            })
        );
        assert_eq!(world.remote.count(Op::PutPayload), 0);
        assert_eq!(
            world.remote.count(Op::Flush),
            2,
            "the temp write and the release flush, none for a repair"
        );
        assert!(!world.names().contains(&CLAIM.to_string()));
    }

    #[test]
    fn a_named_payload_uploaded_by_an_earlier_run_counts() {
        let world = World::new();
        world.put(PAYLOAD, b"payload bytes");
        world.put(LISTING, b"listing");
        let outcome = world
            .commit_with(0, &metadata("a", true), &[])
            .expect("the commit runs");
        assert_eq!(outcome, committed(1));
    }

    #[test]
    fn a_named_listing_that_is_gone_is_put_again_when_uploaded() {
        let world = World::new();
        world.remote.before(Op::ListDir, 3, |inner| {
            inner.delete(id(), LISTING).expect("deleted")
        });
        assert_eq!(world.committed(0, &metadata("a", true)), committed(1));
        assert_eq!(world.read(LISTING), b"listing");
    }

    // =====================================================
    // Retrying a commit that failed past the rename
    // =====================================================

    #[test]
    fn a_retry_after_a_failure_past_the_rename_finds_its_own_version() {
        let world = World::new();
        let bytes = metadata("a", true);
        world.remote.fail(Op::Flush, 3, Fault::Io);
        world.commit(0, &bytes).expect_err("the flush fails");
        let before = world.remote.calls().len();
        assert_eq!(world.committed(0, &bytes), committed(1));
        let retry = calls_after(&world, before);
        for op in [
            Op::PutPayload,
            Op::CreateExclusive,
            Op::WriteTemp,
            Op::RenameIntoPlace,
        ] {
            assert!(
                retry.iter().all(|call| call.op != op),
                "the retry wrote with {op:?}: {retry:?}"
            );
        }
    }

    #[test]
    fn a_retry_finds_its_version_overtaken() {
        let world = World::new();
        let bytes = metadata("a", true);
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", false));
        });
        world.remote.fail(Op::ListDir, 4, Fault::Io);
        world.commit(0, &bytes).expect_err("the last listing fails");
        assert_eq!(
            world.committed(0, &bytes),
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
    }

    #[test]
    fn a_retry_finds_a_higher_version_that_names_our_files_landed() {
        let world = World::new();
        let bytes = metadata("a", true);
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", true));
        });
        world.remote.fail(Op::ListDir, 4, Fault::Io);
        world.commit(0, &bytes).expect_err("the last listing fails");
        assert_eq!(
            world.committed(0, &bytes),
            Outcome::Landed {
                number: number(1),
                highest: 2
            }
        );
    }

    #[test]
    fn a_retry_without_uploads_finds_a_higher_version_that_names_our_files_landed() {
        let world = World::new();
        let bytes = metadata("a", true);
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", true));
        });
        world.remote.fail(Op::ListDir, 4, Fault::Io);
        world.commit(0, &bytes).expect_err("the last listing fails");
        let outcome = world.commit_with(0, &bytes, &[]).expect("the retry runs");
        assert_eq!(
            outcome,
            Outcome::Landed {
                number: number(1),
                highest: 2
            }
        );
    }

    #[test]
    fn a_higher_version_that_names_our_files_during_the_attempt_overtakes() {
        let world = World::new();
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", true));
        });
        let outcome = world.committed(0, &metadata("a", true));
        assert_eq!(
            outcome,
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
    }

    #[test]
    fn a_retry_that_finds_a_higher_version_without_our_listing_is_overtaken() {
        let world = World::new();
        let bytes = metadata("a", true);
        let partial = String::from_utf8(metadata("b", true))
            .expect("UTF-8")
            .replacen(&format!("\"listing\":\"{LISTING}\","), "", 1);
        assert_ne!(
            partial.as_bytes(),
            metadata("b", true),
            "the listing is cut"
        );
        world.remote.after(Op::RenameIntoPlace, 1, move |inner| {
            commit_as_studio(inner, 2, partial.as_bytes());
        });
        world.remote.fail(Op::ListDir, 4, Fault::Io);
        world.commit(0, &bytes).expect_err("the last listing fails");
        assert_eq!(
            world.committed(0, &bytes),
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
    }

    /// A retry after version 2 landed, with `change` made to `meta.2` just
    /// before the retry reads it a second time.
    fn retry_with_changed_higher_version(
        change: impl Fn(&Path) + Send + Sync + 'static,
    ) -> Outcome {
        let world = World::new();
        let bytes = metadata("a", true);
        world.remote.after(Op::RenameIntoPlace, 1, |inner| {
            commit_as_studio(inner, 2, &metadata("b", true));
        });
        world.remote.fail(Op::ListDir, 4, Fault::Io);
        world.commit(0, &bytes).expect_err("the last listing fails");
        let reads = world.remote.count(Op::ReadFile);
        let dir = world.dir();
        world
            .remote
            .before(Op::ReadFile, reads + 3, move |_| change(&dir));
        world.committed(0, &bytes)
    }

    #[test]
    fn a_retry_whose_higher_version_is_removed_before_it_is_read_is_overtaken() {
        let outcome = retry_with_changed_higher_version(|dir| {
            std::fs::remove_file(dir.join("meta.2")).expect("removed by a cleanup");
        });
        assert_eq!(
            outcome,
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
    }

    #[test]
    fn a_retry_whose_higher_version_does_not_parse_is_overtaken() {
        let outcome = retry_with_changed_higher_version(|dir| {
            std::fs::write(dir.join("meta.2"), b"not json").expect("overwritten");
        });
        assert_eq!(
            outcome,
            Outcome::Overtaken {
                number: number(1),
                higher: 2
            }
        );
    }

    #[test]
    fn a_retry_repairs_a_payload_that_went_missing() {
        let world = World::new();
        let bytes = metadata("a", true);
        world.remote.fail(Op::Flush, 3, Fault::Io);
        world.commit(0, &bytes).expect_err("the flush fails");
        world.remote.before(Op::ListDir, 5, |inner| {
            inner.delete(id(), PAYLOAD).expect("deleted");
        });
        assert_eq!(world.committed(0, &bytes), committed(1));
        assert_eq!(world.read(PAYLOAD), b"payload bytes");
    }

    #[test]
    fn a_version_below_the_base_with_the_same_bytes_is_not_ours() {
        let world = World::new();
        let bytes = metadata("a", true);
        world.put("meta.1", &bytes);
        world.put_version(2, "b");
        let outcome = world.committed(1, &bytes);
        assert_eq!(outcome, refused(Refusal::Behind { highest: 2 }));
    }

    #[test]
    fn a_version_listed_and_gone_by_the_time_it_is_read_is_not_ours() {
        let world = World::new();
        world.put_version(1, "a");
        world.put_version(2, "a");
        world.remote.before(Op::ReadFile, 2, |inner| {
            inner.delete(id(), "meta.1").expect("removed by a cleanup");
        });
        let outcome = world.committed(0, &metadata("b", true));
        assert_eq!(outcome, refused(Refusal::Behind { highest: 2 }));
    }

    #[test]
    fn a_version_that_cannot_be_read_while_listed_is_an_error() {
        let world = World::new();
        world.put_version(1, "a");
        world.put_version(2, "a");
        world.remote.fail(Op::ReadFile, 2, Fault::Io);
        let error = world
            .commit(0, &metadata("b", true))
            .expect_err("the read fails");
        assert!(matches!(error, RemoteError::Io { .. }), "{error:?}");
    }
}
