// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `forget` command: deletes a version, or all versions of a piece.

use crate::cli::{Context, ForgetArgs};

pub fn run(ctx: &Context, _args: &ForgetArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "forget")
}
