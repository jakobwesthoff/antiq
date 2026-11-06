// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `rebuild` command: repairs a damaged version.

use crate::cli::{Context, RebuildArgs};

pub fn run(ctx: &Context, _args: &RebuildArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "rebuild")
}
