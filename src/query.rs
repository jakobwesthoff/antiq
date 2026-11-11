// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The fuzzy query that search-word piece references and `find` share.

use crate::error::HasKind;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32String};
use unicode_normalization::{UnicodeNormalization, is_nfc};
use unicode_segmentation::UnicodeSegmentation;

/// Why a query failed. Nothing in it can fail yet.
#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum QueryError {}

impl HasKind for QueryError {}

/// A fuzzy query: the one place antiq scores text against what the user
/// typed, so search-word piece references and `find` rank alike.
///
/// Matching is smart case (a capital makes the match case sensitive), smart
/// normalisation (an accent in the query must match; a plain letter matches
/// its accented forms) and several words that must all match the same text.
///
/// A text matches when the query matches it as stored, or when the query
/// matches it with query and text both composed (NFC), so a word typed with
/// a precomposed accent finds a path stored with a combining one. All words
/// of a query match in the same form.
pub struct Query {
    /// The query as typed.
    stored: Pattern,
    /// The query composed per grapheme cluster.
    composed: Pattern,
    /// Whether composing changed the query. Without that, a text that is
    /// already composed gives the second form nothing new to find.
    composes_query: bool,
    matcher: Matcher,
    empty: bool,
}

/// `text` with every grapheme cluster composed (NFC). The clusters are
/// composed one by one so that the text keeps the clusters nucleo counts
/// in the stored form.
fn composed_text(text: &str) -> String {
    text.graphemes(true)
        .flat_map(|cluster| cluster.nfc())
        .collect()
}

/// The slots nucleo scores a composed `text` on: the first character of
/// each cluster after composing it, the way nucleo itself reduces a
/// cluster. Slot `k` is cluster `k` of `text`.
fn composed_slots(text: &str) -> Utf32String {
    if text.is_ascii() {
        return Utf32String::from(text);
    }
    let slots = text
        .graphemes(true)
        .map(|cluster| {
            cluster
                .nfc()
                .next()
                .expect("a grapheme cluster has at least one character")
        })
        .collect();
    Utf32String::Unicode(slots)
}

impl Query {
    pub fn parse(text: &str) -> Query {
        let text = text.trim();
        let composed = composed_text(text);
        Query {
            stored: Pattern::parse(text, CaseMatching::Smart, Normalization::Smart),
            composed: Pattern::parse(&composed, CaseMatching::Smart, Normalization::Smart),
            composes_query: composed != text,
            matcher: Matcher::new(Config::DEFAULT),
            empty: text.is_empty(),
        }
    }

    /// A query without atoms matches everything.
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// Whether the composed form can match `text` differently from the
    /// stored one.
    fn tries_composed(&self, text: &str) -> bool {
        self.composes_query || !is_nfc(text)
    }

    /// The score of `haystack`, or `None` when it does not match: the
    /// better of the stored and the composed form.
    pub fn score(&mut self, haystack: &str) -> Option<u32> {
        if self.empty {
            return Some(0);
        }
        let stored = Utf32String::from(haystack);
        let stored = self.stored.score(stored.slice(..), &mut self.matcher);
        if !self.tries_composed(haystack) {
            return stored;
        }
        let composed = composed_slots(haystack);
        let composed = self.composed.score(composed.slice(..), &mut self.matcher);
        stored.max(composed)
    }

    /// The positions, counted in characters (`char`s), of the letters of
    /// `text` that the query matched; empty when it does not match, and
    /// also when every word of the query is negated. A letter with
    /// combining marks is marked with them. The stored form decides when
    /// it matches; the composed form only when the stored one does not.
    pub fn matched(&mut self, text: &str) -> Vec<usize> {
        let mut positions = Vec::new();
        let stored = Utf32String::from(text);
        let mut found = self
            .stored
            .indices(stored.slice(..), &mut self.matcher, &mut positions)
            .is_some();
        if !found && self.tries_composed(text) {
            positions.clear();
            let composed = composed_slots(text);
            found = self
                .composed
                .indices(composed.slice(..), &mut self.matcher, &mut positions)
                .is_some();
        }
        if !found {
            return Vec::new();
        }
        // Each word of the query reports its own positions, in no common order.
        positions.sort_unstable();
        positions.dedup();
        let positions = positions.into_iter().map(|position| position as usize);

        // A text with a non-ASCII character is held as one slot per
        // grapheme cluster, so a position counts clusters there. A
        // cluster of several characters (a letter and its combining
        // accent, or `\r\n`) is marked as a whole.
        if text.is_ascii() {
            return positions.collect();
        }
        let mut cluster_start = Vec::new();
        let mut cluster_len = Vec::new();
        let mut characters = 0;
        for cluster in text.graphemes(true) {
            let length = cluster.chars().count();
            cluster_start.push(characters);
            cluster_len.push(length);
            characters += length;
        }
        positions
            .flat_map(|cluster| {
                cluster_start[cluster]..cluster_start[cluster] + cluster_len[cluster]
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, haystack: &str) -> Option<u32> {
        Query::parse(query).score(haystack)
    }

    #[test]
    fn the_matched_positions_are_the_letters_the_query_found() {
        let mut query = Query::parse("scan");
        assert_eq!(query.matched("projects/scanner"), [9, 10, 11, 12]);
        assert!(query.matched("projects/other").is_empty());
    }

    #[test]
    fn a_decomposed_accent_does_not_shift_the_matched_positions() {
        let mut query = Query::parse("scan");
        assert_eq!(query.matched("cafe\u{301}/scanner"), [6, 7, 8, 9]);
    }

    #[test]
    fn a_matched_decomposed_accent_is_marked_with_its_base() {
        let mut query = Query::parse("cafe");
        assert_eq!(query.matched("cafe\u{301}"), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_crlf_in_a_non_ascii_path_counts_as_two_characters() {
        let mut query = Query::parse("scan");
        assert_eq!(query.matched("\u{e9}\r\nscan"), [3, 4, 5, 6]);
    }

    #[test]
    fn a_crlf_in_an_ascii_path_is_unchanged() {
        let mut query = Query::parse("scan");
        assert_eq!(query.matched("a\r\nscan"), [3, 4, 5, 6]);
    }

    #[test]
    fn several_words_mark_every_letter_once_in_text_order() {
        let mut query = Query::parse("proj scan");
        let marked = query.matched("projects/scanner");
        assert_eq!(marked, [0, 1, 2, 3, 9, 10, 11, 12]);
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

    #[test]
    fn a_precomposed_word_matches_a_decomposed_path() {
        assert!(score("caf\u{e9}", "cafe\u{301}").is_some());
    }

    #[test]
    fn a_precomposed_word_scores_a_decomposed_path_like_its_precomposed_twin() {
        let twin = score("caf\u{e9}", "caf\u{e9}");
        assert!(twin.is_some());
        assert_eq!(score("caf\u{e9}", "cafe\u{301}"), twin);
    }

    #[test]
    fn a_precomposed_word_marks_a_decomposed_path() {
        let mut query = Query::parse("caf\u{e9}");
        assert_eq!(query.matched("cafe\u{301}/scanner"), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn several_words_mark_a_decomposed_path_in_one_form() {
        let mut query = Query::parse("caf\u{e9} scan");
        let marked = query.matched("cafe\u{301}/scanner");
        assert_eq!(marked, [0, 1, 2, 3, 4, 6, 7, 8, 9]);
    }

    #[test]
    fn a_letter_that_composes_to_ascii_is_marked_at_its_place() {
        // U+212A KELVIN SIGN composes to the ASCII `K`.
        let mut query = Query::parse("Key");
        assert_eq!(query.matched("\u{212a}ey"), [0, 1, 2]);
    }

    #[test]
    fn a_decomposed_word_matches_a_precomposed_path() {
        assert!(score("cafe\u{301}", "caf\u{e9}").is_some());
    }

    #[test]
    fn a_decomposed_word_marks_a_precomposed_path() {
        let mut query = Query::parse("cafe\u{301}");
        assert_eq!(query.matched("caf\u{e9}/scanner"), [0, 1, 2, 3]);
    }

    #[test]
    fn a_plain_letter_matches_a_decomposed_letter_outside_the_fold_table() {
        assert!(score("e", "e\u{302}\u{323}").is_some());
        assert!(score("\u{3b1}", "\u{3b1}\u{301}").is_some());
        assert!(score("\u{438}", "\u{438}\u{306}").is_some());
    }

    #[test]
    fn a_decomposed_word_still_matches_another_accent_of_its_letter() {
        assert!(score("cafe\u{301}", "caf\u{e8}").is_some());
    }

    #[test]
    fn a_decomposed_word_does_not_match_a_path_without_the_accent() {
        let mut query = Query::parse("cafe\u{301}");
        assert!(query.score("cafe/scanner").is_none());
        assert!(query.matched("cafe/scanner").is_empty());
    }

    #[test]
    fn a_precomposed_word_marks_a_precomposed_path() {
        let mut query = Query::parse("caf\u{e9}");
        assert!(score("caf\u{e9}", "caf\u{e9}/scanner").is_some());
        assert_eq!(query.matched("caf\u{e9}/scanner"), [0, 1, 2, 3]);
    }
}
