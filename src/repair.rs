// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repairs that open every run, after the lock is taken.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::lock::RunLock;
use crate::relpath::{Name, RelPath};
use crate::report::Warning;
use crate::versions::stamp::Stamp;
use serde::{Serialize, Serializer};
use std::error::Error as StdError;
use std::fmt;

/// Where a tree taken out of `trash/` went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    /// Back to its live path.
    Live,
    /// To `recovered/`, where `status` names it.
    Recovered,
}

/// The run directory a leftover chain of empty folders lies in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Trash,
    Doomed,
}

impl fmt::Display for Area {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Area::Trash => "trash",
            Area::Doomed => "doomed",
        })
    }
}

fn stamp_text<S: Serializer>(stamp: &Stamp, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(stamp)
}

/// One repair of the start of a run (spec "Repairs at the start of a
/// run"). In JSON it is `{"repair": "<snake_case variant>", ...fields}`.
/// A `piece` or `folder` is relative to `pieces/` (to the collection root
/// for repair 5), a `tree` or `path` to `<area>/<stamp>/` (to `<area>/`
/// where `stamp` is absent); all are escaped like a path of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "repair", rename_all = "snake_case")]
pub enum Repair {
    /// 1: the sidecars of version `version` of the store folder `piece`,
    /// whose `.tar.zst` is still staged.
    DeleteSidecars {
        piece: RelPath,
        #[serde(serialize_with = "stamp_text")]
        version: Stamp,
    },
    /// 2: the entry `entry` of `tmp/`.
    DeleteTemporary { entry: Name },
    /// 3: the tree `tree` below `doomed/<stamp>/`. `stamp` is absent for an
    /// entry that lies directly in `doomed/`.
    DeleteDoomed {
        #[serde(skip_serializing_if = "Option::is_none")]
        stamp: Option<Name>,
        tree: RelPath,
    },
    /// 4: the tree `tree` below `trash/<stamp>/`, to `destination`. `stamp`
    /// is absent for an entry that lies directly in `trash/`.
    GiveBack {
        #[serde(skip_serializing_if = "Option::is_none")]
        stamp: Option<Name>,
        tree: RelPath,
        destination: Destination,
    },
    /// 5: version `version` of the live piece `piece` that a died unearth
    /// left. With `delete` false the stamp names no version and is only
    /// dropped from the marker.
    DeleteRestoredVersion {
        piece: RelPath,
        #[serde(serialize_with = "stamp_text")]
        version: Stamp,
        delete: bool,
    },
    /// 6: the empty folder `folder` below `pieces/`.
    RemoveEmptyFolder { folder: RelPath },
    /// 3 and 4: the folder `path` below `<area>/<stamp>/` that holds no
    /// non-directory entry. `path` is the root for the stamp folder itself.
    RemoveEmptyChain {
        area: Area,
        stamp: Name,
        path: RelPath,
    },
}

impl Repair {
    /// What the repair does, in the imperative and with paths relative to
    /// the collection root, as a dry run lists it.
    pub fn describe(&self) -> String {
        let stamped = |area: &str, stamp: &Option<Name>, tree: &RelPath| match stamp {
            Some(stamp) => format!(".antiq/{area}/{stamp}/{tree}"),
            None => format!(".antiq/{area}/{tree}"),
        };
        match self {
            Repair::DeleteSidecars { piece, version } => format!(
                "delete the sidecars of {version} in .antiq/pieces/{piece} (the archive is not there)"
            ),
            Repair::DeleteTemporary { entry } => format!("delete .antiq/tmp/{entry}"),
            Repair::DeleteDoomed { stamp, tree } => {
                format!("delete {}", stamped("doomed", stamp, tree))
            }
            Repair::GiveBack {
                stamp,
                tree,
                destination,
            } => {
                let from = stamped("trash", stamp, tree);
                match destination {
                    Destination::Live => format!("move {from} back to {tree}"),
                    Destination::Recovered => {
                        format!("move {from} to {}", stamped("recovered", stamp, tree))
                    }
                }
            }
            Repair::DeleteRestoredVersion {
                piece,
                version,
                delete: true,
            } => format!("delete version {version} of {piece} that an unearth left"),
            Repair::DeleteRestoredVersion {
                piece,
                version,
                delete: false,
            } => format!(
                "drop the stamp {version} from the marker of {piece}, which names no version"
            ),
            Repair::RemoveEmptyFolder { folder } => {
                format!("remove the empty folder .antiq/pieces/{folder}")
            }
            Repair::RemoveEmptyChain { area, stamp, path } => {
                let folder = stamped(&area.to_string(), &Some(stamp.clone()), path);
                format!("remove the empty folder {}", folder.trim_end_matches('/'))
            }
        }
    }
}

/// Why one repair failed: what it was doing, where, and the cause. A
/// failing repair is a warning and never fails the run.
#[derive(Debug, thiserror::Error)]
#[error("cannot {step} {place}: {source}")]
pub struct RepairError {
    pub step: &'static str,
    pub place: String,
    #[source]
    pub source: Box<dyn StdError + Send + Sync>,
}

/// What the repairs of a run did.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The repairs that were done, as they were done.
    pub done: Vec<Repair>,
    /// The repairs that failed, and anything that could not be read.
    pub warnings: Vec<Warning>,
}

/// Repairs what an earlier run left behind. The repairs are not written
/// yet, so there is nothing to repair and nothing to tell.
pub fn run(_collection: &Collection, _lock: &RunLock, _clock: &dyn Clock) -> Outcome {
    Outcome::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a path")
    }

    fn name(text: &str) -> Name {
        Name::from_bytes(text.as_bytes().to_vec()).expect("a name")
    }

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a stamp")
    }

    fn every_repair() -> Vec<Repair> {
        vec![
            Repair::DeleteSidecars {
                piece: path("a/b"),
                version: stamp("2026-11-04T090211"),
            },
            Repair::DeleteTemporary {
                entry: name("upload.part"),
            },
            Repair::DeleteDoomed {
                stamp: Some(name("2026-11-04T090211")),
                tree: path("a/b"),
            },
            Repair::DeleteDoomed {
                stamp: None,
                tree: path("stray"),
            },
            Repair::GiveBack {
                stamp: Some(name("2026-11-04T090211")),
                tree: path("a/b"),
                destination: Destination::Live,
            },
            Repair::GiveBack {
                stamp: None,
                tree: path("stray"),
                destination: Destination::Recovered,
            },
            Repair::GiveBack {
                stamp: Some(name("2026-11-04T090211")),
                tree: path("a/b"),
                destination: Destination::Recovered,
            },
            Repair::DeleteRestoredVersion {
                piece: path("a/b"),
                version: stamp("2026-11-04T090211-2"),
                delete: true,
            },
            Repair::DeleteRestoredVersion {
                piece: path("a/b"),
                version: stamp("2026-11-04T090211"),
                delete: false,
            },
            Repair::RemoveEmptyFolder {
                folder: path("a/b"),
            },
            Repair::RemoveEmptyChain {
                area: Area::Trash,
                stamp: name("2026-11-04T090211"),
                path: path("x/y"),
            },
            Repair::RemoveEmptyChain {
                area: Area::Doomed,
                stamp: name("2026-11-04T090211"),
                path: RelPath::root(),
            },
        ]
    }

    #[test]
    fn each_repair_serialises_with_its_name_first_and_its_fields() {
        let json: Vec<String> = every_repair()
            .iter()
            .map(|repair| serde_json::to_string(repair).expect("serialises"))
            .collect();
        assert_eq!(
            json,
            [
                r#"{"repair":"delete_sidecars","piece":"a/b","version":"2026-11-04T090211"}"#,
                r#"{"repair":"delete_temporary","entry":"upload.part"}"#,
                r#"{"repair":"delete_doomed","stamp":"2026-11-04T090211","tree":"a/b"}"#,
                r#"{"repair":"delete_doomed","tree":"stray"}"#,
                r#"{"repair":"give_back","stamp":"2026-11-04T090211","tree":"a/b","destination":"live"}"#,
                r#"{"repair":"give_back","tree":"stray","destination":"recovered"}"#,
                r#"{"repair":"give_back","stamp":"2026-11-04T090211","tree":"a/b","destination":"recovered"}"#,
                r#"{"repair":"delete_restored_version","piece":"a/b","version":"2026-11-04T090211-2","delete":true}"#,
                r#"{"repair":"delete_restored_version","piece":"a/b","version":"2026-11-04T090211","delete":false}"#,
                r#"{"repair":"remove_empty_folder","folder":"a/b"}"#,
                r#"{"repair":"remove_empty_chain","area":"trash","stamp":"2026-11-04T090211","path":"x/y"}"#,
                r#"{"repair":"remove_empty_chain","area":"doomed","stamp":"2026-11-04T090211","path":""}"#,
            ]
        );
    }

    #[test]
    fn a_percent_sign_and_a_byte_that_is_no_text_are_escaped_in_json() {
        let repair = Repair::DeleteTemporary {
            entry: Name::from_bytes(b"a%b\xff".to_vec()).expect("a name"),
        };
        let value = serde_json::to_value(&repair).expect("serialises");
        assert_eq!(
            value,
            json!({"repair": "delete_temporary", "entry": "a%25b%FF"})
        );
    }

    #[test]
    fn each_repair_describes_what_it_does_with_paths_from_the_collection_root() {
        let lines: Vec<String> = every_repair().iter().map(Repair::describe).collect();
        assert_eq!(
            lines,
            [
                "delete the sidecars of 2026-11-04T090211 in .antiq/pieces/a/b (the archive is not there)",
                "delete .antiq/tmp/upload.part",
                "delete .antiq/doomed/2026-11-04T090211/a/b",
                "delete .antiq/doomed/stray",
                "move .antiq/trash/2026-11-04T090211/a/b back to a/b",
                "move .antiq/trash/stray to .antiq/recovered/stray",
                "move .antiq/trash/2026-11-04T090211/a/b to .antiq/recovered/2026-11-04T090211/a/b",
                "delete version 2026-11-04T090211-2 of a/b that an unearth left",
                "drop the stamp 2026-11-04T090211 from the marker of a/b, which names no version",
                "remove the empty folder .antiq/pieces/a/b",
                "remove the empty folder .antiq/trash/2026-11-04T090211/x/y",
                "remove the empty folder .antiq/doomed/2026-11-04T090211",
            ]
        );
    }

    #[test]
    fn a_failed_repair_reads_with_its_step_and_place() {
        let error = RepairError {
            step: "delete a temporary file",
            place: ".antiq/tmp/x".to_string(),
            source: Box::new(std::io::Error::from_raw_os_error(13)),
        };
        assert!(
            error
                .to_string()
                .starts_with("cannot delete a temporary file .antiq/tmp/x: "),
            "{error}"
        );
    }
}
