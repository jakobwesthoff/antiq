// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fuzzy query that search-word piece references and `list` share.

use crate::error::HasKind;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32String};

/// Why a query failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {}

impl HasKind for QueryError {}

/// A fuzzy query: the one place antiq scores text against what the user
/// typed, so search-word piece references and `list` rank alike.
///
/// Matching is smart case (a capital makes the match case sensitive), smart
/// normalisation (an accent in the query must match; a plain letter matches
/// its accented forms) and several words that must all match the same text.
pub struct Query {
    pattern: Pattern,
    matcher: Matcher,
    empty: bool,
}

impl Query {
    pub fn parse(text: &str) -> Query {
        let text = text.trim();
        Query {
            pattern: Pattern::parse(text, CaseMatching::Smart, Normalization::Smart),
            matcher: Matcher::new(Config::DEFAULT),
            empty: text.is_empty(),
        }
    }

    /// A query without atoms matches everything.
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// The score of `haystack`, or `None` when it does not match.
    pub fn score(&mut self, haystack: &str) -> Option<u32> {
        if self.empty {
            return Some(0);
        }
        let haystack = Utf32String::from(haystack);
        self.pattern.score(haystack.slice(..), &mut self.matcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, haystack: &str) -> Option<u32> {
        Query::parse(query).score(haystack)
    }

    #[test]
    fn a_word_matches_text_that_holds_its_letters_in_order() {
        assert!(score("pa", "proj/alpha").is_some());
        assert!(score("zq", "proj/alpha").is_none());
    }

    #[test]
    fn an_empty_query_matches_everything_with_score_zero() {
        for text in ["", "   "] {
            let mut query = Query::parse(text);
            assert!(query.is_empty(), "{text:?}");
            assert_eq!(query.score("anything"), Some(0));
        }
        assert!(!Query::parse("a").is_empty());
    }

    #[test]
    fn surrounding_whitespace_does_not_change_the_match() {
        let plain = score("pa", "proj/alpha");
        assert!(plain.is_some());
        assert_eq!(score("  pa  ", "proj/alpha"), plain);
    }

    #[test]
    fn lower_case_matches_any_case_and_a_capital_makes_it_exact() {
        assert!(score("alpha", "Alpha").is_some());
        assert!(score("Alpha", "alpha").is_none());
        assert!(score("Alpha", "Alpha").is_some());
    }

    #[test]
    fn several_words_must_all_match() {
        assert!(score("pro alp", "projects/alpha").is_some());
        assert!(score("pro zzz", "projects/alpha").is_none());
    }

    #[test]
    fn a_plain_letter_matches_an_accented_one_and_not_the_other_way_round() {
        assert!(score("cafe", "caf\u{e9}").is_some());
        assert!(score("caf\u{e9}", "cafe").is_none());
    }
}
