// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` command.

use crate::cli::{Context, PieceArgs};

pub fn run(_ctx: &Context, _args: &PieceArgs) -> anyhow::Result<u8> {
    super::not_implemented("pin")
}
