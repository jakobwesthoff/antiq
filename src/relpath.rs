// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Relative paths and names as bytes, and their escaped text form.
//!
//! Paths and names are the bytes the filesystem holds. Text (metadata,
//! listings, JSON) carries them in the escaped form: valid UTF-8 stays as
//! it is, `%` becomes `%25`, and every byte that is not part of valid UTF-8
//! becomes `%XX` with upper-case hex digits. Every byte sequence has
//! exactly one escaped form, and only that form is read back.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// The escaped form of `bytes`.
pub fn escape(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            if character == '%' {
                text.push_str("%25");
            } else {
                text.push(character);
            }
        }
        for byte in chunk.invalid() {
            text.push_str(&format!("%{byte:02X}"));
        }
    }
    text
}

/// The bytes an escaped form stands for. Anything `escape` would not have
/// written, such as `%41` for `A` or a lower-case `%ff`, is refused: two
/// spellings of one name would compare as different strings.
pub fn unescape(text: &str) -> Result<Vec<u8>, RelPathError> {
    let source = text.as_bytes();
    let mut bytes = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if source[index] != b'%' {
            bytes.push(source[index]);
            index += 1;
            continue;
        }
        let digits = source.get(index + 1..index + 3);
        let Some(&[high, low]) = digits else {
            return Err(RelPathError::BadEscape(text.to_string()));
        };
        let (Some(high), Some(low)) = (hex_value(high), hex_value(low)) else {
            return Err(RelPathError::BadEscape(text.to_string()));
        };
        bytes.push(high * 16 + low);
        index += 3;
    }
    if escape(&bytes) != text {
        return Err(RelPathError::NonCanonical(text.to_string()));
    }
    Ok(bytes)
}

fn hex_value(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .map(|value| u8::try_from(value).expect("a hex digit is below 16"))
}

/// What is wrong with `bytes` as a name, if anything.
fn problem(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        b"" => Some("empty"),
        b"." | b".." => Some("`.` or `..`"),
        _ if bytes.contains(&b'/') => Some("`/` inside"),
        _ if bytes.contains(&0) => Some("a NUL byte inside"),
        _ => None,
    }
}

/// One component of a path: not empty, without `/` or NUL, and neither `.`
/// nor `..`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(Vec<u8>);

impl Name {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Name, RelPathError> {
        match problem(&bytes) {
            None => Ok(Name(bytes)),
            Some(problem) => Err(RelPathError::InvalidName(problem)),
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&escape(&self.0))
    }
}

impl Serialize for Name {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Name {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        unescape(&text)
            .and_then(Name::from_bytes)
            .map_err(serde::de::Error::custom)
    }
}

/// A path below some root: names joined by single `/`, without a leading
/// or trailing `/`. The empty path is the root itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(Vec<u8>);

impl RelPath {
    pub fn root() -> RelPath {
        RelPath(Vec::new())
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<RelPath, RelPathError> {
        // Splitting the empty root path gives one empty component, which
        // is not a violation.
        let invalid = bytes
            .split(|byte| *byte == b'/')
            .filter(|_| !bytes.is_empty())
            .find_map(problem);
        match invalid {
            None => Ok(RelPath(bytes)),
            Some(problem) => Err(RelPathError::InvalidPath(problem)),
        }
    }

    /// Reads the escaped form.
    pub fn parse(text: &str) -> Result<RelPath, RelPathError> {
        RelPath::from_bytes(unescape(text)?)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn join(&self, name: &Name) -> RelPath {
        let mut bytes = self.0.clone();
        if !bytes.is_empty() {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(name.as_bytes());
        RelPath(bytes)
    }

    /// `other` below this path.
    pub fn concat(&self, other: &RelPath) -> RelPath {
        match (self.is_root(), other.is_root()) {
            (_, true) => self.clone(),
            (true, false) => other.clone(),
            (false, false) => {
                let mut bytes = self.0.clone();
                bytes.push(b'/');
                bytes.extend_from_slice(&other.0);
                RelPath(bytes)
            }
        }
    }

    /// The names of the path, from the top. The root has none.
    pub fn components(&self) -> impl Iterator<Item = &[u8]> {
        self.0
            .split(|byte| *byte == b'/')
            .filter(|name| !name.is_empty())
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&escape(&self.0))
    }
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        RelPath::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Most bytes the name part of a payload file name takes.
pub const NAME_PART_LIMIT: usize = 64;

/// Most bytes the machine part of an upload temp name takes.
pub const MACHINE_PART_LIMIT: usize = 32;

/// The part of a payload file name that stands for the piece's name. The
/// real name is only in the metadata.
pub fn name_part(name: &Name) -> String {
    escape_part(name.as_bytes(), NAME_PART_LIMIT)
}

/// The part of an upload temp name that stands for the machine.
pub fn machine_part(machine: &str) -> String {
    escape_part(machine.as_bytes(), MACHINE_PART_LIMIT)
}

/// The name an upload is written under until it is complete:
/// `.<final name>.<machine part>.part`.
pub fn temp_name(final_name: &Name, machine: &str) -> Name {
    let mut bytes = vec![b'.'];
    bytes.extend_from_slice(final_name.as_bytes());
    bytes.push(b'.');
    bytes.extend_from_slice(machine_part(machine).as_bytes());
    bytes.extend_from_slice(b".part");
    Name::from_bytes(bytes)
        .expect("a name that starts with `.` and is longer than it is not `.` or `..`")
}

/// Writes every byte outside `[A-Za-z0-9._-]` as `%XX` and stops before the
/// first byte or escape that would take the result past `limit`. A cut can
/// fall between the two escapes of one character but never inside one.
fn escape_part(bytes: &[u8], limit: usize) -> String {
    let mut part = String::new();
    for &byte in bytes {
        let unit = if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            char::from(byte).to_string()
        } else {
            format!("%{byte:02X}")
        };
        if part.len() + unit.len() > limit {
            break;
        }
        part.push_str(&unit);
    }
    part
}

/// The path `given` leads to, relative to `root`. Piece references, `--to`
/// and `--path` take this way into the collection.
///
/// A relative `given` starts at `cwd`. `.` and `..` are resolved from the
/// text of the paths alone, without asking the filesystem. `root` and `cwd`
/// are absolute, and `given` has to spell the root the way `root` does: the
/// root is found by walking up from the current directory, so `root` and
/// `cwd` agree, but an absolute `given` typed through a symbolic link above
/// the root (`/tmp/...` for `/private/tmp/...` on macOS) is not below it.
/// Callers that take paths from the user match the root itself on disk
/// before they call this.
///
/// Names are compared byte for byte, so a differently cased spelling of the
/// root is outside it on a case-insensitive filesystem.
pub fn below_root(root: &Path, given: &Path, cwd: &Path) -> Result<RelPath, RelPathError> {
    for path in [root, cwd] {
        if !path.is_absolute() {
            return Err(RelPathError::NotAbsolute(shown(path)));
        }
    }
    let root = lexical_absolute(cwd, root)?;
    let path = lexical_absolute(cwd, given)?;
    let below = path
        .strip_prefix(&root)
        .map_err(|_| RelPathError::OutsideRoot {
            path: shown(&path),
            root: shown(&root),
        })?;
    RelPath::from_bytes(below.as_os_str().as_bytes().to_vec())
}

/// `given` made absolute against `cwd`, with `.` and `..` resolved and
/// repeated separators collapsed.
fn lexical_absolute(cwd: &Path, given: &Path) -> Result<PathBuf, RelPathError> {
    let mut folded = PathBuf::from("/");
    for component in cwd.join(given).components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if !folded.pop() {
                    return Err(RelPathError::AboveRoot(shown(given)));
                }
            }
            Component::Normal(name) => folded.push(name),
        }
    }
    Ok(folded)
}

fn shown(path: &Path) -> String {
    escape(path.as_os_str().as_bytes())
}

/// Errors of paths and names.
#[derive(Debug, thiserror::Error)]
pub enum RelPathError {
    #[error("`{0}` has an incomplete `%` escape")]
    BadEscape(String),
    #[error("`{0}` is not written the way `escape` writes it")]
    NonCanonical(String),
    #[error("a name cannot be {0}")]
    InvalidName(&'static str),
    #[error("a path cannot have a component that is {0}")]
    InvalidPath(&'static str),
    #[error("`{0}` is not an absolute path")]
    NotAbsolute(String),
    #[error("`{0}` goes above the root of the filesystem")]
    AboveRoot(String),
    #[error("`{path}` is not below `{root}`")]
    OutsideRoot { path: String, root: String },
}

impl crate::error::HasKind for RelPathError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, ErrorKind};
    use serde_json::json;

    #[test]
    fn escape_keeps_valid_utf8_and_slashes_and_escapes_the_rest() {
        assert_eq!(escape(b"a%\xffb"), "a%25%FFb");
        assert_eq!(escape("ä/ö.txt".as_bytes()), "ä/ö.txt");
        assert_eq!(escape(b"\xc3"), "%C3");
        assert_eq!(escape(b""), "");
    }

    #[test]
    fn every_byte_sequence_round_trips_through_the_escaped_form() {
        let mut samples: Vec<Vec<u8>> = (0..=255u8).map(|byte| vec![byte]).collect();
        samples.push((0..=255u8).collect());
        samples.push(b"100%25 \xe4\xf6 caf\xc3\xa9 \xf0\x9f\x92".to_vec());
        for bytes in samples {
            let text = escape(&bytes);
            assert_eq!(unescape(&text).expect("an escaped form unescapes"), bytes);
        }
    }

    #[test]
    fn every_escaped_form_round_trips_through_the_bytes() {
        for text in ["a%25%FFb", "ä/ö", "\0", "%C3", "plain", "%25%25", ""] {
            let bytes = unescape(text).expect("a canonical form unescapes");
            assert_eq!(escape(&bytes), text);
        }
    }

    #[test]
    fn unescape_refuses_what_escape_never_writes() {
        for text in ["%ff", "%41", "%C3%A4", "a%25b%2", "%", "%F", "%G1", "%+f"] {
            assert!(unescape(text).is_err(), "{text:?} was accepted");
        }
    }

    #[test]
    fn unescape_tells_a_broken_escape_from_a_second_spelling() {
        assert!(matches!(unescape("%G1"), Err(RelPathError::BadEscape(_))));
        assert!(matches!(
            unescape("%41"),
            Err(RelPathError::NonCanonical(_))
        ));
    }

    #[test]
    fn a_name_is_one_component_that_is_not_special() {
        assert_eq!(
            Name::from_bytes(b"photos".to_vec())
                .expect("valid")
                .as_bytes(),
            b"photos"
        );
        assert!(Name::from_bytes(b"a\xff".to_vec()).is_ok());
        for bad in [&b""[..], b"a/b", b"a\0b", b".", b".."] {
            let result = Name::from_bytes(bad.to_vec());
            let rejected = matches!(result, Err(RelPathError::InvalidName(_)));
            assert!(rejected, "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_relative_path_is_names_joined_by_single_slashes() {
        for good in ["a", "a/b", "a/%FF", "dir-y/x.txt", "ä/ö"] {
            let path = RelPath::parse(good).expect("valid");
            assert_eq!(path.to_string(), good);
        }
        for bad in ["../x", "/x", "a//b", "a/./b", "a/", "a/..", ".", "a/\0b"] {
            assert!(RelPath::parse(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(RelPath::from_bytes(b"a\0b".to_vec()).is_err());
    }

    #[test]
    fn the_root_is_the_empty_path() {
        let root = RelPath::root();
        assert!(root.is_root());
        assert_eq!(root.as_bytes(), b"");
        assert_eq!(root.to_string(), "");
        assert_eq!(RelPath::parse("").expect("valid"), root);
        assert_eq!(root.components().count(), 0);
    }

    #[test]
    fn paths_join_and_split_into_components() {
        let name = Name::from_bytes(b"c".to_vec()).expect("valid");
        let path = RelPath::parse("a/b").expect("valid").join(&name);
        assert_eq!(path.as_bytes(), b"a/b/c");
        let components: Vec<&[u8]> = path.components().collect();
        assert_eq!(components, [&b"a"[..], b"b", b"c"]);
        assert_eq!(RelPath::root().join(&name).as_bytes(), b"c");
        assert!(!path.is_root());
    }

    #[test]
    fn a_path_is_appended_to_another_with_a_single_slash() {
        let (a, b) = (
            RelPath::parse("a").expect("valid"),
            RelPath::parse("b/c").expect("valid"),
        );
        assert_eq!(a.concat(&b).as_bytes(), b"a/b/c");
        assert_eq!(a.concat(&RelPath::root()), a);
        assert_eq!(RelPath::root().concat(&b), b);
        assert!(RelPath::root().concat(&RelPath::root()).is_root());
    }

    #[test]
    fn paths_sort_by_their_bytes() {
        let mut paths: Vec<RelPath> = ["dir/x", "dir-y", "dir"]
            .iter()
            .map(|text| RelPath::parse(text).expect("valid"))
            .collect();
        paths.sort();
        let sorted: Vec<String> = paths.iter().map(ToString::to_string).collect();
        assert_eq!(sorted, ["dir", "dir-y", "dir/x"]);
    }

    #[test]
    fn paths_and_names_serialise_as_their_escaped_form() {
        let path = RelPath::from_bytes(b"a/\xff".to_vec()).expect("valid");
        assert_eq!(
            serde_json::to_value(&path).expect("serialises"),
            json!("a/%FF")
        );
        let back: RelPath = serde_json::from_value(json!("a/%FF")).expect("deserialises");
        assert_eq!(back, path);
        let name = Name::from_bytes(b"x%y".to_vec()).expect("valid");
        assert_eq!(
            serde_json::to_value(&name).expect("serialises"),
            json!("x%25y")
        );
        let back: Name = serde_json::from_value(json!("x%25y")).expect("deserialises");
        assert_eq!(back, name);
    }

    #[test]
    fn deserialising_checks_the_invariants() {
        for bad in ["../x", "/x", "a//b", "%41"] {
            assert!(
                serde_json::from_value::<RelPath>(json!(bad)).is_err(),
                "{bad:?} was accepted"
            );
        }
        assert!(serde_json::from_value::<Name>(json!("a/b")).is_err());
    }

    #[test]
    fn path_errors_are_failures_without_a_kind_of_their_own() {
        let error: Error = RelPath::parse("../x").expect_err("invalid").into();
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    fn name(bytes: &[u8]) -> Name {
        Name::from_bytes(bytes.to_vec()).expect("a valid name")
    }

    /// `count` plain bytes, then `tail`.
    fn padded(count: usize, tail: &str) -> Vec<u8> {
        let mut bytes = vec![b'a'; count];
        bytes.extend_from_slice(tail.as_bytes());
        bytes
    }

    #[test]
    fn a_plain_name_is_its_own_part() {
        assert_eq!(name_part(&name(b"photos-2020_v1.0")), "photos-2020_v1.0");
    }

    #[test]
    fn bytes_outside_the_safe_set_become_escapes() {
        assert_eq!(name_part(&name(b"a b\xc3\xa9\xff")), "a%20b%C3%A9%FF");
    }

    #[test]
    fn a_percent_sign_becomes_25() {
        assert_eq!(name_part(&name(b"100%")), "100%25");
    }

    #[test]
    fn a_name_of_70_plain_bytes_is_cut_at_64() {
        assert_eq!(name_part(&name(&padded(70, ""))), "a".repeat(64));
    }

    #[test]
    fn an_escape_across_byte_64_is_cut_before_it() {
        let bytes = padded(62, "ébbbbbb");
        assert_eq!(bytes.len(), 70);
        assert_eq!(name_part(&name(&bytes)), "a".repeat(62));
    }

    #[test]
    fn a_cut_may_fall_between_the_escapes_of_one_character() {
        let part = name_part(&name(&padded(61, "é")));
        assert_eq!(part, format!("{}%C3", "a".repeat(61)));
    }

    #[test]
    fn a_part_of_exactly_64_bytes_stays_whole() {
        assert_eq!(name_part(&name(&padded(64, ""))), "a".repeat(64));
    }

    #[test]
    fn the_65th_byte_is_cut() {
        assert_eq!(name_part(&name(&padded(65, ""))), "a".repeat(64));
    }

    #[test]
    fn a_machine_part_is_cut_at_32() {
        assert_eq!(machine_part(&"m".repeat(40)), "m".repeat(32));
    }

    #[test]
    fn an_escape_across_byte_32_is_cut_before_it() {
        let machine = format!("{}ébbb", "m".repeat(30));
        assert_eq!(machine_part(&machine), "m".repeat(30));
    }

    #[test]
    fn a_machine_name_is_escaped_like_a_name_part() {
        assert_eq!(machine_part("Jakob's MacBook"), "Jakob%27s%20MacBook");
    }

    #[test]
    fn a_temp_name_wraps_the_final_name() {
        let temp = temp_name(&name(b"0123abcd-photos.tar.zst"), "mac book");
        assert_eq!(temp.as_bytes(), b".0123abcd-photos.tar.zst.mac%20book.part");
    }

    #[test]
    fn a_temp_name_keeps_non_utf8_bytes_of_the_final_name() {
        let temp = temp_name(&name(b"x\xff"), "m");
        assert_eq!(temp.as_bytes(), b".x\xff.m.part");
    }

    /// Reads `%XX` escapes back; a scanner of its own, since `unescape`
    /// refuses spellings that `escape` would not write.
    fn decode_part(part: &str) -> Vec<u8> {
        let source = part.as_bytes();
        let mut bytes = Vec::new();
        let mut index = 0;
        while index < source.len() {
            if source[index] == b'%' {
                let digits = std::str::from_utf8(&source[index + 1..index + 3])
                    .expect("an escape has two hex digits");
                assert!(
                    digits
                        .bytes()
                        .all(|digit| digit.is_ascii_digit() || (b'A'..=b'F').contains(&digit)),
                    "{part:?} has a bad escape"
                );
                bytes.push(u8::from_str_radix(digits, 16).expect("two hex digits"));
                index += 3;
            } else {
                assert!(
                    source[index].is_ascii_alphanumeric() || b"._-".contains(&source[index]),
                    "{part:?} keeps an unsafe byte"
                );
                bytes.push(source[index]);
                index += 1;
            }
        }
        bytes
    }

    #[test]
    fn a_cut_part_never_ends_inside_an_escape() {
        let mut inputs: Vec<Vec<u8>> = (0..=255u8).map(|byte| vec![byte]).collect();
        inputs.extend((0..=100).map(|count| "é".repeat(count).into_bytes()));
        inputs.extend((0..=100).map(|count| vec![0xff; count]));
        for limit in [NAME_PART_LIMIT, MACHINE_PART_LIMIT] {
            for input in &inputs {
                let part = escape_part(input, limit);
                assert!(part.len() <= limit, "{part:?} is over {limit}");
                assert_eq!(part.is_empty(), input.is_empty(), "{input:?} gave {part:?}");
                let decoded = decode_part(&part);
                assert!(input.starts_with(&decoded), "{part:?} is not a prefix");
            }
        }
    }

    fn below(root: &str, given: &str, cwd: &str) -> Result<RelPath, RelPathError> {
        below_root(Path::new(root), Path::new(given), Path::new(cwd))
    }

    fn below_text(root: &str, given: &str, cwd: &str) -> String {
        below(root, given, cwd).expect("below the root").to_string()
    }

    #[test]
    fn dot_and_dotdot_fold() {
        assert_eq!(below_text("/c", "/c/a/./b/../x", "/c"), "a/x");
    }

    #[test]
    fn repeated_slashes_collapse() {
        assert_eq!(below_text("/c", "/c//a///b", "/c"), "a/b");
    }

    #[test]
    fn a_relative_given_is_joined_to_the_cwd() {
        assert_eq!(below_text("/c", "x/y", "/c/a"), "a/x/y");
    }

    #[test]
    fn a_cwd_inside_the_root_with_dotdot_reaches_a_sibling() {
        assert_eq!(below_text("/c", "../b", "/c/a"), "b");
    }

    #[test]
    fn dotdot_above_the_filesystem_root_is_refused() {
        for (given, cwd) in [("/..", "/c"), ("a/../../..", "/x")] {
            let error = below("/c", given, cwd).expect_err("above the root");
            assert!(matches!(error, RelPathError::AboveRoot(_)), "{error:?}");
        }
        let error = below("/c", "/..", "/c").expect_err("above the root");
        assert_eq!(
            error.to_string(),
            "`/..` goes above the root of the filesystem"
        );
    }

    #[test]
    fn dotdot_out_of_the_collection_is_outside_the_root() {
        let error = below("/c", "/c/a/../..", "/c").expect_err("outside");
        assert!(
            matches!(error, RelPathError::OutsideRoot { .. }),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "`/` is not below `/c`");
    }

    #[test]
    fn a_sibling_with_the_root_as_text_prefix_is_outside() {
        let error = below("/a/b", "/a/bc/x", "/a/b").expect_err("outside");
        assert!(
            matches!(error, RelPathError::OutsideRoot { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_root_itself_is_the_root_path() {
        assert!(below("/c", "/c", "/c").expect("the root").is_root());
        assert!(below("/c", "a/..", "/c").expect("the root").is_root());
    }

    #[test]
    fn a_relative_root_is_refused() {
        let error = below("c", "/c/a", "/c").expect_err("relative root");
        assert!(matches!(error, RelPathError::NotAbsolute(_)), "{error:?}");
        assert_eq!(error.to_string(), "`c` is not an absolute path");
    }

    #[test]
    fn a_relative_cwd_is_refused() {
        let error = below("/c", "a", "c").expect_err("relative cwd");
        assert!(matches!(error, RelPathError::NotAbsolute(_)), "{error:?}");
        assert_eq!(error.to_string(), "`c` is not an absolute path");
    }

    #[test]
    fn a_relative_cwd_is_refused_when_given_is_absolute() {
        let error = below("/c", "/c/a", "c").expect_err("relative cwd");
        assert!(matches!(error, RelPathError::NotAbsolute(_)), "{error:?}");
        assert_eq!(error.to_string(), "`c` is not an absolute path");
    }

    #[test]
    fn a_non_utf8_component_survives_and_prints_escaped() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let given = Path::new(OsStr::from_bytes(b"/c/a\xff"));
        let path = below_root(Path::new("/c"), given, Path::new("/c")).expect("below the root");
        assert_eq!(path.as_bytes(), b"a\xff");
        assert_eq!(path.to_string(), "a%FF");
        let outside = Path::new(OsStr::from_bytes(b"/d/\xff"));
        let error = below_root(Path::new("/c"), outside, Path::new("/c")).expect_err("outside");
        assert_eq!(error.to_string(), "`/d/%FF` is not below `/c`");
    }

    #[test]
    fn a_symlink_in_the_path_is_not_followed() {
        let dir = tempfile::tempdir().expect("a temp directory");
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("deep")).expect("directories");
        std::os::unix::fs::symlink(real.join("deep"), dir.path().join("link")).expect("a link");
        let given = dir.path().join("link/..");
        assert_eq!(
            std::fs::canonicalize(&given).expect("resolves"),
            std::fs::canonicalize(&real).expect("resolves"),
            "the filesystem would resolve `link/..` to `real`"
        );
        let path = below_root(dir.path(), &given, dir.path()).expect("below the root");
        assert!(path.is_root());
    }

    #[test]
    fn the_unresolved_spelling_of_the_root_is_outside() {
        let error = below("/tmp/c", "/private/tmp/c/a", "/private/tmp/c").expect_err("outside");
        assert!(
            matches!(error, RelPathError::OutsideRoot { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_nul_byte_in_the_path_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let given = Path::new(OsStr::from_bytes(b"/c/a\0b"));
        let error = below_root(Path::new("/c"), given, Path::new("/c")).expect_err("a NUL byte");
        assert!(matches!(error, RelPathError::InvalidPath(_)), "{error:?}");
    }
}
