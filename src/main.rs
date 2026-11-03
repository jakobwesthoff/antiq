// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod cli;

use anyhow::Result;
use clap::Parser;

// `about` without a value takes the package description from Cargo.toml,
// so `--help` and the crate metadata say the same thing.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {}

fn main() -> Result<()> {
    // TODO: Add the subcommands once the v1 spec settles them.
    let _cli = Cli::parse();
    Ok(())
}
