// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `push` command: mirrors the collection to the target.

use crate::cli::{Context, PushArgs};

pub fn run(ctx: &Context, _args: &PushArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "push")
}
