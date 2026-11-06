// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The repairs that open every run, after the lock is taken.

use crate::clock::Clock;
use crate::collection::Collection;
use crate::error::HasKind;
use crate::lock::RunLock;
use crate::report::Warning;

/// Why a repair failed. No repair exists yet, so none can.
#[derive(Debug, thiserror::Error)]
pub enum RepairError {}

impl HasKind for RepairError {}

/// Repairs what an earlier run left behind and returns what the user is to
/// be told about. The repairs are not written yet, so there is nothing to
/// repair and nothing to tell.
pub fn run(
    _collection: &Collection,
    _lock: &RunLock,
    _clock: &dyn Clock,
) -> Result<Vec<Warning>, RepairError> {
    Ok(Vec::new())
}
