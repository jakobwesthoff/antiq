// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Helpers shared by the binary tests. Every test crate includes this
//! module and uses a part of it.
#![allow(dead_code)]

pub mod cmd;
pub mod fake_rsync;
pub mod scenario;
pub mod ssh_shim;
pub mod sshd;
pub mod tree;
pub mod versions;
