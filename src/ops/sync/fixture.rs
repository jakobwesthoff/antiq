// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Two machines on one path remote, for the tests of `sync`.

use crate::clock::Clock;
use crate::clock::FixedClock;
use crate::collection::Collection;
use crate::id::{ObjectId, SeqIds};
use crate::lock;
use crate::meta::{HistoryAction, HistoryEntry};
use crate::ops::import::{ImportRequest, import};
use crate::ops::sync::{SyncAction, SyncReport, SyncRequest, Synced, sync_using};
use crate::pack::pool::Pool;
use crate::remote::Remote;
use crate::remote::testing::{FaultRemote, REMOTE_ID, path_remote, remote_root};
use crate::store::Store;
use crate::testing::package::pack_piece;
use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
use std::fs;
use std::path::{Path, PathBuf};

const OTHER_COLLECTION_ID: &str = "01D39ZY06FGSCTVN4T2V9PKHFZ";

/// A remote root that several machines share.
pub(super) struct Shared {
    pub root: tempfile::TempDir,
}

impl Shared {
    pub fn new() -> Shared {
        Shared {
            root: remote_root(),
        }
    }

    pub fn objects(&self) -> PathBuf {
        self.root.path().join("objects")
    }

    /// The names in an object's directory on the remote, sorted.
    pub fn names(&self, id: ObjectId) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.objects().join(id.to_string()))
            .map(|entries| {
                entries
                    .map(|entry| {
                        entry
                            .expect("an entry")
                            .file_name()
                            .to_string_lossy()
                            .into()
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    pub fn read(&self, id: ObjectId, name: &str) -> Vec<u8> {
        fs::read(self.objects().join(id.to_string()).join(name)).expect("the remote file")
    }
}

/// One machine: a collection with a name, attached to the shared remote.
pub(super) struct Machine {
    pub name: &'static str,
    _temp: TempCollection,
    pub collection: Collection,
    pub clock: FixedClock,
    pub ids: SeqIds,
    pub pool: Pool,
    inputs: tempfile::TempDir,
    remote_root: PathBuf,
}

impl Machine {
    /// `anna` and `ben` get their own collection IDs.
    pub fn new(shared: &Shared, name: &'static str) -> Machine {
        Machine::with_jobs(shared, name, 1)
    }

    /// A machine that runs `jobs` remote operations at once.
    pub fn with_jobs(shared: &Shared, name: &'static str, jobs: u32) -> Machine {
        Machine::configured(shared, name, jobs, "")
    }

    /// A machine whose `[collection]` table has the lines `extra` besides
    /// its ID.
    pub fn with_settings(shared: &Shared, name: &'static str, extra: &str) -> Machine {
        Machine::configured(shared, name, 1, extra)
    }

    fn configured(shared: &Shared, name: &'static str, jobs: u32, extra: &str) -> Machine {
        let id = if name == "anna" {
            COLLECTION_ID
        } else {
            OTHER_COLLECTION_ID
        };
        let mut temp = TempCollection::minimal();
        let config = format!(
            "[collection]\nid = \"{id}\"\nremote_jobs = {jobs}\n{extra}\n\n[machine]\nname = \"{name}\"\n\n[[remote]]\nurl = \"{}\"\nid = \"{REMOTE_ID}\"\n",
            shared.root.path().display()
        );
        temp.tree().file(".antiq/config.toml", config);
        let collection = Collection::find(temp.root()).expect("a collection");
        Machine {
            name,
            _temp: temp,
            collection,
            clock: FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time")),
            ids: SeqIds::new(if name == "anna" { 1 } else { 100 }),
            pool: Pool::new(2, 1 << 30),
            inputs: tempfile::tempdir().expect("a temp dir"),
            remote_root: shared.root.path().to_path_buf(),
        }
    }

    /// Adds the line `setting` to the `[collection]` table of the config
    /// and reads the collection again.
    pub fn set_setting(&mut self, setting: &str) {
        let path = self.collection.root().join(".antiq/config.toml");
        let config = fs::read_to_string(&path).expect("the config reads");
        let config = config.replace("\n\n[machine]", &format!("\n{setting}\n\n[machine]"));
        fs::write(&path, config).expect("the config is written");
        self.collection = Collection::find(self.collection.root()).expect("a collection");
    }

    pub fn store(&self) -> Store<'_> {
        Store::new(&self.collection)
    }

    /// This machine's view of the shared remote.
    pub fn remote(&self) -> FaultRemote {
        FaultRemote::new(path_remote(&self.remote_root, self.name))
    }

    /// Runs a real `sync` against this machine's view of the remote.
    pub fn sync(&self) -> SyncReport {
        self.sync_with(&self.remote())
    }

    pub fn sync_with(&self, remote: &dyn Remote) -> SyncReport {
        match self.sync_request(remote, false).expect("the sync runs") {
            Synced::Done(report) => report,
            Synced::Planned { .. } => panic!("expected a real run"),
        }
    }

    /// A dry run against this machine's view of the remote.
    pub fn plan(&self) -> Vec<SyncAction> {
        match self
            .sync_request(&self.remote(), true)
            .expect("the dry run works")
        {
            Synced::Planned { plan, .. } => plan.actions,
            Synced::Done(_) => panic!("expected a dry run"),
        }
    }

    /// The result of a run that mints its IDs from this machine's sequence.
    pub fn sync_request(
        &self,
        remote: &dyn Remote,
        dry_run: bool,
    ) -> Result<Synced, crate::error::Error> {
        sync_using(&SyncRequest {
            collection: &self.collection,
            clock: &self.clock,
            remote,
            ids: &self.ids,
            pool: (!dry_run).then_some(&self.pool),
            dry_run,
        })
    }

    /// A folder with one file in it at the top of the collection: a new
    /// piece, as the user's own folder is.
    pub fn folder(&self, name: &str, file: &str, content: &str) -> PathBuf {
        let folder = self.collection.root().join(name);
        fs::create_dir_all(&folder).expect("the folder is made");
        fs::write(folder.join(file), content).expect("the file is written");
        folder
    }

    /// The tracking entry of `id`.
    pub fn entry(&self, id: ObjectId) -> crate::tracking::TrackingEntry {
        crate::tracking::read(&self.collection, id)
            .expect("the entry reads")
            .expect("there is an entry")
    }

    /// Records a change to `id`'s metadata that adds a history entry, as a
    /// command of this machine would.
    pub fn record_history(&self, id: ObjectId, action: HistoryAction) {
        let current = self
            .store()
            .current(id)
            .expect("reads")
            .expect("the object exists");
        let mut metadata = current.metadata;
        metadata
            .history
            .push(HistoryEntry::new(self.clock.now(), self.name, action));
        let lock =
            lock::acquire(&self.collection.lock_path(), self.name, &self.clock).expect("free");
        self.store()
            .write_pending(&lock, &metadata)
            .expect("recorded");
    }

    /// Imports a package whose piece holds one file, as a new object with a
    /// recorded change and a tracking entry that is `Pending`.
    pub fn import_piece(&self, file_name: &str, content: &str) -> ObjectId {
        let before = self.store().objects().expect("lists");
        let mut tree = Tree::new();
        tree.file_with(&format!("proj/{file_name}"), content, 0o644, 1_000_000_000);
        let input = self.inputs.path().join(format!("{file_name}.tar.zst"));
        fs::write(&input, pack_piece(&tree, "proj").bytes).expect("the input is written");
        let files = [input];
        let request = ImportRequest {
            collection: &self.collection,
            cwd: self.collection.root(),
            files: &files,
            path: Some(Path::new(file_name)),
        };
        let report = import(&request, false, &self.clock, &self.ids).expect("the import runs");
        assert!(
            report.refused.is_empty() && report.skipped.is_empty(),
            "{report:?}"
        );
        let after = self.store().objects().expect("lists");
        *after
            .iter()
            .find(|id| !before.contains(id))
            .expect("the import made an object")
    }

    /// Puts the recorded change of `id` on the remote as `meta.<number>`
    /// together with the files it names, as another machine's commit
    /// would have.
    pub fn publish_by_hand(&self, shared: &Shared, id: ObjectId, number: u32) {
        let local = self.collection.object_dir(id);
        let remote = shared.objects().join(id.to_string());
        fs::create_dir_all(&remote).expect("the object directory");
        for entry in fs::read_dir(&local).expect("lists") {
            let entry = entry.expect("an entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let target = if name == "meta.pending" {
                format!("meta.{number}")
            } else if name.starts_with("meta.") {
                continue;
            } else {
                name
            };
            fs::copy(entry.path(), remote.join(target)).expect("copied");
        }
    }
}
