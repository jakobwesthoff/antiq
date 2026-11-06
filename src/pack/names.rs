// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Name checks for extraction.

use super::TargetRules;
use crate::relpath::RelPath;
use std::collections::HashMap;
use std::fmt;
use unicode_normalization::UnicodeNormalization;

/// What a name limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthUnit {
    Bytes,
    Utf16Units,
}

/// The longest name a volume accepts, in the unit it counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameLimit {
    pub max: usize,
    pub unit: LengthUnit,
}

/// A reason a member cannot be extracted on a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameProblem {
    NotUtf8 {
        path: RelPath,
    },
    TooLong {
        path: RelPath,
        length: usize,
        limit: NameLimit,
    },
    /// Members whose names are one name on the volume.
    Collision {
        paths: Vec<RelPath>,
    },
}

impl fmt::Display for NameProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameProblem::NotUtf8 { path } => {
                write!(
                    f,
                    "`{path}` is not valid UTF-8, which this volume cannot hold"
                )
            }
            NameProblem::TooLong {
                path,
                length,
                limit,
            } => {
                let unit = match limit.unit {
                    LengthUnit::Bytes => "bytes",
                    LengthUnit::Utf16Units => "UTF-16 units",
                };
                write!(
                    f,
                    "`{path}` has a name of {length} {unit}, the volume allows {}",
                    limit.max
                )
            }
            NameProblem::Collision { paths } => {
                let (last, earlier) = paths
                    .split_last()
                    .expect("a collision names at least two paths");
                let earlier: Vec<String> = earlier.iter().map(|path| format!("`{path}`")).collect();
                let joined = earlier.join(", ");
                write!(f, "{joined} and `{last}` become one name on this volume")
            }
        }
    }
}

/// The systems whose name rules `archive` warns about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
}

/// A problem the named system would have with a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortabilityWarning {
    pub platform: Platform,
    pub problem: NameProblem,
}

impl fmt::Display for PortabilityWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let platform = match self.platform {
            Platform::MacOs => "macOS",
            Platform::Linux => "Linux",
        };
        write!(f, "on {platform}: {}", self.problem)
    }
}

impl TargetRules {
    /// APFS with its default settings.
    pub fn macos() -> TargetRules {
        TargetRules {
            case_insensitive: true,
            normalization_insensitive: true,
            requires_utf8: true,
            name_limit: NameLimit {
                max: 255,
                unit: LengthUnit::Utf16Units,
            },
        }
    }

    /// ext4, the filesystem of the Linux machines.
    pub fn linux() -> TargetRules {
        TargetRules {
            case_insensitive: false,
            normalization_insensitive: false,
            requires_utf8: false,
            name_limit: NameLimit {
                max: 255,
                unit: LengthUnit::Bytes,
            },
        }
    }
}

/// One path component as the volume compares it. A component that is not
/// UTF-8 has no case or normalisation to fold and keeps its bytes.
fn fold_component(name: &[u8], rules: &TargetRules) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(name) else {
        return name.to_vec();
    };
    let mut folded = text.to_string();
    if rules.normalization_insensitive {
        folded = folded.nfd().collect();
    }
    if rules.case_insensitive {
        // The volume folds case fully: `ß` and `ss`, `ſ` and `s`, `ﬀ` and
        // `ff`, `ς` and `σ` are each one name. Upper-casing maps the first
        // spelling of each pair onto the second, so the chain ends in equal
        // text. `ı` folds with `i` here although the volume keeps them
        // apart, which errs on the side of refusing.
        folded = folded.to_lowercase().to_uppercase().to_lowercase();
        if rules.normalization_insensitive {
            folded = folded.nfd().collect();
        }
    }
    folded.into_bytes()
}

/// The whole path as the volume compares it. Folding the directories as
/// well makes `a/X` and `A/x` one key.
fn fold_path(path: &RelPath, rules: &TargetRules) -> Vec<u8> {
    let components: Vec<Vec<u8>> = path
        .components()
        .map(|name| fold_component(name, rules))
        .collect();
    components.join(&b'/')
}

/// The problems of a member's own name. Every directory is a member
/// itself, so only the last component needs checking.
fn name_problems(path: &RelPath, rules: &TargetRules, problems: &mut Vec<NameProblem>) {
    let Some(name) = path.components().last() else {
        return;
    };
    if rules.requires_utf8 && std::str::from_utf8(name).is_err() {
        problems.push(NameProblem::NotUtf8 { path: path.clone() });
    }
    let length = match rules.name_limit.unit {
        LengthUnit::Bytes => name.len(),
        LengthUnit::Utf16Units => String::from_utf8_lossy(name).encode_utf16().count(),
    };
    if length > rules.name_limit.max {
        problems.push(NameProblem::TooLong {
            path: path.clone(),
            length,
            limit: rules.name_limit,
        });
    }
}

/// The problems `paths` would have on a volume with `rules`, ordered by
/// the first member each concerns.
pub fn check_paths(paths: &[RelPath], rules: &TargetRules) -> Vec<NameProblem> {
    let keys: Vec<Vec<u8>> = paths.iter().map(|path| fold_path(path, rules)).collect();
    let mut groups: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (index, key) in keys.iter().enumerate() {
        groups.entry(key).or_default().push(index);
    }

    let mut problems = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        name_problems(path, rules, &mut problems);
        let group = &groups[keys[index].as_slice()];
        if group[0] != index {
            continue;
        }
        // The same path twice is a duplicate member, which extraction
        // refuses on its own; only different spellings collide.
        let mut spellings: Vec<&RelPath> = Vec::new();
        for member in group {
            if !spellings.contains(&&paths[*member]) {
                spellings.push(&paths[*member]);
            }
        }
        if spellings.len() > 1 {
            problems.push(NameProblem::Collision {
                paths: spellings.into_iter().cloned().collect(),
            });
        }
    }
    problems
}

/// The problems `paths` would have on macOS and on Linux.
pub fn portability_warnings(paths: &[RelPath]) -> Vec<PortabilityWarning> {
    let mut warnings = Vec::new();
    for (platform, rules) in [
        (Platform::MacOs, TargetRules::macos()),
        (Platform::Linux, TargetRules::linux()),
    ] {
        for problem in check_paths(paths, &rules) {
            warnings.push(PortabilityWarning { platform, problem });
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<RelPath> {
        names.iter().map(|name| rel(name)).collect()
    }

    fn rel(name: &str) -> RelPath {
        RelPath::parse(name).expect("a valid path")
    }

    fn collision(names: &[&str]) -> NameProblem {
        NameProblem::Collision {
            paths: paths(names),
        }
    }

    fn case_insensitive() -> TargetRules {
        TargetRules {
            case_insensitive: true,
            ..TargetRules::linux()
        }
    }

    fn normalization_insensitive() -> TargetRules {
        TargetRules {
            normalization_insensitive: true,
            ..TargetRules::linux()
        }
    }

    fn repeated(count: usize, text: &str) -> String {
        text.repeat(count)
    }

    #[test]
    fn names_differing_in_case_collide_on_a_case_insensitive_volume() {
        let members = paths(&["README", "Readme", "other"]);
        assert_eq!(
            check_paths(&members, &case_insensitive()),
            [collision(&["README", "Readme"])]
        );
    }

    #[test]
    fn names_differing_in_case_do_not_collide_on_a_case_sensitive_volume() {
        let members = paths(&["README", "Readme"]);
        assert_eq!(check_paths(&members, &TargetRules::linux()), []);
    }

    #[test]
    fn nfc_and_nfd_spellings_collide_on_a_normalization_insensitive_volume() {
        let nfc = "caf\u{e9}";
        let nfd = "cafe\u{301}";
        let members = paths(&[nfc, nfd]);
        assert_eq!(
            check_paths(&members, &normalization_insensitive()),
            [collision(&[nfc, nfd])]
        );
    }

    #[test]
    fn nfc_and_nfd_spellings_do_not_collide_on_a_normalization_sensitive_volume() {
        let members = paths(&["caf\u{e9}", "cafe\u{301}"]);
        assert_eq!(check_paths(&members, &TargetRules::linux()), []);
    }

    #[test]
    fn case_and_normalization_together_collide_under_the_macos_rules() {
        let members = paths(&["CAF\u{c9}", "cafe\u{301}"]);
        assert_eq!(
            check_paths(&members, &TargetRules::macos()),
            [collision(&["CAF\u{c9}", "cafe\u{301}"])]
        );
    }

    /// APFS treats each pair below as one name, which lower-casing alone
    /// would keep apart.
    fn assert_one_name_under_macos(first: &str, second: &str) {
        let members = paths(&[first, second]);
        assert_eq!(
            check_paths(&members, &TargetRules::macos()),
            [collision(&[first, second])],
            "{first} against {second}"
        );
    }

    #[test]
    fn final_and_medial_sigma_are_one_name() {
        assert_one_name_under_macos("x\u{3c2}", "x\u{3c3}");
    }

    #[test]
    fn sharp_s_and_double_s_are_one_name() {
        assert_one_name_under_macos("stra\u{df}e", "strasse");
    }

    #[test]
    fn long_s_and_s_are_one_name() {
        assert_one_name_under_macos("\u{17f}", "s");
    }

    #[test]
    fn the_ff_ligature_and_ff_are_one_name() {
        assert_one_name_under_macos("\u{fb00}", "ff");
    }

    #[test]
    fn dotless_i_against_i_is_a_collision_although_the_volume_keeps_them_apart() {
        let members = paths(&["\u{131}", "i"]);
        assert_eq!(
            check_paths(&members, &TargetRules::macos()),
            [collision(&["\u{131}", "i"])]
        );
    }

    #[test]
    fn a_colliding_directory_collides_below_it_and_both_are_reported() {
        let members = paths(&["a", "a/X", "A", "A/x"]);
        assert_eq!(
            check_paths(&members, &case_insensitive()),
            [collision(&["a", "A"]), collision(&["a/X", "A/x"])]
        );
    }

    #[test]
    fn three_names_that_fold_together_form_one_problem() {
        let members = paths(&["abc", "ABC", "Abc"]);
        assert_eq!(
            check_paths(&members, &case_insensitive()),
            [collision(&["abc", "ABC", "Abc"])]
        );
    }

    #[test]
    fn identical_paths_are_not_a_name_collision() {
        let members = paths(&["same", "same"]);
        assert_eq!(check_paths(&members, &case_insensitive()), []);
    }

    #[test]
    fn a_name_that_is_not_utf8_is_compared_by_its_bytes() {
        let odd = RelPath::from_bytes(vec![b'a', 0xff]).expect("a valid path");
        let lookalike = rel("A\u{fffd}");
        assert_eq!(
            check_paths(&[odd.clone(), lookalike], &case_insensitive()),
            []
        );
        assert_eq!(check_paths(&[odd.clone(), odd], &case_insensitive()), []);
    }

    #[test]
    fn a_name_of_256_bytes_is_too_long_on_linux_but_not_on_macos() {
        // 128 two-byte characters are 256 bytes and 128 UTF-16 units.
        let name = repeated(128, "\u{e9}");
        let members = paths(&[&name]);
        assert_eq!(
            check_paths(&members, &TargetRules::linux()),
            [NameProblem::TooLong {
                path: rel(&name),
                length: 256,
                limit: TargetRules::linux().name_limit,
            }]
        );
        assert_eq!(check_paths(&members, &TargetRules::macos()), []);
    }

    #[test]
    fn macos_counts_utf16_units_of_the_name_as_stored() {
        let two_byte = repeated(255, "\u{e9}");
        let two_byte_over = repeated(256, "\u{e9}");
        let emoji_fit = repeated(127, "\u{1f600}");
        let emoji_over = repeated(128, "\u{1f600}");
        let decomposed_fit = repeated(127, "e\u{301}");
        let decomposed_over = repeated(128, "e\u{301}");
        let macos = TargetRules::macos();
        for fitting in [&two_byte, &emoji_fit, &decomposed_fit] {
            assert_eq!(check_paths(&paths(&[fitting]), &macos), [], "{fitting}");
        }
        for over in [&two_byte_over, &emoji_over, &decomposed_over] {
            assert_eq!(
                check_paths(&paths(&[over]), &macos),
                [NameProblem::TooLong {
                    path: rel(over),
                    length: 256,
                    limit: macos.name_limit,
                }]
            );
        }
        // 255 two-byte characters are 510 bytes: fine on macOS, too long on Linux.
        assert_eq!(
            check_paths(&paths(&[&two_byte]), &TargetRules::linux()).len(),
            1
        );
    }

    #[test]
    fn a_name_that_is_not_utf8_is_refused_only_where_the_volume_requires_utf8() {
        let odd = RelPath::from_bytes(vec![b'a', 0xff]).expect("a valid path");
        let members = vec![odd.clone()];
        assert_eq!(
            check_paths(&members, &TargetRules::macos()),
            [NameProblem::NotUtf8 { path: odd }]
        );
        assert_eq!(check_paths(&members, &TargetRules::linux()), []);
    }

    #[test]
    fn only_the_last_component_is_checked_for_length() {
        let long = repeated(256, "x");
        let members = paths(&[&long, &format!("{long}/child")]);
        let problems = check_paths(&members, &TargetRules::linux());
        assert_eq!(problems.len(), 1);
        assert!(matches!(&problems[0], NameProblem::TooLong { path, .. } if *path == rel(&long)));
    }

    #[test]
    fn problems_are_ordered_by_the_first_member_they_concern() {
        let long = repeated(256, "x");
        let members = paths(&["B", &long, "b"]);
        let problems = check_paths(&members, &case_insensitive());
        assert_eq!(problems.len(), 2);
        assert!(matches!(&problems[0], NameProblem::Collision { .. }));
        assert!(matches!(&problems[1], NameProblem::TooLong { .. }));
    }

    #[test]
    fn archive_time_warnings_name_the_system_that_would_refuse() {
        let long = repeated(128, "\u{e9}");
        let members = paths(&["README", "Readme", &long, "plain"]);
        assert_eq!(
            portability_warnings(&members),
            [
                PortabilityWarning {
                    platform: Platform::MacOs,
                    problem: collision(&["README", "Readme"]),
                },
                PortabilityWarning {
                    platform: Platform::Linux,
                    problem: NameProblem::TooLong {
                        path: rel(&long),
                        length: 256,
                        limit: TargetRules::linux().name_limit,
                    },
                },
            ]
        );
        assert_eq!(portability_warnings(&paths(&["plain", "names"])), []);
    }

    #[test]
    fn problems_print_as_one_line_each() {
        let long = repeated(256, "x");
        let odd = RelPath::from_bytes(vec![b'a', 0xff]).expect("a valid path");
        let lines = [
            NameProblem::NotUtf8 { path: odd }.to_string(),
            NameProblem::TooLong {
                path: rel(&long),
                length: 256,
                limit: TargetRules::linux().name_limit,
            }
            .to_string(),
            NameProblem::TooLong {
                path: rel("n"),
                length: 300,
                limit: TargetRules::macos().name_limit,
            }
            .to_string(),
            collision(&["README", "Readme"]).to_string(),
            collision(&["a", "b", "c"]).to_string(),
            PortabilityWarning {
                platform: Platform::MacOs,
                problem: collision(&["a", "A"]),
            }
            .to_string(),
            PortabilityWarning {
                platform: Platform::Linux,
                problem: collision(&["a", "A"]),
            }
            .to_string(),
        ];
        assert_eq!(
            lines,
            [
                "`a%FF` is not valid UTF-8, which this volume cannot hold".to_string(),
                format!("`{long}` has a name of 256 bytes, the volume allows 255"),
                "`n` has a name of 300 UTF-16 units, the volume allows 255".to_string(),
                "`README` and `Readme` become one name on this volume".to_string(),
                "`a`, `b` and `c` become one name on this volume".to_string(),
                "on macOS: `a` and `A` become one name on this volume".to_string(),
                "on Linux: `a` and `A` become one name on this volume".to_string(),
            ]
        );
    }

    #[test]
    fn the_root_path_has_no_name_to_check() {
        assert_eq!(check_paths(&[RelPath::root()], &TargetRules::macos()), []);
    }
}
