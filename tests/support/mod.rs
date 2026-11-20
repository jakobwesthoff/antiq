// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the integration tests.

pub mod cmd;
pub mod fake_rsync;
pub mod scenario;
#[allow(
    dead_code,
    reason = "shared with the unit tests through `#[path]`, which use the rest"
)]
pub mod screen;
pub mod ssh_shim;
#[allow(
    dead_code,
    reason = "shared with the unit tests through `#[path]`, which use the rest"
)]
pub mod tree;
#[allow(
    dead_code,
    reason = "shared with the unit tests through `#[path]`, which use the rest"
)]
pub mod versions;
