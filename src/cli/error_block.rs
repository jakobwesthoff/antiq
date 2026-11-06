// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The block a failed run is reported as.

use super::Description;
use super::console::block::Block;
use super::console::rich::Rich;

/// The error block for `description`: the root message as the title, the
/// candidates of an ambiguous reference, and the steps the run was in.
///
/// The message goes in as plain text, never as markup, because it can hold
/// file names with backticks.
pub fn block_for(description: &Description) -> Block {
    let mut block = Block::error(Rich::new().text(&description.root));
    let candidates = description
        .library_error
        .and_then(antiq::Error::candidates)
        .unwrap_or_default();
    for candidate in candidates {
        block = block.candidate(&candidate.path);
    }
    for step in &description.contexts {
        block = block.doing(step);
    }
    block
}
