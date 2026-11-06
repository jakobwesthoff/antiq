// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The file name of a rendering: `archie-r<ratio>.<glyphs>.ansi`, where the
//! ratio is the cell width over the cell height in hundredths and the glyphs
//! are `sextant` or `block`.
//!
//! `build.rs` includes this file with `#[path]` to read the directory of the
//! renderings, so it stands alone and names nothing of the crate.

/// The glyph set a rendering is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyphs {
    /// Sextants (Unicode 13, "symbols for legacy computing"), which need a
    /// terminal or font that draws them.
    Sextant,
    /// Block elements, which every terminal draws.
    Block,
}

/// What a rendering's file name says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmblemName {
    pub ratio_hundredths: u32,
    pub glyphs: Glyphs,
}

/// The pattern a file name has to follow, for the message of the build.
pub const PATTERN: &str = "archie-r<ratio in hundredths>.<sextant|block>.ansi";

/// Reads the ratio and the glyph set out of `file_name`.
pub fn parse(file_name: &str) -> Result<EmblemName, String> {
    let bad = |why: &str| format!("{file_name} {why}; the name has to be {PATTERN}");
    let rest = file_name
        .strip_prefix("archie-r")
        .ok_or_else(|| bad("does not start with \"archie-r\""))?;
    let rest = rest
        .strip_suffix(".ansi")
        .ok_or_else(|| bad("does not end in \".ansi\""))?;
    let (ratio, glyphs) = rest
        .split_once('.')
        .ok_or_else(|| bad("names no glyph set"))?;
    if ratio.is_empty() || !ratio.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(bad("has a ratio that is not a number"));
    }
    let ratio_hundredths = ratio
        .parse()
        .map_err(|_| bad("has a ratio that is too large"))?;
    if ratio_hundredths == 0 {
        return Err(bad("has a ratio of zero"));
    }
    let glyphs = match glyphs {
        "sextant" => Glyphs::Sextant,
        "block" => Glyphs::Block,
        _ => return Err(bad("names a glyph set other than sextant or block")),
    };
    Ok(EmblemName {
        ratio_hundredths,
        glyphs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_good_name_gives_the_ratio_and_the_glyph_set() {
        assert_eq!(
            parse("archie-r039.sextant.ansi"),
            Ok(EmblemName {
                ratio_hundredths: 39,
                glyphs: Glyphs::Sextant
            })
        );
        assert_eq!(
            parse("archie-r52.block.ansi"),
            Ok(EmblemName {
                ratio_hundredths: 52,
                glyphs: Glyphs::Block
            })
        );
    }

    #[test]
    fn a_bad_name_is_refused_with_the_pattern() {
        for name in [
            "archie-r039.sextant.txt",
            "archie-039.sextant.ansi",
            "other-r039.sextant.ansi",
            "archie-r.block.ansi",
            "archie-r0x9.block.ansi",
            "archie-r000.block.ansi",
            "archie-r039.braille.ansi",
            "archie-r039.ansi",
            "archie-r99999999999.block.ansi",
        ] {
            let error = parse(name).expect_err(name);
            assert!(error.contains(PATTERN), "{name}: {error}");
            assert!(error.contains(name), "{name}: {error}");
        }
    }
}
