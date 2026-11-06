// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Ignore and timeless rules.

use crate::error::HasKind;
use crate::relpath::RelPath;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The name of the file that holds ignore rules, in any directory of a piece.
pub const IGNORE_FILE: &[u8] = b".antiqignore";

/// Why a rule or a rule file failed.
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("cannot read the rule file: {0}")]
    Unreadable(#[source] io::Error),
    #[error("the rule file is not UTF-8 text")]
    NotUtf8,
    /// `line` counts from 1; 0 stands for a failure of the whole rule set.
    #[error("line {line} is not a valid rule: {message}")]
    BadLine { line: usize, message: String },
}

impl HasKind for RulesError {}

/// Builds one matcher from rule lines. Paths are matched relative to the
/// directory the rules belong to, so the builder's root is `.`: any other
/// root makes the matcher cut a byte prefix off the path it is given.
pub(crate) fn compile<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Gitignore, RulesError> {
    let bad_line = |line: usize, error: ignore::Error| RulesError::BadLine {
        line,
        message: error.to_string(),
    };
    let mut builder = GitignoreBuilder::new(".");
    for (index, line) in lines.enumerate() {
        builder
            .add_line(None, line)
            .map_err(|error| bad_line(index + 1, error))?;
    }
    builder.build().map_err(|error| bad_line(0, error))
}

/// Compiles the content of a rule file.
pub(crate) fn parse_rule_file(content: Vec<u8>) -> Result<Gitignore, RulesError> {
    let text = String::from_utf8(content).map_err(|_| RulesError::NotUtf8)?;
    // Editors on some systems put a byte order mark in front of the first
    // rule, which would turn that rule into one that matches nothing.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    compile(text.split('\n'))
}

/// The rules of one kind while the entries of a piece are walked top down.
///
/// A directory comes before everything below it, so by the time an entry is
/// judged its ancestors are decided: a directory that matched hides
/// everything below it, whatever the rules inside say.
pub(crate) struct Scope {
    config: Gitignore,
    /// The rules read so far, by the directory (as path bytes) they belong to.
    files: HashMap<Vec<u8>, Gitignore>,
    hidden: HashSet<Vec<u8>>,
}

impl Scope {
    pub(crate) fn new(config: Gitignore) -> Scope {
        Scope {
            config,
            files: HashMap::new(),
            hidden: HashSet::new(),
        }
    }

    /// Registers the rules read from the rule file of `directory`.
    pub(crate) fn add_file(&mut self, directory: &RelPath, rules: Gitignore) {
        self.files.insert(directory.as_bytes().to_vec(), rules);
    }

    /// Whether the entry at `path` matches: a matching directory above it
    /// hides it, else the first rule that has a say decides, looking at the
    /// rule files from the nearest directory outwards and last at the config.
    /// A matching directory is remembered for the entries below it.
    pub(crate) fn decide(&mut self, path: &RelPath, is_dir: bool) -> bool {
        let matched = self.matches(path.as_bytes(), is_dir);
        if matched && is_dir {
            self.hidden.insert(path.as_bytes().to_vec());
        }
        matched
    }

    fn matches(&self, bytes: &[u8], is_dir: bool) -> bool {
        let (parent, _) = split_last(bytes);
        if self.hidden.contains(parent) {
            return true;
        }
        let mut directory = parent;
        loop {
            if let Some(rules) = self.files.get(directory) {
                let below = if directory.is_empty() {
                    bytes
                } else {
                    &bytes[directory.len() + 1..]
                };
                match rules.matched(Path::new(OsStr::from_bytes(below)), is_dir) {
                    ignore::Match::Ignore(_) => return true,
                    ignore::Match::Whitelist(_) => return false,
                    ignore::Match::None => {}
                }
            }
            if directory.is_empty() {
                break;
            }
            directory = split_last(directory).0;
        }
        self.config
            .matched(Path::new(OsStr::from_bytes(bytes)), is_dir)
            .is_ignore()
    }
}

/// The part of `bytes` before its last `/`, and the part after it. A path
/// without a `/` has the top of the piece, the empty path, as its parent.
pub(crate) fn split_last(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&bytes[..slash], &bytes[slash + 1..]),
        None => (&bytes[..0], bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("valid path")
    }

    fn rules(text: &str) -> Gitignore {
        parse_rule_file(text.as_bytes().to_vec()).expect("valid rules")
    }

    #[test]
    fn a_pattern_with_a_slash_is_anchored_to_its_rule_file() {
        let mut scope = Scope::new(rules(""));
        scope.add_file(&path("a"), rules("docs/*.log\n"));
        assert!(scope.decide(&path("a/docs/x.log"), false));
        assert!(!scope.decide(&path("a/b/docs/x.log"), false));
    }

    #[test]
    fn the_nearest_rule_file_beats_an_outer_one_and_the_config() {
        let mut scope = Scope::new(rules("*.log\n"));
        scope.add_file(&path("a"), rules("!keep.log\n"));
        assert!(!scope.decide(&path("a/keep.log"), false));
        assert!(scope.decide(&path("b/keep.log"), false));
    }

    #[test]
    fn a_matching_directory_hides_what_a_negation_would_bring_back() {
        let mut scope = Scope::new(rules("cache/\n"));
        assert!(scope.decide(&path("cache"), true));
        scope.add_file(&path("cache"), rules("!x\n"));
        assert!(scope.decide(&path("cache/x"), false));
    }

    #[test]
    fn a_byte_order_mark_does_not_spoil_the_first_rule() {
        let mut scope = Scope::new(rules(""));
        scope.add_file(&RelPath::root(), rules("\u{feff}*.tmp\n"));
        assert!(scope.decide(&path("x.tmp"), false));
    }

    #[test]
    fn a_rule_file_that_is_not_text_is_refused() {
        assert!(matches!(
            parse_rule_file(vec![0xff, 0xfe, b'\n']),
            Err(RulesError::NotUtf8)
        ));
    }

    #[test]
    fn a_broken_line_is_named_by_its_number() {
        match parse_rule_file(b"ok\nbad\\".to_vec()) {
            Err(RulesError::BadLine { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected a bad line, got {other:?}"),
        }
    }
}
