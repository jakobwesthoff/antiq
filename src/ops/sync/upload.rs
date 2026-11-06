// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Uploading payloads and listings.

use super::{SyncContext, SyncError, ends_run, run_bounded};
use crate::collection::Collection;
use crate::id::ObjectId;
use crate::meta::{FileName, Metadata};
use crate::pack::hashing::hex;
use crate::relpath::{Name, temp_name};
use crate::remote::Upload;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;

/// A file of an object that the remote may not have yet.
#[derive(Debug, Clone)]
pub(super) struct UploadFile {
    pub name: FileName,
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
    /// The name it is written under until it is complete.
    pub temp: String,
}

impl UploadFile {
    pub fn as_upload(&self, id: ObjectId) -> Upload<'_> {
        Upload::new(id, &self.temp, self.name.as_str(), &self.path, &self.sha256)
    }
}

fn io(action: &'static str, path: &std::path::Path) -> impl FnOnce(std::io::Error) -> SyncError {
    let path = path.display().to_string();
    move |source| SyncError::Io {
        action,
        path,
        source,
    }
}

/// The SHA-256 of a file as lower-case hex, read in chunks.
fn file_sha256(path: &std::path::Path) -> Result<String, SyncError> {
    let mut file = File::open(path).map_err(io("open", path))?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut chunk).map_err(io("read", path))?;
        if read == 0 {
            return Ok(hex(&hasher.finalize()));
        }
        hasher.update(&chunk[..read]);
    }
}

/// The payloads and listings that `pending` names and `base` does not,
/// limited to the ones that are stored here. A file the remote has
/// already is not put again because the put finds it by its hash.
pub(super) fn files_to_upload(
    collection: &Collection,
    machine: &str,
    id: ObjectId,
    pending: &Metadata,
    base: Option<&Metadata>,
) -> Result<Vec<UploadFile>, SyncError> {
    let known = |name: &FileName| {
        base.is_some_and(|base| {
            base.payloads
                .iter()
                .any(|payload| &payload.file == name || payload.listing.as_ref() == Some(name))
        })
    };
    let mut files = Vec::new();
    for payload in &pending.payloads {
        let named = std::iter::once((&payload.file, true))
            .chain(payload.listing.iter().map(|l| (l, false)));
        for (name, is_payload) in named {
            let path = collection.object_dir(id).join(name.as_str());
            if known(name) || !path.is_file() {
                continue;
            }
            let size = fs::metadata(&path).map_err(io("stat", &path))?.len();
            let sha256 = if is_payload && payload.checksum.algorithm == "sha256" {
                payload.checksum.value.clone()
            } else {
                file_sha256(&path)?
            };
            let final_name = Name::from_bytes(name.as_str().as_bytes().to_vec())
                .expect("a payload or listing name is a valid file name");
            let temp = String::from_utf8(temp_name(&final_name, machine).as_bytes().to_vec())
                .expect("a temp name is built from ASCII and the escaped machine name");
            files.push(UploadFile {
                name: name.clone(),
                path,
                sha256,
                size,
                temp,
            });
        }
    }
    Ok(files)
}

/// What the upload of one object's files did.
pub(super) struct Uploaded {
    pub id: ObjectId,
    /// The files that were written, not the ones the remote had already.
    pub written: Vec<(String, u64)>,
    /// Whether a file was put, written or found, which a flush has to make
    /// durable before a commit names it.
    pub put: bool,
    pub error: Option<SyncError>,
}

/// Uploads the files of the objects, one job per object, on at most
/// `ctx.jobs` threads, and flushes the remote once afterwards, which makes
/// the files durable before any commit names them. A flush that fails
/// leaves every object that put something with that error.
pub(super) fn run(
    ctx: &SyncContext<'_>,
    work: Vec<(ObjectId, Vec<UploadFile>)>,
) -> Result<Vec<Uploaded>, SyncError> {
    let mut outcomes = run_bounded(ctx.jobs, work, |(id, files)| {
        let mut outcome = Uploaded {
            id,
            written: Vec::new(),
            put: false,
            error: None,
        };
        for file in &files {
            match ctx.remote.put_payload(&file.as_upload(id)) {
                Ok(transfer) => {
                    outcome.put = true;
                    if !transfer.already_present {
                        outcome
                            .written
                            .push((file.name.as_str().to_string(), file.size));
                    }
                }
                Err(error) => {
                    outcome.error = Some(error.into());
                    break;
                }
            }
        }
        outcome
    });
    // A file found in place is not known to be durable, which the commit
    // that names it needs.
    if outcomes.iter().any(|outcome| outcome.put)
        && let Err(error) = ctx.remote.flush()
    {
        if ends_run(ctx.remote, &error) {
            return Err(error.into());
        }
        let message = error.to_string();
        for outcome in outcomes.iter_mut().filter(|o| o.put) {
            outcome.error = Some(SyncError::Failed(format!(
                "the uploaded files could not be made durable on the remote: {message}"
            )));
        }
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Machine, Shared};
    use super::*;
    use std::fs;

    fn pending_of(machine: &Machine, id: ObjectId) -> Metadata {
        crate::meta::parse(&fs::read(machine.collection.meta_pending_path(id)).expect("pending"))
            .expect("parses")
    }

    #[test]
    fn a_new_object_uploads_its_payload_and_listing_under_the_temp_names_of_the_spec() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let id = anna.import_piece("notes", "hello");
        let pending = pending_of(&anna, id);

        let files = files_to_upload(&anna.collection, "anna", id, &pending, None).expect("lists");

        assert_eq!(files.len(), 2);
        for file in &files {
            assert_eq!(file.temp, format!(".{}.anna.part", file.name.as_str()));
            assert_eq!(file.size, fs::metadata(&file.path).expect("stat").len());
            assert_eq!(file.sha256.len(), 64);
        }
        assert!(files[0].name.as_str().ends_with(".tar.zst"));
        assert!(files[1].name.as_str().ends_with(".listing.zst"));
        assert_eq!(files[0].sha256, pending.payloads[0].checksum.value);
    }

    #[test]
    fn files_the_base_names_and_files_that_are_not_stored_here_are_left_out() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let id = anna.import_piece("notes", "hello");
        let pending = pending_of(&anna, id);

        let known =
            files_to_upload(&anna.collection, "anna", id, &pending, Some(&pending)).expect("lists");
        assert!(known.is_empty(), "the base has both files");

        let listing = pending.payloads[0].listing.clone().expect("a listing");
        fs::remove_file(anna.collection.object_dir(id).join(listing.as_str())).expect("removed");
        let stored = files_to_upload(&anna.collection, "anna", id, &pending, None).expect("lists");
        assert_eq!(stored.len(), 1, "only the payload is stored here");
    }

    #[test]
    fn a_machine_name_is_escaped_in_the_temp_name() {
        let shared = Shared::new();
        let anna = Machine::new(&shared, "anna");
        let id = anna.import_piece("notes", "hello");
        let pending = pending_of(&anna, id);

        let files = files_to_upload(&anna.collection, "ann a", id, &pending, None).expect("lists");

        assert!(
            files[0].temp.ends_with(".ann%20a.part"),
            "{}",
            files[0].temp
        );
    }
}
