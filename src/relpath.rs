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
}
