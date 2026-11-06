// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario tests. The harness itself is tested in `harness`.

#[path = "../support/mod.rs"]
mod support;

mod archive;
mod damaged_version;
mod find;
mod forget;
mod grep;
mod harness;
mod import;
mod init;
mod list;
mod pin;
mod push;
mod restore_from_target;
mod status;
mod unearth;
mod verify;
