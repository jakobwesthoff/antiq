// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version conflicts.

use super::ResolveError;
use crate::tracking::Answer;

/// This version does not answer a version conflict; the answer is refused.
pub(super) fn answer(answer: Answer) -> ResolveError {
    ResolveError::Unsupported(format!(
        "answering a version conflict with {}",
        super::flag(answer)
    ))
}
