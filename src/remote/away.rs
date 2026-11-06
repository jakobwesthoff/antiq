// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A remote that stays away: the times kept in `.antiq/remote-state.json`
//! and the warning about them.

use crate::clock::whole_seconds;
use crate::collection::Collection;
use crate::error::Error;
use crate::fsx::FsxError;
use crate::fsx::atomic::write_atomic;
use crate::lock::RunLock;
use crate::remote::{RemoteError, RemoteState};
use crate::report::Warning;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Map, Value};
use std::io;
use std::path::{Path, PathBuf};

/// Why the state file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum AwayError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not a JSON object, or a time in it is not an RFC 3339
    /// time.
    #[error("{path} is damaged: {reason}")]
    Damaged { path: PathBuf, reason: String },
    #[error(transparent)]
    Write(#[from] FsxError),
}

/// Key of the time a run last found the remote reachable.
const LAST_REACHABLE: &str = "last_reachable";
/// Key of the time a run first found the remote away since then.
const AWAY_FIRST_SEEN: &str = "away_first_seen";

/// Since when the remote has been out of reach: the last time a run found it
/// reachable, else the first time a run found it away. `None` when neither
/// is recorded.
pub fn away_since(collection: &Collection) -> Result<Option<Timestamp>, AwayError> {
    let path = collection.remote_state_path();
    let object = load(&path)?;
    for key in [LAST_REACHABLE, AWAY_FIRST_SEEN] {
        if let Some(time) = time_at(&object, key, &path)? {
            return Ok(Some(time));
        }
    }
    Ok(None)
}

/// Records that a run found the remote reachable, and forgets that it was
/// ever away. Only a run that holds the lock writes the file.
pub fn record_reachable(
    collection: &Collection,
    _lock: &RunLock,
    now: Timestamp,
) -> Result<(), AwayError> {
    update(collection, |object| {
        object.insert(
            LAST_REACHABLE.into(),
            Value::from(whole_seconds(now).to_string()),
        );
        object.remove(AWAY_FIRST_SEEN);
    })
}

/// Records the first time a run found the remote away since it was last
/// reachable. A later away run does not move it.
pub fn record_away(
    collection: &Collection,
    _lock: &RunLock,
    now: Timestamp,
) -> Result<(), AwayError> {
    update(collection, |object| {
        object
            .entry(AWAY_FIRST_SEEN)
            .or_insert_with(|| Value::from(whole_seconds(now).to_string()));
    })
}

/// Key of the time a `sync` last reached its end.
const LAST_SYNC: &str = "last_sync";

/// Records that a `sync` reached its end, which `status` shows as
/// `last_sync`.
pub(crate) fn record_sync(
    collection: &Collection,
    _lock: &RunLock,
    now: Timestamp,
) -> Result<(), AwayError> {
    update(collection, |object| {
        object.insert(
            LAST_SYNC.into(),
            Value::from(whole_seconds(now).to_string()),
        );
    })
}

/// Reads the keys of the state file and writes them back with `change`
/// applied. Keys it does not know stay, since `last_sync` and the keys of
/// later versions belong to other writers. A file that is missing or damaged
/// starts again from nothing; one that cannot be read at all is left alone,
/// as the keys in it may still be there to fix.
fn update(
    collection: &Collection,
    change: impl FnOnce(&mut Map<String, Value>),
) -> Result<(), AwayError> {
    let path = collection.remote_state_path();
    let mut object = match load(&path) {
        Ok(object) => object,
        Err(AwayError::Damaged { .. }) => Map::new(),
        Err(error) => return Err(error),
    };
    change(&mut object);
    let mut bytes = serde_json::to_vec_pretty(&object).expect("a JSON object serialises");
    bytes.push(b'\n');
    write_atomic(&path, &bytes)?;
    Ok(())
}

/// The object in the state file; empty when there is no file.
fn load(path: &Path) -> Result<Map<String, Value>, AwayError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(source) => {
            return Err(AwayError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(AwayError::Damaged {
            path: path.to_path_buf(),
            reason: "it is not a JSON object".into(),
        }),
        Err(error) => Err(AwayError::Damaged {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }),
    }
}

fn time_at(
    object: &Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<Timestamp>, AwayError> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .and_then(|text| text.parse::<Timestamp>().ok())
        .map(|time| Some(whole_seconds(time)))
        .ok_or_else(|| AwayError::Damaged {
            path: path.to_path_buf(),
            reason: format!("`{key}` is not an RFC 3339 time"),
        })
}

/// Gives an away error the `overdue` mark when the remote has been out of
/// reach for longer than `away_warn_days` while remote work is pending, which
/// `--if-reachable` does not turn into exit code 0. Every other result
/// passes unchanged.
///
/// `pending_remote_work` is only asked once an away error is there and the
/// time is over. A dry run applies this as well, so that its exit code is
/// the one the real run would get.
pub fn with_overdue<T>(
    result: Result<T, Error>,
    collection: &Collection,
    pending_remote_work: impl FnOnce() -> bool,
    now: Timestamp,
) -> Result<T, Error> {
    match result {
        Err(Error::Remote(RemoteError::Away {
            message,
            overdue: false,
        })) => {
            let overdue = overdue_since(collection, pending_remote_work, now).is_some();
            Err(RemoteError::Away { message, overdue }.into())
        }
        other => other,
    }
}

/// The warning `status` shows when the remote has been away for too long
/// while remote work is pending.
pub fn status_warning(
    collection: &Collection,
    state: RemoteState,
    pending_remote_work: bool,
    now: Timestamp,
) -> Option<Warning> {
    if state != RemoteState::Away {
        return None;
    }
    let since = overdue_since(collection, || pending_remote_work, now)?;
    let limit = collection.config().collection.away_warn_days;
    Some(Warning::new(format!(
        "The remote has been away since {since}, longer than away_warn_days ({limit}), \
         and work is waiting for it."
    )))
}

/// When the away period began, if it is longer than `away_warn_days` and
/// remote work is pending. A period with no recorded start, one that starts
/// in the future (the clock was set back) and a state file that cannot be
/// used give `None`, since then nothing says how long the remote has been
/// away.
fn overdue_since(
    collection: &Collection,
    pending_remote_work: impl FnOnce() -> bool,
    now: Timestamp,
) -> Option<Timestamp> {
    let since = away_since(collection).ok().flatten()?;
    let limit =
        SignedDuration::from_hours(24 * i64::from(collection.config().collection.away_warn_days));
    (now.duration_since(since) > limit && pending_remote_work()).then_some(since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::lock;
    use crate::testing::tree::{COLLECTION_ID, TempCollection};
    use serde_json::Value;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn time(text: &str) -> Timestamp {
        text.parse().expect("a valid test time")
    }

    /// A collection in a temp directory with the run lock held.
    struct Fixture {
        _temp: TempCollection,
        collection: Collection,
        lock: RunLock,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture::with_config("")
        }

        /// `config` is added to the `[collection]` table.
        fn with_config(config: &str) -> Fixture {
            let mut temp = TempCollection::minimal();
            temp.tree().file(
                ".antiq/config.toml",
                format!("[collection]\nid = \"{COLLECTION_ID}\"\n{config}\n"),
            );
            let collection = Collection::find(temp.root()).expect("a collection");
            let clock = FixedClock::new(time("2026-01-02T03:04:05Z"));
            let lock = lock::acquire(&collection.lock_path(), "imperius", &clock).expect("free");
            Fixture {
                _temp: temp,
                collection,
                lock,
            }
        }

        fn write_state(&self, content: &str) {
            fs::write(self.collection.remote_state_path(), content).expect("the file is writable");
        }

        fn state(&self) -> Value {
            let bytes = fs::read(self.collection.remote_state_path()).expect("the file exists");
            serde_json::from_slice(&bytes).expect("the file is JSON")
        }

        fn record_reachable(&self, at: &str) -> Result<(), AwayError> {
            record_reachable(&self.collection, &self.lock, time(at))
        }

        fn record_away(&self, at: &str) -> Result<(), AwayError> {
            record_away(&self.collection, &self.lock, time(at))
        }
    }

    #[test]
    fn no_file_means_no_time() {
        let fixture = Fixture::new();
        assert_eq!(away_since(&fixture.collection).expect("readable"), None);
    }

    #[test]
    fn the_time_a_run_found_the_remote_reachable_is_kept_in_whole_seconds() {
        let fixture = Fixture::new();
        fixture
            .record_reachable("2026-11-01T09:30:15.750Z")
            .expect("writable");
        assert_eq!(
            fixture.state()["last_reachable"],
            Value::from("2026-11-01T09:30:15Z")
        );
        assert_eq!(
            away_since(&fixture.collection).expect("readable"),
            Some(time("2026-11-01T09:30:15Z"))
        );
    }

    #[test]
    fn recording_keeps_last_sync_and_keys_it_does_not_know() {
        let fixture = Fixture::new();
        fixture.write_state(r#"{"last_sync": "2026-10-31T00:00:00Z", "later_key": [1, 2]}"#);
        fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect("writable");
        fixture
            .record_away("2026-11-01T10:00:00Z")
            .expect("writable");
        let state = fixture.state();
        assert_eq!(state["last_reachable"], Value::from("2026-11-01T09:30:15Z"));
        assert_eq!(
            state["away_first_seen"],
            Value::from("2026-11-01T10:00:00Z")
        );
        assert_eq!(state["last_sync"], Value::from("2026-10-31T00:00:00Z"));
        assert_eq!(state["later_key"], serde_json::json!([1, 2]));
    }

    #[test]
    fn recording_a_sync_keeps_the_other_keys() {
        let fixture = Fixture::new();
        fixture.write_state(
            r#"{"last_reachable": "2026-11-01T09:30:15Z", "away_first_seen": "2026-11-01T10:00:00Z", "later_key": [1, 2]}"#,
        );
        record_sync(
            &fixture.collection,
            &fixture.lock,
            time("2026-11-04T08:00:00.900Z"),
        )
        .expect("writable");
        let state = fixture.state();
        assert_eq!(state["last_sync"], Value::from("2026-11-04T08:00:00Z"));
        assert_eq!(state["last_reachable"], Value::from("2026-11-01T09:30:15Z"));
        assert_eq!(
            state["away_first_seen"],
            Value::from("2026-11-01T10:00:00Z")
        );
        assert_eq!(state["later_key"], serde_json::json!([1, 2]));
    }

    #[test]
    fn a_file_that_is_not_json_is_replaced_on_write() {
        let fixture = Fixture::new();
        fixture.write_state("{not json");
        fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect("writable");
        assert_eq!(
            fixture.state()["last_reachable"],
            Value::from("2026-11-01T09:30:15Z")
        );
    }

    #[test]
    fn a_file_that_is_not_an_object_is_replaced_on_write() {
        let fixture = Fixture::new();
        fixture.write_state("[1, 2]");
        fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect("writable");
        assert_eq!(
            fixture.state()["last_reachable"],
            Value::from("2026-11-01T09:30:15Z")
        );
    }

    #[test]
    fn a_state_file_that_cannot_be_read_is_not_replaced() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.collection.remote_state_path()).expect("a directory");
        let error = fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect_err("a directory is no file");
        assert!(matches!(error, AwayError::Read { .. }), "{error}");
        assert!(fixture.collection.remote_state_path().is_dir());
        let error = away_since(&fixture.collection).expect_err("a directory is no file");
        assert!(matches!(error, AwayError::Read { .. }), "{error}");
    }

    #[test]
    fn a_file_that_is_not_json_is_an_error_to_read() {
        let fixture = Fixture::new();
        fixture.write_state("{not json");
        let error = away_since(&fixture.collection).expect_err("damaged");
        assert!(matches!(error, AwayError::Damaged { .. }), "{error}");
        fixture.write_state("[1]");
        let error = away_since(&fixture.collection).expect_err("damaged");
        assert!(matches!(error, AwayError::Damaged { .. }), "{error}");
    }

    #[test]
    fn a_time_that_is_not_a_time_is_an_error_to_read() {
        let fixture = Fixture::new();
        fixture.write_state(r#"{"last_reachable": "yesterday"}"#);
        let error = away_since(&fixture.collection).expect_err("damaged");
        assert!(matches!(error, AwayError::Damaged { .. }), "{error}");
        assert!(error.to_string().contains("last_reachable"), "{error}");
    }

    #[test]
    fn a_time_that_is_not_a_time_is_overwritten_by_the_next_record() {
        let fixture = Fixture::new();
        fixture.write_state(r#"{"last_reachable": 7}"#);
        fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect("writable");
        assert_eq!(
            away_since(&fixture.collection).expect("readable"),
            Some(time("2026-11-01T09:30:15Z"))
        );
    }

    #[test]
    fn the_first_away_run_is_remembered_and_later_ones_do_not_move_it() {
        let fixture = Fixture::new();
        fixture
            .record_away("2026-11-01T10:00:00Z")
            .expect("writable");
        fixture
            .record_away("2026-11-03T10:00:00Z")
            .expect("writable");
        assert_eq!(
            away_since(&fixture.collection).expect("readable"),
            Some(time("2026-11-01T10:00:00Z"))
        );
    }

    #[test]
    fn a_reachable_run_forgets_the_away_time() {
        let fixture = Fixture::new();
        fixture
            .record_away("2026-11-01T10:00:00Z")
            .expect("writable");
        fixture
            .record_reachable("2026-11-05T08:00:00Z")
            .expect("writable");
        assert!(fixture.state().get("away_first_seen").is_none());
        assert_eq!(
            away_since(&fixture.collection).expect("readable"),
            Some(time("2026-11-05T08:00:00Z"))
        );
    }

    #[test]
    fn an_away_run_keeps_the_time_the_remote_was_last_reachable() {
        let fixture = Fixture::new();
        fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect("writable");
        fixture
            .record_away("2026-11-03T10:00:00Z")
            .expect("writable");
        assert_eq!(
            fixture.state()["last_reachable"],
            Value::from("2026-11-01T09:30:15Z")
        );
        assert_eq!(
            away_since(&fixture.collection).expect("readable"),
            Some(time("2026-11-01T09:30:15Z"))
        );
    }

    #[test]
    fn the_last_reachable_time_wins_over_a_first_away_time() {
        let fixture = Fixture::new();
        fixture.write_state(
            r#"{"last_reachable": "2026-11-01T00:00:00Z", "away_first_seen": "2026-10-01T00:00:00Z"}"#,
        );
        assert_eq!(
            away_since(&fixture.collection).expect("readable"),
            Some(time("2026-11-01T00:00:00Z"))
        );
    }

    #[test]
    fn a_failed_write_is_reported_and_leaves_the_file_as_it_was() {
        let fixture = Fixture::new();
        fixture.write_state(r#"{"last_sync": "2026-10-31T00:00:00Z"}"#);
        let antiq = fixture.collection.remote_state_path();
        let antiq = antiq.parent().expect("inside .antiq");
        fs::set_permissions(antiq, fs::Permissions::from_mode(0o500)).expect("chmod");
        let probe = antiq.join("probe");
        assert!(
            fs::write(&probe, b"x").is_err(),
            "the test needs a user who cannot write into a 0500 directory"
        );
        let error = fixture
            .record_reachable("2026-11-01T09:30:15Z")
            .expect_err("the directory is read-only");
        fs::set_permissions(antiq, fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(matches!(error, AwayError::Write(_)), "{error}");
        assert!(fixture.state().get("last_reachable").is_none());
    }

    // =========================================================
    // The warning of `status`
    // =========================================================

    /// A collection whose remote was last reachable at `last_reachable`
    /// and whose config has `config` below `[collection]`.
    fn away_for(last_reachable: &str, config: &str) -> Fixture {
        let fixture = Fixture::with_config(config);
        fixture.write_state(&format!(r#"{{"last_reachable": "{last_reachable}"}}"#));
        fixture
    }

    fn warning(fixture: &Fixture, state: RemoteState, pending: bool, now: &str) -> Option<Warning> {
        status_warning(&fixture.collection, state, pending, time(now))
    }

    #[test]
    fn eight_days_away_with_pending_work_warns() {
        let fixture = away_for("2026-10-24T00:00:00Z", "");
        let warning = warning(&fixture, RemoteState::Away, true, "2026-11-01T00:00:00Z");
        assert_eq!(
            warning.expect("a warning").message,
            "The remote has been away since 2026-10-24T00:00:00Z, \
             longer than away_warn_days (7), and work is waiting for it."
        );
    }

    #[test]
    fn seven_days_and_twelve_hours_away_warn() {
        let fixture = away_for("2026-10-24T00:00:00Z", "");
        let now = "2026-10-31T12:00:00Z";
        assert!(warning(&fixture, RemoteState::Away, true, now).is_some());
    }

    #[test]
    fn exactly_the_limit_does_not_warn() {
        let fixture = away_for("2026-10-25T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Away, true, now), None);
    }

    #[test]
    fn a_short_time_away_does_not_warn() {
        let fixture = away_for("2026-10-30T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Away, true, now), None);
    }

    #[test]
    fn without_pending_work_nothing_warns() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Away, false, now), None);
    }

    #[test]
    fn a_reachable_remote_does_not_warn() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Reachable, true, now), None);
    }

    #[test]
    fn a_broken_remote_does_not_warn() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Broken, true, now), None);
    }

    #[test]
    fn without_a_remote_nothing_warns() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::None, true, now), None);
    }

    #[test]
    fn the_limit_comes_from_the_config() {
        let fixture = away_for("2026-10-29T00:00:00Z", "away_warn_days = 2");
        let now = "2026-11-01T00:00:00Z";
        let message = warning(&fixture, RemoteState::Away, true, now)
            .expect("three days is more than two")
            .message;
        assert!(message.contains("away_warn_days (2)"), "{message}");
    }

    #[test]
    fn a_limit_of_zero_warns_for_any_time_away() {
        let fixture = away_for("2026-11-01T00:00:00Z", "away_warn_days = 0");
        assert!(warning(&fixture, RemoteState::Away, true, "2026-11-01T00:00:01Z").is_some());
        assert_eq!(
            warning(&fixture, RemoteState::Away, true, "2026-11-01T00:00:00Z"),
            None
        );
    }

    #[test]
    fn no_recorded_time_does_not_warn() {
        let fixture = Fixture::with_config("");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Away, true, now), None);
    }

    #[test]
    fn a_time_in_the_future_does_not_warn() {
        let fixture = away_for("2026-12-01T00:00:00Z", "");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Away, true, now), None);
    }

    #[test]
    fn a_state_file_that_cannot_be_used_does_not_warn() {
        let fixture = Fixture::with_config("");
        fixture.write_state("{not json");
        let now = "2026-11-01T00:00:00Z";
        assert_eq!(warning(&fixture, RemoteState::Away, true, now), None);
    }

    #[test]
    fn the_first_time_found_away_counts_when_no_reachable_time_is_recorded() {
        let fixture = Fixture::with_config("");
        fixture.write_state(r#"{"away_first_seen": "2026-10-20T00:00:00Z"}"#);
        let message = warning(&fixture, RemoteState::Away, true, "2026-11-01T00:00:00Z")
            .expect("twelve days is more than seven")
            .message;
        assert!(message.contains("since 2026-10-20T00:00:00Z"), "{message}");
    }

    // =========================================================
    // The mark on an away error
    // =========================================================

    fn away_error() -> Error {
        RemoteError::Away {
            message: "no route to host".into(),
            overdue: false,
        }
        .into()
    }

    fn is_overdue(result: Result<(), Error>) -> bool {
        match result {
            Err(Error::Remote(RemoteError::Away { overdue, .. })) => overdue,
            other => panic!("expected an away error, got {other:?}"),
        }
    }

    #[test]
    fn an_away_error_after_eight_days_with_pending_work_is_marked() {
        let fixture = away_for("2026-10-24T00:00:00Z", "");
        let result = with_overdue(
            Err(away_error()),
            &fixture.collection,
            || true,
            time("2026-11-01T00:00:00Z"),
        );
        assert!(is_overdue(result));
    }

    #[test]
    fn an_away_error_after_a_short_time_is_not_marked() {
        let fixture = away_for("2026-10-30T00:00:00Z", "");
        let result = with_overdue(
            Err(away_error()),
            &fixture.collection,
            || true,
            time("2026-11-01T00:00:00Z"),
        );
        assert!(!is_overdue(result));
    }

    #[test]
    fn an_away_error_without_pending_work_is_not_marked() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let result = with_overdue(
            Err(away_error()),
            &fixture.collection,
            || false,
            time("2026-11-01T00:00:00Z"),
        );
        assert!(!is_overdue(result));
    }

    #[test]
    fn pending_work_is_only_looked_up_when_the_time_is_over() {
        let fixture = away_for("2026-10-30T00:00:00Z", "");
        let result = with_overdue(
            Err(away_error()),
            &fixture.collection,
            || panic!("the pending work is not needed"),
            time("2026-11-01T00:00:00Z"),
        );
        assert!(!is_overdue(result));
    }

    #[test]
    fn a_broken_error_passes_unchanged() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let result: Result<(), Error> = with_overdue(
            Err(RemoteError::Broken("wrong marker".into()).into()),
            &fixture.collection,
            || panic!("no lookup for an error that is not away"),
            time("2026-11-01T00:00:00Z"),
        );
        assert!(matches!(
            result,
            Err(Error::Remote(RemoteError::Broken(ref message))) if message == "wrong marker"
        ));
    }

    #[test]
    fn a_success_passes_unchanged() {
        let fixture = away_for("2026-10-01T00:00:00Z", "");
        let result = with_overdue(
            Ok(7),
            &fixture.collection,
            || panic!("no lookup without an error"),
            time("2026-11-01T00:00:00Z"),
        );
        assert_eq!(result.expect("still ok"), 7);
    }

    #[test]
    fn the_message_of_a_marked_error_stays() {
        let fixture = away_for("2026-10-24T00:00:00Z", "");
        let error = with_overdue::<()>(
            Err(away_error()),
            &fixture.collection,
            || true,
            time("2026-11-01T00:00:00Z"),
        )
        .expect_err("still an error");
        assert_eq!(error.to_string(), "no route to host");
    }

    #[test]
    fn if_reachable_ends_a_fresh_away_run_with_zero_and_an_overdue_one_with_four() {
        use crate::remote::policy::{Outcome, outcome};
        let fresh = away_for("2026-10-30T00:00:00Z", "");
        let now = time("2026-11-01T00:00:00Z");
        let error = with_overdue::<()>(Err(away_error()), &fresh.collection, || true, now)
            .expect_err("away");
        assert_eq!(outcome(true, &error), Outcome::TreatAsSuccess);
        let overdue = away_for("2026-10-24T00:00:00Z", "");
        let error = with_overdue::<()>(Err(away_error()), &overdue.collection, || true, now)
            .expect_err("away");
        assert_eq!(outcome(true, &error), Outcome::Fail);
        assert_eq!(error.kind().exit_code(), 4);
    }
}
