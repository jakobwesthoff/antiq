// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The pieces of reaching a backup target: its URL, its marker file and the
//! `rsync` call.

pub mod marker;
pub mod rsync;
pub mod url;

#[cfg(test)]
pub(crate) mod fake;
