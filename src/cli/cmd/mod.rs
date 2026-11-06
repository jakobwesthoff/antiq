// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One module per command that has an implementation, and the stand-in for
//! those that have none.

pub mod init;

use crate::cli::Context;
use antiq::collection::Collection;
use anyhow::Context as _;
use std::path::PathBuf;

/// Runs a command that has no implementation: it finds the collection, so
/// that a directory outside one still gets the answer for that, and then
/// fails with the kind `not_implemented`.
pub fn not_implemented(ctx: &Context, command: &'static str) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    Err(antiq::Error::NotImplemented {
        command: command.to_string(),
    }
    .into())
}
