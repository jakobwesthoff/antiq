// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `archive` command.

use crate::cli::output::Output;
use crate::cli::{Context, DryRunArgs};

pub fn run(_ctx: &Context, _args: &DryRunArgs, _out: &mut Output) -> anyhow::Result<u8> {
    super::not_implemented("archive")
}
