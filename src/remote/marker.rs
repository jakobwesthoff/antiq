// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The marker file at the root of a remote.

// TODO: the path remote and `remote add` (the commits that follow) call
// the writer; until they land the library has no caller.
#![allow(dead_code)]

use crate::error::HasKind;
use crate::fsx::FsxError;
use crate::fsx::tree::TreeRoot;
use crate::id::ObjectId;
use crate::relpath::RelPath;
use crate::remote::Durable;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// The name of the marker in the remote's root.
pub const FILE_NAME: &str = "antiq-remote.json";

/// The `format` and `layout` this version reads and writes.
const CURRENT: u32 = 1;

/// Why a marker cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum MarkerError {
    #[error("the marker {FILE_NAME} cannot be read: {reason}")]
    Invalid { reason: String },
    #[error("the marker {FILE_NAME} has {field} {found}, written by a newer antiq than this one")]
    TooNew { field: &'static str, found: u32 },
    #[error("{} already holds a marker", root.display())]
    Exists { root: std::path::PathBuf },
    #[error("cannot {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Fsx(#[from] FsxError),
}

impl HasKind for MarkerError {}

/// The content of `antiq-remote.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub format: u32,
    pub id: ObjectId,
    pub layout: u32,
}

impl Marker {
    pub fn new(id: ObjectId) -> Marker {
        Marker {
            format: CURRENT,
            id,
            layout: CURRENT,
        }
    }

    /// The bytes of the file: one line of JSON.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes =
            serde_json::to_vec(self).expect("a marker of numbers and a string always serialises");
        bytes.push(b'\n');
        bytes
    }

    /// Reads a marker. Keys this version does not know are ignored, and a
    /// `format` or `layout` above the one this version writes is refused,
    /// since a newer antiq may have changed what the root holds.
    pub fn parse(bytes: &[u8]) -> Result<Marker, MarkerError> {
        let marker: Marker =
            serde_json::from_slice(bytes).map_err(|error| MarkerError::Invalid {
                reason: error.to_string(),
            })?;
        for (field, found) in [("format", marker.format), ("layout", marker.layout)] {
            if found > CURRENT {
                return Err(MarkerError::TooNew { field, found });
            }
            if found == 0 {
                return Err(MarkerError::Invalid {
                    reason: format!("{field} is 0"),
                });
            }
        }
        Ok(marker)
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> MarkerError {
    let path = path.display().to_string();
    move |source| MarkerError::Io {
        action,
        path,
        source,
    }
}

/// Creates the marker in `root`. The marker is never replaced.
///
/// The bytes are written and flushed under a temp name and renamed into
/// place, so the name `antiq-remote.json` never holds a partial file: a
/// root with a half-written marker would read as broken until someone
/// deleted it by hand. The rename refuses an existing marker. Then the
/// root is flushed. When only that flush fails, the marker is in place
/// and the error means that its entry may not be durable.
pub(crate) fn write_marker(
    root: &Path,
    id: ObjectId,
    durable: &dyn Durable,
) -> Result<(), MarkerError> {
    let temp_name = format!(".{FILE_NAME}.{}.tmp", std::process::id());
    let temp = root.join(&temp_name);
    if let Err(error) = stage(&temp, &Marker::new(id).to_bytes(), durable) {
        // Best effort: the failure being reported matters more than a
        // temp file that cannot be removed either.
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }

    let renamed = TreeRoot::open(root)
        .map_err(io_error("open", root))
        .and_then(|tree| {
            let name = |text: &str| RelPath::parse(text).expect("a fixed name is a valid path");
            tree.rename(&name(&temp_name), &tree, &name(FILE_NAME))
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::AlreadyExists => MarkerError::Exists {
                        root: root.to_path_buf(),
                    },
                    _ => io_error("create", &root.join(FILE_NAME))(error),
                })
        });
    if let Err(error) = renamed {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    durable.dir(root)?;
    Ok(())
}

/// Writes `bytes` to `temp` and flushes the file.
fn stage(temp: &Path, bytes: &[u8], durable: &dyn Durable) -> Result<(), MarkerError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp)
        .map_err(io_error("create", temp))?;
    file.write_all(bytes).map_err(io_error("write", temp))?;
    durable.file(&file, temp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RealDurable;
    use crate::remote::recording::{Flush, Recording};
    use std::fs;
    use std::sync::Arc;

    const ID: &str = "01BX5ZZKBKACTAV9WEVGEMMVRY";

    fn id() -> ObjectId {
        ID.parse().expect("a ULID")
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .expect("the directory is listed")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into())
            .collect();
        found.sort();
        found
    }

    fn expected_bytes() -> Vec<u8> {
        format!("{{\"format\":1,\"id\":\"{ID}\",\"layout\":1}}\n").into_bytes()
    }

    #[test]
    fn a_marker_is_one_line_of_json_in_a_fixed_key_order() {
        assert_eq!(Marker::new(id()).to_bytes(), expected_bytes());
    }

    #[test]
    fn a_marker_reads_back() {
        let parsed = Marker::parse(&Marker::new(id()).to_bytes()).expect("a marker");
        assert_eq!(parsed, Marker::new(id()));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let text = format!("{{\"format\":1,\"id\":\"{ID}\",\"layout\":1,\"note\":\"x\"}}");
        assert_eq!(
            Marker::parse(text.as_bytes()).expect("a marker"),
            Marker::new(id())
        );
    }

    #[test]
    fn text_that_is_no_json_is_invalid() {
        assert!(matches!(
            Marker::parse(b"not json"),
            Err(MarkerError::Invalid { .. })
        ));
    }

    #[test]
    fn a_marker_without_an_id_is_invalid() {
        assert!(matches!(
            Marker::parse(br#"{"format":1,"layout":1}"#),
            Err(MarkerError::Invalid { .. })
        ));
    }

    #[test]
    fn a_format_of_zero_is_invalid() {
        let text = format!("{{\"format\":0,\"id\":\"{ID}\",\"layout\":1}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::Invalid { .. })
        ));
    }

    #[test]
    fn a_higher_format_is_too_new() {
        let text = format!("{{\"format\":2,\"id\":\"{ID}\",\"layout\":1}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::TooNew {
                field: "format",
                found: 2
            })
        ));
    }

    #[test]
    fn a_higher_layout_is_too_new() {
        let text = format!("{{\"format\":1,\"id\":\"{ID}\",\"layout\":2}}");
        assert!(matches!(
            Marker::parse(text.as_bytes()),
            Err(MarkerError::TooNew {
                field: "layout",
                found: 2
            })
        ));
    }

    #[test]
    fn writing_creates_the_marker_and_leaves_no_temp_file() {
        let root = tempfile::tempdir().expect("temp dir");
        write_marker(root.path(), id(), &RealDurable).expect("the marker is written");
        assert_eq!(
            fs::read(root.path().join(FILE_NAME)).expect("exists"),
            expected_bytes()
        );
        assert_eq!(names(root.path()), [FILE_NAME]);
    }

    #[test]
    fn an_existing_marker_is_never_replaced() {
        let root = tempfile::tempdir().expect("temp dir");
        fs::write(root.path().join(FILE_NAME), "mine").expect("write");
        let error = write_marker(root.path(), id(), &RealDurable).expect_err("refused");
        assert!(matches!(error, MarkerError::Exists { .. }), "{error}");
        assert_eq!(
            fs::read(root.path().join(FILE_NAME)).expect("exists"),
            b"mine"
        );
        assert_eq!(names(root.path()), [FILE_NAME]);
    }

    #[test]
    fn a_failing_file_flush_leaves_neither_marker_nor_temp_file() {
        let root = tempfile::tempdir().expect("temp dir");
        let durable = Recording::new();
        durable.fail_files();
        let error = write_marker(root.path(), id(), &durable).expect_err("the flush fails");
        assert!(matches!(error, MarkerError::Fsx(_)), "{error}");
        assert_eq!(names(root.path()), Vec::<String>::new());
    }

    #[test]
    fn the_content_is_flushed_before_the_rename_and_the_root_after() {
        let root = tempfile::tempdir().expect("temp dir");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let durable = Recording::new();
        let dir = root.path().to_path_buf();
        let log = Arc::clone(&seen);
        durable.observe(move |flush, _| {
            let temp_bytes = names(&dir)
                .into_iter()
                .filter(|name| name != FILE_NAME)
                .map(|name| fs::read(dir.join(name)).expect("readable"))
                .collect::<Vec<_>>();
            log.lock().expect("no panic").push((
                flush.clone(),
                dir.join(FILE_NAME).exists(),
                temp_bytes,
            ));
        });
        write_marker(root.path(), id(), &durable).expect("the marker is written");
        let seen = seen.lock().expect("no panic").clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(matches!(seen[0].0, Flush::File(_)));
        assert!(
            !seen[0].1,
            "the marker must not exist while the file is flushed"
        );
        assert_eq!(
            seen[0].2,
            [expected_bytes()],
            "the temp file holds all bytes"
        );
        assert_eq!(seen[1].0, Flush::Dir(root.path().to_path_buf()));
        assert!(seen[1].1, "the marker is in place when the root is flushed");
        assert!(seen[1].2.is_empty(), "no temp file is left");
    }

    #[test]
    fn a_failing_root_flush_reports_an_error_with_the_marker_in_place() {
        let root = tempfile::tempdir().expect("temp dir");
        let durable = Recording::new();
        durable.fail_dirs();
        let error = write_marker(root.path(), id(), &durable).expect_err("the flush fails");
        assert!(matches!(error, MarkerError::Fsx(_)), "{error}");
        assert_eq!(durable.flushes().len(), 2, "the file, then the root");
        assert_eq!(
            fs::read(root.path().join(FILE_NAME)).expect("exists"),
            expected_bytes()
        );
    }

    #[test]
    fn a_temp_file_that_vanishes_before_the_rename_is_an_error() {
        let root = tempfile::tempdir().expect("temp dir");
        let durable = Recording::new();
        let dir = root.path().to_path_buf();
        durable.observe(move |_, file| {
            if file.is_some() {
                for name in names(&dir) {
                    fs::remove_file(dir.join(name)).expect("the temp file is removed");
                }
            }
        });
        let error = write_marker(root.path(), id(), &durable).expect_err("nothing to rename");
        assert!(
            matches!(
                error,
                MarkerError::Io {
                    action: "create",
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(names(root.path()), Vec::<String>::new());
    }

    #[test]
    fn a_root_that_vanishes_before_the_rename_is_an_error() {
        let parent = tempfile::tempdir().expect("temp dir");
        let root = parent.path().join("remote");
        fs::create_dir(&root).expect("root");
        let durable = Recording::new();
        let doomed = root.clone();
        durable.observe(move |_, file| {
            if file.is_some() {
                fs::remove_dir_all(&doomed).expect("the root is removed");
            }
        });
        let error = write_marker(&root, id(), &durable).expect_err("no root to open");
        assert!(
            matches!(error, MarkerError::Io { action: "open", .. }),
            "{error}"
        );
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let root = tempfile::tempdir().expect("temp dir");
        let error =
            write_marker(&root.path().join("absent"), id(), &RealDurable).expect_err("no root");
        assert!(matches!(error, MarkerError::Io { .. }), "{error}");
    }
}
