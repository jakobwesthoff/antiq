// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Remotes.

pub mod away;
pub mod cleanup;
pub mod commit;
pub mod marker;
pub mod path;
pub mod policy;
pub mod rsync;
pub mod ssh;
#[cfg(test)]
pub mod testing;
pub mod url;
