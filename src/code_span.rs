// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Values inside error messages, fenced so a reader of the text can tell
//! them from the words around them.

use std::fmt::{self, Display};

/// A value shown as a CommonMark code span: in backticks, with one more
/// backtick on each side than the longest run inside the value, and a
/// space inside the fence when the value starts or ends with a backtick.
///
/// The messages of the library carry paths and names from the user's
/// files, which can hold backticks. A plain pair of backticks around them
/// would end early, so the console would style the wrong part of the text.
pub struct CodeSpan<T>(pub T);

impl<T: Display> Display for CodeSpan<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0.to_string();
        let longest = value
            .split(|character| character != '`')
            .map(str::len)
            .max()
            .unwrap_or(0);
        let fence = "`".repeat(longest + 1);
        let pad = if value.starts_with('`') || value.ends_with('`') {
            " "
        } else {
            ""
        };
        write!(formatter, "{fence}{pad}{value}{pad}{fence}")
    }
}

#[cfg(test)]
mod tests {
    use super::CodeSpan;

    #[test]
    fn a_plain_value_gets_one_backtick_on_each_side() {
        assert_eq!(
            CodeSpan("projects/scanner").to_string(),
            "`projects/scanner`"
        );
    }

    #[test]
    fn a_value_with_a_backtick_gets_a_longer_fence() {
        assert_eq!(CodeSpan("a`b").to_string(), "``a`b``");
        assert_eq!(CodeSpan("a``b`c").to_string(), "```a``b`c```");
    }

    #[test]
    fn a_value_at_the_edge_of_the_fence_is_padded() {
        assert_eq!(CodeSpan("`a").to_string(), "`` `a ``");
        assert_eq!(CodeSpan("a`").to_string(), "`` a` ``");
    }

    #[test]
    fn an_empty_value_is_an_empty_span() {
        assert_eq!(CodeSpan("").to_string(), "``");
    }
}
