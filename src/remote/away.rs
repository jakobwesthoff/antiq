// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Warnings about a remote that stays away.

use crate::collection::Collection;
use crate::remote::RemoteState;
use crate::report::Warning;
use jiff::Timestamp;

/// The warning `status` shows when the remote has been away for too long
/// while remote work is pending.
pub fn status_warning(
    _collection: &Collection,
    _state: RemoteState,
    _pending_remote_work: bool,
    _now: Timestamp,
) -> Option<Warning> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_warning_is_given_yet() {
        let collection = Collection::for_tests();
        let now = Timestamp::now();
        assert_eq!(
            status_warning(&collection, RemoteState::Away, true, now),
            None
        );
    }
}
