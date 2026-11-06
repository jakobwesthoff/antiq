// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Column alignment for picker rows.
//!
//! The picker shows every piece as one aligned row: `piece  state  size
//! created`. Aligning the columns needs the widths of all pieces at once, so
//! this is a batch step over the whole set. The display string carries the
//! padding verbatim, which keeps the fuzzy match positions aligned with what
//! is drawn. The piece's ID is not part of the row; it travels as the row's
//! `detail`, which the stats line shows while the row is selected.
//!
//! The piece column takes what the fixed columns leave of the row width and
//! loses its middle when the path is longer. The picker runs this step again
//! when the terminal width changes.

use antiq::ops::find::FoundPiece;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::Row;
use crate::cli::cmd::find::{human_size, state_word};
use crate::cli::cmd::status::{mid, sanitize};

/// The separator between two columns.
const SEPARATOR: &str = "  ";

/// The piece column keeps at least this many columns when the row allows it.
const PIECE_MIN: usize = 20;

/// Render every piece into an aligned [`Row`] for a row `width` columns wide
/// (the terminal width less the selection pointer).
pub fn align_pieces(pieces: &[FoundPiece], width: usize) -> Vec<Row> {
    let cells: Vec<Cells> = pieces.iter().map(cells).collect();
    let widest = |cell: fn(&Cells) -> &str| {
        cells
            .iter()
            .map(|cells| cell(cells).width())
            .max()
            .unwrap_or(0)
    };
    let state_w = widest(|cells| &cells.state);
    let size_w = widest(|cells| &cells.size);
    let created_w = widest(|cells| &cells.created);

    // The piece column gets what the fixed columns and their three
    // separators leave, never more than its longest path needs.
    let fixed = state_w + size_w + created_w + 3 * SEPARATOR.width();
    let piece_w = widest(|cells| &cells.piece).min(width.saturating_sub(fixed));

    pieces
        .iter()
        .zip(&cells)
        .map(|(piece, cells)| Row {
            display: [
                pad(&mid(&cells.piece, piece_w), piece_w),
                pad(&cells.state, state_w),
                pad_left(&cells.size, size_w),
                cells.created.clone(),
            ]
            .join(SEPARATOR),
            detail: piece.id.to_string(),
            search: search_text(piece, &cells.piece),
        })
        .collect()
}

/// The text of one row's cells, before any cut or padding.
struct Cells {
    piece: String,
    state: String,
    size: String,
    created: String,
}

fn cells(piece: &FoundPiece) -> Cells {
    Cells {
        piece: sanitize(&piece.path.to_string()),
        state: state_word(piece.state).to_string(),
        size: human_size(piece.size),
        created: piece.created.strftime("%Y-%m-%d").to_string(),
    }
}

/// What the query is scored against: the piece's path, and its name when an
/// import made the name differ from the path's last component. The text
/// does not depend on the row width, so a resize keeps the ranking.
fn search_text(piece: &FoundPiece, shown_path: &str) -> String {
    let name = sanitize(&piece.name.to_string());
    let last = shown_path.rsplit('/').next().unwrap_or(shown_path);
    if name == last {
        shown_path.to_string()
    } else {
        format!("{shown_path}{SEPARATOR}{name}")
    }
}

/// Truncate `s` to at most `max` display columns, marking a cut with `…`.
///
/// Widths are Unicode display columns (via `unicode-width`), so a wide CJK
/// character counts as two and a zero-width combining mark as none. The ellipsis
/// occupies one column, reserved out of the budget on a cut.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = fit_prefix(s, max - 1).to_string();
    out.push('…');
    out
}

/// The longest prefix of `s` that ends on a grapheme boundary and is at most
/// `max` display columns wide.
///
/// The cut falls only between grapheme clusters: `#` without its VS16 or one
/// regional indicator of a flag draws as a different glyph than the whole
/// cluster, so a cluster that does not fit is dropped whole. The prefix is
/// still a character prefix, so character indices into `s` stay valid for it.
///
/// Width is the *string* width, the same measure `measure` and `pad` use, so
/// a cut cell lines up with the padding around it. It differs from the sum of
/// per-character widths for emoji sequences: `#` plus VS16 is one two-column
/// glyph whose characters sum to one, and a ZWJ family emoji is two columns
/// whose characters sum to more. Prefix widths are therefore not monotonic
/// even between grapheme boundaries (two regional-indicator pairs joined by a
/// ZWJ are two graphemes but one two-column glyph), so every boundary is
/// checked rather than stopping at the first prefix that overflows. Only
/// strings that do not fit pay for that scan.
pub(super) fn fit_prefix(s: &str, max: usize) -> &str {
    if s.width() <= max {
        return s;
    }
    let mut end = 0;
    for (i, grapheme) in s.grapheme_indices(true) {
        let next = i + grapheme.len();
        if s[..next].width() <= max {
            end = next;
        }
    }
    &s[..end]
}

/// The longest suffix of `s` that starts on a grapheme boundary and is at most
/// `max` display columns wide; the mirror of [`fit_prefix`], for text whose
/// end matters more than its start, such as a query being typed.
///
/// Like `fit_prefix` it measures string widths, and since suffix widths are
/// not monotonic either, it tries the grapheme boundaries in order from the
/// start and takes the first suffix that fits. A string that fits costs one
/// width measurement; one that does not costs a measurement per boundary
/// skipped, quadratic in its length. No cheaper cutoff is safe: across
/// grapheme boundaries a string can be narrower than its graphemes' widths
/// summed (two regional-indicator pairs joined by a ZWJ are two graphemes of
/// two columns each, yet two columns together), so summing graphemes from the
/// end cannot tell where the scan may stop.
pub(super) fn fit_suffix(s: &str, max: usize) -> &str {
    s.grapheme_indices(true)
        .map(|(i, _)| &s[i..])
        .find(|suffix| suffix.width() <= max)
        .unwrap_or(&s[s.len()..])
}

/// Right-pad `s` with spaces to `width` display columns (never truncates).
fn pad(s: &str, width: usize) -> String {
    let w = s.width();
    let mut out = s.to_string();
    if w < width {
        out.push_str(&" ".repeat(width - w));
    }
    out
}

/// Left-pad `s` with spaces to `width` display columns (never truncates).
fn pad_left(s: &str, width: usize) -> String {
    format!("{}{s}", " ".repeat(width.saturating_sub(s.width())))
}

#[cfg(test)]
mod tests {
    use super::*;

    use antiq::id::ObjectId;
    use antiq::ops::find::FoundState;
    use antiq::relpath::{Name, RelPath};
    use antiq::tracking::PieceState;
    use std::path::PathBuf;

    fn piece(number: u64, path: &str, state: PieceState, size: Option<u64>) -> FoundPiece {
        let rel = RelPath::parse(path).expect("a valid path");
        let name = Name::from_bytes(rel.components().last().expect("a name").to_vec())
            .expect("a valid name");
        let id = ObjectId::from_ulid(ulid::Ulid::from_parts(1_600_000_000_000 + number, 1));
        FoundPiece {
            id,
            abs_path: format!("/work/{path}"),
            folder: PathBuf::from(format!("/work/{path}")),
            short_id: id.to_string()[..10].to_string(),
            path: rel,
            name,
            state: FoundState::Piece(state),
            created: "2026-09-03T08:00:00Z".parse().expect("a time"),
            size,
        }
    }

    fn sp(count: usize) -> String {
        " ".repeat(count)
    }

    fn three() -> Vec<FoundPiece> {
        vec![
            piece(
                1,
                "2026_09_03_data-extraction",
                PieceState::Archived,
                Some(12 * 1024 * 1024),
            ),
            piece(2, "scratch", PieceState::Live, None),
            piece(3, "remote", PieceState::RemoteOnly, Some(512)),
        ]
    }

    #[test]
    fn archived_live_and_remote_only_rows_are_aligned_in_four_columns() {
        let rows = align_pieces(&three(), 100);
        let shown: Vec<&str> = rows.iter().map(|row| row.display.as_str()).collect();
        assert_eq!(
            shown,
            [
                "2026_09_03_data-extraction  archived     12 MiB  2026-09-03".to_string(),
                format!("scratch{}live{}-  2026-09-03", sp(21), sp(14)),
                format!("remote{}remote-only   512 B  2026-09-03", sp(22)),
            ]
        );
    }

    #[test]
    fn the_whole_id_travels_as_the_detail() {
        let pieces = three();
        let rows = align_pieces(&pieces, 100);
        assert_eq!(rows[1].detail, pieces[1].id.to_string());
    }

    #[test]
    fn a_path_that_does_not_fit_loses_its_middle_and_every_row_fits() {
        let rows = align_pieces(&three(), 50);
        assert!(rows[0].display.contains('…'), "{:?}", rows[0].display);
        for row in &rows {
            assert!(row.display.width() <= 50, "{:?}", row.display);
        }
        assert!(rows[0].display.ends_with("2026-09-03"));
    }

    #[test]
    fn the_search_text_is_the_path_and_the_name_when_it_differs() {
        let mut pieces = three();
        pieces[1].name = Name::from_bytes(b"imported".to_vec()).expect("a name");
        let rows = align_pieces(&pieces, 100);
        assert_eq!(rows[0].search, "2026_09_03_data-extraction");
        assert_eq!(rows[1].search, "scratch  imported");
    }

    #[test]
    fn the_search_text_does_not_depend_on_the_width() {
        let wide = align_pieces(&three(), 200);
        let narrow = align_pieces(&three(), 40);
        for (wide, narrow) in wide.iter().zip(&narrow) {
            assert_eq!(wide.search, narrow.search);
        }
    }

    #[test]
    fn a_control_character_in_a_path_is_shown_escaped() {
        let pieces = [piece(1, "a\nb", PieceState::Live, Some(1))];
        let rows = align_pieces(&pieces, 100);
        assert!(
            rows[0].display.starts_with("a%0Ab"),
            "{:?}",
            rows[0].display
        );
        assert_eq!(rows[0].search, "a%0Ab");
    }

    #[test]
    fn an_object_in_error_shows_the_word_error() {
        let mut pieces = three();
        pieces[2].state = FoundState::Error;
        let rows = align_pieces(&pieces, 100);
        assert!(rows[2].display.contains("error"), "{:?}", rows[2].display);
    }

    #[test]
    fn no_pieces_give_no_rows() {
        assert!(align_pieces(&[], 100).is_empty());
    }

    // -------------------------------------------------------------------------
    // String-width cuts
    // -------------------------------------------------------------------------

    #[test]
    fn truncate_measures_emoji_presentation_by_string_width() {
        // `#` followed by VS16 renders as one two-column emoji, while its
        // per-character widths sum to one. The cut must honour the string
        // width or the cell spills past its column.
        let cut = truncate("#\u{FE0F}xyz", 3);
        assert!(cut.width() <= 3, "{cut:?} is {} columns", cut.width());
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn fit_prefix_keeps_a_zwj_sequence_that_fits_by_string_width() {
        // The family emoji is one two-column glyph, so with the trailing `x`
        // the whole string is three columns wide, although its characters'
        // widths sum to more.
        let s = "🧑\u{200D}🤝\u{200D}🧑x";
        assert_eq!(s.width(), 3);
        assert_eq!(fit_prefix(s, 3), s);
    }

    #[test]
    fn fit_prefix_stops_at_the_widest_prefix_that_fits() {
        assert_eq!(fit_prefix("abcdef", 4), "abcd");
        // A wide character that would straddle the edge is dropped whole.
        assert_eq!(fit_prefix("日本語", 5), "日本");
        assert_eq!(fit_prefix("abc", 0), "");
    }

    #[test]
    fn fit_prefix_returns_a_fitting_string_unchanged() {
        assert_eq!(fit_prefix("abc", 3), "abc");
        assert_eq!(fit_prefix("abc", 10), "abc");
        assert_eq!(fit_prefix("", 0), "");
    }

    #[test]
    fn fit_prefix_finds_a_complete_zwj_sequence_past_a_wider_partial_one() {
        // Two regional-indicator pairs joined by a ZWJ form one two-column
        // ligature, but the prefix that stops after the third indicator is
        // three columns wide. A scan that stopped at the first prefix over
        // budget would keep only the first pair and the joiner.
        let joined = "🇦🇦\u{200D}🇦🇦";
        assert_eq!(joined.width(), 2);
        assert_eq!("🇦🇦\u{200D}🇦".width(), 3);
        assert_eq!(fit_prefix(&format!("{joined}x"), 2), joined);
    }

    #[test]
    fn fit_prefix_never_exceeds_the_budget_inside_a_zwj_sequence() {
        // Cutting into the family emoji leaves a partial sequence whose string
        // width differs from the complete glyph's; whatever prefix is kept
        // must still fit.
        let s = "🧑\u{200D}🤝\u{200D}🧑x";
        for max in 0..=s.width() {
            let kept = fit_prefix(s, max);
            assert!(kept.width() <= max, "{kept:?} exceeds {max}");
        }
    }

    #[test]
    fn fit_prefix_keeps_a_keycap_whole() {
        // `#` and VS16 are one two-column grapheme; one column holds none of
        // it, and `#` alone would draw as a different, narrow glyph.
        assert_eq!(fit_prefix("#\u{FE0F}xyz", 1), "");
    }

    #[test]
    fn fit_prefix_keeps_a_flag_whole() {
        // A lone regional indicator is a different glyph from the flag.
        assert_eq!(fit_prefix("🇩🇪x", 1), "");
    }

    #[test]
    fn fit_prefix_keeps_a_zwj_sequence_with_vs16_whole() {
        assert_eq!(fit_prefix("🏳\u{FE0F}\u{200D}🌈x", 1), "");
    }

    #[test]
    fn fit_suffix_keeps_the_tail() {
        assert_eq!(fit_suffix("abcdef", 4), "cdef");
    }

    #[test]
    fn fit_suffix_returns_a_fitting_string_whole() {
        assert_eq!(fit_suffix("abc", 3), "abc");
    }

    #[test]
    fn fit_suffix_drops_a_wide_character_at_the_cut() {
        assert_eq!(fit_suffix("日本語", 5), "本語");
    }

    #[test]
    fn fit_suffix_keeps_a_flag_whole() {
        // A character-boundary cut would keep the second regional indicator.
        assert_eq!(fit_suffix("🇩🇪a", 2), "a");
    }

    #[test]
    fn fit_suffix_keeps_a_keycap_whole() {
        // A character-boundary cut would keep the VS16 without its `#`.
        assert_eq!(fit_suffix("#\u{FE0F}a", 2), "a");
    }

    #[test]
    fn fit_suffix_keeps_a_zwj_sequence_that_fits() {
        let people = "🧑\u{200D}🤝\u{200D}🧑";
        assert_eq!(fit_suffix(&format!("x{people}"), 2), people);
    }

    #[test]
    fn fit_suffix_to_zero_columns_is_empty() {
        assert_eq!(fit_suffix("abc", 0), "");
    }

    #[test]
    fn truncate_leaves_a_fitting_string_alone() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_to_zero_columns_is_empty() {
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn truncate_to_one_column_is_only_the_ellipsis() {
        assert_eq!(truncate("abc", 1), "…");
    }
}
