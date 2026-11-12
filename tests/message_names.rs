// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A message of the library shows a relative path or a name only through
//! `Shown` or `CodeSpan`.
//!
//! `RelPath` and `Name` print their escaped storage form, which keeps a
//! control character. A field of either type that an `#[error]` or a `help`
//! interpolates bare, as `{piece}` or as a plain argument, prints that form
//! into the message. This test reads the sources and rejects such a field.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("a directory reads") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            rust_files(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// The index just past the delimiter that closes the one opened at `open`.
fn closing(text: &str, open: usize) -> usize {
    let bytes = text.as_bytes();
    let (opener, closer) = match bytes[open] {
        b'(' => (b'(', b')'),
        b'[' => (b'[', b']'),
        b'{' => (b'{', b'}'),
        other => panic!("not a delimiter: {}", other as char),
    };
    let mut depth = 0;
    let mut index = open;
    let mut in_string = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if byte == b'\\' {
                index += 1;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'\'' && bytes.get(index + 2) == Some(&b'\'') {
            index += 2;
        } else if byte == opener {
            depth += 1;
        } else if byte == closer {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
        index += 1;
    }
    panic!("unclosed delimiter at {open}");
}

/// The fields of a variant, as `(name, type)`. A tuple field is named by its
/// index.
fn fields(text: &str, open: usize) -> Vec<(String, String)> {
    let close = closing(text, open);
    let inner = &text[open + 1..close - 1];
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (index, character) in inner.char_indices() {
        match character {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '>' if !inner[..index].ends_with('-') => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    let attribute = Regex::new(r"(?s)#\[.*?\]").expect("a valid pattern");
    let tuple = text.as_bytes()[open] == b'(';
    parts
        .iter()
        .map(|part| attribute.replace_all(part, "").trim().to_string())
        .filter(|part| !part.is_empty() && !part.starts_with("///"))
        .enumerate()
        .map(|(position, part)| {
            let part = part
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join(" ");
            if tuple {
                (position.to_string(), part.trim().to_string())
            } else {
                match part.split_once(':') {
                    Some((name, kind)) => (name.trim().to_string(), kind.trim().to_string()),
                    None => (part.clone(), String::new()),
                }
            }
        })
        .collect()
}

/// The names of the `RelPath` and `Name` fields that the attributes of a
/// variant interpolate bare.
fn bare_uses(attributes: &str, fields: &[(String, String)]) -> Vec<String> {
    let typed = Regex::new(r"\b(RelPath|Name)\b").expect("a valid pattern");
    let mut bare = Vec::new();
    for (name, kind) in fields {
        if !typed.is_match(kind) {
            continue;
        }
        let inline = Regex::new(&format!(r"\{{{}(:[^}}]*)?\}}", regex::escape(name)))
            .expect("a valid pattern");
        let argument = Regex::new(&format!(
            r"[,(]\s*\.{}\s*([,)]|\.to_string\(\))",
            regex::escape(name)
        ))
        .expect("a valid pattern");
        let call = Regex::new(&format!(
            r"(Shown|CodeSpan)\(\s*&?\.{}\s*\)",
            regex::escape(name)
        ))
        .expect("a valid pattern");
        let named = Regex::new(&format!(
            r"\b{}\s*=\s*(Shown|CodeSpan)\(",
            regex::escape(name)
        ))
        .expect("a valid pattern");
        let stripped = call.replace_all(attributes, "");
        let wrapped_by_name = named.is_match(attributes);
        if (inline.is_match(attributes) && !wrapped_by_name) || argument.is_match(&stripped) {
            bare.push(format!("{name}: {kind}"));
        }
    }
    bare
}

#[test]
fn no_message_formats_a_relative_path_or_a_name_bare() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let start = Regex::new(r"#\[(error|diagnostic)\(").expect("a valid pattern");
    let mut offenders = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("a source reads");
        let mut from = 0;
        while let Some(found) = start.find_at(&text, from) {
            // The first attribute of a group has no attribute before it.
            let before = text[..found.start()].trim_end();
            if before.ends_with(']') {
                from = found.end();
                continue;
            }
            // Take the group of attributes, then the name and the fields.
            let mut cursor = found.start();
            let group_start = cursor;
            loop {
                let rest = &text[cursor..];
                let skipped = rest.len() - rest.trim_start().len();
                cursor += skipped;
                if text[cursor..].starts_with("#[") {
                    cursor = closing(&text, cursor + 1);
                } else if text[cursor..].starts_with("///") {
                    cursor += text[cursor..].find('\n').unwrap_or(text.len() - cursor);
                } else {
                    break;
                }
            }
            let attributes = &text[group_start..cursor];
            let after = &text[cursor..];
            let name_end = after
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == ' '))
                .unwrap_or(after.len());
            let mut field_start = cursor + name_end;
            let rest = &text[field_start..];
            field_start += rest.len() - rest.trim_start().len();
            from = cursor.max(found.end());
            if !attributes.contains("#[error(") {
                continue;
            }
            let open = match text.as_bytes().get(field_start) {
                Some(b'{' | b'(') => field_start,
                _ => continue,
            };
            for field in bare_uses(attributes, &fields(&text, open)) {
                let line = text[..group_start].matches('\n').count() + 1;
                offenders.push(format!("{}:{line}: {field}", file.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a message formats these fields bare, use `Shown` or `CodeSpan(Shown(..))`:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_finds_a_bare_field_in_a_made_up_variant() {
    let source = "#[error(\"{piece} is gone, {}\", .root)]\nGone { piece: RelPath, root: Name },\n";
    let open = source
        .find('{')
        .map(|_| source.rfind("Gone").expect("a variant") + 5);
    let open = open.expect("a field list");
    let found = bare_uses(source, &fields(source, open));
    assert_eq!(found, ["piece: RelPath", "root: Name"]);

    let wrapped = "#[error(\"{} is gone, {}\", Shown(.piece), CodeSpan(.root))]\nGone { piece: RelPath, root: Name },\n";
    let open = wrapped.rfind("Gone").expect("a variant") + 5;
    assert!(bare_uses(wrapped, &fields(wrapped, open)).is_empty());
}

// =========================================================
// What the messages print
// =========================================================

mod shown_in_messages {
    use antiq::archive::PackError;
    use antiq::archive::names::NameProblem;
    use antiq::archive::verify::Difference;
    use antiq::commands::push::PushError;
    use antiq::commands::restore::TargetProblem;
    use antiq::markers::MarkerError;
    use antiq::pin::PinError;
    use antiq::relpath::RelPath;
    use antiq::select::SelectError;
    use antiq::walk::WalkError;
    use std::io;

    fn hostile() -> RelPath {
        RelPath::from_bytes(b"a\n100%\xff".to_vec()).expect("a path")
    }

    const SHOWN: &str = "a%0A100%25%FF";

    fn io_error() -> io::Error {
        io::Error::other("x")
    }

    #[test]
    fn a_pin_a_marker_and_a_pack_error_show_the_path_by_the_rule() {
        let messages = [
            PinError::NotAFile { path: hostile() }.to_string(),
            MarkerError::Invalid {
                path: hostile(),
                reason: "bad".into(),
            }
            .to_string(),
            MarkerError::MountPoint { path: hostile() }.to_string(),
            PackError::Read {
                path: hostile(),
                source: io_error(),
            }
            .to_string(),
            PackError::Extract {
                path: hostile(),
                source: io_error(),
            }
            .to_string(),
            WalkError::OutOfDescriptors {
                piece: hostile(),
                path: hostile(),
                source: io_error(),
            }
            .to_string(),
            SelectError::OutOfDescriptors {
                path: hostile(),
                source: io_error(),
            }
            .to_string(),
        ];
        for message in messages {
            assert!(message.contains(SHOWN), "{message:?}");
            assert!(!message.contains('\n'), "{message:?}");
        }
    }

    #[test]
    fn a_name_clash_a_name_problem_and_a_difference_show_their_paths_by_the_rule() {
        let clash = PushError::NameClash {
            first: hostile(),
            second: hostile(),
        }
        .to_string();
        assert_eq!(clash.matches(SHOWN).count(), 2, "{clash:?}");
        let problems = [
            NameProblem::NotUtf8 { path: hostile() }.to_string(),
            NameProblem::Collision {
                paths: vec![hostile(), hostile()],
            }
            .to_string(),
            TargetProblem::Excluded { at: hostile() }.to_string(),
            Difference::Removed(hostile()).to_string(),
        ];
        for text in problems {
            assert!(text.contains(SHOWN), "{text:?}");
            assert!(!text.contains('\n'), "{text:?}");
        }
    }
}
