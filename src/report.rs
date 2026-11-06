// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every command reports back.

use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::device::{self, FsKind};
use crate::lock;
use serde::Serialize;

/// Something the user should know that did not stop the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub message: String,
}

impl Warning {
    pub fn new(message: impl Into<String>) -> Self {
        Warning {
            message: message.into(),
        }
    }
}

/// Implemented by the report of every command. The binary wraps the
/// serialised report in the JSON envelope and takes the exit code from
/// [`CommandReport::partial`].
pub trait CommandReport: Serialize {
    /// Whether some objects were skipped (exit code 3).
    fn partial(&self) -> bool;

    fn warnings(&self) -> &[Warning];
}

/// What every command tells about the collection itself, before its own
/// findings, in this order: the `[collection]` keys the config does not
/// know, a collection on an SMB or NFS mount, and the run that holds the
/// lock. The filesystem is probed for real.
pub fn notes(collection: &Collection) -> Vec<Warning> {
    notes_with(collection, device::fs_kind(collection.root()))
}

/// [`notes`] with the filesystem kind given, so that a test can put the
/// collection on a mount it cannot create.
pub(crate) fn notes_with(
    collection: &Collection,
    fs_kind: Result<FsKind, FsxError>,
) -> Vec<Warning> {
    let mut warnings = Vec::new();
    let mut unknown_keys = collection.config().unknown_collection_keys.clone();
    unknown_keys.sort();
    for key in &unknown_keys {
        warnings.push(Warning::new(format!(
            "the key `{key}` under [collection] is unknown and ignored"
        )));
    }
    match fs_kind {
        Ok(FsKind::Smb) => warnings.push(Warning::new("the collection is on an SMB mount")),
        Ok(FsKind::Nfs) => warnings.push(Warning::new("the collection is on an NFS mount")),
        Ok(FsKind::Local | FsKind::Other) => {}
        Err(error) => warnings.push(Warning::new(format!(
            "cannot tell what kind of filesystem the collection is on: {error}"
        ))),
    }
    // The lock is only read: taking it would make the caller a run of its own.
    match lock::holder(&collection.lock_path()) {
        Ok(Some(holder)) => warnings.push(Warning::new(format!(
            "another run holds the lock (pid {}, machine {}, started {})",
            holder.pid, holder.machine, holder.started
        ))),
        Ok(None) => {}
        Err(error) => warnings.push(Warning::new(format!("cannot read the lock file: {error}"))),
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use serde::Serialize;
    use serde_json::json;

    fn collection_with(config_lines: &str) -> (TempCollection, Collection) {
        let temp = TempCollection::minimal();
        std::fs::write(
            temp.root().join(".antiq/config.toml"),
            format!("[collection]\nid = \"{COLLECTION_ID}\"\n{config_lines}\n"),
        )
        .expect("a config can be written");
        let collection = Collection::find(temp.root()).expect("a collection");
        (temp, collection)
    }

    fn messages(warnings: &[Warning]) -> Vec<&str> {
        warnings.iter().map(|w| w.message.as_str()).collect()
    }

    #[test]
    fn a_plain_local_collection_has_no_notes() {
        let (_temp, collection) = collection_with("");
        assert!(notes_with(&collection, Ok(FsKind::Local)).is_empty());
    }

    #[test]
    fn every_unknown_key_is_a_note_in_alphabetical_order() {
        let (_temp, collection) = collection_with("idle_dyas = 3\nbogus = 1");
        let notes = notes_with(&collection, Ok(FsKind::Local));
        assert_eq!(
            messages(&notes),
            [
                "the key `bogus` under [collection] is unknown and ignored",
                "the key `idle_dyas` under [collection] is unknown and ignored",
            ]
        );
    }

    #[test]
    fn a_collection_on_smb_or_nfs_is_a_note() {
        let (_temp, collection) = collection_with("");
        assert_eq!(
            messages(&notes_with(&collection, Ok(FsKind::Smb))),
            ["the collection is on an SMB mount"]
        );
        assert_eq!(
            messages(&notes_with(&collection, Ok(FsKind::Nfs))),
            ["the collection is on an NFS mount"]
        );
    }

    #[test]
    fn a_filesystem_that_cannot_be_told_is_a_note() {
        let (_temp, collection) = collection_with("");
        let failed = Err(FsxError::Io {
            action: "stat",
            path: "/work".into(),
            source: std::io::Error::other("refused by the test"),
        });
        let notes = notes_with(&collection, failed);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0]
                .message
                .starts_with("cannot tell what kind of filesystem the collection is on"),
            "{notes:?}"
        );
    }

    #[test]
    fn the_run_that_holds_the_lock_is_a_note_after_the_others() {
        let (_temp, collection) = collection_with("bogus = 1");
        let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
        let _held = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("free");
        let notes = notes_with(&collection, Ok(FsKind::Smb));
        assert_eq!(
            messages(&notes),
            [
                "the key `bogus` under [collection] is unknown and ignored",
                "the collection is on an SMB mount",
                &format!(
                    "another run holds the lock (pid {}, machine imperius, started 2026-11-04T10:00:00Z)",
                    std::process::id()
                ),
            ]
        );
    }

    #[test]
    fn a_lock_file_that_cannot_be_read_is_a_note() {
        let (mut temp, collection) = collection_with("");
        temp.tree().dir(".antiq/lock");
        let notes = notes_with(&collection, Ok(FsKind::Local));
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].message.starts_with("cannot read the lock file"),
            "{notes:?}"
        );
    }

    #[test]
    fn the_real_probe_finds_a_temp_directory_local_or_other() {
        let (_temp, collection) = collection_with("");
        let notes = notes(&collection);
        assert!(
            notes.iter().all(|note| !note.message.contains("mount")),
            "{notes:?}"
        );
    }

    #[derive(Serialize)]
    struct ExampleReport {
        skipped: usize,
        warnings: Vec<Warning>,
    }

    impl CommandReport for ExampleReport {
        fn partial(&self) -> bool {
            self.skipped > 0
        }

        fn warnings(&self) -> &[Warning] {
            &self.warnings
        }
    }

    #[test]
    fn a_report_says_whether_it_is_partial_and_lists_its_warnings() {
        let report = ExampleReport {
            skipped: 1,
            warnings: vec![Warning::new("a piece needs a decision")],
        };
        assert!(report.partial());
        assert_eq!(report.warnings()[0].message, "a piece needs a decision");
        assert_eq!(
            serde_json::to_value(&report).expect("report serialises"),
            json!({
                "skipped": 1,
                "warnings": [{"message": "a piece needs a decision"}],
            })
        );
    }
}
