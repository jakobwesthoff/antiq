// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `import` command: packs a folder as a version.

use crate::cli::{Context, ImportArgs};

pub fn run(ctx: &Context, _args: &ImportArgs) -> anyhow::Result<u8> {
    super::not_implemented(ctx, "import")
}
