// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Unearthing from a live package.
//!
//! A gone piece of this collection that was never archived comes back from
//! the live package of its metadata. A live piece of another collection
//! becomes a new piece of this one: a fork under a new ID, from the same
//! package. The package is fetched into `tmp/`, checked there, and
//! extracted from there like an archive; it never goes into the store.

use super::local::{Environment, Subject, carry_out, skipped, taken_out};
use super::remote::{Fetch, fetch_live};
use super::{UnearthError, UnearthPlan, UnearthReport, UnearthSource};
use crate::clock::{Clock, whole_seconds};
use crate::collection::Collection;
use crate::id::{IdSource, ObjectId};
use crate::lock::RunLock;
use crate::ops::fork::mint;
use crate::relpath::RelPath;
use crate::remote::Remote;
use crate::report::Warning;
use crate::tracking::{EntryState, TrackingEntry, VersionTakenOut};
use jiff::Timestamp;
use serde_json::Value;
use std::io::ErrorKind;

/// Unearths the live package of `plan` from `remote`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn unearth_live(
    collection: &Collection,
    plan: &UnearthPlan,
    lock: &RunLock,
    clock: &dyn Clock,
    env: &Environment<'_>,
    mut warnings: Vec<Warning>,
    ids: &dyn IdSource,
    remote: Option<&dyn Remote>,
) -> Result<UnearthReport, UnearthError> {
    let remote = remote.ok_or_else(|| UnearthError::NoLocalArchive {
        path: plan.target.clone(),
    })?;
    let subject = match plan.source {
        UnearthSource::Fork => {
            let time = whole_seconds(clock.now());
            Subject::Fork {
                id: mint(ids, collection, time)?,
                time,
            }
        }
        _ => Subject::Own,
    };
    let arrived = match fetch_live(collection, plan, remote, env.free_space)? {
        Fetch::Arrived(arrived) => arrived,
        Fetch::NoSpace { needed, available } => {
            return Ok(skipped(collection, plan, needed, available, warnings));
        }
    };
    warnings.extend(package_warnings(plan));
    carry_out(
        collection, plan, lock, clock, env, warnings, &arrived, subject,
    )
}

/// What the user should know about the package before the piece is back.
fn package_warnings(plan: &UnearthPlan) -> Vec<Warning> {
    let mut warnings = Vec::new();
    // The stale mark lives in the tracking entry of the owner, so a fork
    // has none to report.
    let stale = plan
        .entry
        .as_ref()
        .filter(|_| plan.source == UnearthSource::LivePackage)
        .and_then(|entry| entry.stale_backup_since);
    if let Some(since) = stale {
        warnings.push(Warning::new(format!(
            "the live package of `{}` was marked stale since {since}; changes after that are missing",
            plan.target
        )));
    }
    // TODO(I32): read the typed `left_out` field of `Payload` once I32 adds it.
    let left_out = plan
        .payload
        .extra
        .get("left_out")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    warnings.extend(
        left_out.map(|path| Warning::new(format!("`{path}` was left out of the live package"))),
    );
    warnings
}

/// The entry of a gone piece while it comes back: what it listed stays,
/// and so does the stale mark of the package it comes back from, until the
/// folder is in place.
pub(super) fn restore_entry(plan: &UnearthPlan) -> TrackingEntry {
    let mut entry = plan
        .entry
        .clone()
        .expect("a piece is gone only while its entry says so");
    entry.path = plan.target.clone();
    entry.state = EntryState::Unearthing;
    entry.version_taken_out = taken_out(plan.current.source);
    entry.unearthed = None;
    entry.unearthed_payload_version = None;
    entry.removing_payload = None;
    entry.set_diverged(false);
    entry
}

/// The entry of a fork while its folder arrives. The new object has
/// nothing but its folder.
pub(super) fn fork_entry(id: ObjectId, target: &RelPath, time: Timestamp) -> TrackingEntry {
    TrackingEntry::new(
        id,
        target.clone(),
        EntryState::Unearthing,
        VersionTakenOut::Pending,
        time,
    )
}

/// What follows a placed piece. The change manifest of a restored piece
/// described the folder that is gone, so it is removed.
pub(super) fn finish(collection: &Collection, plan: &UnearthPlan) -> Option<Warning> {
    if plan.source != UnearthSource::LivePackage {
        return None;
    }
    let manifest = collection.manifest_path(plan.id);
    match std::fs::remove_file(&manifest) {
        Err(error) if error.kind() != ErrorKind::NotFound => Some(Warning::new(format!(
            "cannot remove the manifest of `{}`: {error}",
            plan.target
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsx::FsxError;
    use crate::fsx::space::available_space;
    use crate::fsx::tree::TreeRoot;
    use crate::id::SeqIds;
    use crate::meta::{CreatedSource, FileName, HistoryAction, Lifecycle, PayloadKind};
    use crate::ops::fork::forked_new;
    use crate::ops::unearth::remote::tests::{Full, after_tmp};
    use crate::ops::unearth::remote::{Refreshed, refresh_with};
    use crate::ops::unearth::tests::{FILES, Fixture, ID, NOW, OTHER_ID, id, path, time};
    use crate::ops::unearth::{Planned, Skipped, UnearthRequest, UnearthSource};
    use crate::pack::live::LiveError;
    use crate::pack::{TarZstd, TargetRules};
    use crate::remote::testing::{FaultRemote, Op, path_remote, remote_root};
    use crate::repair::{self, RealEffects};
    use crate::store::{Source, Store};
    use crate::testing::tree::COLLECTION_ID;
    use crate::tracking::{self, EntryState, VersionTakenOut};
    use serde_json::json;
    use sha2::Digest;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    /// Whose piece the live package is, and what else the metadata has.
    #[derive(Clone, Copy)]
    enum Kind {
        /// A piece of this collection that is gone from its folder.
        OwnGone,
        /// A live piece of another collection, known by its metadata only.
        Foreign,
        /// The same, with an archive payload next to the live package.
        ForeignArchived,
    }

    /// A piece whose live package only the remote holds.
    struct Live {
        fx: Fixture,
        root: tempfile::TempDir,
        remote: FaultRemote,
        file: FileName,
    }

    impl Live {
        fn own() -> Live {
            Live::build(Kind::OwnGone)
        }

        fn foreign() -> Live {
            Live::build(Kind::Foreign)
        }

        fn build(kind: Kind) -> Live {
            let mut fx = Fixture::new();
            let archive = fx.archived(ID, "projects/a", FILES);
            let object = fx.collection.object_dir(id(ID));
            let store = Store::new(&fx.collection);
            let listing = store
                .current(id(ID))
                .expect("reads")
                .expect("metadata")
                .metadata
                .payloads[0]
                .listing
                .clone()
                .expect("a listing");
            let file = match kind {
                Kind::ForeignArchived => {
                    FileName::new("0000000000000002-l.tar.zst".to_string()).expect("a name")
                }
                _ => archive.clone(),
            };
            fx.edit_meta(ID, |document| {
                document["lifecycle"] = json!("live");
                if !matches!(kind, Kind::OwnGone) {
                    document["collection"] = json!(OTHER_ID);
                }
                if matches!(kind, Kind::ForeignArchived) {
                    let mut payload = document["payloads"][0].clone();
                    payload["kind"] = json!("live");
                    payload["version"] = json!(2);
                    payload["file"] = json!(file.as_str());
                    payload
                        .as_object_mut()
                        .expect("a payload")
                        .remove("listing");
                    document["payloads"]
                        .as_array_mut()
                        .expect("payloads")
                        .push(payload);
                } else {
                    let payload = document["payloads"][0].as_object_mut().expect("a payload");
                    payload.insert("kind".into(), json!("live"));
                    payload.remove("listing");
                }
            });
            let root = remote_root();
            let there = root.path().join("objects").join(ID);
            fs::create_dir_all(&there).expect("the object directory");
            fs::copy(object.join("meta.1"), there.join("meta.1")).expect("metadata");
            fs::copy(object.join(archive.as_str()), there.join(file.as_str())).expect("package");
            if !matches!(kind, Kind::ForeignArchived) {
                fs::remove_file(object.join(archive.as_str())).expect("the package is not held");
                fs::remove_file(object.join(listing.as_str())).expect("the listing is not held");
            }
            match kind {
                Kind::OwnGone => {
                    let mut entry = fx.entry(ID);
                    entry.local_payloads.clear();
                    fx.write_entry(&mut entry);
                }
                Kind::Foreign | Kind::ForeignArchived => {
                    tracking::remove(&fx.collection, &fx.lock, id(ID)).expect("no entry");
                }
            }
            let remote = FaultRemote::new(path_remote(root.path(), "imperius"));
            Live {
                fx,
                root,
                remote,
                file,
            }
        }

        fn plan(&self, request: &UnearthRequest) -> Box<UnearthPlan> {
            self.fx.planned(request)
        }

        fn on_remote(&self) -> std::path::PathBuf {
            self.root
                .path()
                .join("objects")
                .join(ID)
                .join(self.file.as_str())
        }

        fn environment<'a>(
            free_space: &'a dyn Fn(&Path) -> Result<u64, FsxError>,
            rename: &'a super::super::local::Rename<'a>,
        ) -> Environment<'a> {
            Environment {
                packager: &TarZstd,
                probe: &TargetRules::probe,
                free_space,
                rename: *rename,
            }
        }

        fn unearth_through(
            &self,
            request: &UnearthRequest,
            remote: Option<&dyn Remote>,
            env: &Environment<'_>,
        ) -> Result<UnearthReport, UnearthError> {
            unearth_live(
                &self.fx.collection,
                &self.plan(request),
                &self.fx.lock,
                &self.fx.clock,
                env,
                Vec::new(),
                &SeqIds::new(1),
                remote,
            )
        }

        fn unearth_request(&self, request: &UnearthRequest) -> Result<UnearthReport, UnearthError> {
            let rename: super::super::local::Rename<'_> = &crate::fsx::moves::move_dir;
            self.unearth_through(
                request,
                Some(&self.remote),
                &Live::environment(&available_space, &rename),
            )
        }

        fn unearth(&self) -> Result<UnearthReport, UnearthError> {
            self.unearth_request(&self.fx.request(ID))
        }

        fn current(&self, object: ObjectId) -> crate::store::Current {
            Store::new(&self.fx.collection)
                .current(object)
                .expect("reads")
                .expect("metadata")
        }

        fn tmp_is_empty(&self) -> bool {
            fs::read_dir(self.fx.collection.tmp_dir())
                .expect("lists")
                .count()
                == 0
        }

        fn read(&self, relative: &str) -> String {
            fs::read_to_string(self.fx.at(relative)).expect("reads")
        }
    }

    /// The ID the fork of a run at [`NOW`] gets.
    fn minted() -> ObjectId {
        SeqIds::new(1).mint(time(NOW))
    }

    fn failed_rename() -> FsxError {
        FsxError::CrossesDevices {
            from: "a".into(),
            to: "b".into(),
        }
    }

    fn messages(report: &UnearthReport) -> Vec<&str> {
        report
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect()
    }

    // ---------------------------------------------------------------------
    // Restoring a piece of this collection
    // ---------------------------------------------------------------------

    #[test]
    fn a_gone_piece_is_restored_from_its_live_package() {
        let live = Live::own();

        let report = live.unearth().expect("the unearth works");

        assert!(report.unearthed, "{report:?}");
        assert_eq!(report.id, id(ID));
        assert_eq!(report.payload_version, None);
        assert_eq!(report.from, None);
        assert_eq!(live.read("projects/a/notes.txt"), "hello");
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_restore_records_the_unearth_in_the_pending_metadata() {
        let live = Live::own();

        live.unearth().expect("the unearth works");

        let current = live.current(id(ID));
        assert_eq!(current.source, Source::Pending);
        let metadata = current.metadata;
        assert_eq!(metadata.lifecycle, Lifecycle::Live);
        assert_eq!(metadata.collection, id(COLLECTION_ID));
        let last = metadata.history.last().expect("a history");
        assert_eq!(last.action, HistoryAction::Unearth.as_str());
        assert_eq!(last.time, time(NOW));
        assert_eq!(metadata.payloads.len(), 1);
        assert_eq!(metadata.payloads[0].kind, PayloadKind::Live);
    }

    #[test]
    fn a_restore_leaves_the_entry_live_and_keeps_its_lists() {
        let live = Live::own();
        let before = live.fx.entry(ID);

        live.unearth().expect("the unearth works");

        let entry = live.fx.entry(ID);
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert_eq!(entry.unearthed_payload_version, None);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        assert_eq!(entry.local_payloads, before.local_payloads);
        assert_eq!(entry.committed_versions, before.committed_versions);
    }

    #[test]
    fn a_restore_to_another_path_puts_the_piece_there() {
        let live = Live::own();
        let request = UnearthRequest {
            to: Some(path("projects/b")),
            ..live.fx.request(ID)
        };

        let report = live.unearth_request(&request).expect("the unearth works");

        assert_eq!(report.path, path("projects/b"));
        assert_eq!(live.read("projects/b/notes.txt"), "hello");
        assert_eq!(live.fx.entry(ID).path, path("projects/b"));
        assert_eq!(live.current(id(ID)).metadata.path, path("projects/b"));
    }

    fn mark_stale(live: &Live) {
        let mut entry = live.fx.entry(ID);
        entry.stale_backup_since = Some(time("2026-11-03T09:00:00Z"));
        live.fx.write_entry(&mut entry);
    }

    #[test]
    fn a_restore_clears_the_stale_mark() {
        let live = Live::own();
        mark_stale(&live);

        live.unearth().expect("the unearth works");

        assert_eq!(live.fx.entry(ID).stale_backup_since, None);
    }

    #[test]
    fn the_unearthing_entry_of_a_restore_keeps_the_stale_mark_until_the_folder_is_in_place() {
        let live = Live::own();
        mark_stale(&live);
        let seen = std::cell::RefCell::new(None);
        let rename = |from: &TreeRoot, from_name: &RelPath, to: &TreeRoot, to_name: &RelPath| {
            let entry = live.fx.entry(ID);
            *seen.borrow_mut() = Some((entry.state, entry.stale_backup_since));
            crate::fsx::moves::move_dir(from, from_name, to, to_name)
        };
        let rename: super::super::local::Rename<'_> = &rename;

        live.unearth_through(
            &live.fx.request(ID),
            Some(&live.remote),
            &Live::environment(&available_space, &rename),
        )
        .expect("the unearth works");

        assert_eq!(
            seen.into_inner(),
            Some((EntryState::Unearthing, Some(time("2026-11-03T09:00:00Z"))))
        );
    }

    #[test]
    fn a_stale_live_package_is_a_warning() {
        let live = Live::own();
        mark_stale(&live);

        let report = live.unearth().expect("the unearth works");

        assert_eq!(
            messages(&report),
            [
                "the live package of `projects/a` was marked stale since 2026-11-03T09:00:00Z; changes after that are missing"
            ]
        );
    }

    #[test]
    fn a_restore_removes_the_manifest_of_the_gone_piece() {
        let live = Live::own();
        let manifest = live.fx.collection.manifest_path(id(ID));
        fs::write(&manifest, "{}").expect("a manifest");

        live.unearth().expect("the unearth works");

        assert!(!manifest.exists());
    }

    #[test]
    fn a_manifest_that_cannot_be_removed_is_a_warning() {
        let live = Live::own();
        let manifest = live.fx.collection.manifest_path(id(ID));
        fs::create_dir(&manifest).expect("a directory where the manifest should be");

        let report = live.unearth().expect("the unearth works");

        assert!(report.unearthed);
        assert_eq!(messages(&report).len(), 1, "{report:?}");
        assert!(
            messages(&report)[0].starts_with("cannot remove the manifest of `projects/a`:"),
            "{report:?}"
        );
    }

    fn leave_out(live: &Live, paths: &[&str]) {
        live.fx.edit_meta(ID, |document| {
            let payloads = document["payloads"].as_array_mut().expect("payloads");
            let live_payload = payloads
                .iter_mut()
                .find(|payload| payload["kind"] == "live")
                .expect("a live payload");
            live_payload["left_out"] = json!(paths);
        });
    }

    #[test]
    fn what_a_live_package_left_out_is_a_warning() {
        let live = Live::own();
        leave_out(&live, &["projects/a/cache", "projects/a/tmp"]);

        let report = live.unearth().expect("the unearth works");

        assert_eq!(
            messages(&report),
            [
                "`projects/a/cache` was left out of the live package",
                "`projects/a/tmp` was left out of the live package"
            ]
        );
    }

    // ---------------------------------------------------------------------
    // Forking another collection's piece
    // ---------------------------------------------------------------------

    #[test]
    fn a_live_piece_of_another_collection_is_forked_under_a_new_id() {
        let live = Live::foreign();

        let report = live.unearth().expect("the unearth works");

        assert!(report.unearthed, "{report:?}");
        assert_eq!(report.id, minted());
        assert_eq!(report.from, Some(id(ID)));
        assert_eq!(report.payload_version, None);
        assert_eq!(live.read("projects/a/notes.txt"), "hello");
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_fork_is_a_new_piece_of_this_collection_without_a_payload() {
        let live = Live::foreign();

        live.unearth().expect("the unearth works");

        let current = live.current(minted());
        assert_eq!(current.source, Source::Pending);
        let metadata = current.metadata;
        assert_eq!(metadata.collection, id(COLLECTION_ID));
        assert_eq!(metadata.lifecycle, Lifecycle::Live);
        assert_eq!(metadata.created_source, CreatedSource::Fork);
        assert_eq!(metadata.created, time(NOW));
        assert_eq!(metadata.path, path("projects/a"));
        assert!(metadata.payloads.is_empty());
        assert_eq!(metadata.history.len(), 1);
        assert_eq!(metadata.history[0].action, HistoryAction::Fork.as_str());
    }

    #[test]
    fn a_fork_has_a_live_entry_that_lists_nothing() {
        let live = Live::foreign();

        live.unearth().expect("the unearth works");

        let entry = tracking::read(&live.fx.collection, minted())
            .expect("reads")
            .expect("an entry");
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path("projects/a"));
        assert_eq!(entry.unearthed, Some(time(NOW)));
        assert_eq!(entry.unearthed_payload_version, None);
        assert_eq!(entry.version_taken_out, VersionTakenOut::Pending);
        assert!(entry.local_payloads.is_empty());
        assert!(entry.committed_versions.is_empty());
    }

    /// Every file below `dir` with its bytes.
    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut found = BTreeMap::new();
        for item in fs::read_dir(dir).expect("lists") {
            let item = item.expect("an item");
            found.insert(
                item.file_name().to_string_lossy().into_owned(),
                fs::read(item.path()).unwrap_or_default(),
            );
        }
        found
    }

    fn untouched_by_a_fork(kind: Kind) {
        let live = Live::build(kind);
        let object = live.fx.collection.object_dir(id(ID));
        let objects_before = snapshot(&object);
        let tracking_before = snapshot(&live.fx.collection.tracking_dir());

        live.unearth().expect("the unearth works");

        assert_eq!(snapshot(&object), objects_before);
        let mut tracking_after = snapshot(&live.fx.collection.tracking_dir());
        tracking_after.remove(&format!("{}.json", minted()));
        assert_eq!(tracking_after, tracking_before);
    }

    #[test]
    fn a_fork_writes_nothing_under_the_id_of_the_other_piece() {
        untouched_by_a_fork(Kind::Foreign);
    }

    #[test]
    fn a_fork_writes_nothing_under_the_id_of_a_piece_that_has_an_archive_too() {
        untouched_by_a_fork(Kind::ForeignArchived);
    }

    #[test]
    fn a_fork_leaves_an_entry_the_other_id_has_as_it_is_and_warns_of_nothing() {
        let live = Live::foreign();
        let mut stale = TrackingEntry::new(
            id(ID),
            path("projects/elsewhere"),
            EntryState::Gone,
            VersionTakenOut::Committed(1),
            time(NOW),
        );
        stale.stale_backup_since = Some(time("2026-11-03T09:00:00Z"));
        live.fx.write_entry(&mut stale);
        let before = live.fx.entry(ID);

        let report = live.unearth().expect("the unearth works");

        assert_eq!(live.fx.entry(ID), before);
        assert!(report.warnings.is_empty(), "{report:?}");
    }

    #[test]
    fn what_a_live_package_left_out_is_a_warning_for_a_fork_too() {
        let live = Live::foreign();
        leave_out(&live, &["projects/a/cache"]);

        let report = live.unearth().expect("the unearth works");

        assert_eq!(
            messages(&report),
            ["`projects/a/cache` was left out of the live package"]
        );
    }

    // ---------------------------------------------------------------------
    // A rename that fails
    // ---------------------------------------------------------------------

    #[test]
    fn a_restore_whose_rename_fails_gets_its_entry_back() {
        let live = Live::own();
        let before = live.fx.entry(ID);
        let rename = |_: &TreeRoot, _: &RelPath, _: &TreeRoot, _: &RelPath| Err(failed_rename());
        let rename: super::super::local::Rename<'_> = &rename;

        let error = live
            .unearth_through(
                &live.fx.request(ID),
                Some(&live.remote),
                &Live::environment(&available_space, &rename),
            )
            .expect_err("fails");

        assert!(matches!(error, UnearthError::Fsx(_)), "{error}");
        let entry = live.fx.entry(ID);
        assert_eq!(entry.state, EntryState::Gone);
        assert_eq!(entry.path, before.path);
        assert!(!live.fx.at("projects/a").exists());
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_fork_whose_rename_fails_leaves_no_trace_of_the_new_id() {
        let live = Live::foreign();
        let rename = |_: &TreeRoot, _: &RelPath, _: &TreeRoot, _: &RelPath| Err(failed_rename());
        let rename: super::super::local::Rename<'_> = &rename;

        let error = live
            .unearth_through(
                &live.fx.request(ID),
                Some(&live.remote),
                &Live::environment(&available_space, &rename),
            )
            .expect_err("fails");

        assert!(matches!(error, UnearthError::Fsx(_)), "{error}");
        assert!(
            tracking::read(&live.fx.collection, minted())
                .expect("reads")
                .is_none()
        );
        assert!(!live.fx.collection.object_dir(minted()).exists());
        assert!(live.tmp_is_empty());
    }

    // ---------------------------------------------------------------------
    // The package
    // ---------------------------------------------------------------------

    #[test]
    fn a_package_of_another_size_than_the_metadata_says_is_refused() {
        let live = Live::own();
        let mut bytes = fs::read(live.on_remote()).expect("reads");
        bytes.push(0);
        fs::write(live.on_remote(), bytes).expect("written");

        let error = live.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. } if what.contains("bytes")),
            "{error}"
        );
        assert!(live.tmp_is_empty());
        assert_eq!(live.fx.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_package_of_the_same_size_and_other_bytes_is_refused() {
        let live = Live::own();
        let mut bytes = fs::read(live.on_remote()).expect("reads");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(live.on_remote(), bytes).expect("written");

        let error = live.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::ArchiveDiffers { what, .. } if what.contains("SHA-256")),
            "{error}"
        );
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_package_whose_zstd_frame_is_damaged_is_refused_although_the_metadata_matches_it() {
        let live = Live::own();
        let mut bytes = fs::read(live.on_remote()).expect("reads");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        fs::write(live.on_remote(), &bytes).expect("written");
        let sha256 = crate::pack::hashing::hex(&sha2::Sha256::digest(&bytes));
        live.fx.edit_meta(ID, |document| {
            document["payloads"][0]["checksum"]["value"] = json!(sha256);
        });

        let error = live.unearth().expect_err("refused");

        assert!(
            matches!(&error, UnearthError::Live(LiveError::StreamCheck { .. })),
            "{error}"
        );
        assert!(live.tmp_is_empty());
        assert_eq!(live.fx.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_volume_without_room_for_the_package_skips_before_it_moves() {
        let live = Live::own();
        let plan = live.plan(&live.fx.request(ID));
        let room = |_: &Path| Ok(2 * plan.payload.size - 1);
        let rename: super::super::local::Rename<'_> = &crate::fsx::moves::move_dir;

        let report = live
            .unearth_through(
                &live.fx.request(ID),
                Some(&live.remote),
                &Live::environment(&room, &rename),
            )
            .expect("skips");

        assert!(!report.unearthed);
        assert_eq!(
            report.skipped,
            Some(Skipped::NoSpace {
                needed: 2 * plan.payload.size,
                available: 2 * plan.payload.size - 1
            })
        );
        assert_eq!(live.remote.count(Op::GetPayload), 0);
        assert!(live.tmp_is_empty());
        assert_eq!(live.fx.entry(ID).state, EntryState::Gone);
    }

    #[test]
    fn a_volume_with_room_for_the_package_twice_unearths_it() {
        let live = Live::own();
        let plan = live.plan(&live.fx.request(ID));
        let room = after_tmp(live.fx.collection.tmp_dir(), 2 * plan.payload.size);
        let rename: super::super::local::Rename<'_> = &crate::fsx::moves::move_dir;

        let report = live
            .unearth_through(
                &live.fx.request(ID),
                Some(&live.remote),
                &Live::environment(&room, &rename),
            )
            .expect("unearths");

        assert!(report.unearthed, "{report:?}");
    }

    #[test]
    fn a_dry_run_counts_the_room_of_a_live_package_as_the_real_run_checks_it() {
        let live = Live::own();
        let refresh = |lock| {
            refresh_with(
                Ok(Box::new(FaultRemote::new(path_remote(
                    live.root.path(),
                    "imperius",
                )))),
                &live.fx.collection,
                lock,
                id(ID),
                live.fx.clock.now(),
            )
            .expect("refreshed")
        };
        let needed = |refreshed: &Refreshed| match refreshed
            .plan(&live.fx.collection, &live.fx.request(ID))
            .expect("planned")
        {
            Planned::Unearth(plan) => plan.needed,
            other => panic!("{other:?}"),
        };

        let dry = needed(&refresh(None));
        let real = needed(&refresh(Some(&live.fx.lock)));

        assert_eq!(dry, real);
        assert_eq!(dry, 2 * live.plan(&live.fx.request(ID)).payload.size);
    }

    #[test]
    fn a_volume_that_fits_the_package_once_skips_before_it_moves() {
        let live = Live::own();
        let plan = live.plan(&live.fx.request(ID));
        let room = after_tmp(live.fx.collection.tmp_dir(), plan.payload.size);
        let rename: super::super::local::Rename<'_> = &crate::fsx::moves::move_dir;

        let report = live
            .unearth_through(
                &live.fx.request(ID),
                Some(&live.remote),
                &Live::environment(&room, &rename),
            )
            .expect("skips");

        assert!(!report.unearthed);
        assert_eq!(live.remote.count(Op::GetPayload), 0);
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_full_volume_while_the_package_is_fetched_skips_the_piece() {
        let live = Live::own();
        let full = Full {
            inner: &live.remote,
            file: live.file.as_str().to_string(),
        };
        let rename: super::super::local::Rename<'_> = &crate::fsx::moves::move_dir;

        let report = live
            .unearth_through(
                &live.fx.request(ID),
                Some(&full),
                &Live::environment(&available_space, &rename),
            )
            .expect("skips");

        assert!(!report.unearthed);
        assert!(
            matches!(report.skipped, Some(Skipped::NoSpace { .. })),
            "{report:?}"
        );
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_remote_that_lost_the_package_says_so() {
        let live = Live::own();
        fs::remove_file(live.on_remote()).expect("gone");

        let error = live.unearth().expect_err("fails");

        assert!(
            matches!(&error, UnearthError::NotOnRemote { file, path: at }
                if file == live.file.as_str() && *at == path("projects/a")),
            "{error}"
        );
        assert!(live.tmp_is_empty());
    }

    #[test]
    fn a_plan_that_needs_the_remote_fails_without_one() {
        let live = Live::own();
        let rename: super::super::local::Rename<'_> = &crate::fsx::moves::move_dir;

        let error = live
            .unearth_through(
                &live.fx.request(ID),
                None,
                &Live::environment(&available_space, &rename),
            )
            .expect_err("fails");

        assert!(
            matches!(error, UnearthError::NoLocalArchive { .. }),
            "{error}"
        );
    }

    #[test]
    fn the_phase_begins_before_the_package_is_fetched() {
        let live = Live::own();

        live.unearth().expect("the unearth works");

        let calls = live.remote.calls();
        assert_eq!(calls[0].op, Op::BeginPhase);
        assert_eq!(live.remote.count(Op::BeginPhase), 1);
        assert_eq!(live.remote.count(Op::GetPayload), 1);
    }

    #[test]
    fn the_command_applies_a_fork_under_the_guard_of_the_run() {
        let live = Live::foreign();
        let plan = live.plan(&live.fx.request(ID));
        assert_eq!(plan.source(), UnearthSource::Fork);
        let remote = live.remote;
        let Fixture {
            collection,
            clock,
            lock,
            temp: _temp,
        } = live.fx;
        drop(lock);
        let crate::run::Started::Guarded(guard) =
            crate::run::start(&collection, &clock, false).expect("the lock is free")
        else {
            unreachable!("a real run is guarded");
        };

        let report = super::super::apply_with_remote(
            &collection,
            &plan,
            &guard,
            &clock,
            &SeqIds::new(1),
            Some(&remote),
        )
        .expect("works");

        assert_eq!(report.id, minted());
        assert!(collection.root().join("projects/a/notes.txt").exists());
    }

    // ---------------------------------------------------------------------
    // A fork that stopped half way
    // ---------------------------------------------------------------------

    fn crashed_before_the_rename(live: &Live) {
        let entry = fork_entry(minted(), &path("projects/a"), time(NOW));
        live.fx.write_entry(&mut entry.clone());
    }

    fn next_run(live: &Live) -> Vec<String> {
        repair::run(
            &live.fx.collection,
            &live.fx.lock,
            &live.fx.clock,
            &RealEffects,
        )
        .expect("the repairs run")
        .into_iter()
        .map(|warning| warning.message)
        .collect()
    }

    #[test]
    fn a_fork_that_stopped_before_the_rename_leaves_nothing_after_the_next_run() {
        let live = Live::foreign();
        crashed_before_the_rename(&live);

        let warnings = next_run(&live);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            tracking::read(&live.fx.collection, minted())
                .expect("reads")
                .is_none()
        );
        assert!(!live.fx.collection.object_dir(minted()).exists());
    }

    #[test]
    fn a_fork_that_stopped_after_the_rename_leaves_the_folder_as_a_new_piece() {
        let mut live = Live::foreign();
        crashed_before_the_rename(&live);
        live.fx.tree().file("projects/a/notes.txt", "hello");

        let warnings = next_run(&live);

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("is not the unearthed tree"),
            "{warnings:?}"
        );
        assert!(
            tracking::read(&live.fx.collection, minted())
                .expect("reads")
                .is_none()
        );
        assert!(live.fx.at("projects/a/notes.txt").exists());
    }

    #[test]
    fn a_fork_that_stopped_after_its_metadata_gets_a_live_entry() {
        let mut live = Live::foreign();
        crashed_before_the_rename(&live);
        live.fx.tree().file("projects/a/notes.txt", "hello");
        let forked = forked_new(
            minted(),
            id(COLLECTION_ID),
            &path("projects/a"),
            "imperius",
            time(NOW),
        );
        Store::new(&live.fx.collection)
            .write_pending(&live.fx.lock, &forked)
            .expect("written");

        let warnings = next_run(&live);

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = tracking::read(&live.fx.collection, minted())
            .expect("reads")
            .expect("an entry");
        assert_eq!(entry.state, EntryState::Live);
        assert_eq!(entry.path, path("projects/a"));
        assert_eq!(entry.unearthed, None);
    }
}
