// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `fork` command: extracts a working copy of a version.

use crate::cli::{Context, ForkArgs};

pub fn run(ctx: &Context, _args: &ForkArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "fork")
}
