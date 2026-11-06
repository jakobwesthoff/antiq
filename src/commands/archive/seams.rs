// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The points where a test replaces what the real run does, so it can
//! fill a disk, change a folder between two steps or record the order of
//! the flushes.

#![allow(
    dead_code,
    reason = "the pipeline that uses them follows in a later commit"
)]

use crate::filesystem::FilesystemError;
use crate::filesystem::atomic::{Durable, RealDurable};
use crate::filesystem::space::available_space;
use std::io::Write;
use std::path::Path;

pub(crate) trait Seams: Send + Sync {
    /// How files and directories are made durable.
    fn durable(&self) -> &dyn Durable {
        &RealDurable
    }

    /// The bytes free on the volume holding `dir`.
    fn free(&self, dir: &Path) -> Result<u64, FilesystemError> {
        available_space(dir)
    }

    /// What the archive file is written through.
    fn pack_sink<'a>(&self, sink: Box<dyn Write + 'a>) -> Box<dyn Write + 'a> {
        sink
    }

    /// Runs after the files of the version are written and flushed, before
    /// the archive is read back for its hash. `staging` is the directory that holds them.
    fn before_read_back(&self, _staging: &Path) {}

    /// Runs after the hash of the archive was read back, before the
    /// folder is compared with the archive.
    fn before_verify(&self, _staging: &Path) {}
}

/// The real run.
pub(crate) struct RealSeams;

impl Seams for RealSeams {}
