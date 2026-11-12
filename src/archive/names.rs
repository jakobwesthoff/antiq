// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Name checks for extraction.

use super::TargetRules;
use crate::filesystem::tree::TreeRoot;
use crate::relpath::{Name, RelPath};
use crate::shown::Shown;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// A hard link, on a volume that has none.
    HardLinkUnsupported {
        path: RelPath,
    },
    /// A symlink, on a volume that has none.
    SymlinkUnsupported {
        path: RelPath,
    },
    /// A symlink whose target is longer than the volume stores.
    TargetTooLong {
        path: RelPath,
        length: usize,
        limit: usize,
    },
}

/// What a member is, as far as the volume's rules care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Plain,
    /// A symlink; the length of its target in bytes, as stored.
    Symlink {
        target_len: usize,
    },
    HardLink,
}

/// A member of an archive: its path below the top-level directory and
/// its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberName {
    pub path: RelPath,
    pub kind: MemberKind,
}

impl MemberName {
    /// A file or directory.
    pub fn plain(path: RelPath) -> MemberName {
        MemberName {
            path,
            kind: MemberKind::Plain,
        }
    }
}

impl fmt::Display for NameProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameProblem::NotUtf8 { path } => {
                write!(
                    f,
                    "`{}` is not valid UTF-8, which this volume cannot hold",
                    Shown(path)
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
                    "`{}` has a name of {length} {unit}, the volume allows {}",
                    Shown(path),
                    limit.max
                )
            }
            NameProblem::Collision { paths } => {
                let (last, earlier) = paths
                    .split_last()
                    .expect("a collision names at least two paths");
                let earlier: Vec<String> = earlier
                    .iter()
                    .map(|path| format!("`{}`", Shown(path)))
                    .collect();
                let joined = earlier.join(", ");
                write!(
                    f,
                    "{joined} and `{}` become one name on this volume",
                    Shown(last)
                )
            }
            NameProblem::HardLinkUnsupported { path } => {
                write!(
                    f,
                    "`{}` is a hard link, which this volume cannot hold",
                    Shown(path)
                )
            }
            NameProblem::SymlinkUnsupported { path } => {
                write!(
                    f,
                    "`{}` is a symlink, which this volume cannot hold",
                    Shown(path)
                )
            }
            NameProblem::TargetTooLong {
                path,
                length,
                limit,
            } => write!(
                f,
                "`{}` points to a target of {length} bytes, the volume allows {limit}",
                Shown(path)
            ),
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
            decomposes: false,
            requires_utf8: true,
            hard_links: true,
            symlinks: true,
            symlink_target_limit: 1023,
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
            decomposes: false,
            requires_utf8: false,
            hard_links: true,
            symlinks: true,
            symlink_target_limit: 4095,
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
pub(crate) fn fold_path(path: &RelPath, rules: &TargetRules) -> Vec<u8> {
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
    // The limit applies to the name as the volume holds it.
    let stored: Cow<[u8]> = if rules.decomposes {
        let decomposed: String = String::from_utf8_lossy(name).nfd().collect();
        Cow::Owned(decomposed.into_bytes())
    } else {
        Cow::Borrowed(name)
    };
    let length = match rules.name_limit.unit {
        LengthUnit::Bytes => stored.len(),
        LengthUnit::Utf16Units => String::from_utf8_lossy(&stored).encode_utf16().count(),
    };
    if length > rules.name_limit.max {
        problems.push(NameProblem::TooLong {
            path: path.clone(),
            length,
            limit: rules.name_limit,
        });
    }
}

/// The problems `members` would have on a volume with `rules`, ordered by
/// the first member each concerns.
pub fn check_members(members: &[MemberName], rules: &TargetRules) -> Vec<NameProblem> {
    let paths: Vec<&RelPath> = members.iter().map(|member| &member.path).collect();
    let keys: Vec<Vec<u8>> = paths.iter().map(|path| fold_path(path, rules)).collect();
    let mut groups: HashMap<&[u8], Vec<usize>> = HashMap::new();
    for (index, key) in keys.iter().enumerate() {
        groups.entry(key).or_default().push(index);
    }

    let mut problems = Vec::new();
    for (index, path) in paths.iter().copied().enumerate() {
        name_problems(path, rules, &mut problems);
        match members[index].kind {
            MemberKind::HardLink if !rules.hard_links => {
                problems.push(NameProblem::HardLinkUnsupported { path: path.clone() });
            }
            MemberKind::Symlink { .. } if !rules.symlinks => {
                problems.push(NameProblem::SymlinkUnsupported { path: path.clone() });
            }
            MemberKind::Symlink { target_len } if target_len > rules.symlink_target_limit => {
                problems.push(NameProblem::TargetTooLong {
                    path: path.clone(),
                    length: target_len,
                    limit: rules.symlink_target_limit,
                });
            }
            _ => {}
        }
        let group = &groups[keys[index].as_slice()];
        if group[0] != index {
            continue;
        }
        // The same path twice is a duplicate member, which extraction
        // refuses on its own; only different spellings collide.
        let mut spellings: Vec<&RelPath> = Vec::new();
        for member in group {
            if !spellings.contains(&paths[*member]) {
                spellings.push(paths[*member]);
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

/// The problems `members` would have on macOS and on Linux.
pub fn portability_warnings(members: &[MemberName]) -> Vec<PortabilityWarning> {
    let mut warnings = Vec::new();
    for (platform, rules) in [
        (Platform::MacOs, TargetRules::macos()),
        (Platform::Linux, TargetRules::linux()),
    ] {
        for problem in check_members(members, &rules) {
            warnings.push(PortabilityWarning { platform, problem });
        }
    }
    warnings
}

// =========================================================
// Probing a volume
// =========================================================

/// The directory a probe works in, below the root it probes. Dropping it
/// removes what the probe created, on every way out of the probe.
struct ProbeDirectory<'a> {
    root: &'a TreeRoot,
    path: RelPath,
    created: Vec<RelPath>,
}

/// Distinguishes the probe directories of one process.
static PROBES: AtomicU64 = AtomicU64::new(0);

impl<'a> ProbeDirectory<'a> {
    fn create(root: &'a TreeRoot) -> io::Result<ProbeDirectory<'a>> {
        let number = PROBES.fetch_add(1, Ordering::Relaxed);
        let name = format!(".antiq-probe-{}-{number}", std::process::id());
        let path =
            RelPath::root().join(&Name::from_bytes(name.into_bytes()).expect("a valid name"));
        root.create_dir(&path, 0o700)?;
        Ok(ProbeDirectory {
            root,
            path,
            created: Vec::new(),
        })
    }

    fn entry(&self, name: &[u8]) -> RelPath {
        let name = Name::from_bytes(name.to_vec()).expect("the probe only uses valid names");
        self.path.join(&name)
    }

    /// Creates a file in the probe directory and remembers it for removal.
    fn create_file(&mut self, name: &[u8]) -> io::Result<File> {
        let path = self.entry(name);
        let file = self.root.create_file(&path, 0o600)?;
        self.created.push(path);
        Ok(file)
    }

    /// Whether the probe directory holds an entry that the volume finds
    /// under `name`.
    fn finds(&self, name: &[u8]) -> io::Result<bool> {
        match self.root.lstat(&self.entry(name)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether listing the probe directory gives `name`, byte for byte.
    fn lists(&self, name: &[u8]) -> io::Result<bool> {
        let entries = self.root.read_dir(&self.path)?;
        Ok(entries.iter().any(|entry| entry.name.as_bytes() == name))
    }

    /// Gives the probe file `existing` a second name, `new`.
    fn hard_link(&mut self, existing: &[u8], new: &[u8]) -> io::Result<()> {
        let path = self.entry(new);
        self.root.hard_link(&self.entry(existing), &path)?;
        self.created.push(path);
        Ok(())
    }

    /// Creates a symlink to `target`.
    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        let path = self.entry(name);
        self.root.create_symlink(&path, target)?;
        self.created.push(path);
        Ok(())
    }
}

/// Whether the one call that tried to create a link shows that the volume
/// has none: it was refused as not permitted or not supported. Any other
/// failure says nothing about links and is the probe's error.
fn link_supported(attempt: io::Result<()>) -> io::Result<bool> {
    match attempt {
        Ok(()) => Ok(true),
        Err(error) => match error.raw_os_error() {
            Some(errno) if [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP].contains(&errno) => {
                Ok(false)
            }
            _ => Err(error),
        },
    }
}

impl Drop for ProbeDirectory<'_> {
    fn drop(&mut self) {
        // Best effort: a probe that failed halfway is the reason to be
        // here, and its error is the one worth reporting.
        for path in self.created.iter().rev() {
            let _ = self.root.remove_file(path);
        }
        let _ = self.root.remove_dir(&self.path);
    }
}

/// Whether a failed attempt to create a file with a name that is not
/// UTF-8 shows that the volume refuses such names. APFS answers with
/// `EILSEQ` and exFAT with `EINVAL`; the probe has created two plain
/// names before it asks, so the name is what `EINVAL` objects to. Any
/// other failure says nothing about names.
fn refuses_non_utf8(attempt: io::Result<File>) -> io::Result<bool> {
    match attempt {
        Ok(_) => Ok(false),
        Err(error) if matches!(error.raw_os_error(), Some(libc::EILSEQ | libc::EINVAL)) => Ok(true),
        Err(error) => Err(error),
    }
}

/// APFS counts a name's length in UTF-16 code units, other filesystems in
/// bytes.
const NAME_LIMIT_UNIT: LengthUnit = if cfg!(target_os = "macos") {
    LengthUnit::Utf16Units
} else {
    LengthUnit::Bytes
};

/// The longest symlink target in bytes: one below the platform's path
/// limit of 1024 on macOS and 4096 on Linux.
const SYMLINK_TARGET_LIMIT: usize = if cfg!(target_os = "macos") {
    1023
} else {
    4095
};

impl TargetRules {
    /// The rules of the volume that `root` is on, found by trying them in
    /// a directory of their own below `root`, which is empty again when
    /// the probe returns.
    pub fn probe(root: &TreeRoot) -> io::Result<TargetRules> {
        let mut directory = ProbeDirectory::create(root)?;

        // A name that exists in lower case is found under its upper-case
        // spelling only on a volume that ignores case.
        let lower = directory.create_file(b"antiq-probe-case")?;
        let case_insensitive = directory.finds(b"ANTIQ-PROBE-CASE")?;

        let hard_links =
            link_supported(directory.hard_link(b"antiq-probe-case", b"antiq-probe-hard"))?;
        let symlinks =
            link_supported(directory.symlink(b"antiq-probe-symlink", b"antiq-probe-case"))?;

        // A volume that ignores normalisation finds the decomposed
        // spelling of the precomposed name just written. One that stores
        // names decomposed lists it under another spelling than the one
        // it was created with.
        let precomposed = "antiq-probe-caf\u{e9}".as_bytes();
        directory.create_file(precomposed)?;
        let normalization_insensitive = directory.finds("antiq-probe-cafe\u{301}".as_bytes())?;
        let decomposes = !directory.lists(precomposed)?;

        let requires_utf8 = refuses_non_utf8(directory.create_file(b"antiq-probe-\xff"))?;

        // The open file stands for the volume: the root has no public
        // descriptor of its own to ask.
        let name_max = rustix::fs::fstatvfs(&lower)?.f_namemax;
        let max = usize::try_from(name_max).map_err(io::Error::other)?;

        Ok(TargetRules {
            case_insensitive,
            normalization_insensitive,
            decomposes,
            requires_utf8,
            hard_links,
            symlinks,
            symlink_target_limit: SYMLINK_TARGET_LIMIT,
            name_limit: NameLimit {
                max,
                unit: NAME_LIMIT_UNIT,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<RelPath> {
        names.iter().map(|name| rel(name)).collect()
    }

    fn plain(paths: &[RelPath]) -> Vec<MemberName> {
        paths.iter().cloned().map(MemberName::plain).collect()
    }

    fn check_paths(paths: &[RelPath], rules: &TargetRules) -> Vec<NameProblem> {
        check_members(&plain(paths), rules)
    }

    fn member(path: &str, kind: MemberKind) -> MemberName {
        MemberName {
            path: rel(path),
            kind,
        }
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

    fn without_links() -> TargetRules {
        TargetRules {
            hard_links: false,
            symlinks: false,
            ..TargetRules::linux()
        }
    }

    #[test]
    fn a_hard_link_is_a_problem_on_a_volume_without_hard_links() {
        let members = [
            member("file", MemberKind::Plain),
            member("again", MemberKind::HardLink),
        ];
        assert_eq!(
            check_members(&members, &without_links()),
            [NameProblem::HardLinkUnsupported { path: rel("again") }]
        );
        assert_eq!(check_members(&members, &TargetRules::linux()), []);
    }

    #[test]
    fn a_symlink_is_a_problem_on_a_volume_without_symlinks() {
        let members = [member("link", MemberKind::Symlink { target_len: 4 })];
        assert_eq!(
            check_members(&members, &without_links()),
            [NameProblem::SymlinkUnsupported { path: rel("link") }]
        );
        assert_eq!(check_members(&members, &TargetRules::linux()), []);
    }

    fn symlink_with_target(target_len: usize) -> [MemberName; 1] {
        [member("link", MemberKind::Symlink { target_len })]
    }

    #[test]
    fn a_symlink_target_of_1023_bytes_fits_on_macos() {
        assert_eq!(
            check_members(&symlink_with_target(1023), &TargetRules::macos()),
            []
        );
    }

    #[test]
    fn a_symlink_target_of_1024_bytes_is_too_long_on_macos() {
        assert_eq!(
            check_members(&symlink_with_target(1024), &TargetRules::macos()),
            [NameProblem::TargetTooLong {
                path: rel("link"),
                length: 1024,
                limit: 1023,
            }]
        );
    }

    #[test]
    fn a_symlink_target_of_4095_bytes_fits_on_linux_and_4096_does_not() {
        assert_eq!(
            check_members(&symlink_with_target(4095), &TargetRules::linux()),
            []
        );
        assert_eq!(
            check_members(&symlink_with_target(4096), &TargetRules::linux()),
            [NameProblem::TargetTooLong {
                path: rel("link"),
                length: 4096,
                limit: 4095,
            }]
        );
    }

    #[test]
    fn a_symlink_on_a_volume_without_symlinks_is_not_also_measured() {
        let rules = TargetRules {
            symlinks: false,
            ..TargetRules::macos()
        };
        assert_eq!(
            check_members(&symlink_with_target(5000), &rules),
            [NameProblem::SymlinkUnsupported { path: rel("link") }]
        );
    }

    #[test]
    fn archive_time_warnings_include_a_target_macos_cannot_hold() {
        assert_eq!(
            portability_warnings(&symlink_with_target(1024)),
            [PortabilityWarning {
                platform: Platform::MacOs,
                problem: NameProblem::TargetTooLong {
                    path: rel("link"),
                    length: 1024,
                    limit: 1023,
                },
            }]
        );
    }

    #[test]
    fn the_macos_and_linux_rules_have_hard_links_and_symlinks() {
        for rules in [TargetRules::macos(), TargetRules::linux()] {
            assert!(rules.hard_links && rules.symlinks);
        }
        let members = [
            member("link", MemberKind::Symlink { target_len: 4 }),
            member("again", MemberKind::HardLink),
        ];
        assert_eq!(portability_warnings(&members), []);
    }

    fn hfs_plus() -> TargetRules {
        TargetRules {
            decomposes: true,
            ..TargetRules::macos()
        }
    }

    #[test]
    fn a_volume_that_decomposes_counts_the_name_in_its_decomposed_form() {
        // 200 precomposed characters are 200 UTF-16 units and, stored
        // decomposed, 400.
        let name = repeated(200, "\u{e9}");
        let members = paths(&[&name]);
        assert_eq!(
            check_paths(&members, &hfs_plus()),
            [NameProblem::TooLong {
                path: rel(&name),
                length: 400,
                limit: hfs_plus().name_limit,
            }]
        );
    }

    #[test]
    fn a_volume_that_keeps_the_name_as_written_counts_it_as_written() {
        let name = repeated(200, "\u{e9}");
        assert_eq!(check_paths(&paths(&[&name]), &TargetRules::macos()), []);
    }

    #[test]
    fn a_decomposing_volume_counts_bytes_of_the_decomposed_form_too() {
        // 100 precomposed characters are 200 bytes and, decomposed, 300.
        let name = repeated(100, "\u{e9}");
        let rules = TargetRules {
            decomposes: true,
            ..TargetRules::linux()
        };
        assert_eq!(
            check_paths(&paths(&[&name]), &rules),
            [NameProblem::TooLong {
                path: rel(&name),
                length: 300,
                limit: rules.name_limit,
            }]
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
            portability_warnings(&plain(&members)),
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
        assert_eq!(
            portability_warnings(&plain(&paths(&["plain", "names"]))),
            []
        );
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
            NameProblem::HardLinkUnsupported { path: rel("h") }.to_string(),
            NameProblem::SymlinkUnsupported { path: rel("s") }.to_string(),
            NameProblem::TargetTooLong {
                path: rel("t"),
                length: 1024,
                limit: 1023,
            }
            .to_string(),
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
                "`h` is a hard link, which this volume cannot hold".to_string(),
                "`s` is a symlink, which this volume cannot hold".to_string(),
                "`t` points to a target of 1024 bytes, the volume allows 1023".to_string(),
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

/// Probing real directories.
#[cfg(test)]
mod probe_tests {
    use super::*;
    // Only the macOS volume test builds a name that is not UTF-8.
    #[cfg(target_os = "macos")]
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root() -> (tempfile::TempDir, TreeRoot) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = TreeRoot::open(dir.path()).expect("the dir opens");
        (dir, root)
    }

    fn is_empty(root: &TreeRoot) -> bool {
        root.read_dir(&RelPath::root())
            .expect("the root lists")
            .is_empty()
    }

    #[test]
    fn a_probe_leaves_the_root_empty() {
        let (_dir, root) = temp_root();
        TargetRules::probe(&root).expect("the probe runs");
        assert!(is_empty(&root));
    }

    #[test]
    fn the_probe_directory_removes_what_it_created_when_it_is_dropped() {
        let (_dir, root) = temp_root();
        {
            let mut probe = ProbeDirectory::create(&root).expect("the directory is created");
            probe.create_file(b"one").expect("a file is created");
            probe.create_file(b"two").expect("a file is created");
            assert!(!is_empty(&root));
        }
        assert!(is_empty(&root));
    }

    #[test]
    fn two_probe_directories_get_different_names() {
        let (_dir, root) = temp_root();
        let first = ProbeDirectory::create(&root).expect("the first is created");
        let second = ProbeDirectory::create(&root).expect("the second is created");
        assert_ne!(first.path, second.path);
    }

    #[test]
    fn a_probe_directory_finds_an_entry_under_the_name_it_was_created_with() {
        let (_dir, root) = temp_root();
        let mut probe = ProbeDirectory::create(&root).expect("the directory is created");
        probe.create_file(b"here").expect("a file is created");
        assert!(probe.finds(b"here").expect("the lookup works"));
        assert!(!probe.finds(b"missing").expect("the lookup works"));
    }

    #[test]
    fn a_probe_directory_lists_a_name_byte_for_byte() {
        let (_dir, root) = temp_root();
        let mut probe = ProbeDirectory::create(&root).expect("the directory is created");
        probe.create_file(b"here").expect("a file is created");
        assert!(probe.lists(b"here").expect("the listing works"));
        assert!(!probe.lists(b"HERE").expect("the listing works"));
        assert!(!probe.lists(b"missing").expect("the listing works"));
    }

    #[test]
    fn the_probe_directory_removes_its_links_too() {
        let (_dir, root) = temp_root();
        {
            let mut probe = ProbeDirectory::create(&root).expect("the directory is created");
            probe.create_file(b"file").expect("a file is created");
            probe.hard_link(b"file", b"again").expect("a hard link");
            probe.symlink(b"link", b"file").expect("a symlink");
            assert!(probe.lists(b"again").expect("lists"));
            assert!(probe.lists(b"link").expect("lists"));
        }
        assert!(is_empty(&root));
    }

    #[test]
    fn a_link_that_was_created_shows_that_the_volume_has_links() {
        assert!(link_supported(Ok(())).expect("a known answer"));
    }

    #[test]
    fn a_link_that_the_volume_refuses_shows_that_it_has_none() {
        for errno in [libc::EPERM, libc::ENOTSUP, libc::EOPNOTSUPP] {
            let refused = io::Error::from_raw_os_error(errno);
            assert!(
                !link_supported(Err(refused)).expect("a known answer"),
                "{errno}"
            );
        }
    }

    #[test]
    fn any_other_failure_of_a_link_is_a_real_error() {
        for errno in [libc::ENOSPC, libc::EACCES, libc::EINVAL, libc::EEXIST] {
            let failed = io::Error::from_raw_os_error(errno);
            assert!(link_supported(Err(failed)).is_err(), "{errno}");
        }
    }

    #[test]
    fn the_probe_finds_hard_links_and_symlinks_on_the_temp_volume() {
        let (_dir, root) = temp_root();
        let rules = TargetRules::probe(&root).expect("the probe runs");
        assert!(rules.hard_links);
        assert!(rules.symlinks);
        assert_eq!(rules.symlink_target_limit, SYMLINK_TARGET_LIMIT);
        assert!(is_empty(&root));
    }

    #[test]
    fn a_lookup_that_fails_for_another_reason_is_an_error() {
        let (_dir, root) = temp_root();
        let mut probe = ProbeDirectory::create(&root).expect("the directory is created");
        probe.create_file(b"plain").expect("a file is created");
        // A name below a file cannot be looked up: the error is not "not
        // found".
        let directory = probe.path.clone();
        probe.path = directory.join(&Name::from_bytes(b"plain".to_vec()).expect("a valid name"));
        assert!(probe.finds(b"inside").is_err());
        probe.path = directory;
    }

    #[test]
    fn an_invalid_argument_shows_that_the_volume_refuses_non_utf8_as_exfat_answers_it() {
        let invalid = io::Error::from_raw_os_error(libc::EINVAL);
        assert!(refuses_non_utf8(Err(invalid)).expect("a known answer"));
    }

    #[test]
    fn only_an_illegal_byte_sequence_or_an_invalid_argument_shows_that_the_volume_refuses_non_utf8()
    {
        let illegal = io::Error::from_raw_os_error(libc::EILSEQ);
        assert!(refuses_non_utf8(Err(illegal)).expect("a known answer"));
        let created = tempfile::tempfile().expect("a file");
        assert!(!refuses_non_utf8(Ok(created)).expect("a known answer"));
        let other = io::Error::from_raw_os_error(libc::ENOSPC);
        assert!(refuses_non_utf8(Err(other)).is_err());
    }

    #[test]
    fn a_root_that_cannot_be_written_gives_an_error_and_stays_empty() {
        let (dir, root) = temp_root();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555))
            .expect("the mode can be set");
        assert!(
            !rustix::process::geteuid().is_root(),
            "this test needs a user the permissions bind"
        );
        let result = TargetRules::probe(&root);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("the mode can be restored");
        assert!(result.is_err());
        assert!(is_empty(&root));
    }

    /// What the volume does, observed with plain paths.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_probe_reports_what_the_temp_volume_does() {
        let (dir, root) = temp_root();
        let rules = TargetRules::probe(&root).expect("the probe runs");

        std::fs::write(dir.path().join("lower"), b"").expect("a file");
        let case_insensitive = dir.path().join("LOWER").exists();
        std::fs::write(dir.path().join("caf\u{e9}"), b"").expect("a file");
        let normalization_insensitive = dir.path().join("cafe\u{301}").exists();
        let non_utf8 = dir.path().join(std::ffi::OsStr::from_bytes(b"a\xff"));
        let requires_utf8 = std::fs::write(non_utf8, b"").is_err();
        let name_max = rustix::fs::statvfs(dir.path())
            .expect("the volume answers")
            .f_namemax as usize;

        assert_eq!(rules.case_insensitive, case_insensitive);
        assert_eq!(rules.normalization_insensitive, normalization_insensitive);
        assert_eq!(rules.requires_utf8, requires_utf8);
        assert_eq!(rules.name_limit.max, name_max);
        assert_eq!(rules.name_limit.unit, LengthUnit::Utf16Units);
        assert!(!rules.decomposes, "APFS keeps names as written");
        assert_eq!(rules.symlink_target_limit, 1023);
    }

    /// A disk image with a file system of its own, attached below a temp
    /// directory and detached when dropped. Needs `hdiutil`.
    #[cfg(target_os = "macos")]
    struct DiskImage {
        _dir: tempfile::TempDir,
        mount: std::path::PathBuf,
    }

    #[cfg(target_os = "macos")]
    impl DiskImage {
        fn attach(filesystem: &str) -> DiskImage {
            use std::process::Command;
            let dir = tempfile::tempdir().expect("a temp dir");
            let image = dir.path().join("volume.dmg");
            let mount = dir.path().join("mount");
            std::fs::create_dir(&mount).expect("a mount point");
            let created = Command::new("hdiutil")
                .args(["create", "-size", "10m", "-fs", filesystem])
                .args(["-volname", "antiqprobe"])
                .arg(&image)
                .output()
                .expect("hdiutil runs");
            assert!(created.status.success(), "{created:?}");
            let attached = Command::new("hdiutil")
                .args(["attach", "-nobrowse", "-mountpoint"])
                .arg(&mount)
                .arg(&image)
                .output()
                .expect("hdiutil runs");
            assert!(attached.status.success(), "{attached:?}");
            DiskImage { _dir: dir, mount }
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for DiskImage {
        fn drop(&mut self) {
            let _ = std::process::Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(&self.mount)
                .output();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "attaches a disk image with hdiutil"]
    fn the_probe_finds_that_names_are_stored_decomposed_on_a_real_hfs_plus_volume() {
        let volume = DiskImage::attach("HFS+");
        let root = TreeRoot::open(&volume.mount).expect("the volume opens");
        let rules = TargetRules::probe(&root).expect("the probe runs");
        assert!(rules.decomposes);
        assert!(is_empty(&root));
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "attaches a disk image with hdiutil"]
    fn the_probe_answers_and_reports_non_utf8_names_as_refused_on_a_real_exfat_volume() {
        let volume = DiskImage::attach("ExFAT");
        let root = TreeRoot::open(&volume.mount).expect("the volume opens");
        let rules = TargetRules::probe(&root).expect("the probe runs");
        assert!(rules.requires_utf8);
        assert!(!rules.hard_links, "exFAT answers ENOTSUP to a hard link");
        assert!(rules.symlinks);
        assert!(is_empty(&root));
    }
}
