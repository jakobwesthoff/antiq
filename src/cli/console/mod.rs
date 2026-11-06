// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How antiq talks to a person at a terminal.

pub mod block;
pub mod cards;
pub mod dryrun;
pub mod env;
pub mod render;
pub mod rich;
pub mod symbols;
pub mod theme;

#[cfg(test)]
mod tests;
