// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The library behind the antiq command-line tool. The binary in
//! `src/main.rs` parses arguments and reports errors; everything else
//! lives here.

// Tests print paths in assertion messages, which no user reads as output.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

// Lets a fixture shared with the binary tests through `#[path]` use `antiq::`
// paths in both crates.
extern crate self as antiq;

pub mod archive;
pub mod clock;
pub mod code_span;
pub mod collection;
pub mod commands;
pub mod config;
pub mod error;
pub mod external;
pub mod filesystem;
pub mod id;
pub mod idle;
pub mod listing;
pub mod lock;
pub mod machine;
pub mod markers;
pub mod pin;
pub mod plan;
pub mod progress;
pub mod query;
pub mod reference;
pub mod relpath;
pub mod repair;
pub mod report;
pub mod rules;
pub mod run;
pub mod select;
pub mod shown;
pub mod sysmem;
pub mod target;
pub mod tree;
pub mod versions;
pub mod walk;

pub use error::Error;

#[cfg(test)]
mod testing;
