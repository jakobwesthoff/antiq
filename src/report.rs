// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every command reports back.

use crate::code_span::CodeSpan;
use crate::collection::Collection;
use crate::filesystem::FilesystemError;
use crate::filesystem::device::{self, FsKind};
use crate::lock::{self, Holder};
use crate::shown::{escape_controls, escape_lines};
use miette::Diagnostic;
use serde::Serialize;
use serde::ser::SerializeMap;
use std::fmt::Display;

/// Something the user should know that did not stop the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub message: String,
    pub kind: Option<WarningKind>,
    /// The value a [`Warning::marked`] message starts with.
    lead: Option<String>,
}

/// What a warning is about. The message, the help and the code of a kind
/// are what the console and the JSON output show of it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, miette::Diagnostic)]
pub enum WarningKind {
    /// A key of the config that antiq does not know, under `[collection]`
    /// or `[target]`.
    #[diagnostic(
        code(antiq::config::unknown_key),
        severity(Warning),
        help("Correct the name or remove the line in `.antiq/config.toml`.")
    )]
    #[error("unknown setting {} under [{table}], which antiq ignores", CodeSpan(.key))]
    UnknownKey { table: &'static str, key: String },
    /// The collection is on a network mount, `SMB` or `NFS`.
    #[diagnostic(code(antiq::filesystem::network_mount), severity(Warning))]
    #[error("the antiq collection is on an {filesystem} mount")]
    NetworkMount { filesystem: &'static str },
    /// The filesystem of the collection could not be probed.
    #[diagnostic(code(antiq::filesystem::unknown), severity(Warning))]
    #[error("cannot tell what kind of filesystem the antiq collection is on: {reason}")]
    FilesystemUnknown { reason: String },
    /// The lock file could not be read.
    #[diagnostic(code(antiq::lock::unreadable), severity(Warning))]
    #[error("cannot read the lock file: {reason}")]
    LockUnreadable { reason: String },
    /// Another run holds the lock. A note more than a warning: nothing is
    /// wrong, the run is simply there.
    #[diagnostic(code(antiq::lock::in_use), severity(Advice))]
    #[error(
        "another antiq run holds the lock: its pid is {pid} on machine {machine}, started {started}"
    )]
    LockHeld {
        pid: u32,
        machine: String,
        started: String,
    },
    /// An installed copy of the agent skill is older than this antiq. The
    /// binary searches the skills directories and words both texts, since
    /// they name the copies it found.
    #[diagnostic(code(antiq::skill::outdated), severity(Warning), help("{fix}"))]
    #[error("{message}")]
    SkillOutdated { message: String, fix: String },
}

impl Warning {
    pub fn new(message: impl Into<String>) -> Self {
        Warning {
            message: escape_controls(&message.into()),
            kind: None,
            lead: None,
        }
    }

    /// A warning that starts with a value from the user's files, such as a
    /// piece path, and goes on with `rest`. The value stands in a code span,
    /// so a console shows it as it is spelled and does not capitalise it.
    pub fn marked(lead: impl Display, rest: impl Display) -> Self {
        let lead = lead.to_string();
        Warning {
            message: format!("{}{}", CodeSpan(&lead), escape_controls(&rest.to_string())),
            kind: None,
            lead: Some(lead),
        }
    }

    /// The value and the text after it, for a warning made by
    /// [`Warning::marked`].
    pub fn lead(&self) -> Option<(&str, &str)> {
        let lead = self.lead.as_deref()?;
        let rest = self.message.get(CodeSpan(lead).to_string().len()..)?;
        Some((lead, rest))
    }

    pub fn of(kind: WarningKind) -> Self {
        Warning {
            message: escape_controls(&kind.to_string()),
            kind: Some(kind),
            lead: None,
        }
    }
}

/// A warning with a kind is `{code, message, fix}`, the fix when the kind
/// has a help. One without a kind is `{message}`.
impl Serialize for Warning {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serializer.serialize_map(None)?;
        if let Some(code) = self.kind.as_ref().and_then(|kind| kind.code()) {
            object.serialize_entry("code", &code.to_string())?;
        }
        object.serialize_entry("message", &self.message)?;
        if let Some(help) = self.kind.as_ref().and_then(|kind| kind.help()) {
            object.serialize_entry("fix", &escape_lines(&help.to_string()))?;
        }
        object.end()
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
/// know, the `[target]` keys it does not know, a collection on an SMB or
/// NFS mount, and the run that holds the
/// lock. The filesystem is probed for real.
pub fn notes(collection: &Collection) -> Vec<Warning> {
    notes_with(collection, device::fs_kind(collection.root()))
}

/// [`notes`] with the filesystem kind given, so that a test can put the
/// collection on a mount it cannot create.
pub(crate) fn notes_with(
    collection: &Collection,
    fs_kind: Result<FsKind, FilesystemError>,
) -> Vec<Warning> {
    let mut warnings = setting_notes_with(collection, fs_kind);
    // The lock is only read: taking it would make the caller a run of its own.
    match lock::holder(&collection.lock_path()) {
        Ok(Some(holder)) => warnings.push(lock_note(&holder)),
        Ok(None) => {}
        Err(error) => warnings.push(unreadable_lock(&error)),
    }
    warnings
}

/// What a command that holds the lock tells about the collection: the
/// notes of [`notes`] without the one about the lock, which would name the
/// command itself.
pub fn setting_notes(collection: &Collection) -> Vec<Warning> {
    setting_notes_with(collection, device::fs_kind(collection.root()))
}

/// [`setting_notes`] with the filesystem kind given.
pub(crate) fn setting_notes_with(
    collection: &Collection,
    fs_kind: Result<FsKind, FilesystemError>,
) -> Vec<Warning> {
    let mut warnings = Vec::new();
    let mut unknown_keys = collection.config().unknown_collection_keys.clone();
    unknown_keys.sort();
    for key in &unknown_keys {
        warnings.push(Warning::of(WarningKind::UnknownKey {
            table: "collection",
            key: key.clone(),
        }));
    }
    let mut unknown_target_keys = collection.config().unknown_target_keys.clone();
    unknown_target_keys.sort();
    for key in &unknown_target_keys {
        warnings.push(Warning::of(WarningKind::UnknownKey {
            table: "target",
            key: key.clone(),
        }));
    }
    match fs_kind {
        Ok(FsKind::Smb) => {
            warnings.push(Warning::of(WarningKind::NetworkMount { filesystem: "SMB" }))
        }
        Ok(FsKind::Nfs) => {
            warnings.push(Warning::of(WarningKind::NetworkMount { filesystem: "NFS" }))
        }
        Ok(FsKind::Local | FsKind::Other) => {}
        Err(error) => warnings.push(Warning::of(WarningKind::FilesystemUnknown {
            reason: error.to_string(),
        })),
    }
    warnings
}

/// What the run lock tells a command that takes none: the note for a run
/// that holds it, or a warning when the lock file cannot be read. At most
/// one of the two is set.
pub fn lock_status(collection: &Collection) -> (Option<String>, Option<Warning>) {
    match lock::holder(&collection.lock_path()) {
        Ok(Some(holder)) => (Some(lock_note(&holder).message), None),
        Ok(None) => (None, None),
        Err(error) => (None, Some(unreadable_lock(&error))),
    }
}

/// The note for a run that holds the lock while a command that takes none
/// looks at the collection.
pub fn lock_note(holder: &Holder) -> Warning {
    Warning::of(WarningKind::LockHeld {
        pid: holder.pid,
        machine: holder.machine.clone(),
        started: holder.started.to_string(),
    })
}

pub(crate) fn unreadable_lock(error: &dyn std::fmt::Display) -> Warning {
    Warning::of(WarningKind::LockUnreadable {
        reason: error.to_string(),
    })
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
    fn a_warning_message_shows_a_control_character_escaped() {
        assert_eq!(Warning::new("x\ny").message, "x%0Ay");
    }

    #[test]
    fn a_marked_warning_escapes_the_text_after_its_value() {
        let warning = Warning::marked("a\nb", ": cannot read\u{202e}");
        assert_eq!(warning.message, "`a%0Ab`: cannot read%E2%80%AE");
        assert_eq!(warning.lead(), Some(("a\nb", ": cannot read%E2%80%AE")));
    }

    #[test]
    fn the_message_of_a_warning_kind_is_escaped() {
        let warning = Warning::of(WarningKind::LockHeld {
            pid: 7,
            machine: "m\nx".into(),
            started: "now".into(),
        });
        assert_eq!(
            warning.message,
            "another antiq run holds the lock: its pid is 7 on machine m%0Ax, started now"
        );
    }

    #[test]
    fn the_fix_of_a_warning_in_json_is_escaped() {
        let warning = Warning::of(WarningKind::SkillOutdated {
            message: "old".into(),
            fix: "copy to a\tb\n\nthen \x1b[2J".into(),
        });
        let json = serde_json::to_value(&warning).expect("a warning serialises");
        assert_eq!(json["fix"], "copy to a%09b\n\nthen %1B[2J");
    }

    #[test]
    fn a_marked_warning_fences_its_value_and_hands_it_back_with_the_text_after_it() {
        let warning = Warning::marked("beta/a`b", ": cannot read `x`");
        assert_eq!(warning.message, "``beta/a`b``: cannot read `x`");
        assert_eq!(warning.lead(), Some(("beta/a`b", ": cannot read `x`")));
        assert_eq!(Warning::new("beta: plain").lead(), None);
        assert_eq!(
            serde_json::to_value(&warning).expect("serialises"),
            json!({"message": "``beta/a`b``: cannot read `x`"})
        );
    }

    #[test]
    fn a_warning_with_a_kind_serialises_with_its_code_and_its_fix() {
        let mount = Warning::of(WarningKind::NetworkMount { filesystem: "SMB" });
        assert_eq!(
            serde_json::to_string(&mount).expect("a warning serialises"),
            r#"{"code":"antiq::filesystem::network_mount","message":"the antiq collection is on an SMB mount"}"#
        );
        let unknown = Warning::of(WarningKind::UnknownKey {
            table: "collection",
            key: "bogus".into(),
        });
        assert_eq!(
            serde_json::to_string(&unknown).expect("a warning serialises"),
            r#"{"code":"antiq::config::unknown_key","message":"unknown setting `bogus` under [collection], which antiq ignores","fix":"Correct the name or remove the line in `.antiq/config.toml`."}"#
        );
    }

    #[test]
    fn a_note_has_the_severity_of_advice_and_the_others_of_a_warning() {
        use miette::{Diagnostic, Severity};
        let held = WarningKind::LockHeld {
            pid: 1,
            machine: "m".into(),
            started: "t".into(),
        };
        let unreadable = WarningKind::LockUnreadable { reason: "r".into() };
        assert_eq!(held.severity(), Some(Severity::Advice));
        assert_eq!(unreadable.severity(), Some(Severity::Warning));
    }

    #[test]
    fn the_notes_of_a_collection_carry_what_they_are_about() {
        let (_temp, collection) = collection_with("bogus = 1\n[target]\nrsynk = \"x\"");
        let kinds: Vec<Option<WarningKind>> = notes_with(&collection, Ok(FsKind::Smb))
            .into_iter()
            .map(|warning| warning.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                Some(WarningKind::UnknownKey {
                    table: "collection",
                    key: "bogus".into()
                }),
                Some(WarningKind::UnknownKey {
                    table: "target",
                    key: "rsynk".into()
                }),
                Some(WarningKind::NetworkMount { filesystem: "SMB" }),
            ]
        );
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
                "unknown setting `bogus` under [collection], which antiq ignores",
                "unknown setting `idle_dyas` under [collection], which antiq ignores",
            ]
        );
    }

    #[test]
    fn every_unknown_target_key_is_a_note_after_the_collection_ones() {
        let (_temp, collection) =
            collection_with("bogus = 1\n[target]\nrsynk = \"x\"\nfsync = false\nalpha = 1");
        let notes = notes_with(&collection, Ok(FsKind::Local));
        assert_eq!(
            messages(&notes),
            [
                "unknown setting `bogus` under [collection], which antiq ignores",
                "unknown setting `alpha` under [target], which antiq ignores",
                "unknown setting `rsynk` under [target], which antiq ignores",
            ]
        );
    }

    #[test]
    fn a_collection_on_smb_or_nfs_is_a_note() {
        let (_temp, collection) = collection_with("");
        assert_eq!(
            messages(&notes_with(&collection, Ok(FsKind::Smb))),
            ["the antiq collection is on an SMB mount"]
        );
        assert_eq!(
            messages(&notes_with(&collection, Ok(FsKind::Nfs))),
            ["the antiq collection is on an NFS mount"]
        );
    }

    #[test]
    fn a_filesystem_that_cannot_be_told_is_a_note() {
        let (_temp, collection) = collection_with("");
        let failed = Err(FilesystemError::Io {
            action: "stat",
            path: "/work".into(),
            source: std::io::Error::other("refused by the test"),
        });
        let notes = notes_with(&collection, failed);
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0]
                .message
                .starts_with("cannot tell what kind of filesystem the antiq collection is on"),
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
                "unknown setting `bogus` under [collection], which antiq ignores",
                "the antiq collection is on an SMB mount",
                &format!(
                    "another antiq run holds the lock: its pid is {} on machine imperius, started 2026-11-04T10:00:00Z",
                    std::process::id()
                ),
            ]
        );
    }

    #[test]
    fn the_notes_of_a_run_that_holds_the_lock_leave_the_lock_out() {
        let (_temp, collection) = collection_with("bogus = 1");
        let clock = FixedClock::new("2026-11-04T10:00:00Z".parse().expect("a time"));
        let _held = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("free");
        let notes = setting_notes_with(&collection, Ok(FsKind::Nfs));
        assert_eq!(
            messages(&notes),
            [
                "unknown setting `bogus` under [collection], which antiq ignores",
                "the antiq collection is on an NFS mount",
            ]
        );
    }

    #[test]
    fn the_real_probe_gives_the_notes_of_a_run_that_holds_the_lock() {
        let (_temp, collection) = collection_with("bogus = 1");
        assert_eq!(
            messages(&setting_notes(&collection)),
            ["unknown setting `bogus` under [collection], which antiq ignores"]
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
