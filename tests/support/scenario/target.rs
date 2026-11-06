// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A path target outside the collection, its marker and its copy back.

use super::collection::{Clock, Collection};
use crate::support::tree::Tree;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub struct Target {
    tree: Tree,
    /// Canonical, taken while the directory exists, so the name survives
    /// `remove`.
    root: PathBuf,
}

impl Target {
    pub fn new() -> Target {
        let tree = Tree::new();
        let root = fs::canonicalize(tree.root()).expect("the root exists");
        Target { tree, root }
    }

    pub fn path(&self) -> PathBuf {
        self.root.clone()
    }

    pub fn tree_mut(&mut self) -> &mut Tree {
        &mut self.tree
    }

    /// Deletes the directory: the target of a push that finds it missing.
    pub fn remove(&self) {
        fs::remove_dir_all(&self.root).expect("the target can be removed");
    }

    /// The name antiq gives this machine, which a marker made by this
    /// machine carries.
    pub fn machine_name(&self) -> String {
        antiq::machine::detect().expect("the machine has a name")
    }

    /// The marker `push --init` writes for `collection` from this machine.
    pub fn marker(&self, collection: &Collection) {
        let root = collection.root();
        self.marker_with(
            &collection.id(),
            &self.machine_name(),
            root.to_str().expect("the path is UTF-8"),
        );
    }

    /// A marker with any values: another machine, a moved collection.
    /// Spec, "Push": one line of JSON in a fixed key order.
    pub fn marker_with(&self, id: &str, host: &str, path: &str) {
        let quote = |text: &str| serde_json::to_string(text).expect("a string serialises");
        let line = format!(
            "{{\"format\":1,\"id\":{},\"host\":{},\"path\":{}}}\n",
            quote(id),
            quote(host),
            quote(path)
        );
        fs::write(self.root.join(".antiq-target"), line).expect("the marker can be written");
    }

    /// What restoring the collection from the target does: `rsync -aH`
    /// into a new directory, which becomes a collection of its own.
    pub fn copy_back(&self, clock: &Clock) -> Collection {
        self.copy_back_using("rsync", clock)
    }

    pub fn copy_back_using(&self, rsync: &str, clock: &Clock) -> Collection {
        let tree = Tree::new();
        let status = Command::new(rsync)
            .arg("-aH")
            .arg(format!("{}/", self.root.display()))
            .arg(format!("{}/", tree.root().display()))
            .status()
            .unwrap_or_else(|error| panic!("cannot run rsync ({rsync}): {error}"));
        assert!(status.success(), "rsync failed ({rsync}): {status}");
        Collection::around(tree, *clock)
    }
}
