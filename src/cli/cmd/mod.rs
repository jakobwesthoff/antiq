// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One module per command. Each has a `run` that builds the call into the
//! library and renders its result.

pub mod archive;
pub mod fork;
pub mod import;
pub mod init;
pub mod pin;
pub mod reconcile;
pub mod remote;
pub mod resolve;
pub mod status;
pub mod sync;
pub mod unearth;
pub mod unpin;
