// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `pin` and `unpin` commands: create or remove the `.antiq-pin` of a piece.

use crate::cli::{Context, PieceArgs};

pub fn pin(ctx: &Context, _args: &PieceArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "pin")
}

pub fn unpin(ctx: &Context, _args: &PieceArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "unpin")
}
