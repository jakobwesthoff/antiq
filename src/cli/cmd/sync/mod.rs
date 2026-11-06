// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `sync` command.

pub mod conflict;
pub mod live;
pub mod tiers;

use crate::cli::output::Output;
use crate::cli::{Context, SyncArgs};

pub fn run(_ctx: &Context, _args: &SyncArgs, _out: &mut Output) -> anyhow::Result<u8> {
    super::not_implemented("sync")
}
