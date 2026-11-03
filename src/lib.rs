// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The library behind the antiq command-line tool. The binary in
//! `src/main.rs` parses arguments and reports errors; everything else
//! lives here.

pub mod brake;
pub mod clock;
pub mod collection;
pub mod config;
pub mod error;
pub mod fsx;
pub mod id;
pub mod idle;
pub mod index;
pub mod interact;
pub mod listing;
pub mod lock;
pub mod machine;
pub mod meta;
pub mod ops;
pub mod pack;
pub mod pin;
pub mod plan;
pub mod reference;
pub mod relpath;
pub mod remote;
pub mod repair;
pub mod report;
pub mod run;
pub mod select;
pub mod store;
pub mod sysmem;
pub mod tracking;
pub mod tree;
pub mod walk;

pub use error::Error;

#[cfg(test)]
mod testing;
