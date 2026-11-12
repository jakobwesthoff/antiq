// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The extraction that `unearth` and `fork` share.

use crate::archive::TargetRules;
use crate::archive::extract::ExtractionFlush;
use crate::archive::extract::unpack_counted;
use crate::archive::names::{MemberName, check_members};
use crate::archive::verify::{compare_with_listing_counted, content_bytes};
use crate::archive::{PackError, WalkRules};
use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::error::{ErrorKind, HasKind};
use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, RealDurable};
use crate::filesystem::atomic::{create_dir_chain, flush_dir_of, named};
use crate::filesystem::moves::{self, Moved};
use crate::filesystem::remove::remove_tree;
use crate::filesystem::space;
use crate::filesystem::tree::TreeRoot;
use crate::listing::{self, Listing, ListingEntry, ListingError, ListingType};
use crate::markers::{self, Restored};
use crate::markers::{MarkerError, RESTORED_FILE, RealSite, Site};
use crate::pin::PIN_FILE;
use crate::progress::{Operation, Tracker};
use crate::reference::{self, ReferenceError, SearchWord};
use crate::relpath::{Name, RelPath};
use crate::select::Rules;
use crate::shown::Shown;
use crate::tree::{EntryKind, TreeEntry};
use crate::versions::VersionsError;
use crate::versions::damage::{self, Check, Condition, Damage, Places};
use crate::versions::naming::{self, Candidate, VersionLookupError};
use crate::versions::stamp::Stamp;
use crate::versions::store::{self, StoreVersion};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use rustix::io::Errno;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

/// Why a restore failed.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum RestoreError {
    #[error("{0}")]
    Usage(String),
    #[error("cannot find the piece: {0}")]
    Reference(
        #[from]
        #[diagnostic_source]
        ReferenceError,
    ),
    #[error("cannot read the store: {0}")]
    Versions(
        #[from]
        #[diagnostic_source]
        VersionsError,
    ),
    #[error("cannot read the listing: {0}")]
    Listing(
        #[from]
        #[diagnostic_source]
        ListingError,
    ),
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("{0}")]
    Version(
        #[from]
        #[diagnostic_source]
        VersionLookupError,
    ),
    #[error("version {stamp} of {piece} has no archive; `forget` removes what is left of it")]
    Incomplete { piece: String, stamp: String },
    #[error(
        "version {stamp} of {piece} is damaged: {}. `fork` extracts it with a warning and keeps it, `rebuild` rebuilds its listing and checksum file from the archive, or the missing file can be copied back from the target",
        reasons.join("; ")
    )]
    Damaged {
        piece: String,
        stamp: String,
        reasons: Vec<String>,
    },
    #[error(
        "version {stamp} of {piece} has several files in one slot ({}); remove the extra ones by hand, or `forget` the version",
        files.join(", ")
    )]
    Doubled {
        piece: String,
        stamp: String,
        files: Vec<String>,
    },
    #[diagnostic(
        code(antiq::restore::format_too_new),
        help("Use a version of antiq that reads the newer format.")
    )]
    #[error(
        "version {} of {} has a listing of a newer format than this antiq reads",
        CodeSpan(.stamp),
        CodeSpan(.piece)
    )]
    FormatTooNew { piece: String, stamp: String },
    #[error("version {stamp} of {piece} holds `{name}` at its top, and it is not a file")]
    MarkerNotAFile {
        piece: String,
        stamp: String,
        name: String,
    },
    #[error("cannot restore to `{path}`: {problem}")]
    Target {
        path: RelPath,
        problem: TargetProblem,
    },
    #[error("cannot {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error("{0}")]
    Filesystem(
        #[from]
        #[diagnostic_source]
        FilesystemError,
    ),
    #[error("{0}")]
    Marker(
        #[from]
        #[diagnostic_source]
        MarkerError,
    ),
    #[error("cannot extract the archive: {0}")]
    Pack(
        #[from]
        #[diagnostic_source]
        PackError,
    ),
    #[error("the extracted tree differs from the listing: {}", differences.join("; "))]
    Differs { differences: Vec<String> },
}

impl HasKind for RestoreError {
    fn kind(&self) -> ErrorKind {
        match self {
            RestoreError::Usage(_) => ErrorKind::Usage,
            RestoreError::Reference(error) => error.kind(),
            RestoreError::Listing(error) => error.kind(),
            RestoreError::FormatTooNew { .. } => ErrorKind::FormatTooNew,
            _ => ErrorKind::Other,
        }
    }
}

// =========================================================
// Choosing the piece and the version
// =========================================================

/// The version to bring back, with everything the checks on it read.
#[derive(Debug)]
pub(crate) struct Chosen {
    pub piece: RelPath,
    pub stamp: Stamp,
    pub version: StoreVersion,
    pub listing: Listing,
}

/// A version picked to be extracted, sound or not. `unearth` takes only
/// sound ones ([`Chosen`]); `fork` also takes a damaged one.
#[derive(Debug)]
pub(crate) struct Picked {
    pub piece: RelPath,
    pub stamp: Stamp,
    pub version: StoreVersion,
    /// The listing, when the version has one that reads to its end.
    pub listing: Option<Listing>,
    /// What is wrong with the version; empty for a sound one.
    pub damage: Vec<String>,
}

/// Which versions a choice may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Sound versions only.
    Sound,
    /// Also a damaged version, unless it has a doubled slot.
    Damaged,
}

impl Policy {
    /// `unearth` takes sound versions and removes the one it brings back,
    /// so it acts only on the path of a piece. `fork` keeps the version.
    fn search_word(self) -> SearchWord {
        match self {
            Policy::Sound => SearchWord::OnlyOffers,
            Policy::Damaged => SearchWord::Acts,
        }
    }
}

/// Finds the piece `given` names in the store and the version of it to
/// bring back, and refuses a version that cannot be: one without an
/// archive, a damaged one, one of a newer format, and one whose top holds
/// a marker's name as anything but a file.
pub(crate) fn choose_version(
    collection: &Collection,
    places: &Places,
    cwd: &Path,
    given: &OsStr,
    version: Option<&str>,
    zone: &TimeZone,
) -> Result<Chosen, RestoreError> {
    let picked = pick(collection, places, cwd, given, version, zone, Policy::Sound)?;
    Ok(Chosen {
        piece: picked.piece,
        stamp: picked.stamp,
        version: picked.version,
        listing: picked
            .listing
            .expect("a sound version has a listing that reads"),
    })
}

/// Like [`choose_version`], but a damaged version is returned with its
/// damage. Its listing is returned when it reads to its end. Refused are
/// the versions that cannot be extracted at all: one without an archive,
/// one of a newer format, one with two files in a slot, and one whose
/// top holds a marker's name as anything but a file where the listing
/// shows it.
pub(crate) fn choose_version_to_fork(
    collection: &Collection,
    places: &Places,
    cwd: &Path,
    given: &OsStr,
    version: Option<&str>,
    zone: &TimeZone,
) -> Result<Picked, RestoreError> {
    pick(
        collection,
        places,
        cwd,
        given,
        version,
        zone,
        Policy::Damaged,
    )
}

fn pick(
    collection: &Collection,
    places: &Places,
    cwd: &Path,
    given: &OsStr,
    version: Option<&str>,
    zone: &TimeZone,
    policy: Policy,
) -> Result<Picked, RestoreError> {
    if given.is_empty() {
        return Err(RestoreError::Usage("the piece is empty".to_string()));
    }
    if version == Some("") {
        return Err(RestoreError::Usage("the version is empty".to_string()));
    }

    let scan = store::scan(places.pieces_dir)?;
    let piece =
        reference::resolve_stored_piece(collection, cwd, given, &scan, policy.search_word())?;
    let mut versions = scan
        .pieces
        .iter()
        .find(|stored| stored.path == piece)
        .map(|stored| stored.versions.clone())
        .expect("the resolved piece is one of the scan");
    versions.sort_by_key(|candidate| std::cmp::Reverse(candidate.stamp));

    let version = match version {
        None => versions
            .iter()
            .find(|candidate| !candidate.is_incomplete())
            .expect("a piece of the store has a version with an archive"),
        Some(input) => {
            let candidates: Vec<Candidate> = versions
                .iter()
                .map(|candidate| Candidate {
                    stamp: candidate.stamp,
                    incomplete: candidate.is_incomplete(),
                })
                .collect();
            let stamp = naming::resolve_version(&piece.to_string(), input, &candidates, zone)?;
            versions
                .iter()
                .find(|candidate| candidate.stamp == stamp)
                .expect("a found stamp is one of the candidates")
        }
    };
    let (piece_text, stamp_text) = (piece.to_string(), version.stamp.to_string());
    if version.is_incomplete() {
        return Err(RestoreError::Incomplete {
            piece: piece_text,
            stamp: stamp_text,
        });
    }

    let inspection = damage::inspect(places, &piece, version, Check::Full)?;
    let damage = match inspection.condition {
        Condition::Sound => Vec::new(),
        Condition::Incomplete | Condition::BeingStored => {
            return Err(RestoreError::Incomplete {
                piece: piece_text,
                stamp: stamp_text,
            });
        }
        Condition::NewerFormat => {
            return Err(RestoreError::FormatTooNew {
                piece: piece_text,
                stamp: stamp_text,
            });
        }
        Condition::Damaged(damages) => {
            // Two files in one slot cannot be extracted by either command.
            let doubled: Vec<String> = damages
                .iter()
                .filter_map(|damage| match damage {
                    Damage::Doubled { files, .. } => Some(files),
                    _ => None,
                })
                .flatten()
                .map(|file| Shown(file).to_string())
                .collect();
            if !doubled.is_empty() {
                return Err(RestoreError::Doubled {
                    piece: piece_text,
                    stamp: stamp_text,
                    files: doubled,
                });
            }
            if policy == Policy::Sound {
                // A listing of a newer format is refused as such, whatever
                // else is damaged beside it.
                if let Some(listing_name) = version.listings.first() {
                    read_listing(
                        places,
                        &piece,
                        listing_name,
                        false,
                        &piece_text,
                        &stamp_text,
                    )?;
                }
                return Err(RestoreError::Damaged {
                    piece: piece_text,
                    stamp: stamp_text,
                    reasons: damages.iter().map(ToString::to_string).collect(),
                });
            }
            damages.iter().map(ToString::to_string).collect()
        }
    };

    // A version with a damaged listing has none to read. A sound one has
    // it, or `inspect` would have said so.
    let listing = match version.listings.first() {
        Some(listing_name) => read_listing(
            places,
            &piece,
            listing_name,
            must_read_listing(policy, &damage),
            &piece_text,
            &stamp_text,
        )?,
        None => None,
    };
    if let Some(listing) = &listing
        && let Some(name) = marker_not_a_file(&listing.entries)
    {
        return Err(RestoreError::MarkerNotAFile {
            piece: piece_text,
            stamp: stamp_text,
            name,
        });
    }

    Ok(Picked {
        piece,
        stamp: version.stamp,
        version: version.clone(),
        listing,
        damage,
    })
}

/// Whether a listing that does not read ends the pick. It does for a
/// version `inspect` found no damage in, as that version's listing read
/// before.
fn must_read_listing(policy: Policy, damage: &[String]) -> bool {
    policy == Policy::Sound || damage.is_empty()
}

/// Reads the listing of a version. With `must_read` a listing that does not
/// read is an error; without it the version goes on without a listing.
fn read_listing(
    places: &Places,
    piece: &RelPath,
    listing_name: &OsStr,
    must_read: bool,
    piece_text: &str,
    stamp_text: &str,
) -> Result<Option<Listing>, RestoreError> {
    let store_root = TreeRoot::open(places.pieces_dir).map_err(|source| RestoreError::Read {
        path: Shown(&places.pieces_dir).to_string(),
        source,
    })?;
    let listing_path = piece.join(
        &Name::from_bytes(listing_name.as_encoded_bytes().to_vec())
            .expect("a scanned file name is a valid name"),
    );
    let file = store_root
        .open_read(&listing_path)
        .map_err(|source| RestoreError::Read {
            path: listing_path.to_string(),
            source,
        })?;
    match listing::read_full(file) {
        Ok(listing) => Ok(Some(listing)),
        // A newer listing next to other damage is classed as damaged; it is
        // still one this antiq cannot read.
        Err(ListingError::FormatTooNew { .. }) => Err(RestoreError::FormatTooNew {
            piece: piece_text.to_string(),
            stamp: stamp_text.to_string(),
        }),
        Err(error) if must_read => Err(error.into()),
        Err(_) => Ok(None),
    }
}

/// The first entry at the top of the piece that has a marker's name and is
/// not what that marker is: `.antiq-restored` must be a file, and
/// `.antiq-pin` a file or a symlink. A hard link is extracted as a file.
pub(crate) fn marker_not_a_file(entries: &[ListingEntry]) -> Option<String> {
    entries.iter().find_map(|entry| {
        let mut components = entry.path.components();
        let (Some(name), None) = (components.next(), components.next()) else {
            return None;
        };
        let is_file = matches!(entry.kind, ListingType::File | ListingType::Hardlink);
        let fits = if name == RESTORED_FILE.as_bytes() {
            is_file
        } else if name == PIN_FILE.as_bytes() {
            is_file || entry.kind == ListingType::Symlink
        } else {
            return None;
        };
        (!fits).then(|| String::from_utf8_lossy(name).into_owned())
    })
}

// =========================================================
// Checking the destination
// =========================================================

/// Why a destination is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetProblem {
    Root,
    InsideAntiq,
    NotSelected,
    Excluded { at: RelPath },
    InsidePiece { piece: RelPath },
    NotAFolder { at: RelPath },
    SpelledDifferently { at: RelPath },
    Taken,
    Unholdable { reason: String },
    MountPoint,
}

impl std::fmt::Display for TargetProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetProblem::Root => f.write_str("it is the collection root"),
            TargetProblem::InsideAntiq => f.write_str("it is inside `.antiq/`"),
            TargetProblem::NotSelected => f.write_str("the patterns do not select it"),
            TargetProblem::Excluded { at } => write!(f, "`{at}` is excluded"),
            TargetProblem::InsidePiece { piece } => write!(f, "`{piece}` is a piece"),
            TargetProblem::NotAFolder { at } => {
                write!(f, "`{at}` is a file or a symlink, not a folder")
            }
            TargetProblem::SpelledDifferently { at } => {
                write!(f, "`{at}` is spelled differently on disk")
            }
            TargetProblem::Taken => f.write_str("it is taken"),
            TargetProblem::Unholdable { reason } => f.write_str(reason),
            TargetProblem::MountPoint => {
                f.write_str("it is on another volume than `.antiq/tmp/`: mount point")
            }
        }
    }
}

/// The proper ancestors of `path`, shallowest first.
fn ancestors(path: &RelPath) -> Vec<RelPath> {
    let names: Vec<&[u8]> = path.components().collect();
    (1..names.len())
        .map(|taken| {
            RelPath::from_bytes(names[..taken].join(&b'/'))
                .expect("a prefix of a valid path is a valid path")
        })
        .collect()
}

/// Checks that `target` is a place a folder can be put: one the patterns
/// select that is not excluded, not in `.antiq/`, not inside another piece
/// and free. Returns the folders above it that do not exist yet,
/// shallowest first.
///
/// The check looks at the file system, so it runs once before the
/// extraction and again right before the rename, when the world may have
/// changed.
pub(crate) fn check_destination(
    root: &TreeRoot,
    rules: &Rules,
    target: &RelPath,
) -> Result<Vec<RelPath>, RestoreError> {
    let refuse = |problem| RestoreError::Target {
        path: target.clone(),
        problem,
    };
    if target.is_root() {
        return Err(refuse(TargetProblem::Root));
    }
    if target
        .components()
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(b".antiq"))
    {
        return Err(refuse(TargetProblem::InsideAntiq));
    }
    if !rules.matches(target) {
        return Err(refuse(TargetProblem::NotSelected));
    }

    let mut missing = Vec::new();
    for ancestor in ancestors(target) {
        if rules.matches(&ancestor) {
            return Err(refuse(TargetProblem::InsidePiece { piece: ancestor }));
        }
        match look_at(root, &ancestor)? {
            Some(found) => {
                if found.kind != EntryKind::Directory {
                    return Err(refuse(TargetProblem::NotAFolder { at: ancestor }));
                }
                if rules.excludes(&ancestor, found.device, found.inode) {
                    return Err(refuse(TargetProblem::Excluded { at: ancestor }));
                }
                // A volume that ignores case or normalisation reaches the
                // folder under the spelling asked for. The piece would
                // then be stored under a path the selection never reports.
                // A folder that vanished since the lstat is refused alike.
                if store::lookup_exact(root, &ancestor)?.as_ref() != Some(&ancestor) {
                    return Err(refuse(TargetProblem::SpelledDifferently { at: ancestor }));
                }
            }
            None => {
                if rules.excludes(&ancestor, 0, 0) {
                    return Err(refuse(TargetProblem::Excluded { at: ancestor }));
                }
                missing.push(ancestor);
            }
        }
    }
    if rules.excludes(target, 0, 0) {
        return Err(refuse(TargetProblem::Excluded { at: target.clone() }));
    }
    match look_at(root, target)? {
        Some(_) => Err(refuse(TargetProblem::Taken)),
        None => Ok(missing),
    }
}

/// `lstat` of `path`; `None` when nothing is there. Nothing is followed.
fn look_at(root: &TreeRoot, path: &RelPath) -> Result<Option<TreeEntry>, RestoreError> {
    match root.lstat(path) {
        Ok(found) => Ok(Some(found)),
        // A name past the limit of the volume cannot exist; the names are
        // held to the rules of the volume by `check_destination_names`.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(Errno::NAMETOOLONG.raw_os_error()) =>
        {
            Ok(None)
        }
        Err(source) => Err(RestoreError::Io {
            action: "look at",
            path: path.to_string(),
            source,
        }),
    }
}

/// Holds the names of the destination to the rules of the volume: every
/// name on the way must be one the volume can hold, and the ones made here
/// must be stored as written.
pub(crate) fn check_destination_names(
    target: &RelPath,
    missing: &[RelPath],
    volume: &TargetRules,
) -> Result<(), RestoreError> {
    let refuse = |reason| RestoreError::Target {
        path: target.clone(),
        problem: TargetProblem::Unholdable { reason },
    };
    let names: Vec<RelPath> = ancestors(target)
        .into_iter()
        .chain(std::iter::once(target.clone()))
        .collect();
    let members: Vec<MemberName> = names.iter().cloned().map(MemberName::plain).collect();
    if let Some(problem) = check_members(&members, volume).first() {
        return Err(refuse(problem.to_string()));
    }
    if volume.decomposes {
        for made in names
            .iter()
            .filter(|name| missing.contains(name) || *name == target)
        {
            let name = made.components().last().expect("a name has a last part");
            let changes =
                std::str::from_utf8(name).is_ok_and(|text| text.nfd().collect::<String>() != text);
            if changes {
                return Err(refuse(format!(
                    "the volume would store `{made}` decomposed"
                )));
            }
        }
    }
    Ok(())
}

// =========================================================
// Flushing the extracted tree
// =========================================================

/// The operating system families that flush differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum System {
    MacOs,
    Linux,
    Other,
}

impl System {
    fn current() -> System {
        if cfg!(target_os = "macos") {
            System::MacOs
        } else if cfg!(target_os = "linux") {
            System::Linux
        } else {
            System::Other
        }
    }
}

/// How the extracted tree is made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    /// Every file and directory is flushed on its own.
    PerItem,
    /// A cheap flush per item where the platform needs one, and one
    /// barrier over the whole volume at the end.
    Barrier,
}

/// The kernel `(major, minor)` of a `uname` release such as
/// `5.15.0-91-generic`.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_kernel(release: &str) -> Option<(u32, u32)> {
    let mut numbers = release.split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Picks the method from the platform and from what the one try of the
/// barrier answered. An answer that is no refusal is an error.
///
/// macOS refuses the full flush with `ENOTSUP` on filesystems that cannot
/// do it. On Linux `syncfs` reports the write errors of the whole
/// filesystem from kernel 5.8 on only; before that it can return success
/// for data that failed to reach the disk, so older kernels flush per
/// item. `EINVAL` and `ENOSYS` mean the call is unknown.
pub(crate) fn choose_method(
    os: System,
    kernel: Option<(u32, u32)>,
    probe: &io::Result<()>,
) -> io::Result<Method> {
    let refusals: &[Errno] = match os {
        System::MacOs => &[Errno::NOTSUP],
        System::Linux => &[Errno::INVAL, Errno::NOSYS],
        System::Other => return Ok(Method::PerItem),
    };
    let accepted = match probe {
        Ok(()) => true,
        Err(error)
            if refusals
                .iter()
                .any(|refusal| error.raw_os_error() == Some(refusal.raw_os_error())) =>
        {
            false
        }
        Err(error) => {
            return Err(match error.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(error.kind(), error.to_string()),
            });
        }
    };
    let kernel_reports_errors =
        os != System::Linux || kernel.is_some_and(|kernel| kernel >= (5, 8));
    Ok(if accepted && kernel_reports_errors {
        Method::Barrier
    } else {
        Method::PerItem
    })
}

type Barrier = Box<dyn Fn(&File) -> io::Result<()> + Send + Sync>;

/// What the platform provides; tests put their own in its place.
pub(crate) struct Platform {
    pub os: System,
    pub kernel: Option<(u32, u32)>,
    /// Flushes everything the volume of the file holds.
    pub barrier: Barrier,
    /// The flush of one item in [`Method::PerItem`].
    pub durable: Box<dyn Durable>,
}

impl Platform {
    pub(crate) fn real() -> Platform {
        Platform {
            os: System::current(),
            kernel: kernel_version(),
            barrier: Box::new(real_barrier),
            durable: Box::new(RealDurable),
        }
    }
}

#[cfg(target_os = "linux")]
fn kernel_version() -> Option<(u32, u32)> {
    parse_kernel(rustix::system::uname().release().to_str().ok()?)
}

#[cfg(not(target_os = "linux"))]
fn kernel_version() -> Option<(u32, u32)> {
    None
}

/// `syncfs` on Linux. `sync_all` is `F_FULLFSYNC` on macOS, which asks
/// the drive to empty its cache for everything written before, not only
/// for this file.
#[cfg(target_os = "linux")]
fn real_barrier(file: &File) -> io::Result<()> {
    Ok(rustix::fs::syncfs(file)?)
}

#[cfg(not(target_os = "linux"))]
fn real_barrier(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Makes the files and directories of an extraction durable, per item or
/// with one barrier at the end.
pub(crate) struct TreeFlush {
    method: Method,
    os: System,
    probe: File,
    probe_path: PathBuf,
    barrier: Barrier,
    durable: Box<dyn Durable>,
}

impl TreeFlush {
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn begin(extract_dir: &Path) -> io::Result<TreeFlush> {
        TreeFlush::begin_with(extract_dir, Platform::real())
    }

    pub(crate) fn begin_with(extract_dir: &Path, platform: Platform) -> io::Result<TreeFlush> {
        let probe_path = extract_dir.join(".flush-probe");
        let probe = File::create(&probe_path)?;
        // Opened before the extraction starts, so that on Linux the
        // descriptor's error state is older than every write it must report.
        let tried = (platform.barrier)(&probe);
        let method = choose_method(platform.os, platform.kernel, &tried).inspect_err(|_| {
            // No `TreeFlush` exists to remove the probe on drop.
            let _ = fs::remove_file(&probe_path);
        })?;
        Ok(TreeFlush {
            method,
            os: platform.os,
            probe,
            probe_path,
            barrier: platform.barrier,
            durable: platform.durable,
        })
    }

    #[cfg(test)]
    pub(crate) fn method(&self) -> Method {
        self.method
    }

    /// Runs the barrier of [`Method::Barrier`]; [`Method::PerItem`] has
    /// nothing left to do. The probe file is removed either way.
    pub(crate) fn finish(self) -> io::Result<()> {
        match self.method {
            Method::Barrier => (self.barrier)(&self.probe),
            Method::PerItem => Ok(()),
        }
    }
}

impl Drop for TreeFlush {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.probe_path);
    }
}

impl TreeFlush {
    /// The flush per item that the barrier relies on: macOS needs the
    /// data handed to the drive with `fsync` before the full flush can
    /// cover it, Linux needs nothing as `syncfs` writes it all.
    fn cheap(&self, file: &File) -> io::Result<()> {
        match self.os {
            System::MacOs => Ok(rustix::fs::fsync(file)?),
            System::Linux | System::Other => Ok(()),
        }
    }
}

fn into_io(error: FilesystemError) -> io::Error {
    match error {
        FilesystemError::Io { source, .. } => source,
        other => io::Error::other(other),
    }
}

impl ExtractionFlush for TreeFlush {
    fn file(&self, file: &File, path: &RelPath) -> io::Result<()> {
        match self.method {
            Method::PerItem => self
                .durable
                .file(file, Path::new(&path.to_string()))
                .map_err(into_io),
            Method::Barrier => self.cheap(file),
        }
    }

    fn directory(&self, directory: &File, path: &RelPath) -> io::Result<()> {
        match self.method {
            Method::PerItem => self
                .durable
                .dir(directory, Path::new(&path.to_string()))
                .map_err(into_io),
            Method::Barrier => self.cheap(directory),
        }
    }
}

// =========================================================
// Bringing a version back
// =========================================================

/// The points of `bring_back` at which a test looks at the disk or makes
/// the run stop, as a crash would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    SpaceChecked,
    StagingMade,
    Extracted,
    Verified,
    MarkerWritten,
    Flushed,
    /// The folders above the destination exist, and nothing is renamed yet.
    ParentsMade,
    Renamed,
    ParentFlushed,
    /// The version is deleted from the store (reached by `unearth`).
    VersionDeleted,
    /// The marker holds the time alone (reached by `unearth`).
    MarkerRewritten,
}

/// The folder below which `bring_back` stages an extraction.
const EXTRACT: &str = ".antiq/tmp/extract";

/// Everything `bring_back` takes from the system. The real one is
/// [`RealHooks`]; tests override single parts.
pub(crate) trait Hooks {
    /// Called after each step. An error ends the run there.
    fn reached(&self, _step: Step) -> Result<(), RestoreError> {
        Ok(())
    }
    fn available_space(&self, path: &Path) -> Result<u64, FilesystemError> {
        space::available_space(path)
    }
    fn platform(&self) -> Platform {
        Platform::real()
    }
    fn durable(&self) -> &dyn Durable {
        &RealDurable
    }
    fn site(&self) -> &dyn Site {
        &RealSite
    }
    fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
        Ok(root.lstat(path)?.device)
    }
    fn move_dir(
        &self,
        root: &TreeRoot,
        from: &RelPath,
        to: &RelPath,
    ) -> Result<Moved, FilesystemError> {
        moves::move_dir(root, from, root, to)
    }
    /// What the volume of `staged` can hold.
    fn target_rules(&self, staged: &TreeRoot) -> io::Result<TargetRules> {
        TargetRules::probe(staged)
    }
}

pub(crate) struct RealHooks;

impl Hooks for RealHooks {}

/// Why a version was not brought back although nothing is wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Skipped {
    /// `needed` is `None` when the version has no listing to read the size
    /// from and the volume ran full during the extraction.
    NoSpace { needed: Option<u64>, available: u64 },
}

/// What `bring_back` did.
#[derive(Debug)]
pub(crate) enum Brought {
    /// The folder is in place. `time` is what `.antiq-restored` holds.
    /// `failure` is what went wrong after the rename: the folder is then
    /// in place but not known to be durable.
    Done {
        time: Timestamp,
        warnings: Vec<String>,
        failure: Option<RestoreError>,
        /// The regular files the extraction wrote: symlinks and hard links
        /// are not files, as in the `files` of a listing.
        files: u64,
    },
    Skipped {
        reason: Skipped,
        warnings: Vec<String>,
    },
}

/// What a failed attempt to place the version leaves for the cleanup to
/// remove.
#[derive(Default)]
struct Placed {
    /// The staging folder, once it is made.
    staged: Option<RelPath>,
    /// The folders above the destination that this run creates.
    created: Vec<RelPath>,
    /// Set when the staged tree has been renamed to the destination: from
    /// then on the folder is in place and nothing is taken back.
    renamed: bool,
    warnings: Vec<String>,
    files: u64,
}

fn io_at(action: &'static str, path: impl ToString) -> impl FnOnce(io::Error) -> RestoreError {
    let path = path.to_string();
    move |source| RestoreError::Io {
        action,
        path,
        source,
    }
}

/// The most differences an error message lists.
const DIFFERENCES_SHOWN: usize = 20;

/// The differences as the error lists them: the first ones, and how many
/// more there are.
fn summarize(mut differences: Vec<String>) -> Vec<String> {
    if differences.len() > DIFFERENCES_SHOWN {
        let more = differences.len() - DIFFERENCES_SHOWN;
        differences.truncate(DIFFERENCES_SHOWN);
        differences.push(format!("and {more} more"));
    }
    differences
}

/// What `bring_back_to` extracts and what it checks the tree against.
pub(crate) struct Source<'a> {
    /// The piece the version belongs to: where its archive is stored.
    pub piece: &'a RelPath,
    pub version: &'a StoreVersion,
    /// The listing, if the version has one that reads. Without it neither
    /// the free space nor the extracted tree is checked.
    pub listing: Option<&'a Listing>,
    /// Whether the extracted tree is compared with the listing. A damaged
    /// version is not: its listing may not describe its archive.
    pub compare: bool,
}

impl<'a> Source<'a> {
    pub(crate) fn sound(chosen: &'a Chosen) -> Source<'a> {
        Source {
            piece: &chosen.piece,
            version: &chosen.version,
            listing: Some(&chosen.listing),
            compare: true,
        }
    }

    pub(crate) fn picked(picked: &'a Picked) -> Source<'a> {
        Source {
            piece: &picked.piece,
            version: &picked.version,
            listing: picked.listing.as_ref(),
            compare: picked.damage.is_empty(),
        }
    }
}

/// Where the folder goes and what `.antiq-restored` records.
pub(crate) struct Placement<'a> {
    pub destination: &'a RelPath,
    /// The stamp of the version, for an unearth; none for a fork.
    pub marker_version: Option<Stamp>,
    /// Reports the extraction and the comparison with the listing.
    pub tracker: Tracker<'a>,
}

/// Brings the chosen version back as a folder at the piece's own path;
/// see [`bring_back_to`].
#[cfg(test)]
pub(crate) fn bring_back(
    collection: &Collection,
    places: &Places,
    rules: &Rules,
    chosen: &Chosen,
    marker_version: Option<Stamp>,
    now: Timestamp,
    hooks: &dyn Hooks,
) -> Result<Brought, RestoreError> {
    bring_back_to(
        collection,
        places,
        rules,
        &Source::sound(chosen),
        &Placement {
            destination: &chosen.piece,
            marker_version,
            tracker: Tracker::silent(),
        },
        now,
        hooks,
    )
}

/// Extracts the version into staging below `.antiq/tmp/extract/`, compares
/// the tree with the listing, writes `.antiq-restored`, makes the tree
/// durable, renames it into place and flushes what the rename changed. The
/// version is not touched. Whatever fails before the rename removes the
/// staging and the folders above the destination that this run made.
///
/// A full volume anywhere before the rename ends in [`Brought::Skipped`]
/// with nothing left behind. After the rename the folder is in place, and a
/// failure from there on comes back in [`Brought::Done`] as `failure`.
pub(crate) fn bring_back_to(
    collection: &Collection,
    places: &Places,
    rules: &Rules,
    source: &Source,
    placement: &Placement,
    now: Timestamp,
    hooks: &dyn Hooks,
) -> Result<Brought, RestoreError> {
    let root =
        TreeRoot::open(collection.root()).map_err(io_at("open", Shown(&collection.root())))?;
    let tmp = collection.tmp_dir();
    if let Some(listing) = source.listing {
        let needed = listing.bytes;
        let available = hooks.available_space(&tmp)?;
        if available < needed {
            return Ok(Brought::Skipped {
                reason: Skipped::NoSpace {
                    needed: Some(needed),
                    available,
                },
                warnings: Vec::new(),
            });
        }
    }
    hooks.reached(Step::SpaceChecked)?;

    let time = Timestamp::from_second(now.as_second()).expect("a whole second of a time is valid");
    let mut placed = Placed::default();
    let result = place(
        collection,
        places,
        rules,
        source,
        placement,
        time,
        hooks,
        &root,
        &mut placed,
    );
    let brought = match result {
        Ok(()) => Ok(Brought::Done {
            time,
            warnings: placed.warnings,
            failure: None,
            files: placed.files,
        }),
        Err(error) if placed.renamed => Ok(Brought::Done {
            time,
            warnings: placed.warnings,
            failure: Some(error),
            files: placed.files,
        }),
        Err(error) => {
            take_back(&root, &mut placed);
            if space::is_out_of_space(&error) {
                Ok(Brought::Skipped {
                    reason: Skipped::NoSpace {
                        needed: source.listing.map(|listing| listing.bytes),
                        available: hooks.available_space(&tmp).unwrap_or(0),
                    },
                    warnings: placed.warnings,
                })
            } else {
                Err(error)
            }
        }
    };
    remove_extract_dir(&root);
    brought
}

/// Removes `.antiq/tmp/extract/` once no staging is left in it, so that a
/// run leaves `tmp/` empty. A staging that stays keeps the folder; repair 2
/// empties both at the start of the next run.
fn remove_extract_dir(root: &TreeRoot) {
    let extract = RelPath::parse(EXTRACT).expect("a valid path");
    let _ = root.remove_dir(&extract);
}

/// Removes the staging and the folders this run made above the
/// destination, deepest first. A folder that cannot be removed stays; the
/// staging is a warning as the next run empties `.antiq/tmp/`.
fn take_back(root: &TreeRoot, placed: &mut Placed) {
    if let Some(staged) = &placed.staged
        && let Err(error) = remove_tree(root, staged)
    {
        placed.warnings.push(format!(
            "cannot remove the staging folder `{staged}`: {error}; the next run empties it"
        ));
    }
    for parent in placed.created.iter().rev() {
        let _ = root.remove_dir(parent);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "each argument is one input of the run"
)]
fn place(
    collection: &Collection,
    places: &Places,
    rules: &Rules,
    source: &Source,
    placement: &Placement,
    time: Timestamp,
    hooks: &dyn Hooks,
    root: &TreeRoot,
    placed: &mut Placed,
) -> Result<(), RestoreError> {
    let target = placement.destination;
    let extract = RelPath::parse(EXTRACT).expect("a valid path");
    create_dir_chain(hooks.durable(), root, &extract)?;
    let staged = make_staging(root, &extract)?;
    placed.staged = Some(staged.clone());
    hooks.reached(Step::StagingMade)?;

    // The rules of the volume the tree is extracted to, and with them the
    // checks on the destination, come before the first byte is written.
    let staging = root.sub(&staged).map_err(io_at("open", &staged))?;
    let volume = hooks
        .target_rules(&staging)
        .map_err(io_at("probe the volume of", &staged))?;
    let missing = check_destination(root, rules, target)?;
    // Names are judged by the volume the folder will be on, so a destination
    // on another volume than the staging is refused before they are.
    check_same_volume(root, hooks, &extract, target, &missing)?;
    check_destination_names(target, &missing, &volume)?;

    let archive_name = source
        .version
        .archives
        .first()
        .expect("a sound version has an archive");
    let archive = places
        .pieces_dir
        .join(OsStr::from_bytes(source.piece.as_bytes()))
        .join(archive_name);
    let flush = TreeFlush::begin_with(&collection.tmp_dir().join("extract"), hooks.platform())
        .map_err(io_at("start the flush in", &extract))?;
    let extracting = placement.tracker.operation(Operation::Extracting);
    if let Some(listing) = source.listing {
        extracting.set_total(content_bytes(&listing.entries));
    }
    let unpacked = unpack_counted(&archive, &staging, &volume, &flush, &extracting)?;
    placed.files = unpacked.files;
    hooks.reached(Step::Extracted)?;

    if let Some(listing) = source.listing
        && source.compare
    {
        let verifying = placement.tracker.operation(Operation::Verifying);
        let compared = compare_with_listing_counted(
            &listing.entries,
            &staging,
            &WalkRules {
                staged: true,
                ..WalkRules::default()
            },
            &verifying,
        )?;
        if !compared.differences().is_empty() {
            return Err(RestoreError::Differs {
                differences: summarize(
                    compared
                        .differences()
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                ),
            });
        }
    }
    if !source.compare
        && let Some(name) = marker_in_tree_not_a_file(root, &staged)?
    {
        return Err(RestoreError::MarkerNotAFile {
            piece: source.piece.to_string(),
            stamp: source.version.stamp.to_string(),
            name,
        });
    }
    hooks.reached(Step::Verified)?;

    let marker = Restored {
        time,
        version: placement.marker_version,
    };
    markers::put(
        root,
        &staged,
        RESTORED_FILE,
        marker.render().as_bytes(),
        hooks.site(),
    )?;
    hooks.reached(Step::MarkerWritten)?;

    // The top gets its archived mode last and is flushed through a handle
    // opened before, which stays valid when the mode keeps the owner out
    // and after the rename.
    let top = root
        .open_dir_file(&staged)
        .map_err(io_at("open", &staged))?;
    root.set_mode(&staged, unpacked.top_mode & 0o7777)
        .map_err(io_at("set the mode of", &staged))?;
    hooks.durable().dir(&top, &named(root, &staged))?;
    flush
        .finish()
        .map_err(io_at("flush the volume of", &extract))?;
    hooks.reached(Step::Flushed)?;

    // The world may have changed since the first check.
    let missing = check_destination(root, rules, target)?;
    check_destination_names(target, &missing, &volume)?;
    let parent = parent_of(target);
    placed.created = missing;
    if !placed.created.is_empty() {
        create_dir_chain(hooks.durable(), root, &parent)?;
    }
    hooks.reached(Step::ParentsMade)?;
    let moved = hooks.move_dir(root, &staged, target)?;
    placed.renamed = true;
    if let Some(error) = moved.mode_restore_error {
        placed.warnings.push(error.to_string());
    }
    hooks.reached(Step::Renamed)?;

    hooks.durable().dir(&top, &named(root, target))?;
    flush_dir_of(hooks.durable(), root, &parent)?;
    hooks.reached(Step::ParentFlushed)?;
    Ok(())
}

/// The first marker name at the top of the extracted tree that is not what
/// that marker is, as [`marker_not_a_file`] judges the listing. A tree that
/// is not compared with a listing is judged by what was extracted.
fn marker_in_tree_not_a_file(
    root: &TreeRoot,
    staged: &RelPath,
) -> Result<Option<String>, RestoreError> {
    for name in [RESTORED_FILE, PIN_FILE] {
        let at = staged.join(&Name::from_bytes(name.into()).expect("a marker name is a name"));
        let Some(found) = look_at(root, &at)? else {
            continue;
        };
        let fits = match found.kind {
            EntryKind::File => true,
            EntryKind::Symlink => name == PIN_FILE,
            _ => false,
        };
        if !fits {
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

/// Makes the first unused numbered folder below `extract`.
fn make_staging(root: &TreeRoot, extract: &RelPath) -> Result<RelPath, RestoreError> {
    for number in 1_u64.. {
        let name = Name::from_bytes(number.to_string().into_bytes()).expect("digits are a name");
        let staged = extract.join(&name);
        match root.create_dir(&staged, 0o700) {
            Ok(()) => return Ok(staged),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_at("create", &staged)(error)),
        }
    }
    unreachable!("a u64 counter does not run out")
}

/// The folder that holds `path`.
fn parent_of(path: &RelPath) -> RelPath {
    ancestors(path).pop().unwrap_or_else(RelPath::root)
}

/// The folder, relative to the collection root, whose volume the staging
/// is judged on in a dry run: see [`Collection::measured_tmp_dir`].
pub(crate) fn measured_tmp(collection: &Collection) -> RelPath {
    let folder = if collection.measured_tmp_dir() == collection.tmp_dir() {
        ".antiq/tmp"
    } else {
        ".antiq"
    };
    RelPath::parse(folder).expect("a layout folder is a valid path")
}

/// Refuses a destination whose nearest existing folder is on another
/// volume than the staging, as the rename could not cross it.
pub(crate) fn check_same_volume(
    root: &TreeRoot,
    hooks: &dyn Hooks,
    extract: &RelPath,
    target: &RelPath,
    missing: &[RelPath],
) -> Result<(), RestoreError> {
    let nearest = ancestors(target)
        .into_iter()
        .rfind(|ancestor| !missing.contains(ancestor))
        .unwrap_or_else(RelPath::root);
    let device = |path: &RelPath| hooks.device(root, path).map_err(io_at("look at", path));
    if device(&nearest)? != device(extract)? {
        return Err(RestoreError::Target {
            path: target.clone(),
            problem: TargetProblem::MountPoint,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filesystem::atomic::recording::{Flush, Recording};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn errno(errno: Errno) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(errno.raw_os_error()))
    }

    #[test]
    fn the_staging_is_judged_on_tmp_and_on_antiq_while_tmp_is_missing() {
        let temp = crate::testing::tree::TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        assert_eq!(measured_tmp(&collection).to_string(), ".antiq/tmp");

        std::fs::remove_dir(collection.tmp_dir()).expect("tmp is empty");

        assert_eq!(measured_tmp(&collection).to_string(), ".antiq");
    }

    #[test]
    fn the_kernel_comes_from_the_release() {
        assert_eq!(parse_kernel("5.8.0"), Some((5, 8)));
        assert_eq!(parse_kernel("5.15.0-91-generic"), Some((5, 15)));
        assert_eq!(parse_kernel("4.19.2"), Some((4, 19)));
        assert_eq!(parse_kernel("6.1"), Some((6, 1)));
    }

    #[test]
    fn a_release_without_numbers_has_no_kernel() {
        assert_eq!(parse_kernel("garbage"), None);
        assert_eq!(parse_kernel(""), None);
        assert_eq!(parse_kernel("5"), None);
        assert_eq!(parse_kernel("5.x.1"), None);
    }

    #[test]
    fn macos_takes_the_barrier_when_the_full_flush_works() {
        assert_eq!(
            choose_method(System::MacOs, None, &Ok(())).unwrap(),
            Method::Barrier
        );
    }

    #[test]
    fn macos_flushes_per_item_when_the_full_flush_is_not_supported() {
        let refused = errno(Errno::NOTSUP);
        assert_eq!(
            choose_method(System::MacOs, None, &refused).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn linux_takes_the_barrier_from_kernel_5_8_on() {
        assert_eq!(
            choose_method(System::Linux, Some((5, 8)), &Ok(())).unwrap(),
            Method::Barrier
        );
        assert_eq!(
            choose_method(System::Linux, Some((6, 1)), &Ok(())).unwrap(),
            Method::Barrier
        );
    }

    #[test]
    fn linux_flushes_per_item_below_kernel_5_8_or_without_a_kernel() {
        assert_eq!(
            choose_method(System::Linux, Some((5, 7)), &Ok(())).unwrap(),
            Method::PerItem
        );
        assert_eq!(
            choose_method(System::Linux, Some((4, 19)), &Ok(())).unwrap(),
            Method::PerItem
        );
        assert_eq!(
            choose_method(System::Linux, None, &Ok(())).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn linux_flushes_per_item_when_syncfs_is_refused() {
        for refusal in [Errno::INVAL, Errno::NOSYS] {
            assert_eq!(
                choose_method(System::Linux, Some((5, 15)), &errno(refusal)).unwrap(),
                Method::PerItem,
                "{refusal:?}"
            );
        }
    }

    #[test]
    fn other_systems_flush_per_item() {
        assert_eq!(
            choose_method(System::Other, Some((9, 9)), &Ok(())).unwrap(),
            Method::PerItem
        );
    }

    #[test]
    fn a_probe_error_that_is_no_refusal_is_returned() {
        let error = choose_method(System::MacOs, None, &errno(Errno::IO)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
        let error = choose_method(System::Linux, Some((5, 15)), &errno(Errno::NOTSUP)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::NOTSUP.raw_os_error()));
    }

    /// A platform whose barrier counts its calls.
    fn counting(
        os: System,
        kernel: Option<(u32, u32)>,
        probe: io::Result<()>,
    ) -> (Platform, Arc<AtomicUsize>, Arc<Recording>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let recording = Arc::new(Recording::new());
        let seen = Arc::clone(&calls);
        let probe = std::sync::Mutex::new(Some(probe));
        let platform = Platform {
            os,
            kernel,
            barrier: Box::new(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                probe
                    .lock()
                    .expect("no test panics holding it")
                    .take()
                    .unwrap_or(Ok(()))
            }),
            durable: Box::new(Shared(Arc::clone(&recording))),
        };
        (platform, calls, recording)
    }

    /// A [`Durable`] that a test can keep a handle on.
    struct Shared(Arc<Recording>);

    impl Durable for Shared {
        fn file(&self, file: &File, path: &Path) -> Result<(), FilesystemError> {
            self.0.file(file, path)
        }
        fn dir(&self, dir: &File, path: &Path) -> Result<(), FilesystemError> {
            self.0.dir(dir, path)
        }
    }

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid test path")
    }

    #[test]
    fn begin_tries_the_barrier_once_and_takes_it_when_it_works() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, calls, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        assert_eq!(flush.method(), Method::Barrier);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn begin_removes_the_probe_when_the_barrier_fails_for_another_reason() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, _) = counting(System::Linux, Some((6, 1)), errno(Errno::IO));
        assert!(TreeFlush::begin_with(dir.path(), platform).is_err());
        assert!(
            !dir.path().join(".flush-probe").exists(),
            "the probe is removed"
        );
    }

    #[test]
    fn begin_falls_back_to_per_item_flushes_when_the_barrier_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, recording) = counting(System::MacOs, None, errno(Errno::NOTSUP));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        assert_eq!(flush.method(), Method::PerItem);
        let file = File::open(dir.path()).unwrap();
        flush.file(&file, &rel("a/b")).unwrap();
        flush.directory(&file, &rel("a")).unwrap();
        assert_eq!(
            recording.flushes(),
            vec![
                Flush::File(PathBuf::from("a/b")),
                Flush::Dir(PathBuf::from("a"))
            ]
        );
    }

    #[test]
    fn the_barrier_mode_flushes_nothing_per_item_through_the_durable() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, recording) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        let file = File::open(dir.path()).unwrap();
        flush.file(&file, &rel("a")).unwrap();
        flush.directory(&file, &rel("")).unwrap();
        assert!(recording.flushes().is_empty());
    }

    #[test]
    fn finish_runs_exactly_one_more_barrier_in_barrier_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, calls, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        flush.finish().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn finish_runs_no_barrier_in_per_item_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, calls, _) = counting(System::MacOs, None, errno(Errno::NOTSUP));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        flush.finish().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failing_barrier_is_returned_by_finish() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let platform = Platform {
            os: System::MacOs,
            kernel: None,
            barrier: Box::new(move |_| {
                if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(())
                } else {
                    errno(Errno::IO)
                }
            }),
            durable: Box::new(RealDurable),
        };
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        let error = flush.finish().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
    }

    #[test]
    fn a_per_item_error_keeps_its_errno() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, recording) = counting(System::MacOs, None, errno(Errno::NOTSUP));
        recording.fail_files_from(0);
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        let file = File::open(dir.path()).unwrap();
        let error = flush.file(&file, &rel("a")).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
    }

    #[test]
    fn the_probe_file_is_gone_after_finish_and_after_a_drop() {
        let dir = tempfile::tempdir().unwrap();
        let (platform, _, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        flush.finish().unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        let (platform, _, _) = counting(System::MacOs, None, Ok(()));
        let flush = TreeFlush::begin_with(dir.path(), platform).unwrap();
        drop(flush);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_real_platform_takes_the_barrier_on_a_local_volume() {
        let dir = tempfile::tempdir().unwrap();
        let flush = TreeFlush::begin(dir.path()).unwrap();
        assert_eq!(flush.method(), Method::Barrier);
        flush.finish().unwrap();
    }
}

#[cfg(test)]
mod choice_tests {
    use super::*;
    use crate::error::Error;
    use crate::testing::tree::{TempCollection, Tree};
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile;

    const OLD: &str = "2026-11-03T081500";
    const NEW: &str = "2026-11-04T090211";

    fn source() -> Tree {
        let mut tree = Tree::new();
        tree.file("projects/scanner/main.rs", "fn main() {}");
        tree.file("projects/scanner/src/lib.rs", "pub fn f() {}");
        tree.file("projects/scanner-next/main.rs", "fn main() {}");
        tree.file("tools/scan-old/run.sh", "echo");
        tree
    }

    struct World {
        collection: TempCollection,
        store: Store,
    }

    impl World {
        fn new() -> World {
            World {
                collection: TempCollection::minimal(),
                store: Store::new(),
            }
        }

        fn add(&mut self, piece: &str, stamp: &str) -> StoredVersion {
            self.store.add_version(&source(), piece, stamp)
        }

        fn choose(&self, given: &str, version: Option<&str>) -> Result<Chosen, RestoreError> {
            self.choose_in(given, version, &TimeZone::UTC)
        }

        fn choose_in(
            &self,
            given: &str,
            version: Option<&str>,
            zone: &TimeZone,
        ) -> Result<Chosen, RestoreError> {
            let collection =
                Collection::find(self.collection.root()).expect("the collection is found");
            choose_version(
                &collection,
                &self.store.places(),
                self.collection.root(),
                OsStr::new(given),
                version,
                zone,
            )
        }
    }

    fn scanner() -> World {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world.add("projects/scanner", NEW);
        world
    }

    fn chosen_stamp(result: Result<Chosen, RestoreError>) -> String {
        result.expect("a version is chosen").stamp.to_string()
    }

    fn message(result: Result<Chosen, RestoreError>) -> String {
        result.expect_err("the choice is refused").to_string()
    }

    #[test]
    fn a_path_names_the_piece_and_the_newest_version_is_taken() {
        let chosen = scanner().choose("projects/scanner", None).expect("chosen");
        assert_eq!(chosen.piece.to_string(), "projects/scanner");
        assert_eq!(chosen.stamp.to_string(), NEW);
        assert_eq!(chosen.listing.entries.len(), 3);
    }

    #[test]
    fn a_search_word_that_fits_one_piece_is_only_offered_to_unearth() {
        let world = scanner();
        let error = world.choose("scanner", None).expect_err("only offered");
        assert_eq!(error.kind(), ErrorKind::InexactReference);
        let details = Error::from(error).details();
        let candidates = details["candidates"].as_array().expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["path"], "projects/scanner");
    }

    #[test]
    fn a_search_word_that_fits_one_piece_still_names_the_piece_to_fork() {
        let world = scanner();
        let collection =
            Collection::find(world.collection.root()).expect("the collection is found");
        let picked = choose_version_to_fork(
            &collection,
            &world.store.places(),
            world.collection.root(),
            OsStr::new("scanner"),
            None,
            &TimeZone::UTC,
        )
        .expect("chosen");
        assert_eq!(picked.piece.to_string(), "projects/scanner");
        assert_eq!(picked.stamp.to_string(), NEW);
    }

    #[test]
    fn an_ambiguous_word_keeps_its_candidates() {
        let mut world = scanner();
        world.add("tools/scan-old", NEW);
        let error = world.choose("scan", None).expect_err("two pieces match");
        assert_eq!(error.kind(), ErrorKind::AmbiguousReference);
        let details = Error::from(error).details();
        let candidates = details["candidates"].as_array().expect("candidates");
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn an_unknown_piece_has_its_own_kind() {
        let error = scanner()
            .choose("nothing-like-it", None)
            .expect_err("unknown");
        assert_eq!(error.kind(), ErrorKind::UnknownReference);
    }

    #[test]
    fn an_empty_piece_and_an_empty_version_are_usage_errors() {
        let world = scanner();
        let error = world.choose("", None).expect_err("empty piece");
        assert_eq!(error.kind(), ErrorKind::Usage);
        let error = world
            .choose("projects/scanner", Some(""))
            .expect_err("empty version");
        assert_eq!(error.kind(), ErrorKind::Usage);
    }

    #[test]
    fn a_version_is_named_by_its_stamp() {
        let world = scanner();
        assert_eq!(
            chosen_stamp(world.choose("projects/scanner", Some(OLD))),
            OLD
        );
    }

    #[test]
    fn a_version_is_named_by_a_prefix_of_its_utc_stamp() {
        let world = scanner();
        assert_eq!(
            chosen_stamp(world.choose("projects/scanner", Some("2026-11-03T08"))),
            OLD
        );
    }

    #[test]
    fn a_version_is_named_by_its_local_date_and_time() {
        let world = scanner();
        let berlin = TimeZone::get("Europe/Berlin").expect("the zone exists");
        // 09:02:11 UTC is 10:02:11 in Berlin in November.
        let by_date = world.choose_in("projects/scanner", Some("2026-11-04"), &berlin);
        assert_eq!(chosen_stamp(by_date), NEW);
        let by_time = world.choose_in("projects/scanner", Some("2026-11-04 10:02"), &berlin);
        assert_eq!(chosen_stamp(by_time), NEW);
    }

    #[test]
    fn several_matches_list_every_stamp() {
        let text = message(scanner().choose("projects/scanner", Some("2026-11")));
        assert!(text.contains(OLD) && text.contains(NEW), "{text}");
    }

    #[test]
    fn no_match_lists_the_versions_there_are() {
        let error = scanner()
            .choose("projects/scanner", Some("1999"))
            .expect_err("no match");
        assert!(
            matches!(
                error,
                RestoreError::Version(VersionLookupError::NoSuch { .. })
            ),
            "{error:?}"
        );
        let text = error.to_string();
        assert!(text.contains(OLD) && text.contains(NEW), "{text}");
    }

    #[test]
    fn an_incomplete_version_named_by_stamp_is_refused_naming_forget() {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        let error = world
            .choose("projects/scanner", Some(NEW))
            .expect_err("no archive");
        assert!(
            matches!(error, RestoreError::Incomplete { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("forget"), "{error}");
    }

    #[test]
    fn a_piece_with_only_incomplete_versions_is_unknown_and_the_error_names_forget() {
        let mut world = World::new();
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        for given in ["projects/scanner", "scanner"] {
            let error = world.choose(given, None).expect_err("no archive");
            assert!(
                matches!(
                    error,
                    RestoreError::Reference(ReferenceError::OnlyIncomplete { .. })
                ),
                "{given}: {error:?}"
            );
            assert_eq!(error.kind(), ErrorKind::UnknownReference);
            let help = miette::Diagnostic::diagnostic_source(&error)
                .and_then(miette::Diagnostic::help)
                .expect("an explained error has a help");
            assert!(help.to_string().contains("forget"), "{error}");
        }
    }

    #[test]
    fn the_path_of_an_incomplete_piece_is_not_taken_for_a_piece_that_fits_it_loosely() {
        let mut world = World::new();
        world.add("projects/scanner-next", NEW);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        let error = world
            .choose("projects/scanner", None)
            .expect_err("no archive");
        assert!(
            matches!(
                error,
                RestoreError::Reference(ReferenceError::OnlyIncomplete { .. })
            ),
            "{error:?}"
        );
        let collection =
            Collection::find(world.collection.root()).expect("the collection is found");
        let error = choose_version_to_fork(
            &collection,
            &world.store.places(),
            world.collection.root(),
            OsStr::new("projects/scanner"),
            None,
            &TimeZone::UTC,
        )
        .expect_err("no archive");
        assert!(
            matches!(
                error,
                RestoreError::Reference(ReferenceError::OnlyIncomplete { .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_word_that_matches_nothing_even_among_incomplete_pieces_stays_unknown() {
        let mut world = World::new();
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        let error = world.choose("nothing-like-it", None).expect_err("unknown");
        assert!(
            matches!(
                error,
                RestoreError::Reference(ReferenceError::Unknown { .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn the_newest_version_with_an_archive_is_taken_over_a_newer_incomplete_one() {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Archive);
        assert_eq!(chosen_stamp(world.choose("projects/scanner", None)), OLD);
    }

    #[test]
    fn a_damaged_newest_version_is_refused_and_the_older_one_is_not_taken() {
        let mut world = World::new();
        world.add("projects/scanner", OLD);
        world
            .add("projects/scanner", NEW)
            .remove(VersionFile::Listing);
        let error = world.choose("projects/scanner", None).expect_err("damaged");
        assert!(matches!(error, RestoreError::Damaged { .. }), "{error:?}");
    }

    fn assert_damaged(damage: impl FnOnce(&StoredVersion), expected: &str) {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        damage(&version);
        let error = world.choose("projects/scanner", None).expect_err("damaged");
        let RestoreError::Damaged { reasons, .. } = &error else {
            panic!("expected damage, got {error:?}");
        };
        assert!(
            reasons.iter().any(|reason| reason.contains(expected)),
            "{reasons:?} lacks {expected:?}"
        );
        let text = error.to_string();
        for remedy in ["`fork`", "`rebuild`", "copied back"] {
            assert!(text.contains(remedy), "{text} lacks {remedy}");
        }
    }

    #[test]
    fn a_missing_listing_is_damage() {
        assert_damaged(|v| v.remove(VersionFile::Listing), "listing is missing");
    }

    #[test]
    fn a_missing_checksum_file_is_damage() {
        assert_damaged(
            |v| v.remove(VersionFile::Checksum),
            "checksum file is missing",
        );
    }

    #[test]
    fn a_truncated_archive_is_damage() {
        assert_damaged(
            |v| v.truncate(VersionFile::Archive, 10),
            "archive has 10 bytes",
        );
    }

    #[test]
    fn a_cut_listing_is_damage() {
        assert_damaged(|v| v.cut_listing(), "listing is damaged");
    }

    #[test]
    fn a_doubled_slot_shows_its_files_by_the_rule_for_names() {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        version.add_twin(VersionFile::Archive, "o%ther");
        let error = world.choose("projects/scanner", None).expect_err("doubled");
        assert!(error.to_string().contains("o%25ther--"), "{error}");
    }

    #[test]
    fn a_doubled_slot_names_every_file_and_forget() {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        let twin = version.add_twin(VersionFile::Archive, "other");
        let error = world.choose("projects/scanner", None).expect_err("doubled");
        let RestoreError::Doubled { files, .. } = &error else {
            panic!("expected a doubled slot, got {error:?}");
        };
        assert_eq!(files.len(), 2, "{files:?}");
        let twin = twin
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        assert!(files.contains(&twin), "{files:?}");
        let text = error.to_string();
        assert!(
            text.contains("`forget`") && !text.contains("`fork`"),
            "{text}"
        );
    }

    #[test]
    fn a_listing_of_a_newer_format_has_its_own_kind() {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        version.edit_listing(|text| text.replacen("# antiq listing 1", "# antiq listing 2", 1));
        let error = world
            .choose("projects/scanner", None)
            .expect_err("newer format");
        assert_eq!(error.kind(), ErrorKind::FormatTooNew);
        assert!(
            matches!(error, RestoreError::FormatTooNew { .. }),
            "{error:?}"
        );
    }

    fn entry(path: &str, kind: ListingType) -> ListingEntry {
        ListingEntry {
            path: RelPath::parse(path).expect("a valid path"),
            kind,
            size: 0,
            mtime: 0,
            mode: 0o644,
            link: None,
            sha256: None,
        }
    }

    #[test]
    fn a_marker_name_at_the_top_must_be_what_the_marker_is() {
        let refused = |path: &str, kind| marker_not_a_file(&[entry(path, kind)]);
        assert_eq!(
            refused(".antiq-restored", ListingType::Dir).as_deref(),
            Some(".antiq-restored")
        );
        assert_eq!(
            refused(".antiq-restored", ListingType::Symlink).as_deref(),
            Some(".antiq-restored")
        );
        assert_eq!(
            refused(".antiq-pin", ListingType::Dir).as_deref(),
            Some(".antiq-pin")
        );
        assert_eq!(refused(".antiq-restored", ListingType::File), None);
        assert_eq!(refused(".antiq-pin", ListingType::File), None);
        assert_eq!(refused(".antiq-pin", ListingType::Symlink), None);
        assert_eq!(refused(".antiq-pin", ListingType::Hardlink), None);
        assert_eq!(refused("sub/.antiq-pin", ListingType::Dir), None);
        assert_eq!(refused("other", ListingType::Dir), None);
    }

    #[test]
    fn a_version_whose_top_holds_a_marker_name_as_a_directory_is_refused() {
        let mut world = World::new();
        let version = world.add("projects/scanner", NEW);
        // A directory entry for the marker's name, with the counts of the
        // head left as they are: directories count for neither.
        version.edit_listing(|text| {
            let line = text
                .lines()
                .find(|line| line.ends_with("  src/"))
                .expect("the listing has a directory line")
                .to_string();
            text.replacen(&line, &line.replace("src/", ".antiq-pin/"), 1)
        });
        let error = world
            .choose("projects/scanner", None)
            .expect_err("a marker name");
        assert!(
            matches!(error, RestoreError::MarkerNotAFile { .. }),
            "{error:?}"
        );
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    use crate::archive::names::{LengthUnit, NameLimit};
    use crate::testing::tree::Tree;
    use std::os::unix::fs::PermissionsExt;

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid test path")
    }

    fn rules(patterns: &[&str], exclude: &[&str]) -> Rules {
        let patterns: Vec<String> = patterns.iter().map(ToString::to_string).collect();
        let exclude: Vec<RelPath> = exclude.iter().map(|path| rel(path)).collect();
        Rules::new(&patterns, &exclude).expect("valid rules")
    }

    /// The paths of the destination checks run against a fresh tree.
    struct Disk {
        tree: Tree,
    }

    impl Disk {
        fn new() -> Disk {
            Disk { tree: Tree::new() }
        }

        fn check(&self, rules: &Rules, target: &str) -> Result<Vec<RelPath>, RestoreError> {
            let root = TreeRoot::open(self.tree.root()).expect("the tree opens");
            let rules = rules.clone().bind(&root).expect("rules bind");
            check_destination(&root, &rules, &rel(target))
        }
    }

    fn problem(result: Result<Vec<RelPath>, RestoreError>) -> TargetProblem {
        match result.expect_err("the destination is refused") {
            RestoreError::Target { problem, .. } => problem,
            other => panic!("expected a target problem, got {other:?}"),
        }
    }

    fn all() -> Rules {
        rules(&["**"], &[])
    }

    #[test]
    fn the_collection_root_is_refused() {
        assert_eq!(problem(Disk::new().check(&all(), "")), TargetProblem::Root);
    }

    #[test]
    fn anything_in_antiq_is_refused_whatever_its_case() {
        let disk = Disk::new();
        for target in [".antiq/x", ".ANTIQ/x", ".antiq"] {
            assert_eq!(
                problem(disk.check(&all(), target)),
                TargetProblem::InsideAntiq,
                "{target}"
            );
        }
    }

    #[test]
    fn a_path_no_pattern_selects_is_refused() {
        let disk = Disk::new();
        assert_eq!(
            problem(disk.check(&rules(&["projects/*"], &[]), "tools/x")),
            TargetProblem::NotSelected
        );
    }

    #[test]
    fn an_excluded_destination_is_refused() {
        let disk = Disk::new();
        assert_eq!(
            problem(disk.check(&rules(&["projects/*"], &["projects/x"]), "projects/x")),
            TargetProblem::Excluded {
                at: rel("projects/x")
            }
        );
    }

    #[test]
    fn a_destination_below_an_existing_excluded_folder_is_refused() {
        let mut disk = Disk::new();
        disk.tree.dir("vendor");
        assert_eq!(
            problem(disk.check(&rules(&["*/*"], &["vendor"]), "vendor/x")),
            TargetProblem::Excluded { at: rel("vendor") }
        );
    }

    #[test]
    fn a_destination_below_a_missing_excluded_folder_is_refused() {
        let disk = Disk::new();
        assert_eq!(
            problem(disk.check(&rules(&["*/*"], &["vendor"]), "vendor/x")),
            TargetProblem::Excluded { at: rel("vendor") }
        );
    }

    #[test]
    fn a_folder_excluded_by_identity_is_refused() {
        // Two spellings of one folder exist only on a volume that ignores
        // case.
        let mut disk = Disk::new();
        disk.tree.dir("Vendor");
        let root = TreeRoot::open(disk.tree.root()).expect("opens");
        if !TargetRules::probe(&root).expect("probe").case_insensitive {
            eprintln!("skipped: the volume keeps case");
            return;
        }
        assert_eq!(
            problem(disk.check(&rules(&["*/*"], &["Vendor"]), "vendor/x")),
            TargetProblem::Excluded { at: rel("vendor") }
        );
    }

    #[test]
    fn a_destination_inside_a_selected_folder_is_inside_a_piece() {
        let disk = Disk::new();
        assert_eq!(
            problem(disk.check(&rules(&["projects/**"], &[]), "projects/a/b")),
            TargetProblem::InsidePiece {
                piece: rel("projects/a")
            }
        );
    }

    #[test]
    fn a_file_above_the_destination_is_refused() {
        let mut disk = Disk::new();
        disk.tree.file("projects", "x");
        assert_eq!(
            problem(disk.check(&rules(&["projects/*"], &[]), "projects/a")),
            TargetProblem::NotAFolder {
                at: rel("projects")
            }
        );
    }

    #[test]
    fn a_symlink_above_the_destination_is_refused_and_nothing_is_written_through_it() {
        let mut disk = Disk::new();
        let elsewhere = Tree::new();
        disk.tree.symlink("projects", elsewhere.root());
        assert_eq!(
            problem(disk.check(&rules(&["projects/*"], &[]), "projects/a")),
            TargetProblem::NotAFolder {
                at: rel("projects")
            }
        );
        assert!(
            std::fs::read_dir(elsewhere.root())
                .expect("lists")
                .next()
                .is_none()
        );
    }

    #[test]
    fn anything_at_the_destination_makes_it_taken() {
        let mut disk = Disk::new();
        disk.tree.file("projects/file", "x");
        disk.tree.dir("projects/folder");
        disk.tree.symlink("projects/dangling", "/nowhere/at/all");
        for target in ["projects/file", "projects/folder", "projects/dangling"] {
            assert_eq!(
                problem(disk.check(&rules(&["projects/*"], &[]), target)),
                TargetProblem::Taken,
                "{target}"
            );
        }
    }

    #[test]
    fn a_folder_the_volume_finds_under_another_spelling_is_refused() {
        let mut disk = Disk::new();
        disk.tree.dir("Projects");
        let root = TreeRoot::open(disk.tree.root()).expect("opens");
        if !TargetRules::probe(&root).expect("probe").case_insensitive {
            eprintln!("skipped: the volume keeps case");
            return;
        }
        assert_eq!(
            problem(disk.check(&rules(&["projects/*"], &[]), "projects/a")),
            TargetProblem::SpelledDifferently {
                at: rel("projects")
            }
        );
    }

    #[test]
    fn missing_parents_come_back_shallowest_first() {
        let mut disk = Disk::new();
        disk.tree.dir("a");
        assert_eq!(
            disk.check(&rules(&["a/*/*/*"], &[]), "a/b/c/d")
                .expect("allowed"),
            vec![rel("a/b"), rel("a/b/c")]
        );
    }

    #[test]
    fn a_free_destination_in_an_existing_parent_needs_no_parent() {
        let mut disk = Disk::new();
        disk.tree.dir("projects");
        assert!(
            disk.check(&rules(&["projects/*"], &[]), "projects/a")
                .expect("allowed")
                .is_empty()
        );
    }

    #[test]
    fn an_unreadable_ancestor_is_an_error_not_a_refusal() {
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let mut disk = Disk::new();
        disk.tree.unreadable_dir("projects");
        match disk
            .check(&rules(&["projects/*/*"], &[]), "projects/a/b")
            .expect_err("unreadable")
        {
            RestoreError::Io { path, .. } => assert_eq!(path, "projects/a"),
            other => panic!("expected a look error, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_parent_of_the_destination_is_an_error() {
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        let mut disk = Disk::new();
        disk.tree.dir("projects");
        std::fs::set_permissions(
            disk.tree.path("projects"),
            std::fs::Permissions::from_mode(0o000),
        )
        .expect("mode");
        let result = disk.check(&rules(&["projects/*"], &[]), "projects/a");
        std::fs::set_permissions(
            disk.tree.path("projects"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
        assert!(matches!(
            result.expect_err("unreadable"),
            RestoreError::Io { .. }
        ));
    }

    #[test]
    fn a_name_past_the_limit_of_the_volume_does_not_exist_and_is_no_look_error() {
        let mut disk = Disk::new();
        disk.tree.dir("projects");
        let target = format!("projects/{}", "n".repeat(300));
        assert!(
            disk.check(&rules(&["projects/*"], &[]), &target)
                .expect("a name that cannot exist is free")
                .is_empty()
        );
    }

    #[test]
    fn every_problem_says_what_is_wrong() {
        let at = rel("a/b");
        let texts: Vec<String> = [
            TargetProblem::Root,
            TargetProblem::InsideAntiq,
            TargetProblem::NotSelected,
            TargetProblem::Excluded { at: at.clone() },
            TargetProblem::InsidePiece { piece: at.clone() },
            TargetProblem::NotAFolder { at: at.clone() },
            TargetProblem::SpelledDifferently { at },
            TargetProblem::Taken,
            TargetProblem::Unholdable {
                reason: "too long".to_string(),
            },
            TargetProblem::MountPoint,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(
            texts,
            [
                "it is the collection root",
                "it is inside `.antiq/`",
                "the patterns do not select it",
                "`a/b` is excluded",
                "`a/b` is a piece",
                "`a/b` is a file or a symlink, not a folder",
                "`a/b` is spelled differently on disk",
                "it is taken",
                "too long",
                "it is on another volume than `.antiq/tmp/`: mount point",
            ]
        );
    }

    fn volume(decomposes: bool, max: usize) -> TargetRules {
        TargetRules {
            decomposes,
            name_limit: NameLimit {
                max,
                unit: LengthUnit::Bytes,
            },
            ..TargetRules::linux()
        }
    }

    #[test]
    fn a_name_past_the_limit_of_the_volume_is_unholdable() {
        let target = rel(&format!("projects/{}", "n".repeat(20)));
        let refused =
            check_destination_names(&target, &[], &volume(false, 10)).expect_err("refused");
        match refused {
            RestoreError::Target {
                problem: TargetProblem::Unholdable { reason },
                ..
            } => assert!(reason.contains("the volume allows 10"), "{reason}"),
            other => panic!("expected unholdable, got {other:?}"),
        }
    }

    #[test]
    fn a_name_that_is_not_utf8_is_unholdable_where_the_volume_needs_utf8() {
        let target = RelPath::from_bytes(b"projects/\xff".to_vec()).expect("a path");
        let rules = TargetRules {
            requires_utf8: true,
            ..TargetRules::linux()
        };
        assert!(matches!(
            check_destination_names(&target, &[], &rules).expect_err("refused"),
            RestoreError::Target {
                problem: TargetProblem::Unholdable { .. },
                ..
            }
        ));
    }

    #[test]
    fn a_parent_made_here_that_the_volume_would_store_decomposed_is_refused() {
        let target = rel("caf\u{e9}/x");
        let refused = check_destination_names(&target, &[rel("caf\u{e9}")], &volume(true, 255))
            .expect_err("refused");
        match refused {
            RestoreError::Target {
                problem: TargetProblem::Unholdable { reason },
                ..
            } => assert_eq!(reason, "the volume would store `caf\u{e9}` decomposed"),
            other => panic!("expected unholdable, got {other:?}"),
        }
    }

    #[test]
    fn the_destination_itself_is_held_to_the_decomposing_rule() {
        let target = rel("caf\u{e9}");
        assert!(check_destination_names(&target, &[], &volume(true, 255)).is_err());
    }

    #[test]
    fn an_existing_decomposed_parent_is_accepted() {
        let target = rel("caf\u{e9}/x");
        check_destination_names(&target, &[], &volume(true, 255)).expect("accepted");
    }

    #[test]
    fn plain_names_pass_on_a_decomposing_volume() {
        check_destination_names(&rel("projects/a"), &[rel("projects")], &volume(true, 255))
            .expect("accepted");
    }
}

#[cfg(test)]
mod bring_tests {
    use super::*;
    use crate::archive::names::{LengthUnit, NameLimit};
    use crate::testing::package::{RawMember, compress, raw_tar};
    use crate::testing::tree::{TempCollection, Tree, snapshot};
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    const STAMP: &str = "2026-11-04T090211";
    const MTIME: i64 = 1_700_000_000;

    type Log = Arc<Mutex<Vec<String>>>;
    type Fault = Arc<dyn Fn(&str, &Path) -> Option<io::Error> + Send + Sync>;
    type Callback = Box<dyn Fn() -> Result<(), RestoreError>>;

    fn rel(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid test path")
    }

    fn enospc() -> io::Error {
        io::Error::from_raw_os_error(Errno::NOSPC.raw_os_error())
    }

    fn at_error(action: &'static str) -> RestoreError {
        RestoreError::Io {
            action,
            path: String::new(),
            source: enospc(),
        }
    }

    /// A durable that logs what it is asked to flush and fails where the
    /// test says so.
    #[derive(Clone)]
    struct Flaky {
        log: Option<Log>,
        fault: Fault,
    }

    impl Durable for Flaky {
        fn file(&self, _file: &File, path: &Path) -> Result<(), FilesystemError> {
            self.flush("file", path)
        }
        fn dir(&self, _dir: &File, path: &Path) -> Result<(), FilesystemError> {
            self.flush("dir", path)
        }
    }

    impl Flaky {
        fn flush(&self, kind: &str, path: &Path) -> Result<(), FilesystemError> {
            if let Some(log) = &self.log {
                log.lock()
                    .expect("log")
                    .push(format!("{kind} {}", path.display()));
            }
            match (self.fault)(kind, path) {
                Some(source) => Err(FilesystemError::Io {
                    action: "flush",
                    path: path.display().to_string(),
                    source,
                }),
                None => Ok(()),
            }
        }
    }

    /// How the tree is flushed: with one barrier, or item by item.
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Barrier,
        PerItem,
    }

    struct Spy {
        root: PathBuf,
        log: Log,
        callbacks: Vec<(Step, Callback)>,
        mode: Mode,
        space: Option<u64>,
        durable: Flaky,
        barrier_fault: Option<io::Error>,
        probe_fault: bool,
        move_fault: Option<fn() -> FilesystemError>,
        volume: Option<TargetRules>,
        device: Option<fn(&RelPath) -> u64>,
        before_rename_fault: bool,
        restore_failure: bool,
    }

    impl Spy {
        fn new(root: &Path) -> Spy {
            let log: Log = Arc::default();
            Spy {
                root: root.to_path_buf(),
                log: log.clone(),
                callbacks: Vec::new(),
                mode: Mode::Barrier,
                space: None,
                durable: Flaky {
                    log: None,
                    fault: Arc::new(|_, _| None),
                },
                barrier_fault: None,
                probe_fault: false,
                move_fault: None,
                volume: None,
                device: None,
                before_rename_fault: false,
                restore_failure: false,
            }
        }

        fn on(mut self, step: Step, run: impl Fn() -> Result<(), RestoreError> + 'static) -> Spy {
            self.callbacks.push((step, Box::new(run)));
            self
        }

        fn fault(
            mut self,
            fault: impl Fn(&str, &Path) -> Option<io::Error> + Send + Sync + 'static,
        ) -> Spy {
            self.durable.fault = Arc::new(fault);
            self
        }

        fn logging_flushes(mut self) -> Spy {
            self.durable.log = Some(self.log.clone());
            self
        }

        fn entries(&self) -> Vec<String> {
            self.log.lock().expect("log").clone()
        }
    }

    struct SpySite<'a>(&'a Spy);

    impl Site for SpySite<'_> {
        fn durable(&self) -> &dyn Durable {
            &self.0.durable
        }
        fn before_rename(&self) -> io::Result<()> {
            if self.0.before_rename_fault {
                Err(enospc())
            } else {
                Ok(())
            }
        }
    }

    impl Hooks for Spy {
        fn reached(&self, step: Step) -> Result<(), RestoreError> {
            self.log.lock().expect("log").push(format!("{step:?}"));
            for (at, run) in &self.callbacks {
                if *at == step {
                    run()?;
                }
            }
            Ok(())
        }

        fn available_space(&self, path: &Path) -> Result<u64, FilesystemError> {
            match self.space {
                Some(space) => Ok(space),
                None => space::available_space(path),
            }
        }

        fn platform(&self) -> Platform {
            let log = self.log.clone();
            let barrier_fault = self.barrier_fault.as_ref().map(|e| e.raw_os_error());
            let probe_fault = self.probe_fault;
            let calls = Arc::new(Mutex::new(0));
            let extract = self.root.join(".antiq/tmp/extract");
            let barrier: Barrier = Box::new(move |file: &File| {
                let mut calls = calls.lock().expect("calls");
                *calls += 1;
                let device = file.metadata().expect("probe metadata").dev();
                let expected = fs::metadata(&extract).expect("extract dir").dev();
                log.lock().expect("log").push(if device == expected {
                    "barrier".to_string()
                } else {
                    "barrier on another volume".to_string()
                });
                // The first call is the try of `begin`.
                if probe_fault && *calls == 1 {
                    return Err(io::Error::from_raw_os_error(Errno::IO.raw_os_error()));
                }
                match barrier_fault {
                    Some(code) if *calls > 1 => {
                        Err(io::Error::from_raw_os_error(code.expect("an errno")))
                    }
                    _ => Ok(()),
                }
            });
            match self.mode {
                Mode::Barrier => Platform {
                    os: System::Linux,
                    kernel: Some((6, 1)),
                    barrier,
                    durable: Box::new(self.durable.clone()),
                },
                Mode::PerItem => Platform {
                    os: System::Other,
                    kernel: None,
                    barrier,
                    durable: Box::new(self.durable.clone()),
                },
            }
        }

        fn durable(&self) -> &dyn Durable {
            &self.durable
        }

        fn site(&self) -> &dyn Site {
            // Leaked once per call; the value is two words and tests are short.
            Box::leak(Box::new(SpySite(self)))
        }

        fn device(&self, root: &TreeRoot, path: &RelPath) -> io::Result<u64> {
            match self.device {
                Some(device) => Ok(device(path)),
                None => Ok(root.lstat(path)?.device),
            }
        }

        fn target_rules(&self, staged: &TreeRoot) -> io::Result<TargetRules> {
            match &self.volume {
                Some(rules) => Ok(rules.clone()),
                None => TargetRules::probe(staged),
            }
        }

        fn move_dir(
            &self,
            root: &TreeRoot,
            from: &RelPath,
            to: &RelPath,
        ) -> Result<Moved, FilesystemError> {
            if self.restore_failure {
                let _ = moves::move_dir(root, from, root, to)?;
                return Ok(Moved {
                    mode_restore_error: Some(FilesystemError::Locked {
                        path: "restore the mode".into(),
                    }),
                });
            }
            match self.move_fault {
                Some(fault) => Err(fault()),
                None => moves::move_dir(root, from, root, to),
            }
        }
    }

    fn piece_source() -> Tree {
        let mut tree = Tree::new();
        tree.file_with("projects/scanner/main.rs", "fn main() {}", 0o644, MTIME);
        tree.file_with(
            "projects/scanner/src/lib.rs",
            "pub fn f() {}",
            0o755,
            MTIME + 60,
        );
        tree.symlink("projects/scanner/link", "main.rs");
        tree.hard_link("projects/scanner/main.rs", "projects/scanner/copy.rs");
        tree.dir("projects/scanner/empty");
        tree
    }

    struct World {
        collection: TempCollection,
        store: Store,
        source: Tree,
        rules: Rules,
        now: Timestamp,
    }

    impl World {
        fn new() -> World {
            World::new_with_version().0
        }

        fn new_with_version() -> (World, StoredVersion) {
            let source = piece_source();
            let mut store = Store::new();
            let version = store.add_version(&source, "projects/scanner", STAMP);
            let world = World {
                collection: TempCollection::minimal(),
                store,
                source,
                rules: Rules::new(&["projects/*".to_string()], &[]).expect("rules"),
                now: "2026-11-05T10:00:00.7Z".parse().expect("a time"),
            };
            (world, version)
        }

        fn root(&self) -> &Path {
            self.collection.root()
        }

        fn destination(&self) -> PathBuf {
            self.root().join("projects/scanner")
        }

        fn extract_dir(&self) -> PathBuf {
            self.root().join(".antiq/tmp/extract")
        }

        fn spy(&self) -> Spy {
            Spy::new(self.root())
        }

        fn run(&self, hooks: &dyn Hooks) -> Result<Brought, RestoreError> {
            self.run_reporting(hooks, Tracker::silent())
        }

        fn run_reporting(
            &self,
            hooks: &dyn Hooks,
            tracker: Tracker<'_>,
        ) -> Result<Brought, RestoreError> {
            let collection = Collection::find(self.root()).expect("collection");
            let places = self.store.places();
            let chosen = choose_version(
                &collection,
                &places,
                self.root(),
                OsStr::new("projects/scanner"),
                None,
                &TimeZone::UTC,
            )
            .expect("the version is chosen");
            let rules = self
                .rules
                .clone()
                .bind(&TreeRoot::open(self.root()).expect("root"))
                .expect("bound");
            bring_back_to(
                &collection,
                &places,
                &rules,
                &Source::sound(&chosen),
                &Placement {
                    destination: &chosen.piece,
                    marker_version: Some(chosen.stamp),
                    tracker,
                },
                self.now,
                hooks,
            )
        }
    }

    fn done(result: Result<Brought, RestoreError>) -> (Timestamp, Vec<String>) {
        match result.expect("the version comes back") {
            Brought::Done { time, warnings, .. } => (time, warnings),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    fn skipped(result: Result<Brought, RestoreError>) -> Skipped {
        match result.expect("the run ends without an error") {
            Brought::Skipped { reason, .. } => reason,
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn the_extraction_counts_the_content_bytes_of_the_regular_files_and_not_the_links() {
        use crate::testing::recording::Recorder;
        let world = World::new();
        let recorder = Recorder::new();

        done(world.run_reporting(&world.spy(), Tracker::new(&recorder, "projects/scanner")));

        // The bytes of main.rs and src/lib.rs; the symlink and the hard link are members
        // of the archive but have no content bytes of their own, as in a listing.
        assert_eq!(
            recorder.lines().first().map(String::as_str),
            Some("projects/scanner Extracting 25/25")
        );
    }

    #[test]
    fn the_files_written_are_the_regular_files_and_not_the_links() {
        let world = World::new();
        match world.run(&world.spy()).expect("the version comes back") {
            // main.rs and src/lib.rs; the symlink and the hard link are
            // members of the archive but not files.
            Brought::Done { files, .. } => assert_eq!(files, 2),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn the_tree_comes_back_with_content_modes_times_links_and_parents() {
        let world = World::new();
        done(world.run(&world.spy()));
        let mut expected = snapshot(&world.source.path("projects/scanner"));
        let mut found = snapshot(&world.destination());
        assert!(found.remove(".antiq-restored").is_some());
        expected.remove(".antiq-restored");
        assert_eq!(found, expected);
        let main = fs::metadata(world.destination().join("main.rs")).expect("main.rs");
        assert_eq!(main.mtime(), MTIME);
        assert_eq!(main.nlink(), 2, "the hard link is a hard link again");
        assert_eq!(
            fs::read_link(world.destination().join("link")).expect("link"),
            Path::new("main.rs")
        );
        assert!(world.destination().join("empty").is_dir());
    }

    #[test]
    fn the_top_carries_the_time_of_the_marker_write_not_the_archived_one() {
        let mut world = World::new();
        let archived = std::time::UNIX_EPOCH + std::time::Duration::from_secs(MTIME as u64);
        fs::File::open(world.source.path("projects/scanner"))
            .expect("opens the top")
            .set_modified(archived)
            .expect("sets the time");
        let mut store = Store::new();
        store.add_version(&world.source, "projects/scanner", STAMP);
        world.store = store;
        done(world.run(&world.spy()));
        let top = fs::metadata(world.destination()).expect("destination");
        assert_ne!(top.mtime(), MTIME);
    }

    #[test]
    fn the_marker_holds_the_time_and_the_version_and_staging_is_empty() {
        let world = World::new();
        let (time, warnings) = done(world.run(&world.spy()));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(time.to_string(), "2026-11-05T10:00:00Z");
        assert_eq!(
            fs::read_to_string(world.destination().join(".antiq-restored")).expect("marker"),
            format!("restored 2026-11-05T10:00:00Z version {STAMP}\n")
        );
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn the_steps_come_in_order_and_the_barrier_runs_before_the_rename() {
        let world = World::new();
        let root = world.root().to_path_buf();
        let destination = world.destination();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (at_flushed, at_renamed) = (seen.clone(), seen.clone());
        let (d1, d2) = (destination.clone(), destination.clone());
        let spy = world
            .spy()
            .logging_flushes()
            .on(Step::Flushed, move || {
                at_flushed
                    .lock()
                    .expect("seen")
                    .push(format!("destination at Flushed: {}", d1.exists()));
                Ok(())
            })
            .on(Step::Renamed, move || {
                at_renamed.lock().expect("seen").push(format!(
                    "destination at Renamed: {}, staged left: {}",
                    d2.exists(),
                    root.join(".antiq/tmp/extract/1").exists()
                ));
                Ok(())
            });
        done(world.run(&spy));
        let log = spy.entries();
        let steps: Vec<&str> = log
            .iter()
            .map(String::as_str)
            .filter(|entry| !entry.starts_with("dir ") && !entry.starts_with("file "))
            .collect();
        assert_eq!(
            steps,
            [
                "SpaceChecked",
                "StagingMade",
                "barrier",
                "Extracted",
                "Verified",
                "MarkerWritten",
                "barrier",
                "Flushed",
                "ParentsMade",
                "Renamed",
                "ParentFlushed"
            ]
        );
        let renamed = log.iter().position(|e| e == "Renamed").expect("Renamed");
        assert_eq!(
            &log[renamed..],
            [
                "Renamed",
                "dir projects/scanner",
                "dir projects",
                "ParentFlushed"
            ]
        );
        assert_eq!(
            *seen.lock().expect("seen"),
            [
                "destination at Flushed: false",
                "destination at Renamed: true, staged left: false"
            ]
        );
    }

    #[test]
    fn the_per_item_method_flushes_every_file_and_directory_and_no_barrier() {
        let world = World::new();
        let mut spy = world.spy().logging_flushes();
        spy.mode = Mode::PerItem;
        done(world.run(&spy));
        let log = spy.entries();
        assert!(log.iter().any(|e| e == "file src/lib.rs"), "{log:?}");
        assert!(log.iter().any(|e| e == "dir src"), "{log:?}");
        assert_eq!(
            log.iter().filter(|e| e.starts_with("barrier")).count(),
            1,
            "only the try"
        );
    }

    #[test]
    fn a_file_changed_after_the_extraction_refuses_and_nothing_moves() {
        let world = World::new();
        let staged = world.extract_dir().join("1/main.rs");
        let spy = world.spy().on(Step::Extracted, move || {
            fs::write(&staged, "fn xxxx() {}").expect("changes the file");
            Ok(())
        });
        let error = world.run(&spy).expect_err("the tree differs");
        assert!(matches!(error, RestoreError::Differs { .. }), "{error:?}");
        assert!(!world.root().join("projects").exists());
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_destination_taken_after_the_check_is_refused_and_staging_goes() {
        let world = World::new();
        let destination = world.destination();
        let spy = world.spy().on(Step::Flushed, move || {
            fs::create_dir_all(&destination).expect("takes the place");
            Ok(())
        });
        match world.run(&spy).expect_err("taken") {
            RestoreError::Target { problem, .. } => assert_eq!(problem, TargetProblem::Taken),
            other => panic!("expected taken, got {other:?}"),
        }
        assert_eq!(fs::read_dir(world.destination()).expect("lists").count(), 0);
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_symlink_above_the_destination_after_the_verify_is_refused() {
        let world = World::new();
        let elsewhere = Tree::new();
        let (link, target) = (
            world.root().join("projects"),
            elsewhere.root().to_path_buf(),
        );
        let spy = world.spy().on(Step::Verified, move || {
            std::os::unix::fs::symlink(&target, &link).expect("link");
            Ok(())
        });
        match world.run(&spy).expect_err("refused") {
            RestoreError::Target { problem, .. } => {
                assert_eq!(
                    problem,
                    TargetProblem::NotAFolder {
                        at: rel("projects")
                    }
                );
            }
            other => panic!("expected a target problem, got {other:?}"),
        }
        assert_eq!(fs::read_dir(elsewhere.root()).expect("lists").count(), 0);
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_failing_rename_removes_the_parents_this_run_made() {
        let world = World::new();
        let mut spy = world.spy();
        spy.move_fault = Some(|| FilesystemError::Locked { path: "x".into() });
        let error = world.run(&spy).expect_err("the rename fails");
        assert!(matches!(error, RestoreError::Filesystem(_)), "{error:?}");
        assert!(!world.root().join("projects").exists());
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_failing_rename_leaves_parents_that_existed() {
        let mut world = World::new();
        world.collection.tree().dir("projects");
        let mut spy = world.spy();
        spy.move_fault = Some(|| FilesystemError::Locked { path: "x".into() });
        world.run(&spy).expect_err("the rename fails");
        assert!(world.root().join("projects").is_dir());
    }

    #[test]
    fn parents_that_cannot_be_made_durable_are_removed_again() {
        let mut world = World::new();
        world.rules = Rules::new(&["projects/*/*".to_string()], &[]).expect("rules");
        let source = {
            let mut tree = piece_source();
            tree.file("projects/team/scanner/x", "x");
            tree
        };
        let mut store = Store::new();
        store.add_version(&source, "projects/team/scanner", STAMP);
        world.store = store;
        let spy = world.spy().fault(|kind, path| {
            (kind == "dir" && path == Path::new("projects/team"))
                .then(|| io::Error::from_raw_os_error(Errno::IO.raw_os_error()))
        });
        let collection = Collection::find(world.root()).expect("collection");
        let chosen = choose_version(
            &collection,
            &world.store.places(),
            world.root(),
            OsStr::new("projects/team/scanner"),
            None,
            &TimeZone::UTC,
        )
        .expect("chosen");
        let rules = world
            .rules
            .clone()
            .bind(&TreeRoot::open(world.root()).expect("root"))
            .expect("bound");
        let error = bring_back(
            &collection,
            &world.store.places(),
            &rules,
            &chosen,
            None,
            world.now,
            &spy,
        )
        .expect_err("the parents cannot be flushed");
        assert!(matches!(error, RestoreError::Filesystem(_)), "{error:?}");
        assert!(!world.root().join("projects").exists());
    }

    #[test]
    fn a_top_without_owner_write_arrives_with_its_mode() {
        assert_ne!(
            rustix::process::getuid().as_raw(),
            0,
            "the test needs a non-root user"
        );
        struct Unlock(PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
            }
        }
        let mut world = World::new();
        fs::set_permissions(
            world.source.path("projects/scanner"),
            fs::Permissions::from_mode(0o500),
        )
        .expect("mode");
        let mut store = Store::new();
        store.add_version(&world.source, "projects/scanner", STAMP);
        world.store = store;
        fs::set_permissions(
            world.source.path("projects/scanner"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("mode");
        let _unlock = Unlock(world.destination());
        done(world.run(&world.spy()));
        let mode = fs::metadata(world.destination())
            .expect("destination")
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o500);
    }

    #[test]
    fn no_room_skips_before_anything_is_staged() {
        let world = World::new();
        let mut spy = world.spy();
        spy.space = Some(5);
        match skipped(world.run(&spy)) {
            Skipped::NoSpace { needed, available } => {
                assert!(needed.is_some_and(|needed| needed > 5));
                assert_eq!(available, 5);
            }
        }
        assert!(!spy.entries().contains(&"StagingMade".to_string()));
        assert!(!world.extract_dir().exists(), "extract/ is gone");
        assert!(!world.root().join("projects").exists());
    }

    fn assert_no_space_and_clean(world: &World, spy: &Spy) {
        let reason = skipped(world.run(spy));
        assert!(matches!(reason, Skipped::NoSpace { .. }));
        assert!(!world.extract_dir().exists(), "extract/ is gone");
        assert!(!world.root().join("projects").exists());
    }

    #[test]
    fn a_full_volume_during_the_extraction_skips_and_removes_staging() {
        let world = World::new();
        let mut spy = world.spy().fault(|kind, _| (kind == "file").then(enospc));
        spy.mode = Mode::PerItem;
        assert_no_space_and_clean(&world, &spy);
    }

    #[test]
    fn a_full_volume_at_the_barrier_skips_and_removes_staging() {
        let world = World::new();
        let mut spy = world.spy();
        spy.barrier_fault = Some(enospc());
        assert_no_space_and_clean(&world, &spy);
    }

    #[test]
    fn a_full_volume_at_the_marker_skips_and_removes_staging() {
        let world = World::new();
        let mut spy = world.spy();
        spy.before_rename_fault = true;
        assert_no_space_and_clean(&world, &spy);
    }

    #[test]
    fn a_failed_marker_write_leaves_no_temp_file_in_tmp() {
        let world = World::new();
        let mut spy = world.spy();
        spy.before_rename_fault = true;
        assert_no_space_and_clean(&world, &spy);
        let left: Vec<String> = fs::read_dir(world.root().join(".antiq/tmp"))
            .expect("lists tmp")
            .map(|item| {
                item.expect("item")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".antiq-restored"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_full_volume_while_creating_the_parents_skips_and_removes_them() {
        let world = World::new();
        let spy = world
            .spy()
            .fault(|kind, path| (kind == "dir" && path == Path::new("projects")).then(enospc));
        assert_no_space_and_clean(&world, &spy);
    }

    #[test]
    fn a_full_volume_at_the_rename_skips_and_removes_the_parents() {
        let world = World::new();
        let mut spy = world.spy();
        spy.move_fault = Some(|| FilesystemError::Io {
            action: "move",
            path: "x".into(),
            source: enospc(),
        });
        assert_no_space_and_clean(&world, &spy);
    }

    #[test]
    fn a_full_volume_after_the_rename_is_returned_with_the_folder_in_place() {
        let world = World::new();
        // The chain flushes "projects" before the rename too, so only the
        // second flush of it, the parent flush after the rename, fails.
        let flushed_before = Arc::new(Mutex::new(false));
        let spy = world.spy().fault(move |kind, path| {
            if kind == "dir" && path == Path::new("projects") {
                let mut seen = flushed_before.lock().expect("gate");
                let fail = std::mem::replace(&mut *seen, true);
                return fail.then(enospc);
            }
            None
        });
        let Brought::Done { failure, time, .. } = world
            .run(&spy)
            .expect("a failure after the rename is no error")
        else {
            panic!("the folder is in place");
        };
        let error = failure.expect("the flush failure comes back beside the folder");
        assert!(space::is_out_of_space(&error), "{error:?}");
        assert_eq!(time.to_string(), "2026-11-05T10:00:00Z");
        assert!(world.destination().join("main.rs").exists());
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn another_failure_during_the_extraction_is_an_error_and_staging_goes() {
        let world = World::new();
        let mut spy = world.spy().fault(|kind, _| {
            (kind == "file").then(|| io::Error::from_raw_os_error(Errno::IO.raw_os_error()))
        });
        spy.mode = Mode::PerItem;
        let error = world.run(&spy).expect_err("fails");
        assert!(matches!(error, RestoreError::Pack(_)), "{error:?}");
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_member_name_the_volume_cannot_hold_is_refused_before_anything_is_written() {
        let mut world = World::new();
        world
            .source
            .file("projects/scanner/a-long-member-name", "x");
        world
            .store
            .add_version(&world.source, "projects/scanner", STAMP);
        let mut spy = world.spy();
        spy.volume = Some(TargetRules {
            name_limit: NameLimit {
                max: 9,
                unit: LengthUnit::Bytes,
            },
            ..TargetRules::linux()
        });
        let error = world.run(&spy).expect_err("refused");
        assert!(
            matches!(error, RestoreError::Pack(PackError::UnholdableNames(_))),
            "{error:?}"
        );
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_destination_on_another_volume_than_tmp_is_refused_as_a_mount_point() {
        let world = World::new();
        let mut spy = world.spy();
        spy.device = Some(|path| {
            if path.to_string().starts_with(".antiq") {
                1
            } else {
                2
            }
        });
        match world.run(&spy).expect_err("refused") {
            RestoreError::Target { problem, .. } => assert_eq!(problem, TargetProblem::MountPoint),
            other => panic!("expected a target problem, got {other:?}"),
        }
        assert!(!spy.entries().contains(&"Extracted".to_string()));
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_destination_on_another_volume_is_a_mount_point_before_its_names_are_judged() {
        let world = World::new();
        let mut spy = world.spy();
        spy.device = Some(|path| u64::from(!path.to_string().starts_with(".antiq")));
        spy.volume = Some(TargetRules {
            name_limit: NameLimit {
                max: 5,
                unit: LengthUnit::Bytes,
            },
            ..TargetRules::linux()
        });
        match world.run(&spy).expect_err("refused") {
            RestoreError::Target { problem, .. } => assert_eq!(problem, TargetProblem::MountPoint),
            other => panic!("expected a target problem, got {other:?}"),
        }
    }

    #[test]
    fn a_destination_that_the_names_of_the_volume_cannot_hold_is_refused_before_extraction() {
        let world = World::new();
        let mut spy = world.spy();
        spy.volume = Some(TargetRules {
            name_limit: NameLimit {
                max: 5,
                unit: LengthUnit::Bytes,
            },
            ..TargetRules::linux()
        });
        match world.run(&spy).expect_err("refused") {
            RestoreError::Target { problem, .. } => {
                assert!(matches!(problem, TargetProblem::Unholdable { .. }));
            }
            other => panic!("expected a target problem, got {other:?}"),
        }
        assert!(!spy.entries().contains(&"Extracted".to_string()));
    }

    #[test]
    fn the_real_hooks_bring_a_version_back_on_this_volume() {
        let world = World::new();
        done(world.run(&RealHooks));
        assert!(world.destination().join("main.rs").exists());
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_failure_at_every_step_before_the_rename_removes_staging_and_parents() {
        for step in [
            Step::SpaceChecked,
            Step::StagingMade,
            Step::Extracted,
            Step::Verified,
            Step::MarkerWritten,
            Step::Flushed,
            Step::ParentsMade,
        ] {
            let world = World::new();
            let spy = world.spy().on(step, || {
                Err(RestoreError::Io {
                    action: "stop",
                    path: String::new(),
                    source: io::Error::from_raw_os_error(Errno::IO.raw_os_error()),
                })
            });
            world.run(&spy).expect_err("the run stops");
            assert!(!world.root().join("projects").exists(), "{step:?}");
            assert!(!world.extract_dir().exists(), "{step:?}: extract/ is gone");
        }
    }

    #[test]
    fn many_differences_are_listed_up_to_a_limit_and_counted() {
        let all: Vec<String> = (0..25).map(|n| format!("difference {n}")).collect();
        let shown = summarize(all.clone());
        assert_eq!(shown.len(), 21);
        assert_eq!(shown[19], "difference 19");
        assert_eq!(shown[20], "and 5 more");
        assert_eq!(summarize(all[..3].to_vec()), all[..3].to_vec());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staging_that_cannot_be_removed_is_a_warning_of_the_skipped_run() {
        struct Unlock(PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let _ = std::process::Command::new("chflags")
                    .arg("nouchg")
                    .arg(&self.0)
                    .status();
            }
        }
        let world = World::new();
        let locked = world.extract_dir().join("1/main.rs");
        let _unlock = Unlock(locked.clone());
        let spy = world.spy().on(Step::Extracted, move || {
            let status = std::process::Command::new("chflags")
                .arg("uchg")
                .arg(&locked)
                .status()
                .expect("chflags runs");
            assert!(status.success());
            Err(at_error("fill"))
        });
        match world.run(&spy).expect("skipped") {
            Brought::Skipped { warnings, .. } => {
                assert_eq!(warnings.len(), 1, "{warnings:?}");
                assert!(
                    warnings[0].starts_with("cannot remove the staging folder"),
                    "{warnings:?}"
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert!(
            world.extract_dir().join("1").is_dir(),
            "the staging that cannot go keeps extract/ for repair 2"
        );
    }

    #[test]
    fn a_flush_probe_that_fails_leaves_neither_the_probe_nor_extract() {
        let world = World::new();
        let mut spy = world.spy();
        spy.probe_fault = true;
        assert!(world.run(&spy).is_err(), "the failed probe ends the run");
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_failure_after_the_rename_leaves_no_extract() {
        let world = World::new();
        let spy = world.spy().on(Step::Renamed, || Err(at_error("fill")));
        match world.run(&spy).expect("the folder is in place") {
            Brought::Done { failure, .. } => assert!(failure.is_some()),
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    #[test]
    fn a_mode_that_cannot_be_put_back_after_the_rename_is_a_warning() {
        let world = World::new();
        let mut spy = world.spy();
        spy.restore_failure = true;
        let (_, warnings) = done(world.run(&spy));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("restore the mode"), "{warnings:?}");
    }

    #[test]
    fn staging_takes_the_first_unused_number() {
        let mut world = World::new();
        world.collection.tree().dir(".antiq/tmp/extract/1");
        world.collection.tree().dir(".antiq/tmp/extract/2");
        let staged = world.extract_dir().join("3");
        let seen = Arc::new(Mutex::new(false));
        let flag = seen.clone();
        let spy = world.spy().on(Step::StagingMade, move || {
            *flag.lock().expect("flag") = staged.is_dir();
            Ok(())
        });
        done(world.run(&spy));
        assert!(*seen.lock().expect("flag"));
        assert!(
            world.extract_dir().join("1").is_dir(),
            "an old folder stays"
        );
    }

    // ----- bringing a version back somewhere else, or without its listing

    impl World {
        fn chosen(&self) -> Chosen {
            let collection = Collection::find(self.root()).expect("collection");
            choose_version(
                &collection,
                &self.store.places(),
                self.root(),
                OsStr::new("projects/scanner"),
                None,
                &TimeZone::UTC,
            )
            .expect("the version is chosen")
        }

        fn run_source(
            &self,
            source: &Source,
            destination: &str,
            hooks: &dyn Hooks,
        ) -> Result<Brought, RestoreError> {
            let collection = Collection::find(self.root()).expect("collection");
            let rules = self
                .rules
                .clone()
                .bind(&TreeRoot::open(self.root()).expect("root"))
                .expect("bound");
            let destination = rel(destination);
            bring_back_to(
                &collection,
                &self.store.places(),
                &rules,
                source,
                &Placement {
                    destination: &destination,
                    marker_version: None,
                    tracker: Tracker::silent(),
                },
                self.now,
                hooks,
            )
        }
    }

    #[test]
    fn a_version_comes_back_at_another_path_and_the_store_keeps_it() {
        let world = World::new();
        let chosen = world.chosen();
        done(world.run_source(&Source::sound(&chosen), "projects/copy", &world.spy()));
        let mut expected = snapshot(&world.source.path("projects/scanner"));
        let mut found = snapshot(&world.root().join("projects/copy"));
        assert!(found.remove(".antiq-restored").is_some());
        expected.remove(".antiq-restored");
        assert_eq!(found, expected);
        assert!(
            !world.destination().exists(),
            "the original path stays free"
        );
        assert!(
            world
                .store
                .scan()
                .pieces
                .iter()
                .any(|piece| !piece.versions.is_empty()),
            "the version is still in the store"
        );
        let text = fs::read_to_string(world.root().join("projects/copy/.antiq-restored"))
            .expect("the marker");
        assert!(
            text.starts_with("restored ") && !text.contains("version"),
            "{text}"
        );
    }

    #[test]
    fn the_folders_above_another_path_are_made() {
        let mut world = World::new();
        world.rules = Rules::new(&["projects/*/*".to_string()], &[]).expect("rules");
        let chosen = world.chosen();
        done(world.run_source(&Source::sound(&chosen), "projects/a/copy", &world.spy()));
        assert!(world.root().join("projects/a/copy/main.rs").is_file());
    }

    #[test]
    fn a_destination_that_is_taken_is_refused_whatever_the_piece_is() {
        let world = World::new();
        fs::create_dir_all(world.root().join("projects/copy")).expect("takes the place");
        let chosen = world.chosen();
        let error = world
            .run_source(&Source::sound(&chosen), "projects/copy", &world.spy())
            .expect_err("taken");
        let RestoreError::Target { path, problem } = error else {
            panic!("expected a target problem");
        };
        assert_eq!(path.to_string(), "projects/copy");
        assert_eq!(problem, TargetProblem::Taken);
        assert!(!world.extract_dir().exists(), "extract/ is gone");
    }

    fn without_listing(chosen: &Chosen) -> Source<'_> {
        Source {
            piece: &chosen.piece,
            version: &chosen.version,
            listing: None,
            compare: false,
        }
    }

    #[test]
    fn without_a_listing_no_room_is_asked_for_and_the_tree_is_not_compared() {
        let world = World::new();
        let chosen = world.chosen();
        let mut spy = world.spy();
        spy.space = Some(0);
        done(world.run_source(&without_listing(&chosen), "projects/copy", &spy));
        assert!(world.root().join("projects/copy/main.rs").is_file());
        assert!(
            spy.entries().contains(&"SpaceChecked".to_string()),
            "{:?}",
            spy.entries()
        );
    }

    #[test]
    fn a_full_volume_without_a_listing_skips_and_says_no_size_is_needed() {
        let world = World::new();
        let chosen = world.chosen();
        let mut spy = world.spy().fault(|kind, _| (kind == "file").then(enospc));
        spy.mode = Mode::PerItem;
        let Skipped::NoSpace { needed, .. } =
            skipped(world.run_source(&without_listing(&chosen), "projects/copy", &spy));
        assert_eq!(needed, None);
        assert!(!world.extract_dir().exists(), "extract/ is gone");
        assert!(!world.root().join("projects").exists());
    }

    /// A listing that names a file the archive does not hold.
    fn wrong_listing(chosen: &Chosen) -> Listing {
        let mut listing = chosen.listing.clone();
        listing.entries[0].path = rel("elsewhere.rs");
        listing
    }

    #[test]
    fn a_damaged_version_is_not_compared_with_its_listing_but_a_sound_one_is() {
        let world = World::new();
        let chosen = world.chosen();
        let listing = wrong_listing(&chosen);
        let source = |compare| Source {
            piece: &chosen.piece,
            version: &chosen.version,
            listing: Some(&listing),
            compare,
        };
        let error = world
            .run_source(&source(true), "projects/copy", &world.spy())
            .expect_err("a sound version must equal its listing");
        assert!(matches!(error, RestoreError::Differs { .. }), "{error:?}");
        done(world.run_source(&source(false), "projects/copy", &world.spy()));
        assert!(world.root().join("projects/copy/main.rs").is_file());
    }

    /// A damaged version without a listing, whose archive holds `main.rs`
    /// and `members` at its top. `pack` refuses a marker's name that is not
    /// a file, so the archive is written by hand.
    fn broken_with_top(members: Vec<RawMember<'_>>) -> (World, Picked) {
        let (world, version) = World::new_with_version();
        let mut all = vec![
            RawMember::dir(b"scanner/"),
            RawMember::file(b"scanner/main.rs", b"fn main() {}"),
        ];
        all.extend(members);
        version.overwrite(VersionFile::Archive, &compress(&raw_tar(&all)));
        version.overwrite(VersionFile::Listing, b"not a zstd stream");
        let collection = Collection::find(world.root()).expect("collection");
        let picked = choose_version_to_fork(
            &collection,
            &world.store.places(),
            world.root(),
            OsStr::new("projects/scanner"),
            None,
            &TimeZone::UTC,
        )
        .expect("a damaged version is picked");
        assert!(picked.listing.is_none() && !picked.damage.is_empty());
        (world, picked)
    }

    fn marker_name_refused(name: &str, members: Vec<RawMember<'_>>) {
        let (world, picked) = broken_with_top(members);
        let error = world
            .run_source(&Source::picked(&picked), "projects/copy", &world.spy())
            .expect_err("the marker name is refused");
        match error {
            RestoreError::MarkerNotAFile { name: found, .. } => assert_eq!(found, name),
            other => panic!("expected MarkerNotAFile, got {other:?}"),
        }
        assert!(!world.extract_dir().exists(), "extract/ is gone");
        assert!(!world.root().join("projects/copy").exists());
    }

    #[test]
    fn an_unchecked_tree_with_a_restored_marker_name_as_a_folder_is_refused() {
        marker_name_refused(
            ".antiq-restored",
            vec![RawMember::dir(b"scanner/.antiq-restored/")],
        );
    }

    #[test]
    fn an_unchecked_tree_with_a_restored_marker_name_as_a_symlink_is_refused() {
        marker_name_refused(
            ".antiq-restored",
            vec![RawMember::link(
                b"scanner/.antiq-restored",
                b'2',
                b"main.rs",
            )],
        );
    }

    #[test]
    fn an_unchecked_tree_with_a_pin_name_as_a_folder_is_refused() {
        marker_name_refused(".antiq-pin", vec![RawMember::dir(b"scanner/.antiq-pin/")]);
    }

    #[test]
    fn an_unchecked_tree_with_a_pin_file_or_symlink_comes_back() {
        for member in [
            RawMember::file(b"scanner/.antiq-pin", b""),
            RawMember::link(b"scanner/.antiq-pin", b'2', b"main.rs"),
        ] {
            let (world, picked) = broken_with_top(vec![member]);
            done(world.run_source(&Source::picked(&picked), "projects/copy", &world.spy()));
            assert!(world.root().join("projects/copy/main.rs").is_file());
        }
    }

    #[test]
    fn a_damaged_version_with_a_listing_still_needs_room_for_its_bytes() {
        let world = World::new();
        let chosen = world.chosen();
        let mut spy = world.spy();
        spy.space = Some(5);
        let source = Source {
            compare: false,
            ..Source::sound(&chosen)
        };
        let Skipped::NoSpace { needed, available } =
            skipped(world.run_source(&source, "projects/copy", &spy));
        assert!(needed.is_some_and(|needed| needed > 5));
        assert_eq!(available, 5);
        assert!(!spy.entries().contains(&"StagingMade".to_string()));
        assert!(!world.root().join("projects").exists());
    }
}

#[cfg(test)]
mod fork_choice_tests {
    use super::*;
    use crate::error::Error;
    use crate::testing::tree::{TempCollection, Tree};
    use crate::testing::versions::{Store, StoredVersion};
    use crate::versions::names::VersionFile;

    const NEW: &str = "2026-11-04T090211";

    struct World {
        collection: TempCollection,
        store: Store,
    }

    impl World {
        fn with(source: &Tree) -> (World, StoredVersion) {
            let mut store = Store::new();
            let version = store.add_version(source, "projects/scanner", NEW);
            let world = World {
                collection: TempCollection::minimal(),
                store,
            };
            (world, version)
        }

        fn scanner() -> (World, StoredVersion) {
            let mut source = Tree::new();
            source.file("projects/scanner/main.rs", "fn main() {}");
            World::with(&source)
        }

        fn unearth_picks(&self) -> Result<Chosen, RestoreError> {
            let collection =
                Collection::find(self.collection.root()).expect("the collection is found");
            choose_version(
                &collection,
                &self.store.places(),
                self.collection.root(),
                OsStr::new("projects/scanner"),
                None,
                &TimeZone::UTC,
            )
        }

        fn picks(&self) -> Result<Picked, RestoreError> {
            let collection =
                Collection::find(self.collection.root()).expect("the collection is found");
            choose_version_to_fork(
                &collection,
                &self.store.places(),
                self.collection.root(),
                OsStr::new("projects/scanner"),
                None,
                &TimeZone::UTC,
            )
        }
    }

    #[test]
    fn a_sound_version_is_picked_with_its_listing_and_no_damage() {
        let (world, _) = World::scanner();
        let picked = world.picks().expect("picked");
        assert!(picked.damage.is_empty(), "{:?}", picked.damage);
        assert_eq!(picked.listing.expect("a listing").entries.len(), 1);
    }

    #[test]
    fn a_damaged_checksum_file_is_picked_with_the_damage_and_the_listing() {
        let (world, version) = World::scanner();
        version.overwrite(VersionFile::Checksum, b"not a checksum line");
        let picked = world.picks().expect("a damaged version is picked");
        assert_eq!(picked.damage.len(), 1, "{:?}", picked.damage);
        assert!(
            picked.damage[0].contains("checksum file"),
            "{:?}",
            picked.damage
        );
        assert!(picked.listing.is_some());
    }

    #[test]
    fn a_missing_listing_is_picked_without_a_listing() {
        let (world, version) = World::scanner();
        version.remove(VersionFile::Listing);
        let picked = world.picks().expect("a damaged version is picked");
        assert!(picked.listing.is_none());
        assert!(
            picked
                .damage
                .iter()
                .any(|d| d.contains("listing is missing")),
            "{:?}",
            picked.damage
        );
    }

    #[test]
    fn a_listing_that_does_not_read_to_its_end_is_picked_without_a_listing() {
        let (world, version) = World::scanner();
        version.cut_listing();
        let picked = world.picks().expect("a damaged version is picked");
        assert!(picked.listing.is_none());
        assert!(
            picked
                .damage
                .iter()
                .any(|d| d.contains("listing is damaged")),
            "{:?}",
            picked.damage
        );
    }

    #[test]
    fn a_newer_listing_next_to_other_damage_is_still_refused_as_too_new() {
        let (world, version) = World::scanner();
        version.edit_listing(|text| text.replacen("# antiq listing 1", "# antiq listing 2", 1));
        version.remove(VersionFile::Checksum);
        let error = world.picks().expect_err("a newer format is refused");
        assert!(
            matches!(error, RestoreError::FormatTooNew { .. }),
            "{error:?}"
        );
        assert_eq!(
            Error::from(crate::commands::unearth::UnearthError::from(error)).kind(),
            ErrorKind::FormatTooNew
        );
    }

    #[test]
    fn unearth_refuses_a_newer_listing_next_to_other_damage_as_too_new() {
        let (world, version) = World::scanner();
        version.edit_listing(|text| text.replacen("# antiq listing 1", "# antiq listing 2", 1));
        version.remove(VersionFile::Checksum);
        let error = world
            .unearth_picks()
            .expect_err("a newer format is refused");
        assert!(
            matches!(error, RestoreError::FormatTooNew { .. }),
            "{error:?}"
        );
        assert_eq!(
            Error::from(crate::commands::unearth::UnearthError::from(error)).kind(),
            ErrorKind::FormatTooNew
        );
    }

    #[test]
    fn unearth_refuses_a_doubled_slot_naming_the_files_and_forget_not_fork() {
        let (world, version) = World::scanner();
        let twin = version.add_twin(VersionFile::Archive, "other");
        let error = world
            .unearth_picks()
            .expect_err("a doubled slot is refused");
        assert!(matches!(error, RestoreError::Doubled { .. }), "{error:?}");
        let text = error.to_string();
        let twin = twin
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        assert!(text.contains(&twin) && text.contains("forget"), "{text}");
        assert!(!text.contains("fork"), "{text}");
    }

    #[test]
    fn a_doubled_slot_is_refused_naming_the_files_and_not_suggesting_fork() {
        let (world, version) = World::scanner();
        let twin = version.add_twin(VersionFile::Archive, "other");
        let error = world.picks().expect_err("a doubled slot is refused");
        assert!(matches!(error, RestoreError::Doubled { .. }), "{error:?}");
        let text = error.to_string();
        let twin = twin
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        assert!(text.contains(&twin), "{text}");
        assert!(
            text.contains("by hand") && text.contains("`forget`"),
            "{text}"
        );
        assert!(!text.contains("`fork`"), "{text}");
    }

    #[test]
    fn a_version_without_an_archive_is_refused() {
        let (world, version) = World::scanner();
        version.remove(VersionFile::Archive);
        let error = world.picks().expect_err("an incomplete version is refused");
        assert!(
            matches!(
                error,
                RestoreError::Reference(ReferenceError::OnlyIncomplete { .. })
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_marker_name_that_is_a_folder_is_refused_when_the_listing_shows_it() {
        let mut source = Tree::new();
        source.file("projects/scanner/src/main.rs", "fn main() {}");
        let (world, version) = World::with(&source);
        // The head's counts stay as they are: directories count for neither.
        version.edit_listing(|text| text.replacen("  src/", "  .antiq-pin/", 1));
        version.overwrite(VersionFile::Checksum, b"not a checksum line");
        let error = world.picks().expect_err("the marker name is refused");
        assert!(
            matches!(error, RestoreError::MarkerNotAFile { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_plain_choice_still_refuses_a_damaged_version() {
        let (world, version) = World::scanner();
        version.overwrite(VersionFile::Checksum, b"not a checksum line");
        let collection = Collection::find(world.collection.root()).expect("the collection");
        let error = choose_version(
            &collection,
            &world.store.places(),
            world.collection.root(),
            OsStr::new("projects/scanner"),
            None,
            &TimeZone::UTC,
        )
        .expect_err("refused");
        assert!(matches!(error, RestoreError::Damaged { .. }), "{error:?}");
    }

    fn unreadable_listing(
        world: &World,
        version: &StoredVersion,
        must_read: bool,
    ) -> Result<Option<Listing>, RestoreError> {
        version.overwrite(VersionFile::Listing, b"not a zstd stream");
        read_listing(
            &world.store.places(),
            &version.piece,
            OsStr::new(version.names.of(VersionFile::Listing)),
            must_read,
            "projects/scanner",
            NEW,
        )
    }

    #[test]
    fn a_listing_that_does_not_read_is_an_error_when_it_must() {
        let (world, version) = World::scanner();
        let error = unreadable_listing(&world, &version, true).expect_err("must read");
        assert!(matches!(error, RestoreError::Listing(_)), "{error:?}");
    }

    #[test]
    fn a_listing_that_does_not_read_is_none_when_it_need_not() {
        let (world, version) = World::scanner();
        let listing = unreadable_listing(&world, &version, false).expect("goes on");
        assert!(listing.is_none());
    }

    #[test]
    fn a_listing_must_read_for_the_plain_choice_and_for_a_version_without_damage() {
        assert!(must_read_listing(Policy::Sound, &["damage".to_string()]));
        assert!(must_read_listing(Policy::Damaged, &[]));
        assert!(!must_read_listing(Policy::Damaged, &["damage".to_string()]));
    }
}
