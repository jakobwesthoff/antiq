// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `resolve` command.

use crate::cli::{Context, ResolveArgs};

pub fn run(_ctx: &Context, _args: &ResolveArgs) -> anyhow::Result<u8> {
    super::not_implemented("resolve")
}
