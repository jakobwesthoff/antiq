// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `remote add`, `remote show` and `remote remove` commands.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::config::{ConfigDoc, ConfigError, RemoteConfig};
use crate::error::{ErrorKind, HasKind};
use crate::fsx::FsxError;
use crate::fsx::atomic::{flush_dir, write_atomic};
use crate::fsx::tree::TreeRoot;
use crate::id::{IdSource, ObjectId};
use crate::lock::RunLock;
use crate::machine::MachineError;
use crate::plan::{Plan, PlanAction};
use crate::reference::Candidate;
use crate::relpath::machine_part;
use crate::remote::marker::{self, MarkerError, write_marker};
use crate::remote::path::{PathRemote, RootState, Seams, assess, inspect, probe_exclusive};
use crate::remote::rsync::Rsync;
use crate::remote::ssh::{RootFound, SshRemote, SshSeams, SshTarget, Unproven};
use crate::remote::url::{RemoteUrl, UrlError};
use crate::remote::{KnownVersions, Remote, RemoteError, RemoteState};
use crate::report::{CommandReport, Warning};
use crate::run::{RunGuard, Started};
use crate::store::{Store, StoreError};
use crate::tracking::{self, PieceState, TrackingError};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Errors of the `remote` commands.
#[derive(Debug, thiserror::Error)]
pub enum RemoteOpError {
    #[error("a remote is attached already ({url}); version 1 accepts one")]
    AlreadyAttached { url: String },
    #[error(transparent)]
    Url(#[from] UrlError),
    #[error("--host-key-alias applies to ssh remotes only")]
    HostKeyAliasNeedsSsh,
    #[error(
        "{} already holds {}; attach it with `remote add` without --init",
        root.display(),
        marker::FILE_NAME
    )]
    AlreadyARemote { root: PathBuf },
    #[error(
        "{} holds `{name}`, a name antiq uses in a remote; `remote add --init` does not take over a directory that has it",
        root.display()
    )]
    LayoutNameTaken { root: PathBuf, name: String },
    #[error(
        "{} does not exist, so `remote add --init` cannot create {}",
        parent.display(),
        root.display()
    )]
    ParentMissing { root: PathBuf, parent: PathBuf },
    #[error("exclusive creation does not work in {}: {reason}", root.display())]
    ExclusiveCreate { root: PathBuf, reason: String },
    #[error("hard links do not work in {}: {reason}", root.display())]
    HardLink { root: PathBuf, reason: String },
    #[error(
        "the remote was initialised at {} but the config could not be written: {source}; `remote add {url}` without --init attaches it",
        root.display()
    )]
    ConfigWrite {
        root: PathBuf,
        url: String,
        source: FsxError,
    },
    #[error("the remote is attached, but copying its metadata failed: {message}; `sync` copies it")]
    MetadataCopy { message: String },
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: io::Error,
    },
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Fsx(#[from] FsxError),
    #[error(transparent)]
    Marker(#[from] MarkerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error(transparent)]
    Tracking(#[from] TrackingError),
}

impl HasKind for RemoteOpError {
    fn kind(&self) -> ErrorKind {
        match self {
            RemoteOpError::Remote(error) => error.kind(),
            RemoteOpError::Store(error) => error.kind(),
            _ => ErrorKind::Other,
        }
    }
}

/// What a `remote` command does or would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RemoteAction {
    CreateRemote { url: String },
    AttachRemote { url: String },
    CopyMetadata { objects: usize },
    DetachRemote { url: String },
}

impl PlanAction for RemoteAction {
    fn describe(&self) -> String {
        match self {
            RemoteAction::CreateRemote { url } => format!("create the remote at {url}"),
            RemoteAction::AttachRemote { url } => format!("attach the remote at {url}"),
            RemoteAction::CopyMetadata { objects: 1 } => "copy the metadata of 1 object".into(),
            RemoteAction::CopyMetadata { objects } => {
                format!("copy the metadata of {objects} objects")
            }
            RemoteAction::DetachRemote { url } => format!("detach the remote at {url}"),
        }
    }
}

/// What `remote add` is asked to do.
#[derive(Debug)]
pub struct AddRequest<'a> {
    pub url: &'a str,
    pub init: bool,
    pub host_key_alias: Option<&'a str>,
}

/// What a run of `remote add` did or would do.
#[derive(Debug, Serialize)]
pub struct AddReport {
    pub kind: &'static str,
    pub url: String,
    /// The remote's ULID; a dry run of `--init` has none yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub attached: bool,
    /// Whether the run made the remote's marker.
    pub created: bool,
    /// How many objects the metadata copy brought in; absent in a dry run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pulled: Option<usize>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    pub actions: Vec<RemoteAction>,
    pub warnings: Vec<Warning>,
}

impl AddReport {
    /// The lines of a console for a dry run.
    pub fn plan_lines(&self) -> Vec<String> {
        plan_lines(&self.actions)
    }
}

impl CommandReport for AddReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// What `remote show` found.
#[derive(Debug, Serialize)]
pub struct ShowReport {
    pub attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<RemoteState>,
    /// What is wrong, for a remote that is away or broken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What was probed on the remote, by name. A path remote has nothing
    /// to probe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probed: Option<serde_json::Map<String, serde_json::Value>>,
    pub warnings: Vec<Warning>,
}

impl CommandReport for ShowReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// Looks at the attached remote and changes nothing, locally or on the
/// remote.
pub fn show(collection: &Collection) -> Result<ShowReport, RemoteOpError> {
    show_with(collection, &Seams::default())
}

pub(crate) fn show_with(
    collection: &Collection,
    seams: &Seams,
) -> Result<ShowReport, RemoteOpError> {
    let Some(config) = collection.config().remotes.first() else {
        return Ok(ShowReport {
            attached: false,
            kind: None,
            url: None,
            id: None,
            state: None,
            reason: None,
            probed: None,
            warnings: Vec::new(),
        });
    };
    let root = match RemoteUrl::parse(&config.url)? {
        RemoteUrl::Path { root } => root,
        RemoteUrl::Ssh {
            user,
            host,
            port,
            root,
        } => {
            let target = SshTarget {
                user,
                host,
                port,
                root,
            };
            return show_ssh(collection, config, &target, SshSeams::default());
        }
    };
    let state = inspect(&root, seams);
    let assessment = assess(&root, &state, config.id);
    // The mount test only means something for a root that is there.
    let mut warnings = Vec::new();
    if matches!(state, RootState::Marker(_) | RootState::NoMarker { .. }) {
        warnings.extend(mount_warning(&root, seams));
    }
    Ok(ShowReport {
        attached: true,
        kind: Some("path"),
        url: Some(config.url.clone()),
        id: Some(config.id),
        state: Some(assessment.reachability.into()),
        reason: Some(assessment.reason).filter(|reason| !reason.is_empty()),
        probed: Some(serde_json::Map::new()),
        warnings,
    })
}

/// `remote show` for an ssh remote: one read-only script over a connection
/// of its own, since a master would be started for a single call.
pub(crate) fn show_ssh(
    collection: &Collection,
    config: &RemoteConfig,
    target: &SshTarget,
    seams: SshSeams,
) -> Result<ShowReport, RemoteOpError> {
    let remote = SshRemote::connect_with(target, config, &collection.config().machine, seams)?;
    let (assessment, probed) = remote.show()?;
    Ok(ShowReport {
        attached: true,
        kind: Some("ssh"),
        url: Some(config.url.clone()),
        id: Some(config.id),
        state: Some(assessment.reachability.into()),
        reason: Some(assessment.reason).filter(|reason| !reason.is_empty()),
        probed: Some(probed),
        warnings: remote.take_warnings(),
    })
}

/// What `remote remove` did or would do.
#[derive(Debug, Serialize)]
pub struct RemoveReport {
    pub detached: bool,
    pub url: String,
    /// The pieces whose archive only the remote holds, which cannot be
    /// reached without it.
    pub remote_only: Vec<Candidate>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    pub actions: Vec<RemoteAction>,
    pub warnings: Vec<Warning>,
}

impl RemoveReport {
    /// The lines of a console for a dry run.
    pub fn plan_lines(&self) -> Vec<String> {
        plan_lines(&self.actions)
    }
}

fn plan_lines(actions: &[RemoteAction]) -> Vec<String> {
    let mut plan = Plan::new();
    for action in actions {
        plan.push(action.clone());
    }
    plan.lines()
}

impl CommandReport for RemoveReport {
    fn partial(&self) -> bool {
        false
    }

    fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// Detaches the remote: the `[[remote]]` entry goes, nothing on the remote
/// is touched and the local metadata stays.
pub fn remove(collection: &Collection, started: &Started) -> Result<RemoveReport, RemoteOpError> {
    let config = collection
        .config()
        .remotes
        .first()
        .ok_or(RemoteError::NoRemote)?;
    let url = config.url.clone();
    let remote_only = remote_only_pieces(collection)?;
    let actions = vec![RemoteAction::DetachRemote { url: url.clone() }];

    let Started::Guarded(guard) = started else {
        return Ok(RemoveReport {
            detached: false,
            url,
            remote_only,
            dry_run: true,
            actions,
            warnings: Vec::new(),
        });
    };
    let mut doc = ConfigDoc::read(&collection.config_path())?;
    doc.remove_remote();
    write_atomic(&collection.config_path(), doc.render()?.as_bytes())?;
    Ok(RemoveReport {
        detached: true,
        url,
        remote_only,
        dry_run: false,
        actions,
        warnings: guard.warnings().to_vec(),
    })
}

/// The pieces this machine has no archive of, which only the remote holds.
/// An object whose metadata cannot be read is left out: `status` lists it
/// as damaged.
fn remote_only_pieces(collection: &Collection) -> Result<Vec<Candidate>, RemoteOpError> {
    let store = Store::new(collection);
    let tracked = tracking::read_all(collection)?;
    let root = TreeRoot::open(collection.root()).map_err(|source| RemoteOpError::Io {
        action: "open",
        path: collection.root().display().to_string(),
        source,
    })?;
    let mut pieces = Vec::new();
    for object in store.objects()? {
        let Ok(Some(current)) = store.current(object) else {
            continue;
        };
        let entry = tracked.entries.iter().find(|entry| entry.id == object);
        let folder_present = entry.is_some_and(|entry| root.lstat(&entry.path).is_ok());
        let state = tracking::derive_state(
            entry,
            Some(&current.metadata),
            folder_present,
            collection.id(),
        );
        if matches!(state, Ok(PieceState::RemoteOnly)) {
            pieces.push(Candidate {
                id: object.to_string(),
                path: current.metadata.path.to_string(),
            });
        }
    }
    Ok(pieces)
}

/// Attaches the remote at `request.url`, or creates it with `--init`.
pub fn add(
    collection: &Collection,
    request: &AddRequest<'_>,
    started: &Started,
    clock: &dyn Clock,
    ids: &dyn IdSource,
) -> Result<AddReport, RemoteOpError> {
    add_with(collection, request, started, clock, ids, &Seams::default())
}

pub(crate) fn add_with(
    collection: &Collection,
    request: &AddRequest<'_>,
    started: &Started,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    seams: &Seams,
) -> Result<AddReport, RemoteOpError> {
    // =====================================================
    // Every refusal, before anything is written
    // =====================================================

    let root = match RemoteUrl::parse(request.url)? {
        RemoteUrl::Path { root } => root,
        RemoteUrl::Ssh {
            user,
            host,
            port,
            root,
        } => {
            let target = SshTarget {
                user,
                host,
                port,
                root,
            };
            return add_ssh(
                collection,
                request,
                started,
                clock,
                ids,
                &target,
                SshSeams::default(),
            );
        }
    };
    if request.host_key_alias.is_some() {
        return Err(RemoteOpError::HostKeyAliasNeedsSsh);
    }
    let machine = collection.machine_name()?;
    let mut doc = ConfigDoc::read(&collection.config_path())?;
    refuse_when_attached(collection, &doc)?;

    let state = inspect(&root, seams);
    let attached_id = check_root(&root, &state, request.init, collection.id())?;
    let remote_id = attached_id.unwrap_or_else(|| ids.mint(clock.now()));
    let mut warnings = Vec::new();
    if !matches!(state, RootState::Missing) {
        warnings.extend(mount_warning(&root, seams));
    }
    let url = request.url.to_string();

    let guard = match started {
        Started::Guarded(guard) => guard,
        Started::DryRun { .. } => {
            let remote = attached_id
                .filter(|_| !request.init)
                .map(|id| PathRemote::with_seams(root.clone(), id, &machine, seams.clone()));
            return dry_run(
                "path",
                &url,
                attached_id,
                remote.as_ref().map(|remote| remote as &dyn Remote),
                warnings,
            );
        }
    };
    warnings.splice(0..0, guard.warnings().iter().cloned());

    doc.add_remote(&RemoteConfig {
        url: url.clone(),
        id: remote_id,
        host_key_alias: None,
    })?;
    let config_text = doc.render()?;

    // =====================================================
    // The writes
    // =====================================================

    let made_root = request.init && matches!(state, RootState::Missing);
    let prepared = prepare_root(&root, made_root, &machine, request.init, remote_id, seams);
    if let Err(error) = prepared {
        // A root made by this run goes again; one that has anything in it
        // by now (a marker whose last flush failed) stays.
        if made_root {
            let _ = fs::remove_dir(&root);
        }
        return Err(error);
    }
    if made_root {
        warnings.extend(mount_warning(&root, seams));
    }

    let remote = PathRemote::with_seams(root.clone(), remote_id, &machine, seams.clone());
    attach(
        collection,
        guard,
        &Attachment {
            kind: "path",
            url,
            init: request.init,
            root,
            config_text: &config_text,
            remote: &remote,
            remote_id,
        },
        warnings,
    )
}

/// The config decides whether a remote is attached, in the spelling it
/// holds one in, so the refusal asks the editor that would add the entry
/// and sees what it would see.
fn refuse_when_attached(collection: &Collection, doc: &ConfigDoc) -> Result<(), RemoteOpError> {
    let placeholder = RemoteConfig {
        url: String::new(),
        id: collection.id(),
        host_key_alias: None,
    };
    if doc.clone().add_remote(&placeholder).is_ok() {
        return Ok(());
    }
    let url = collection
        .config()
        .remotes
        .first()
        .map_or_else(|| "an entry of the config".to_string(), |r| r.url.clone());
    Err(RemoteOpError::AlreadyAttached { url })
}

/// What `attach` records once the remote's root is ready.
struct Attachment<'a> {
    kind: &'static str,
    url: String,
    init: bool,
    /// The root, for the error of a config that cannot be written.
    root: PathBuf,
    config_text: &'a str,
    remote: &'a dyn Remote,
    remote_id: ObjectId,
}

/// Writes the `[[remote]]` entry, which is what attaches the remote, and
/// copies the metadata. A failure after the entry leaves the remote
/// attached and the copy to the next `sync`.
fn attach(
    collection: &Collection,
    guard: &RunGuard,
    attachment: &Attachment<'_>,
    mut warnings: Vec<Warning>,
) -> Result<AddReport, RemoteOpError> {
    let Attachment {
        kind,
        url,
        init,
        root,
        config_text,
        remote,
        remote_id,
    } = attachment;
    if let Err(source) = write_atomic(&collection.config_path(), config_text.as_bytes()) {
        return Err(if *init {
            RemoteOpError::ConfigWrite {
                root: root.clone(),
                url: url.clone(),
                source,
            }
        } else {
            source.into()
        });
    }

    let pulled =
        copy_metadata(*remote, &Store::new(collection), guard.lock()).map_err(|error| {
            RemoteOpError::MetadataCopy {
                message: error.to_string(),
            }
        })?;
    warnings.extend(remote.take_warnings());

    let action = if *init {
        RemoteAction::CreateRemote { url: url.clone() }
    } else {
        RemoteAction::AttachRemote { url: url.clone() }
    };
    let mut actions = vec![action];
    if !init {
        actions.push(RemoteAction::CopyMetadata { objects: pulled });
    }
    Ok(AddReport {
        kind,
        url: url.clone(),
        id: Some(*remote_id),
        attached: true,
        created: *init,
        pulled: Some(pulled),
        dry_run: false,
        actions,
        warnings,
    })
}

// =========================================================
// remote add over ssh
// =========================================================

/// `remote add` for an ssh remote. The probes look at the NAS in one
/// read-only script, so a refusal and a dry run write nothing there.
pub(crate) fn add_ssh(
    collection: &Collection,
    request: &AddRequest<'_>,
    started: &Started,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    target: &SshTarget,
    seams: SshSeams,
) -> Result<AddReport, RemoteOpError> {
    let machine_name = collection.machine_name()?;
    let mut doc = ConfigDoc::read(&collection.config_path())?;
    refuse_when_attached(collection, &doc)?;
    let machine = &collection.config().machine;
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    let rsync = Rsync::resolve(machine.rsync.as_deref(), &path_var).map_err(RemoteError::from)?;

    let host_key_alias = request.host_key_alias.map(String::from);
    let config = RemoteConfig {
        url: request.url.to_string(),
        // The ULID of the remote is known once the probe has read the
        // marker; the session does not need it.
        id: collection.id(),
        host_key_alias,
    };
    let remote = SshRemote::connect_with(target, &config, machine, seams)?;
    remote.begin_connection()?;
    let probe = remote.probe()?;
    let attached_id = check_ssh_root(&remote, &probe, target, request.init)?;
    remote.tools_of(&probe)?;
    let remote_id = attached_id.unwrap_or_else(|| ids.mint(clock.now()));
    let remote = remote.with_id(remote_id);
    let url = request.url.to_string();

    let guard = match started {
        Started::Guarded(guard) => guard,
        Started::DryRun { .. } => {
            let existing = attached_id.filter(|_| !request.init).map(|_| &remote);
            let mut report = dry_run(
                "ssh",
                &url,
                attached_id,
                existing.map(|remote| remote as &dyn Remote),
                Vec::new(),
            )?;
            report.warnings.extend(remote.take_warnings());
            return Ok(report);
        }
    };

    doc.add_remote(&RemoteConfig {
        id: remote_id,
        ..config
    })?;
    doc.set_machine_rsync(&rsync.path().to_string_lossy());
    let config_text = doc.render()?;

    let root = PathBuf::from(&target.root);
    let made_root = request.init && matches!(probe.root, RootFound::Missing { .. });
    if let Err(error) = prepare_ssh_root(&remote, &root, made_root, &machine_name, request.init) {
        if made_root {
            remote.remove_made_root();
        }
        return Err(error);
    }

    let warnings = guard.warnings().to_vec();
    attach(
        collection,
        guard,
        &Attachment {
            kind: "ssh",
            url,
            init: request.init,
            root,
            config_text: &config_text,
            remote: &remote,
            remote_id,
        },
        warnings,
    )
}

/// Decides what the probe says about the root, as `check_root` does for a
/// path remote. Returns the ULID the marker names.
fn check_ssh_root(
    remote: &SshRemote,
    probe: &crate::remote::ssh::Probe,
    target: &SshTarget,
    init: bool,
) -> Result<Option<ObjectId>, RemoteOpError> {
    let root = PathBuf::from(&target.root);
    if let (RootFound::Missing { parent_exists }, true) = (&probe.root, init) {
        // Only the last component is made, as on a path remote.
        if !parent_exists {
            return Err(RemoteOpError::ParentMissing {
                parent: root
                    .parent()
                    .expect("a validated root has a parent")
                    .to_path_buf(),
                root,
            });
        }
        return Ok(None);
    }
    match (remote.marker_in(probe)?, init) {
        (Some(found), false) => Ok(Some(found.id)),
        (Some(_), true) => Err(RemoteOpError::AlreadyARemote { root }),
        (None, false) => Err(RemoteError::Broken(format!(
            "{} holds no {}; use `remote add --init` to create a remote there",
            target.root,
            marker::FILE_NAME
        ))
        .into()),
        (None, true) => match &probe.layout_name {
            Some(name) => Err(RemoteOpError::LayoutNameTaken {
                root,
                name: name.clone(),
            }),
            None => Ok(None),
        },
    }
}

/// Makes the root if this run is to, proves that exclusive creation and
/// hard links work in it, and for `--init` writes the marker.
fn prepare_ssh_root(
    remote: &SshRemote,
    root: &Path,
    make_root: bool,
    machine: &str,
    init: bool,
) -> Result<(), RemoteOpError> {
    if make_root {
        remote.make_root()?;
    }
    let probe_name = format!(
        ".antiq-probe.{}.{}.{}",
        machine_part(machine),
        std::process::id(),
        ulid::Ulid::from_datetime(std::time::SystemTime::now())
            .to_string()
            .to_lowercase()
    );
    let proven = remote.prove_creation(&probe_name).and_then(|()| {
        if init {
            remote.write_marker(&format!(
                ".{}.{}.tmp",
                marker::FILE_NAME,
                std::process::id()
            ))
        } else {
            Ok(())
        }
    });
    proven.map_err(|failed| match failed {
        Unproven::ExclusiveCreate(reason) => RemoteOpError::ExclusiveCreate {
            root: root.to_path_buf(),
            reason,
        },
        Unproven::HardLink(reason) => RemoteOpError::HardLink {
            root: root.to_path_buf(),
            reason,
        },
        Unproven::Remote(error) => error.into(),
    })
}

/// Looks at what `state` says about the root and decides whether `remote
/// add` can go on. Returns the remote's ULID when the marker names one.
fn check_root(
    root: &Path,
    state: &RootState,
    init: bool,
    own: ObjectId,
) -> Result<Option<ObjectId>, RemoteOpError> {
    let refuse = |state: &RootState| -> RemoteOpError {
        // `own` only stands in for the ULID a marker is compared with,
        // which none of the states handed here has.
        assess(root, state, own)
            .into_result()
            .expect_err("a root without a usable marker is not reachable")
            .into()
    };
    match (init, state) {
        (false, RootState::Marker(found)) => Ok(Some(found.id)),
        (false, RootState::NoMarker { mount_point: true }) => {
            let reason = assess(root, state, own).reason;
            Err(RemoteError::Broken(format!(
                "{reason}; use `remote add --init` to create a remote there"
            ))
            .into())
        }
        (false, state) => Err(refuse(state)),
        (true, RootState::Marker(_)) => Err(RemoteOpError::AlreadyARemote {
            root: root.to_path_buf(),
        }),
        (true, RootState::ServerGone(_) | RootState::Unusable(_)) => Err(refuse(state)),
        (true, RootState::Missing) => {
            // Only the last component is made: a parent that is missing
            // may be the mount point of a share that is not mounted.
            let parent = root.parent().expect("a validated root has a parent");
            if !parent.is_dir() {
                return Err(RemoteOpError::ParentMissing {
                    root: root.to_path_buf(),
                    parent: parent.to_path_buf(),
                });
            }
            Ok(None)
        }
        (true, RootState::NoMarker { .. }) => {
            refuse_layout_names(root)?;
            Ok(None)
        }
    }
}

/// A directory that holds a name antiq writes in a remote is not taken
/// over by `--init`: it may be somebody's `objects/`.
fn refuse_layout_names(root: &Path) -> Result<(), RemoteOpError> {
    let io_error = |source| RemoteOpError::Io {
        action: "list",
        path: root.display().to_string(),
        source,
    };
    for entry in fs::read_dir(root).map_err(io_error)? {
        let name = entry
            .map_err(io_error)?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name == "objects" || name.starts_with(".antiq-clock.") {
            return Err(RemoteOpError::LayoutNameTaken {
                root: root.to_path_buf(),
                name,
            });
        }
    }
    Ok(())
}

fn mount_warning(root: &Path, seams: &Seams) -> Option<Warning> {
    match (seams.mount)(root) {
        Ok(true) => None,
        Ok(false) => Some(Warning::new(format!(
            "{} is not a mount point",
            root.display()
        ))),
        Err(error) => Some(Warning::new(format!(
            "cannot tell whether {} is a mount point: {error}",
            root.display()
        ))),
    }
}

fn dry_run(
    kind: &'static str,
    url: &str,
    attached_id: Option<ObjectId>,
    existing: Option<&dyn Remote>,
    warnings: Vec<Warning>,
) -> Result<AddReport, RemoteOpError> {
    let actions = match existing {
        Some(remote) => {
            let objects = remote.list_objects()?.len();
            vec![
                RemoteAction::AttachRemote { url: url.into() },
                RemoteAction::CopyMetadata { objects },
            ]
        }
        None => vec![RemoteAction::CreateRemote { url: url.into() }],
    };
    Ok(AddReport {
        kind,
        url: url.into(),
        id: attached_id,
        attached: false,
        created: false,
        pulled: None,
        dry_run: true,
        actions,
        warnings,
    })
}

/// Makes the root if this run is to, proves that exclusive creation works
/// in it, and for `--init` writes the marker.
fn prepare_root(
    root: &Path,
    make_root: bool,
    machine: &str,
    init: bool,
    id: ObjectId,
    seams: &Seams,
) -> Result<(), RemoteOpError> {
    if make_root {
        fs::create_dir(root).map_err(|source| RemoteOpError::Io {
            action: "create",
            path: root.display().to_string(),
            source,
        })?;
        let parent = root.parent().expect("a validated root has a parent");
        flush_dir(parent)?;
    }
    probe_exclusive(root, &machine_part(machine), seams).map_err(|failed| {
        RemoteOpError::ExclusiveCreate {
            root: root.to_path_buf(),
            reason: failed.0,
        }
    })?;
    if init {
        write_marker(root, id, &*seams.durable)?;
    }
    Ok(())
}

/// Copies the metadata versions the remote has and this machine lacks into
/// the local store, byte for byte, and returns how many objects they
/// belong to.
fn copy_metadata(
    remote: &dyn Remote,
    store: &Store<'_>,
    lock: &RunLock,
) -> Result<usize, RemoteOpError> {
    let mut known = KnownVersions::new();
    for id in store.objects()? {
        if let Some(highest) = store.committed_versions(id)?.last() {
            known.insert(id, *highest);
        }
    }
    let files = remote.pull_metadata(&known)?;
    let mut objects = BTreeSet::new();
    for file in files {
        store.store_committed(lock, file.id, file.number, &file.bytes)?;
        objects.insert(file.id);
    }
    Ok(objects.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::id::SeqIds;
    use crate::remote::marker::Marker;
    use crate::run;
    use crate::testing::rerun_while_lock_unattributed;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    const REMOTE_ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";
    const OTHER_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    /// A collection on disk with a named machine, and a directory in which
    /// the remotes of the test are made.
    struct World {
        collection_dir: TempCollection,
        remotes: tempfile::TempDir,
    }

    fn world() -> World {
        let mut collection_dir = TempCollection::minimal();
        collection_dir.tree().file(
            ".antiq/config.toml",
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n\n[machine]\nname = \"laptop\"\n"),
        );
        World {
            collection_dir,
            remotes: tempfile::tempdir().expect("temp dir"),
        }
    }

    impl World {
        fn collection(&self) -> Collection {
            Collection::find(self.collection_dir.root()).expect("the collection is found")
        }

        fn config_text(&self) -> String {
            fs::read_to_string(self.collection().config_path()).expect("the config is read")
        }

        fn root(&self, name: &str) -> PathBuf {
            self.remotes.path().join(name)
        }

        /// A remote root with a marker for `REMOTE_ID`.
        fn remote(&self, name: &str) -> PathBuf {
            let root = self.root(name);
            fs::create_dir_all(&root).expect("dirs");
            fs::write(
                root.join(marker::FILE_NAME),
                Marker::new(id(REMOTE_ID)).to_bytes(),
            )
            .expect("the marker is written");
            root
        }

        fn run(
            &self,
            url: &Path,
            init: bool,
            dry_run: bool,
            seams: &Seams,
        ) -> Result<AddReport, RemoteOpError> {
            self.run_text(url.to_str().expect("UTF-8"), init, None, dry_run, seams)
        }

        fn run_text(
            &self,
            url: &str,
            init: bool,
            host_key_alias: Option<&str>,
            dry_run: bool,
            seams: &Seams,
        ) -> Result<AddReport, RemoteOpError> {
            let collection = self.collection();
            let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
            let started =
                rerun_while_lock_unattributed(|| run::start(&collection, &clock, dry_run))
                    .expect("the run starts");
            let request = AddRequest {
                url,
                init,
                host_key_alias,
            };
            add_with(
                &collection,
                &request,
                &started,
                &clock,
                &SeqIds::new(1),
                seams,
            )
        }

        fn add(&self, url: &Path) -> Result<AddReport, RemoteOpError> {
            self.run(url, false, false, &no_mount_point())
        }

        fn init(&self, url: &Path) -> Result<AddReport, RemoteOpError> {
            self.run(url, true, false, &no_mount_point())
        }
    }

    fn no_mount_point() -> Seams {
        Seams {
            mount: Arc::new(|_| Ok(false)),
            ..Seams::default()
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .expect("the directory is listed")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    // =====================================================
    // --init
    // =====================================================

    #[test]
    fn init_writes_the_marker_and_the_config_entry() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        let report = world.init(&root).expect("the remote is created");
        let found = fs::read(root.join(marker::FILE_NAME)).expect("the marker exists");
        let marker = Marker::parse(&found).expect("a marker");
        assert_eq!(report.id, Some(marker.id));
        assert!(report.created && report.attached);
        assert_eq!(report.kind, "path");
        let config = world.collection().config().clone();
        assert_eq!(config.remotes.len(), 1);
        assert_eq!(config.remotes[0].url, root.to_str().expect("UTF-8"));
        assert_eq!(config.remotes[0].id, marker.id);
        assert_eq!(report.pulled, Some(0));
        assert_eq!(
            report.actions,
            [RemoteAction::CreateRemote {
                url: root.to_str().expect("UTF-8").into()
            }]
        );
    }

    #[test]
    fn init_makes_the_last_component_of_a_missing_root() {
        let world = world();
        let root = world.root("new-remote");
        world.init(&root).expect("the remote is created");
        assert!(root.join(marker::FILE_NAME).is_file());
    }

    #[test]
    fn init_does_not_make_a_missing_parent() {
        let world = world();
        let root = world.root("absent").join("antiq");
        let before = world.config_text();
        let error = world.init(&root).expect_err("no parent");
        assert!(
            matches!(error, RemoteOpError::ParentMissing { .. }),
            "{error}"
        );
        assert!(!world.root("absent").exists());
        assert_eq!(world.config_text(), before);
    }

    #[test]
    fn init_refuses_a_root_that_holds_a_marker_and_leaves_it() {
        let world = world();
        let root = world.remote("antiq");
        let before = fs::read(root.join(marker::FILE_NAME)).expect("exists");
        let error = world.init(&root).expect_err("already a remote");
        assert!(
            matches!(error, RemoteOpError::AlreadyARemote { .. }),
            "{error}"
        );
        assert_eq!(
            fs::read(root.join(marker::FILE_NAME)).expect("exists"),
            before
        );
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn init_refuses_a_root_that_holds_an_objects_directory() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir_all(root.join("objects")).expect("dirs");
        let error = world.init(&root).expect_err("a stranger's directory");
        assert!(
            matches!(&error, RemoteOpError::LayoutNameTaken { name, .. } if name == "objects"),
            "{error}"
        );
        assert_eq!(names(&root), ["objects"]);
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn init_refuses_a_root_that_holds_a_clock_file() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        fs::write(root.join(".antiq-clock.desktop"), "").expect("write");
        let error = world.init(&root).expect_err("a clock file is there");
        assert!(
            matches!(&error, RemoteOpError::LayoutNameTaken { name, .. } if name == ".antiq-clock.desktop"),
            "{error}"
        );
    }

    #[test]
    fn init_accepts_a_directory_with_other_things_and_a_stale_probe_file() {
        let world = world();
        let root = world.root("volume");
        fs::create_dir(&root).expect("dir");
        fs::create_dir(root.join(".Spotlight-V100")).expect("dir");
        fs::write(root.join(".antiq-probe.desktop.1.abc"), "").expect("write");
        world.init(&root).expect("the remote is created");
        assert_eq!(
            names(&root),
            [
                ".Spotlight-V100",
                ".antiq-probe.desktop.1.abc",
                marker::FILE_NAME
            ]
        );
    }

    #[test]
    fn init_refuses_a_marker_that_cannot_be_read() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        fs::write(root.join(marker::FILE_NAME), "not json").expect("write");
        let error = world.init(&root).expect_err("a broken marker");
        assert!(
            matches!(error, RemoteOpError::Remote(RemoteError::Broken(_))),
            "{error}"
        );
        assert_eq!(
            fs::read(root.join(marker::FILE_NAME)).expect("exists"),
            b"not json"
        );
    }

    #[test]
    fn a_failing_exclusive_create_refuses_init_and_removes_the_root_it_made() {
        let world = world();
        let root = world.root("new-remote");
        let seams = Seams {
            exclusive: Arc::new(|_| Err(io::Error::from_raw_os_error(13))),
            ..no_mount_point()
        };
        let error = world.run(&root, true, false, &seams).expect_err("no probe");
        assert!(
            matches!(error, RemoteOpError::ExclusiveCreate { .. }),
            "{error}"
        );
        assert!(!root.exists(), "the root made by this run is removed again");
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn a_second_create_that_succeeds_refuses_init_and_leaves_an_existing_root_as_it_was() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        let seams = Seams {
            exclusive: Arc::new(|_| Ok(())),
            ..no_mount_point()
        };
        let error = world
            .run(&root, true, false, &seams)
            .expect_err("no exclusion");
        assert!(
            matches!(error, RemoteOpError::ExclusiveCreate { .. }),
            "{error}"
        );
        assert_eq!(names(&root), Vec::<String>::new());
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn a_probe_leaves_no_file_behind() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        world.init(&root).expect("the remote is created");
        assert_eq!(names(&root), [marker::FILE_NAME]);
    }

    #[test]
    fn a_config_that_cannot_be_written_leaves_the_marker_and_says_how_to_attach() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        let antiq = world.collection().antiq_dir();
        let config = antiq.join("config.toml");
        // A first run makes the lock file, which a run in a directory that
        // cannot be written needs to find in place.
        let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
        drop(run::start(&world.collection(), &clock, false).expect("the run starts"));
        fs::set_permissions(&antiq, fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = fs::write(antiq.join("probe"), "").is_err();
        let error = world.init(&root).expect_err("no config write");
        fs::set_permissions(&antiq, fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
        );
        assert!(
            matches!(error, RemoteOpError::ConfigWrite { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("without --init"), "{error}");
        assert!(root.join(marker::FILE_NAME).is_file());
        assert!(
            !fs::read_to_string(config)
                .expect("config")
                .contains("[[remote]]")
        );
    }

    // =====================================================
    // Attaching
    // =====================================================

    #[test]
    fn attaching_takes_the_remote_id_from_the_marker() {
        let world = world();
        let root = world.remote("antiq");
        let report = world.add(&root).expect("the remote is attached");
        assert_eq!(report.id, Some(id(REMOTE_ID)));
        assert!(report.attached);
        assert!(!report.created);
        assert_eq!(world.collection().config().remotes[0].id, id(REMOTE_ID));
        assert_eq!(names(&root), [marker::FILE_NAME]);
    }

    #[test]
    fn attaching_a_directory_without_a_marker_is_away_and_writes_nothing() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        let before = world.config_text();
        let error = world.add(&root).expect_err("no marker");
        assert!(
            matches!(error, RemoteOpError::Remote(RemoteError::Away { .. })),
            "{error}"
        );
        assert_eq!(world.config_text(), before);
        assert_eq!(names(&root), Vec::<String>::new());
    }

    #[test]
    fn attaching_a_mount_point_without_a_marker_is_broken_and_points_to_init() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        let seams = Seams {
            mount: Arc::new(|_| Ok(true)),
            ..Seams::default()
        };
        let error = world
            .run(&root, false, false, &seams)
            .expect_err("no marker");
        assert!(
            matches!(&error, RemoteOpError::Remote(RemoteError::Broken(message))
                if message.contains("mount point") && message.contains("remote add --init")),
            "{error}"
        );
        assert_eq!(names(&root), Vec::<String>::new());
    }

    #[test]
    fn attaching_a_missing_root_is_away() {
        let world = world();
        let error = world.add(&world.root("absent")).expect_err("no root");
        assert!(
            matches!(error, RemoteOpError::Remote(RemoteError::Away { .. })),
            "{error}"
        );
    }

    #[test]
    fn attaching_a_root_with_a_marker_that_cannot_be_read_is_broken() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        fs::write(root.join(marker::FILE_NAME), "not json").expect("write");
        let error = world.add(&root).expect_err("broken");
        assert!(
            matches!(error, RemoteOpError::Remote(RemoteError::Broken(_))),
            "{error}"
        );
    }

    #[test]
    fn a_failing_exclusive_create_refuses_attaching_and_writes_nothing() {
        let world = world();
        let root = world.remote("antiq");
        let seams = Seams {
            exclusive: Arc::new(|_| Err(io::Error::from_raw_os_error(13))),
            ..no_mount_point()
        };
        let before = world.config_text();
        let error = world
            .run(&root, false, false, &seams)
            .expect_err("no probe");
        assert!(
            matches!(error, RemoteOpError::ExclusiveCreate { .. }),
            "{error}"
        );
        assert_eq!(world.config_text(), before);
        assert_eq!(names(&root), [marker::FILE_NAME]);
    }

    #[test]
    fn a_second_remote_is_refused_and_the_config_stays_as_it_was() {
        let world = world();
        let first = world.remote("first");
        world.add(&first).expect("attached");
        let before = world.config_text();
        let second = world.remote("second");
        let error = world.add(&second).expect_err("one remote only");
        assert!(
            matches!(error, RemoteOpError::AlreadyAttached { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("first"), "{error}");
        assert_eq!(world.config_text(), before);
    }

    #[test]
    fn an_empty_remote_list_in_the_config_refuses_before_anything_is_written() {
        let world = world();
        let config = world.collection().config_path();
        let text = format!(
            "remote = []\n{}",
            fs::read_to_string(&config).expect("config")
        );
        fs::write(&config, &text).expect("write");
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        let error = world.init(&root).expect_err("the config has a remote key");
        assert!(
            matches!(error, RemoteOpError::AlreadyAttached { .. }),
            "{error}"
        );
        assert_eq!(names(&root), Vec::<String>::new());
        let attach = world.remote("existing");
        let error = world.add(&attach).expect_err("the config has a remote key");
        assert!(
            matches!(error, RemoteOpError::AlreadyAttached { .. }),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&config).expect("config"), text);
    }

    #[test]
    fn a_host_key_alias_belongs_to_ssh_remotes() {
        let world = world();
        let root = world.remote("antiq");
        let error = world
            .run_text(
                root.to_str().expect("UTF-8"),
                false,
                Some("nas"),
                false,
                &no_mount_point(),
            )
            .expect_err("path remote");
        assert!(
            matches!(error, RemoteOpError::HostKeyAliasNeedsSsh),
            "{error}"
        );
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn a_url_that_does_not_parse_is_refused() {
        let world = world();
        let error = world
            .run_text("antiq/relative", false, None, false, &no_mount_point())
            .expect_err("relative");
        assert!(matches!(error, RemoteOpError::Url(_)), "{error}");
    }

    // =====================================================
    // The metadata copy
    // =====================================================

    fn put_version(root: &Path, object: &str, number: u32, bytes: &[u8]) {
        let dir = root.join("objects").join(object);
        fs::create_dir_all(&dir).expect("dirs");
        fs::write(dir.join(format!("meta.{number}")), bytes).expect("write");
    }

    #[test]
    fn attaching_copies_the_metadata_of_every_object_byte_for_byte() {
        let world = world();
        let root = world.remote("antiq");
        put_version(&root, REMOTE_ID, 1, b"first");
        put_version(&root, REMOTE_ID, 2, b"second");
        put_version(&root, OTHER_ID, 1, b"other");
        let report = world.add(&root).expect("attached");
        assert_eq!(report.pulled, Some(2));
        let collection = world.collection();
        let store = Store::new(&collection);
        assert_eq!(
            store.committed_versions(id(REMOTE_ID)).expect("versions"),
            [1, 2]
        );
        assert_eq!(
            fs::read(store.meta_path(id(REMOTE_ID), 2)).expect("exists"),
            b"second"
        );
        assert_eq!(
            fs::read(store.meta_path(id(OTHER_ID), 1)).expect("exists"),
            b"other"
        );
    }

    #[test]
    fn a_copy_that_fails_leaves_the_remote_attached_and_says_sync_finishes_it() {
        let world = world();
        let root = world.remote("antiq");
        put_version(&root, REMOTE_ID, 1, b"first");
        let file = root.join("objects").join(REMOTE_ID).join("meta.1");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).expect("chmod");
        let denied = fs::read(&file).is_err();
        let error = world.add(&root).expect_err("the copy fails");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 000 denies reading; run it as a non-root user"
        );
        assert!(
            matches!(error, RemoteOpError::MetadataCopy { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("`sync`"), "{error}");
        assert_eq!(world.collection().config().remotes.len(), 1);
    }

    #[test]
    fn a_copy_that_cannot_store_a_version_fails_the_same_way() {
        let world = world();
        let root = world.remote("antiq");
        put_version(&root, REMOTE_ID, 1, b"remote bytes");
        // Something that is no directory sits where the object's directory
        // would go.
        let collection = world.collection();
        fs::write(collection.object_dir(id(REMOTE_ID)), b"in the way").expect("write");
        let error = world.add(&root).expect_err("the object cannot be made");
        assert!(
            matches!(error, RemoteOpError::MetadataCopy { .. }),
            "{error}"
        );
    }

    // =====================================================
    // Dry runs and warnings
    // =====================================================

    #[test]
    fn a_dry_run_of_an_attach_plans_the_copy_and_writes_nothing() {
        let world = world();
        let root = world.remote("antiq");
        put_version(&root, REMOTE_ID, 1, b"first");
        let before = world.config_text();
        let report = world
            .run(&root, false, true, &no_mount_point())
            .expect("planned");
        assert!(report.dry_run && !report.attached);
        assert_eq!(report.pulled, None);
        assert_eq!(
            report.plan_lines(),
            [
                format!("Would attach the remote at {}", root.display()),
                "Would copy the metadata of 1 object".to_string()
            ]
        );
        assert_eq!(world.config_text(), before);
        assert_eq!(names(&root), [marker::FILE_NAME, "objects"]);
        assert!(!world.collection().lock_path().exists());
        assert!(
            world
                .collection()
                .objects_dir()
                .read_dir()
                .expect("dir")
                .next()
                .is_none()
        );
    }

    #[test]
    fn a_dry_run_of_init_plans_the_creation_and_makes_nothing() {
        let world = world();
        let root = world.root("new-remote");
        let report = world
            .run(&root, true, true, &no_mount_point())
            .expect("planned");
        assert!(report.dry_run && !report.attached && !report.created);
        assert_eq!(report.id, None);
        assert_eq!(
            report.plan_lines(),
            [format!("Would create the remote at {}", root.display())]
        );
        assert!(!root.exists());
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn a_dry_run_refuses_what_the_real_run_refuses() {
        let world = world();
        let root = world.remote("antiq");
        let error = world
            .run(&root, true, true, &no_mount_point())
            .expect_err("already a remote");
        assert!(
            matches!(error, RemoteOpError::AlreadyARemote { .. }),
            "{error}"
        );
    }

    #[test]
    fn a_dry_run_makes_no_probe_file() {
        let world = world();
        let root = world.remote("antiq");
        let seams = Seams {
            exclusive: Arc::new(|_| panic!("a dry run writes nothing on the remote")),
            ..no_mount_point()
        };
        world.run(&root, false, true, &seams).expect("planned");
    }

    // =====================================================
    // show
    // =====================================================

    impl World {
        fn show(&self, seams: &Seams) -> Result<ShowReport, RemoteOpError> {
            show_with(&self.collection(), seams)
        }

        fn append_to_config(&self, text: &str) {
            let config = self.collection().config_path();
            let mut found = fs::read_to_string(&config).expect("config");
            found.push_str(text);
            fs::write(&config, found).expect("write");
        }
    }

    #[test]
    fn without_a_remote_show_says_so() {
        let world = world();
        let report = world.show(&no_mount_point()).expect("shown");
        assert!(!report.attached);
        assert_eq!(report.kind, None);
        assert_eq!(report.state, None);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn a_reachable_remote_is_shown_with_kind_url_id_and_state() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        let report = world.show(&no_mount_point()).expect("shown");
        assert!(report.attached);
        assert_eq!(report.kind, Some("path"));
        assert_eq!(report.url.as_deref(), root.to_str());
        assert_eq!(report.id, Some(id(REMOTE_ID)));
        assert_eq!(report.state, Some(RemoteState::Reachable));
        assert_eq!(report.reason, None);
        assert_eq!(report.probed, Some(serde_json::Map::new()));
    }

    #[test]
    fn a_remote_whose_root_is_gone_is_shown_as_away_with_the_reason() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        fs::remove_dir_all(&root).expect("the share goes");
        let report = world.show(&no_mount_point()).expect("shown");
        assert_eq!(report.state, Some(RemoteState::Away));
        assert!(
            report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("does not exist")),
            "{:?}",
            report.reason
        );
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn a_remote_with_another_marker_is_shown_as_broken() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        fs::write(
            root.join(marker::FILE_NAME),
            Marker::new(id(OTHER_ID)).to_bytes(),
        )
        .expect("write");
        let report = world.show(&no_mount_point()).expect("shown");
        assert_eq!(report.state, Some(RemoteState::Broken));
        assert!(report.reason.is_some());
    }

    #[test]
    fn showing_a_root_that_is_no_mount_point_warns_and_a_mount_point_does_not() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        let warned = world.show(&no_mount_point()).expect("shown");
        assert!(
            warned.warnings[0].message.contains("not a mount point"),
            "{:?}",
            warned.warnings
        );
        let mounted = Seams {
            mount: Arc::new(|_| Ok(true)),
            ..Seams::default()
        };
        assert!(world.show(&mounted).expect("shown").warnings.is_empty());
    }

    #[test]
    fn showing_changes_nothing() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        let before = (names(&root), world.config_text());
        world.show(&no_mount_point()).expect("shown");
        assert_eq!(names(&root), before.0);
        assert_eq!(world.config_text(), before.1);
    }

    #[test]
    fn a_url_in_the_config_that_does_not_fit_cannot_be_shown() {
        let world = world();
        world.append_to_config(&format!(
            "\n[[remote]]\nurl = \"relative\"\nid = \"{REMOTE_ID}\"\n"
        ));
        let error = world.show(&no_mount_point()).expect_err("no URL");
        assert!(matches!(error, RemoteOpError::Url(_)), "{error}");
    }

    // =====================================================
    // remove
    // =====================================================

    impl World {
        fn remove(&self, dry_run: bool) -> Result<RemoveReport, RemoteOpError> {
            let collection = self.collection();
            let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
            let started =
                rerun_while_lock_unattributed(|| run::start(&collection, &clock, dry_run))
                    .expect("the run starts");
            remove(&collection, &started)
        }

        /// Archived metadata for object `number` at `path`, with no entry
        /// of this machine: an archive only the remote holds.
        fn remote_only(&self, number: u8, path: &str) -> ObjectId {
            let object = ObjectId::from_ulid(ulid::Ulid::from_parts(
                1_600_000_000_000 + u64::from(number),
                u128::from(number),
            ));
            let known = archived_metadata(object, path);
            let dir = self.collection().object_dir(object);
            fs::create_dir_all(&dir).expect("dirs");
            fs::write(
                dir.join("meta.1"),
                crate::meta::to_bytes(&known).expect("serialises"),
            )
            .expect("write");
            object
        }
    }

    fn archived_metadata(object: ObjectId, at: &str) -> crate::meta::Metadata {
        use crate::meta::PayloadKind;
        use crate::meta::{Checksum, CreatedSource, FileName, Lifecycle, Metadata, Payload};
        use crate::relpath::{Name, RelPath};
        let path = RelPath::parse(at).expect("a valid path");
        let name =
            Name::from_bytes(path.components().last().expect("a name").to_vec()).expect("a name");
        Metadata {
            format: 1,
            id: object,
            collection: COLLECTION_ID.parse().expect("an ID"),
            path,
            name,
            created: "2026-01-01T00:00:00Z".parse().expect("a time"),
            created_source: CreatedSource::Birth,
            lifecycle: Lifecycle::Archived,
            payloads: vec![Payload {
                kind: PayloadKind::Archive,
                version: 1,
                file: FileName::new("0123456789abcdef-x.tar.zst").expect("a name"),
                package_format: "tar+zstd/1".into(),
                size: 1,
                checksum: Checksum::sha256("00"),
                listing: None,
                machine: "laptop".into(),
                created: "2026-01-01T00:00:00Z".parse().expect("a time"),
                verified: None,
                superseded: false,
                encoder: None,
                extra: serde_json::Map::new(),
            }],
            history: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn removing_without_a_remote_says_so() {
        let world = world();
        let error = world.remove(false).expect_err("no remote");
        assert!(
            matches!(error, RemoteOpError::Remote(RemoteError::NoRemote)),
            "{error}"
        );
        assert_eq!(error.kind(), ErrorKind::NoRemote);
    }

    #[test]
    fn removing_detaches_and_keeps_everything_else() {
        let world = world();
        let root = world.remote("antiq");
        put_version(&root, REMOTE_ID, 1, b"first");
        world.add(&root).expect("attached");
        let collection = world.collection();
        let local = Store::new(&collection).meta_path(id(REMOTE_ID), 1);
        let before_remote = names(&root);
        let report = world.remove(false).expect("detached");
        assert!(report.detached);
        assert_eq!(report.url, root.to_str().expect("UTF-8"));
        assert!(world.collection().config().remotes.is_empty());
        assert!(!world.config_text().contains("[[remote]]"));
        assert!(world.config_text().contains("name = \"laptop\""));
        assert_eq!(fs::read(local).expect("kept"), b"first");
        assert_eq!(names(&root), before_remote);
        assert_eq!(
            report.actions,
            [RemoteAction::DetachRemote {
                url: root.to_str().expect("UTF-8").into()
            }]
        );
    }

    #[test]
    fn removing_lists_the_pieces_only_the_remote_holds() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        let remote_only = world.remote_only(1, "projects/old");
        // An archived piece whose archive this machine holds is not listed.
        let kept = world.remote_only(2, "projects/kept");
        let entry = crate::tracking::TrackingEntry {
            local_payloads: vec![crate::tracking::LocalPayload {
                kind: crate::meta::PayloadKind::Archive,
                version: 1,
                file: crate::meta::FileName::new("0123456789abcdef-x.tar.zst").expect("a name"),
            }],
            ..crate::tracking::TrackingEntry::new(
                kept,
                crate::relpath::RelPath::parse("projects/kept").expect("a path"),
                crate::tracking::EntryState::Gone,
                crate::tracking::VersionTakenOut::Committed(1),
                "2026-01-01T00:00:00Z".parse().expect("a time"),
            )
        };
        let tracking = world.collection().tracking_path(kept);
        fs::write(tracking, serde_json::to_vec(&entry).expect("serialises")).expect("write");
        let report = world.remove(false).expect("detached");
        assert_eq!(
            report.remote_only,
            [Candidate {
                id: remote_only.to_string(),
                path: "projects/old".into()
            }]
        );
    }

    #[test]
    fn a_folder_at_the_path_of_a_remote_only_piece_makes_it_unearthed_and_not_listed() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        let object = world.remote_only(1, "projects/old");
        let entry = crate::tracking::TrackingEntry::new(
            object,
            crate::relpath::RelPath::parse("projects/old").expect("a path"),
            crate::tracking::EntryState::Live,
            crate::tracking::VersionTakenOut::Committed(1),
            "2026-01-01T00:00:00Z".parse().expect("a time"),
        );
        fs::write(
            world.collection().tracking_path(object),
            serde_json::to_vec(&entry).expect("serialises"),
        )
        .expect("write");
        fs::create_dir_all(world.collection_dir.root().join("projects/old")).expect("dirs");
        let report = world.remove(false).expect("detached");
        assert!(report.remote_only.is_empty(), "{:?}", report.remote_only);
    }

    #[test]
    fn a_dry_run_of_removing_plans_the_detach_and_changes_nothing() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        world.remote_only(1, "projects/old");
        let before = world.config_text();
        let report = world.remove(true).expect("planned");
        assert!(report.dry_run && !report.detached);
        assert_eq!(
            report.plan_lines(),
            [format!("Would detach the remote at {}", root.display())]
        );
        assert_eq!(report.remote_only.len(), 1);
        assert_eq!(world.config_text(), before);
    }

    #[test]
    fn a_remote_that_is_gone_can_still_be_removed() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        fs::remove_dir_all(&root).expect("the share goes");
        world.remove(false).expect("detached");
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn a_config_that_cannot_be_written_keeps_the_remote_attached() {
        let world = world();
        let root = world.remote("antiq");
        world.add(&root).expect("attached");
        let antiq = world.collection().antiq_dir();
        let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
        drop(run::start(&world.collection(), &clock, false).expect("the run starts"));
        fs::set_permissions(&antiq, fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = fs::write(antiq.join("probe"), "").is_err();
        let error = world.remove(false).expect_err("no config write");
        fs::set_permissions(&antiq, fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
        );
        assert!(matches!(error, RemoteOpError::Fsx(_)), "{error}");
        assert_eq!(world.collection().config().remotes.len(), 1);
    }

    #[test]
    fn a_collection_root_that_cannot_be_opened_stops_the_listing_of_remote_only_pieces() {
        let error = remote_only_pieces(&Collection::for_tests()).expect_err("no root");
        assert!(
            matches!(error, RemoteOpError::Io { action: "open", .. }),
            "{error}"
        );
    }

    #[test]
    fn the_other_reports_are_never_partial_and_list_their_warnings() {
        let shown = ShowReport {
            attached: false,
            kind: None,
            url: None,
            id: None,
            state: None,
            reason: None,
            probed: None,
            warnings: vec![Warning::new("one")],
        };
        assert!(!shown.partial());
        assert_eq!(shown.warnings(), [Warning::new("one")]);
        let removed = RemoveReport {
            detached: true,
            url: "/x".into(),
            remote_only: Vec::new(),
            dry_run: false,
            actions: Vec::new(),
            warnings: vec![Warning::new("two")],
        };
        assert!(!removed.partial());
        assert_eq!(removed.warnings(), [Warning::new("two")]);
    }

    #[test]
    fn a_second_create_that_fails_for_another_reason_refuses_too() {
        let world = world();
        let root = world.remote("antiq");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let seams = Seams {
            exclusive: Arc::new(move |_| {
                match counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => Ok(()),
                    _ => Err(io::Error::from_raw_os_error(13)),
                }
            }),
            ..no_mount_point()
        };
        let error = world
            .run(&root, false, false, &seams)
            .expect_err("no probe");
        assert!(
            matches!(error, RemoteOpError::ExclusiveCreate { .. }),
            "{error}"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_config_that_cannot_be_written_refuses_an_attach_too() {
        let world = world();
        let root = world.remote("antiq");
        let antiq = world.collection().antiq_dir();
        let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
        drop(run::start(&world.collection(), &clock, false).expect("the run starts"));
        fs::set_permissions(&antiq, fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = fs::write(antiq.join("probe"), "").is_err();
        let error = world.add(&root).expect_err("no config write");
        fs::set_permissions(&antiq, fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
        );
        assert!(matches!(error, RemoteOpError::Fsx(_)), "{error}");
    }

    #[test]
    fn init_cannot_list_a_root_that_cannot_be_read() {
        let world = world();
        let root = world.root("antiq");
        fs::create_dir(&root).expect("dir");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o111)).expect("chmod");
        let denied = fs::read_dir(&root).is_err();
        let error = world.init(&root).expect_err("no listing");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 111 denies listing; run it as a non-root user"
        );
        assert!(
            matches!(error, RemoteOpError::Io { action: "list", .. }),
            "{error}"
        );
    }

    #[test]
    fn init_cannot_make_a_root_in_a_directory_that_cannot_be_written() {
        let world = world();
        let parent = world.root("parent");
        fs::create_dir(&parent).expect("dir");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).expect("chmod");
        let denied = fs::write(parent.join("probe"), "").is_err();
        let error = world.init(&parent.join("antiq")).expect_err("no root");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).expect("restore");
        assert!(
            denied,
            "the test needs a user for whom mode 555 denies writing; run it as a non-root user"
        );
        assert!(
            matches!(
                error,
                RemoteOpError::Io {
                    action: "create",
                    ..
                }
            ),
            "{error}"
        );
        assert!(world.collection().config().remotes.is_empty());
    }

    #[test]
    fn the_copy_brings_only_what_this_machine_lacks() {
        let world = world();
        let root = world.remote("antiq");
        put_version(&root, REMOTE_ID, 1, b"first");
        put_version(&root, REMOTE_ID, 2, b"second");
        let collection = world.collection();
        let store = Store::new(&collection);
        fs::create_dir_all(collection.object_dir(id(REMOTE_ID))).expect("dirs");
        fs::write(store.meta_path(id(REMOTE_ID), 1), b"first").expect("write");
        let report = world.add(&root).expect("attached");
        assert_eq!(report.pulled, Some(1));
        assert_eq!(
            store.committed_versions(id(REMOTE_ID)).expect("versions"),
            [1, 2]
        );
    }

    #[test]
    fn the_error_kinds_follow_the_remote_and_the_store() {
        let away = RemoteOpError::Remote(RemoteError::Away {
            message: "gone".into(),
            overdue: false,
        });
        assert_eq!(away.kind(), ErrorKind::RemoteAway);
        assert_eq!(
            RemoteOpError::Store(StoreError::NoPending).kind(),
            ErrorKind::Other
        );
        assert_eq!(RemoteOpError::HostKeyAliasNeedsSsh.kind(), ErrorKind::Other);
    }

    #[test]
    fn a_report_is_never_partial_and_lists_its_warnings() {
        let report = AddReport {
            kind: "path",
            url: "/x".into(),
            id: None,
            attached: true,
            created: false,
            pulled: Some(0),
            dry_run: false,
            actions: Vec::new(),
            warnings: vec![Warning::new("watch out")],
        };
        assert!(!report.partial());
        assert_eq!(report.warnings(), [Warning::new("watch out")]);
    }

    #[test]
    fn a_root_that_is_no_mount_point_gets_a_warning() {
        let world = world();
        let root = world.remote("antiq");
        let report = world.add(&root).expect("attached");
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].message.contains("not a mount point"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_mount_point_gets_no_warning() {
        let world = world();
        let root = world.remote("antiq");
        let seams = Seams {
            mount: Arc::new(|_| Ok(true)),
            ..Seams::default()
        };
        let report = world.run(&root, false, false, &seams).expect("attached");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    }

    #[test]
    fn a_mount_test_that_fails_is_reported_as_a_warning() {
        let world = world();
        let root = world.remote("antiq");
        let seams = Seams {
            mount: Arc::new(|_| {
                Err(FsxError::Io {
                    action: "stat",
                    path: "the root".into(),
                    source: io::Error::from_raw_os_error(5),
                })
            }),
            ..Seams::default()
        };
        let report = world.run(&root, false, false, &seams).expect("attached");
        assert!(
            report.warnings[0].message.contains("cannot tell"),
            "{:?}",
            report.warnings
        );
    }

    // =====================================================
    // An ssh remote, with the ssh shim as the far side
    // =====================================================

    /// The ssh shim in a state directory of its own, and the directory on
    /// this machine that plays the NAS.
    struct Shim {
        state: tempfile::TempDir,
        control: tempfile::TempDir,
        nas: tempfile::TempDir,
    }

    fn short_dir() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("/tmp is writable");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
            .expect("the directory is ours");
        dir
    }

    impl Shim {
        fn new() -> Shim {
            assert_ne!(
                rustix::process::geteuid().as_raw(),
                0,
                "these tests assume a non-root user"
            );
            let shim = Shim {
                state: short_dir(),
                control: short_dir(),
                nas: tempfile::tempdir().expect("temp dir"),
            };
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-shim.sh");
            std::os::unix::fs::symlink(script, shim.state.path().join("ssh")).expect("symlink");
            shim
        }

        fn seams(&self, exclusive_create: &'static str) -> SshSeams {
            SshSeams {
                exclusive_create,
                program: Some(self.state.path().join("ssh")),
                control_dir: Some(self.control.path().to_path_buf()),
                ..SshSeams::default()
            }
        }

        /// `remote add --init` of a directory below the NAS root.
        fn init_with(
            &self,
            exclusive_create: &'static str,
        ) -> (World, Result<AddReport, RemoteOpError>) {
            let world = world();
            let target = SshTarget {
                user: None,
                host: "nas".into(),
                port: None,
                root: self
                    .nas
                    .path()
                    .join("fresh")
                    .to_str()
                    .expect("UTF-8")
                    .into(),
            };
            let collection = world.collection();
            let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
            let started = run::start(&collection, &clock, false).expect("the run starts");
            let request = AddRequest {
                url: "ssh://nas/srv/fresh",
                init: true,
                host_key_alias: None,
            };
            let report = add_ssh(
                &collection,
                &request,
                &started,
                &clock,
                &SeqIds::new(1),
                &target,
                self.seams(exclusive_create),
            );
            (world, report)
        }
    }

    fn names_below(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .expect("the directory is listed")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn an_exclusive_create_that_always_succeeds_is_refused_and_the_root_goes() {
        let shim = Shim::new();
        let (world, report) = shim.init_with("exit 0\n");
        let error = report.expect_err("no exclusive create");
        assert!(
            matches!(&error, RemoteOpError::ExclusiveCreate { reason, .. } if reason.contains("second create")),
            "{error}"
        );
        assert_eq!(names_below(shim.nas.path()), Vec::<String>::new());
        assert!(!world.config_text().contains("[[remote]]"));
    }

    #[test]
    fn an_exclusive_create_that_always_fails_is_refused_and_the_root_goes() {
        let shim = Shim::new();
        // The first create already fails, so the second one never runs.
        let calls = shim.state.path().join("creates.log");
        let script: &'static str = Box::leak(
            format!("echo x >> {}\necho 'no way' >&2\nexit 1\n", calls.display()).into_boxed_str(),
        );
        let (world, report) = shim.init_with(script);
        assert_eq!(fs::read_to_string(&calls).expect("log").lines().count(), 1);
        let error = report.expect_err("no exclusive create");
        assert!(
            matches!(&error, RemoteOpError::ExclusiveCreate { reason, .. } if reason == "no way"),
            "{error}"
        );
        assert_eq!(names_below(shim.nas.path()), Vec::<String>::new());
        assert!(!world.config_text().contains("[[remote]]"));
    }

    #[test]
    fn a_second_exclusive_create_that_fails_for_another_reason_is_refused() {
        let shim = Shim::new();
        let (_world, report) = shim.init_with("if [ -e \"$1\" ]; then exit 1; fi\n: > \"$1\"\n");
        let error = report.expect_err("the second create is no proof");
        assert!(
            matches!(&error, RemoteOpError::ExclusiveCreate { reason, .. } if reason.contains("status 1")),
            "{error}"
        );
        assert_eq!(names_below(shim.nas.path()), Vec::<String>::new());
    }

    #[test]
    fn a_connection_lost_while_proving_the_root_ends_the_run_with_that_error() {
        let shim = Shim::new();
        // The script runs on the far side, which is this machine: it makes
        // the shim refuse the connection from the next command on.
        let script: &'static str = Box::leak(
            format!(
                "if ( set -C; : > \"$1\" ) 2>/dev/null; then\n    echo 'connect-refused command' > {}/fault\n    exit 0\nfi\nexit 17\n",
                shim.state.path().display()
            )
            .into_boxed_str(),
        );
        let (world, report) = shim.init_with(script);
        let error = report.expect_err("the connection is gone");
        assert!(
            matches!(&error, RemoteOpError::Remote(RemoteError::Away { .. })),
            "{error}"
        );
        assert!(!world.config_text().contains("[[remote]]"));
    }
}
