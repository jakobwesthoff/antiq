// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Finding a collection and its local layout.

use crate::code_span::CodeSpan;
use crate::config::{Config, ConfigError};
use crate::error::{ErrorKind, HasKind};
use crate::id::ObjectId;
use std::fs;
use std::path::{Path, PathBuf};

/// The directory that marks a collection.
const ANTIQ_DIR: &str = ".antiq";

/// The directories below `.antiq/` that exist from the start, in the order
/// `init` makes them. `lock` is made by the first run that needs it.
pub(crate) const LAYOUT: [&str; 5] = ["tmp", "trash", "doomed", "recovered", "pieces"];

/// A collection: the directory that holds `.antiq/`, with the config read
/// from it.
#[derive(Debug)]
pub struct Collection {
    root: PathBuf,
    config: Config,
}

impl Collection {
    pub(crate) fn new(root: PathBuf, config: Config) -> Collection {
        Collection { root, config }
    }

    /// The root of the collection that `start` lies in, found like git finds
    /// a repository: the nearest directory at or above `start` that holds
    /// `.antiq/`. The config is not read, so a root can be named even when
    /// its config is broken.
    pub fn find_root(start: &Path) -> Result<PathBuf, CollectionError> {
        // Resolving the start first makes `..` and links behave as they do
        // for git, and gives every root in one canonical spelling.
        let resolved = fs::canonicalize(start).map_err(|source| CollectionError::Io {
            path: start.to_path_buf(),
            source,
        })?;
        // The marker is looked at without following links: a link to
        // another collection's `.antiq` would borrow that collection's
        // store and lock.
        resolved
            .ancestors()
            .find(|dir| {
                fs::symlink_metadata(dir.join(ANTIQ_DIR)).is_ok_and(|marker| marker.is_dir())
            })
            .map(Path::to_path_buf)
            .ok_or_else(|| CollectionError::NotACollection {
                start: start.to_path_buf(),
            })
    }

    /// Finds the collection `start` lies in and reads its config.
    pub fn find(start: &Path) -> Result<Collection, CollectionError> {
        let root = Collection::find_root(start)?;
        let config = Config::read(&root.join(ANTIQ_DIR).join("config.toml"))?;
        Ok(Collection::new(root, config))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn id(&self) -> ObjectId {
        self.config.collection.id
    }

    pub fn antiq_dir(&self) -> PathBuf {
        self.root.join(ANTIQ_DIR)
    }

    pub fn lock_path(&self) -> PathBuf {
        self.antiq_dir().join("lock")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.antiq_dir().join("tmp")
    }

    pub fn doomed_dir(&self) -> PathBuf {
        self.antiq_dir().join("doomed")
    }

    pub fn pieces_dir(&self) -> PathBuf {
        self.antiq_dir().join("pieces")
    }

    /// The folder to measure the device and the free space of the staging
    /// area on: `tmp/`, or `.antiq/` while `tmp/` is missing, since a run
    /// creates `tmp/` there before it stages anything. A dry run asks this
    /// before the run has created it.
    pub fn measured_tmp_dir(&self) -> PathBuf {
        let tmp = self.tmp_dir();
        if fs::symlink_metadata(&tmp).is_ok() {
            tmp
        } else {
            self.antiq_dir()
        }
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.antiq_dir().join("trash")
    }
}

#[cfg(test)]
impl Collection {
    /// A collection for tests that only need a value to pass around. Nothing
    /// exists at its root.
    pub fn for_tests() -> Self {
        let id = crate::testing::tree::COLLECTION_ID
            .parse()
            .expect("the fixture ID is a ULID");
        Collection::new(PathBuf::from("/collection-for-tests"), Config::new(id))
    }
}

/// Errors of finding a collection.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum CollectionError {
    #[diagnostic(
        code(antiq::collection::not_a_collection),
        help("Change into your collection, or create one here with `antiq init`.")
    )]
    #[error(
        "{} is not inside an antiq collection, which has an `.antiq/` folder and none of its parent directories has one",
        CodeSpan(.start.display())
    )]
    NotACollection { start: PathBuf },
    #[error("cannot look for a collection at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    #[diagnostic(transparent)]
    Config(#[from] ConfigError),
}

impl HasKind for CollectionError {
    fn kind(&self) -> ErrorKind {
        match self {
            CollectionError::NotACollection { .. } => ErrorKind::NotACollection,
            CollectionError::Io { .. } => ErrorKind::Other,
            CollectionError::Config(error) => error.kind(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tree::{COLLECTION_ID, TempCollection, Tree};
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Roots are canonical, and temp directories are not on macOS (`/var` is
    /// a link to `private/var`), so expectations are canonicalised too.
    fn canonical(path: &Path) -> PathBuf {
        fs::canonicalize(path).expect("the path exists")
    }

    fn id(text: &str) -> ObjectId {
        text.parse().expect("a ULID")
    }

    #[test]
    fn finds_the_root_from_a_nested_directory() {
        let mut temp = TempCollection::minimal();
        let nested = temp.tree().dir("projects/web/src");
        let found = Collection::find(&nested).expect("inside a collection");
        assert_eq!(found.root(), canonical(temp.root()));
        assert_eq!(found.id(), id(COLLECTION_ID));
    }

    #[test]
    fn finds_the_root_from_the_root_itself() {
        let temp = TempCollection::minimal();
        let found = Collection::find(temp.root()).expect("at the root");
        assert_eq!(found.root(), canonical(temp.root()));
    }

    #[test]
    fn finds_the_root_through_dotdot() {
        let mut temp = TempCollection::minimal();
        temp.tree().dir("a/b");
        let start = temp.root().join("a/b/../..");
        let found = Collection::find(&start).expect("inside a collection");
        assert_eq!(found.root(), canonical(temp.root()));
    }

    #[test]
    fn finds_the_root_through_a_symlinked_ancestor() {
        let mut temp = TempCollection::minimal();
        temp.tree().dir("deep/er");
        let links = Tree::new();
        let link = links.root().join("link");
        symlink(temp.root(), &link).expect("a symlink");
        let found = Collection::find(&link.join("deep/er")).expect("inside a collection");
        assert_eq!(found.root(), canonical(temp.root()));
    }

    #[test]
    fn the_nearest_collection_wins() {
        let mut temp = TempCollection::minimal();
        let inner = temp.tree().dir("inner");
        temp.tree().dir("inner/.antiq");
        temp.tree().file(
            "inner/.antiq/config.toml",
            "[collection]\nid = \"01BX5ZZKBKACTAV9WEVGEMMVRY\"\n",
        );
        let found = Collection::find(&inner).expect("inside a collection");
        assert_eq!(found.root(), canonical(&inner));
        assert_eq!(found.id(), id("01BX5ZZKBKACTAV9WEVGEMMVRY"));
    }

    #[test]
    fn a_file_named_antiq_is_no_marker() {
        let mut temp = TempCollection::minimal();
        let inner = temp.tree().dir("inner");
        temp.tree().file("inner/.antiq", "not a directory");
        let found = Collection::find(&inner).expect("the outer collection");
        assert_eq!(found.root(), canonical(temp.root()));
    }

    #[test]
    fn a_symlink_named_antiq_is_no_marker() {
        let mut temp = TempCollection::minimal();
        let inner = temp.tree().dir("inner");
        let target = temp.root().join(".antiq");
        temp.tree().symlink("inner/.antiq", target);
        let found = Collection::find(&inner).expect("the outer collection");
        assert_eq!(found.root(), canonical(temp.root()));
    }

    #[test]
    fn outside_any_collection_is_not_a_collection() {
        let mut tree = Tree::new();
        let start = tree.dir("somewhere/deeper");
        let outside = canonical(tree.root());
        assert!(
            outside.ancestors().all(|dir| !dir.join(".antiq").exists()),
            "precondition: no directory above the temp dir {} holds .antiq",
            outside.display()
        );
        let error = Collection::find(&start).expect_err("no collection here");
        assert_eq!(error.kind(), ErrorKind::NotACollection);
        assert!(error.to_string().contains("somewhere/deeper"), "{error}");
    }

    #[test]
    fn a_missing_start_is_an_io_error() {
        let tree = Tree::new();
        let start = tree.root().join("missing");
        let error = Collection::find_root(&start).expect_err("nothing there");
        assert!(matches!(error, CollectionError::Io { .. }), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(error.to_string().contains("missing"), "{error}");
    }

    #[test]
    fn find_root_names_a_root_whose_config_is_broken() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/config.toml", "[collection\n");
        let nested = temp.tree().dir("a");
        let root = Collection::find_root(&nested).expect("the marker is enough");
        assert_eq!(root, canonical(temp.root()));
    }

    #[test]
    fn find_reports_a_broken_config_with_its_path() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(".antiq/config.toml", "[collection\n");
        let error = Collection::find(temp.root()).expect_err("the config is broken");
        assert!(matches!(error, CollectionError::Config(_)), "{error}");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn find_fails_reading_when_config_toml_is_missing() {
        let mut tree = Tree::new();
        tree.dir(".antiq");
        let error = Collection::find(tree.root()).expect_err("there is no config");
        assert!(matches!(error, CollectionError::Config(_)), "{error}");
        assert!(error.to_string().contains("config.toml"), "{error}");
    }

    #[test]
    fn layout_paths_match_the_spec_listing() {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        let antiq = canonical(temp.root()).join(".antiq");
        assert_eq!(collection.antiq_dir(), antiq);
        assert_eq!(collection.lock_path(), antiq.join("lock"));
        assert_eq!(collection.tmp_dir(), antiq.join("tmp"));
        assert_eq!(collection.trash_dir(), antiq.join("trash"));
        assert_eq!(collection.doomed_dir(), antiq.join("doomed"));
        assert_eq!(collection.pieces_dir(), antiq.join("pieces"));
    }

    #[test]
    fn the_staging_area_is_measured_on_tmp_or_on_antiq_while_tmp_is_missing() {
        let temp = TempCollection::minimal();
        let collection = Collection::find(temp.root()).expect("a collection");
        assert_eq!(collection.measured_tmp_dir(), collection.tmp_dir());

        fs::remove_dir(collection.tmp_dir()).expect("tmp is empty");

        assert_eq!(collection.measured_tmp_dir(), collection.antiq_dir());
    }

    #[test]
    fn the_config_is_the_one_read_from_disk() {
        let mut temp = TempCollection::minimal();
        temp.tree().file(
            ".antiq/config.toml",
            format!("[collection]\nid = \"{COLLECTION_ID}\"\nidle_days = 99\n"),
        );
        let collection = Collection::find(temp.root()).expect("a collection");
        assert_eq!(collection.config().collection.idle_days, 99);
    }

    #[test]
    fn the_test_collection_has_the_fixture_id() {
        assert_eq!(Collection::for_tests().id(), id(COLLECTION_ID));
    }
}
