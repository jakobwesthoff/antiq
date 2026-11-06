// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `unpin` command.

use crate::cli::output::Output;
use crate::cli::{Context, PieceArgs};
use antiq::ops::pin::Direction;

pub fn run(ctx: &Context, args: &PieceArgs, out: &mut Output) -> anyhow::Result<u8> {
    super::pin::run_direction(ctx, args, out, Direction::Unpin)
}
