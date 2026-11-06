// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Archie's emblem as terminal text, in the rendering that fits the
//! terminal.
//!
//! The renderings are files in `assets/emblem/terminal/`, embedded at
//! compile time (`build.rs`). Each is drawn for one cell shape and one
//! glyph set. A cell that is narrower or wider than the shape a rendering
//! was drawn for squeezes or stretches the picture, so the rendering whose
//! shape is nearest to the terminal's is taken. Sextants come out as boxes
//! of a missing glyph where neither the terminal nor its font draws them,
//! so a terminal that is not known to draw them gets block elements.

mod detect;
mod glyphs;
#[allow(dead_code)] // the file name parser is used by build.rs, which includes this file
mod name;
mod tmux;

pub use detect::facts_of_stdout;
pub use name::Glyphs;

use std::sync::OnceLock;

/// The rendering to take when the terminal reports no cell shape. It is
/// chafa's default shape, and the shape Menlo, SF Mono and DejaVu Sans Mono
/// have to within a hundredth.
const FALLBACK_RATIO_HUNDREDTHS: u64 = 52;

/// A rendering as `build.rs` embeds it.
struct Embedded {
    ratio_hundredths: u32,
    glyphs: Glyphs,
    text: &'static str,
}

const EMBEDDED: &[Embedded] = include!(concat!(env!("OUT_DIR"), "/emblem_table.rs"));

/// One embedded rendering: the lines to print and how wide they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendering {
    /// Cell width over cell height, in hundredths, the rendering is drawn for.
    pub ratio_hundredths: u32,
    pub glyphs: Glyphs,
    /// The rows, each ending in a reset.
    pub lines: Vec<String>,
    /// Columns of the widest row.
    pub width: usize,
}

impl Rendering {
    /// Reads a rendering from the text of its file.
    pub fn new(ratio_hundredths: u32, glyphs: Glyphs, text: &str) -> Rendering {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let width = lines
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0);
        Rendering {
            ratio_hundredths,
            glyphs,
            lines,
            width,
        }
    }

    pub fn height(&self) -> usize {
        self.lines.len()
    }
}

/// Columns `text` takes on a terminal once its SGR sequences are gone. The
/// glyphs of the renderings and of the version text are one column wide.
pub fn visible_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            // An SGR sequence runs from `ESC [` to its final letter.
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            width += 1;
        }
    }
    width
}

/// The shape of a terminal's cell as a fraction: width over height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellShape {
    width: u64,
    height: u64,
}

impl CellShape {
    /// The shape of one cell, from its size in any common unit.
    pub fn of_cell(width: u32, height: u32) -> Option<CellShape> {
        (width > 0 && height > 0).then_some(CellShape {
            width: u64::from(width),
            height: u64::from(height),
        })
    }

    /// The shape of the cells of a window that is `pixel_width` by
    /// `pixel_height` pixels and `columns` by `rows` cells. A terminal that
    /// reports no pixels reports zeros, which give no shape.
    pub fn of_window(
        pixel_width: u16,
        pixel_height: u16,
        columns: u16,
        rows: u16,
    ) -> Option<CellShape> {
        if pixel_width == 0 || pixel_height == 0 || columns == 0 || rows == 0 {
            return None;
        }
        Some(CellShape {
            width: u64::from(pixel_width) * u64::from(rows),
            height: u64::from(pixel_height) * u64::from(columns),
        })
    }

    /// How far this shape is from `ratio_hundredths`, as a numerator over
    /// the common denominator `height * 100`. Integers keep a tie a tie.
    fn distance_to(&self, ratio_hundredths: u64) -> u64 {
        (self.width * 100).abs_diff(ratio_hundredths * self.height)
    }
}

/// What the choice of a rendering depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalFacts {
    /// Stdout is a terminal.
    pub terminal: bool,
    /// The terminal takes colour: `NO_COLOR`, `CLICOLOR` and the console's
    /// colour decision say yes.
    pub color: bool,
    /// The terminal takes 24-bit colour, which the renderings use.
    pub truecolor: bool,
    pub columns: usize,
    pub rows: usize,
    /// `None` when neither the window nor tmux knows it.
    pub cell: Option<CellShape>,
    pub glyphs: Glyphs,
}

/// Rows the prompt takes below the output.
const PROMPT_ROWS: usize = 1;

/// The rendering that fits the terminal described by `facts` and leaves
/// `beside` columns next to it, or `None` when no emblem should be shown:
/// stdout is no terminal, takes no colour or no 24-bit colour, or the window
/// holds neither the picture and what stands beside it nor the prompt below.
pub fn for_terminal(facts: &TerminalFacts, beside: usize) -> Option<&'static Rendering> {
    choose(renderings(), facts, beside)
}

/// `for_terminal` over the renderings `available`.
pub fn choose<'a>(
    available: &'a [Rendering],
    facts: &TerminalFacts,
    beside: usize,
) -> Option<&'a Rendering> {
    if !(facts.terminal && facts.color && facts.truecolor) {
        return None;
    }
    // Sextants only where the terminal draws them; blocks stand in
    // otherwise, also for a sextant set that lacks files.
    let wanted = |glyphs| available.iter().filter(move |r| r.glyphs == glyphs);
    let candidates: Vec<&Rendering> = match wanted(facts.glyphs).count() {
        0 => wanted(Glyphs::Block).collect(),
        _ => wanted(facts.glyphs).collect(),
    };
    let shape = facts.cell.unwrap_or(CellShape {
        width: FALLBACK_RATIO_HUNDREDTHS,
        height: 100,
    });
    // The ratio is part of the key, so a tie goes to the smaller one.
    let rendering = candidates.into_iter().min_by_key(|r| {
        (
            shape.distance_to(u64::from(r.ratio_hundredths)),
            r.ratio_hundredths,
        )
    })?;
    let fits =
        facts.columns >= rendering.width + beside && facts.rows >= rendering.height() + PROMPT_ROWS;
    fits.then_some(rendering)
}

/// Every embedded rendering, read once.
fn renderings() -> &'static [Rendering] {
    static RENDERINGS: OnceLock<Vec<Rendering>> = OnceLock::new();
    RENDERINGS.get_or_init(|| {
        EMBEDDED
            .iter()
            .map(|e| Rendering::new(e.ratio_hundredths, e.glyphs, e.text))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rendering of `width` blocks in each of `height` rows.
    fn rendering(ratio: u32, glyphs: Glyphs, width: usize, height: usize) -> Rendering {
        let row = format!("\x1b[38;2;1;2;3m{}\x1b[0m", "#".repeat(width));
        Rendering::new(ratio, glyphs, &vec![row; height].join("\n"))
    }

    fn set() -> Vec<Rendering> {
        let mut all = Vec::new();
        for (ratio, width) in [(39, 56), (43, 51), (47, 47), (52, 42)] {
            all.push(rendering(ratio, Glyphs::Sextant, width, 22));
            all.push(rendering(ratio, Glyphs::Block, width, 22));
        }
        all
    }

    fn facts() -> TerminalFacts {
        TerminalFacts {
            terminal: true,
            color: true,
            truecolor: true,
            columns: 120,
            rows: 40,
            cell: CellShape::of_cell(52, 100),
            glyphs: Glyphs::Block,
        }
    }

    fn picked(facts: &TerminalFacts) -> Option<(u32, Glyphs)> {
        choose(&set(), facts, 3 + 43).map(|r| (r.ratio_hundredths, r.glyphs))
    }

    #[test]
    fn the_embedded_renderings_are_all_there_and_22_rows_high() {
        let all = renderings();
        assert_eq!(all.len(), 8);
        assert!(all.iter().all(|r| r.height() == 22));
        let widths: Vec<_> = all
            .iter()
            .filter(|r| r.glyphs == Glyphs::Block)
            .map(|r| (r.ratio_hundredths, r.width))
            .collect();
        assert_eq!(widths, [(39, 56), (43, 51), (47, 47), (52, 42)]);
    }

    #[test]
    fn the_width_of_a_row_leaves_out_its_colour_codes() {
        assert_eq!(visible_width("\x1b[38;2;1;2;3mab\x1b[0m c"), 4);
    }

    #[test]
    fn the_nearest_ratio_wins() {
        for (cell, ratio) in [
            ((39, 100), 39),
            ((40, 100), 39),
            ((45, 100), 43),
            ((46, 100), 47),
            ((50, 100), 52),
            ((90, 100), 52),
            ((10, 100), 39),
        ] {
            let facts = TerminalFacts {
                cell: CellShape::of_cell(cell.0, cell.1),
                ..facts()
            };
            assert_eq!(picked(&facts).map(|p| p.0), Some(ratio), "{cell:?}");
        }
    }

    #[test]
    fn a_tie_goes_to_the_smaller_ratio() {
        let facts = TerminalFacts {
            cell: CellShape::of_cell(41, 100),
            ..facts()
        };
        assert_eq!(picked(&facts).map(|p| p.0), Some(39));
    }

    #[test]
    fn a_cell_shape_comes_from_the_pixels_of_the_window() {
        // 1560 by 780 pixels over 100 by 20 cells: 15.6 wide, 39 high, 0.4.
        let shape = CellShape::of_window(1560, 780, 100, 20).expect("pixels are known");
        assert_eq!(shape.distance_to(40), 0);
    }

    #[test]
    fn a_window_without_pixels_or_cells_has_no_shape() {
        assert_eq!(CellShape::of_window(0, 0, 100, 20), None);
        assert_eq!(CellShape::of_window(100, 0, 100, 20), None);
        assert_eq!(CellShape::of_window(100, 100, 0, 20), None);
        assert_eq!(CellShape::of_cell(0, 39), None);
    }

    #[test]
    fn no_known_shape_takes_the_rendering_of_0_52() {
        let facts = TerminalFacts {
            cell: None,
            ..facts()
        };
        assert_eq!(picked(&facts), Some((52, Glyphs::Block)));
    }

    #[test]
    fn sextants_are_taken_where_the_terminal_draws_them() {
        let facts = TerminalFacts {
            glyphs: Glyphs::Sextant,
            ..facts()
        };
        assert_eq!(picked(&facts), Some((52, Glyphs::Sextant)));
    }

    #[test]
    fn blocks_stand_in_when_no_sextant_rendering_exists() {
        let blocks: Vec<_> = set()
            .into_iter()
            .filter(|r| r.glyphs == Glyphs::Block)
            .collect();
        let facts = TerminalFacts {
            glyphs: Glyphs::Sextant,
            ..facts()
        };
        let got = choose(&blocks, &facts, 46).map(|r| r.glyphs);
        assert_eq!(got, Some(Glyphs::Block));
    }

    #[test]
    fn nothing_is_chosen_from_no_renderings() {
        assert_eq!(choose(&[], &facts(), 46), None);
    }

    #[test]
    fn no_emblem_without_a_terminal_colour_or_24_bit_colour() {
        for broken in [
            TerminalFacts {
                terminal: false,
                ..facts()
            },
            TerminalFacts {
                color: false,
                ..facts()
            },
            TerminalFacts {
                truecolor: false,
                ..facts()
            },
        ] {
            assert_eq!(picked(&broken), None, "{broken:?}");
        }
    }

    #[test]
    fn no_emblem_when_the_window_is_too_small() {
        // Rendering of 0.52 is 42 wide: 42 + 46 = 88 columns, 23 rows.
        let at = |columns, rows| TerminalFacts {
            columns,
            rows,
            ..facts()
        };
        assert!(picked(&at(88, 23)).is_some());
        assert_eq!(picked(&at(87, 23)), None);
        assert_eq!(picked(&at(88, 22)), None);
    }
}
