// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version conflicts during sync.

use super::{SyncContext, SyncError};
use crate::id::ObjectId;
use crate::tracking::{self, DecisionKind};

/// Records that this machine's change to the object conflicts with a newer
/// version on the remote, and waits for `resolve`. `committed` is this
/// machine's own version of the change when it reached the remote, which
/// the entry remembers as committed by this machine.
// TODO(I31): replace the body with the conflict handling of ADR 0017. Until
// then the piece is only marked and skipped.
pub(super) fn record(
    ctx: &SyncContext<'_>,
    id: ObjectId,
    committed: Option<u32>,
) -> Result<(), SyncError> {
    let Some(mut entry) = tracking::read(ctx.collection, id)? else {
        return Ok(());
    };
    if let Some(number) = committed
        && !entry.committed_versions.contains(&number)
    {
        entry.committed_versions.push(number);
    }
    entry.decide(DecisionKind::VersionConflict);
    tracking::write(ctx.collection, ctx.lock, ctx.clock, &mut entry)?;
    Ok(())
}
