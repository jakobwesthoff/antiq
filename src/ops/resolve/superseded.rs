// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Superseded archives.

use super::ResolveError;
use crate::tracking::Answer;

/// This version does not answer a superseded archive; the answer is refused.
pub(super) fn answer(answer: Answer) -> ResolveError {
    ResolveError::Unsupported(format!(
        "answering a superseded archive with {}",
        super::flag(answer)
    ))
}
