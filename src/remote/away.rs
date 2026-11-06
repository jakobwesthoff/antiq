// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A remote that stays away: the times kept in `.antiq/remote-state.json`
//! and the warning about them.

use crate::clock::whole_seconds;
use crate::collection::Collection;
use crate::fsx::FsxError;
use crate::fsx::atomic::write_atomic;
use crate::lock::RunLock;
use crate::remote::RemoteState;
use crate::report::Warning;
use jiff::Timestamp;
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

/// The warning `status` shows when the remote has been away for too long
/// while remote work is pending.
pub fn status_warning(
    _collection: &Collection,
    _state: RemoteState,
    _pending_remote_work: bool,
    _now: Timestamp,
) -> Option<Warning> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::lock;
    use crate::testing::tree::TempCollection;
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
            let temp = TempCollection::minimal();
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

    #[test]
    fn no_warning_is_given_yet() {
        let collection = Collection::for_tests();
        let now = Timestamp::now();
        assert_eq!(
            status_warning(&collection, RemoteState::Away, true, now),
            None
        );
    }
}
