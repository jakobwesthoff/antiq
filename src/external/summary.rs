// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The lines a program printed to stderr, grouped by kind.

/// How many groups a terminal or a pipe shows. The JSON error carries all.
pub const GROUPS_SHOWN: usize = 5;

/// Lines of one kind: the same text once the quoted parts are replaced.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Group {
    pub count: usize,
    pub line: String,
    /// What the first line of the group quoted, when it quoted anything.
    pub example: Option<String>,
}

/// The groups of a stderr text, in order of first appearance.
pub fn group_lines(stderr: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for raw in stderr.lines() {
        let raw = raw.trim_end();
        if raw.trim().is_empty() {
            continue;
        }
        let (line, example) = normalise(raw);
        match groups.iter_mut().find(|group| group.line == line) {
            Some(group) => group.count += 1,
            None => groups.push(Group {
                count: 1,
                line,
                example,
            }),
        }
    }
    groups
}

/// `line` with every closed `"..."` replaced by `"…"`, and the content of
/// the first one. Only double quotes are read: rsync quotes the paths it
/// names that way, and a single quote is as likely an apostrophe.
fn normalise(line: &str) -> (String, Option<String>) {
    let mut normalised = String::with_capacity(line.len());
    let mut example = None;
    let mut rest = line;
    while let Some(open) = rest.find('"') {
        let Some(length) = rest[open + 1..].find('"') else {
            break;
        };
        let close = open + 1 + length;
        normalised.push_str(&rest[..open]);
        normalised.push_str("\"…\"");
        example.get_or_insert_with(|| rest[open + 1..close].to_string());
        rest = &rest[close + 1..];
    }
    normalised.push_str(rest);
    (normalised, example)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(count: usize, example: &str) -> Group {
        Group {
            count,
            line: "rsync: [receiver] mkstemp \"…\" failed: Operation not permitted (1)".into(),
            example: Some(example.into()),
        }
    }

    #[test]
    fn lines_that_differ_in_the_quoted_path_form_one_group() {
        let stderr = "rsync: [receiver] mkstemp \"/t/a/.x.1\" failed: Operation not permitted (1)\n\
                      rsync: [receiver] mkstemp \"/t/b/.y.2\" failed: Operation not permitted (1)\n";
        assert_eq!(group_lines(stderr), [rejected(2, "/t/a/.x.1")]);
    }

    #[test]
    fn a_line_without_quotes_is_its_own_group_without_an_example() {
        let stderr = "rsync error: some files/attributes were not transferred (code 23)\n";
        assert_eq!(
            group_lines(stderr),
            [Group {
                count: 1,
                line: "rsync error: some files/attributes were not transferred (code 23)".into(),
                example: None,
            }]
        );
    }

    #[test]
    fn groups_keep_the_order_of_their_first_line() {
        let stderr = "b \"1\"\na\nb \"2\"\na\n";
        let lines: Vec<(usize, String)> = group_lines(stderr)
            .into_iter()
            .map(|group| (group.count, group.line))
            .collect();
        assert_eq!(lines, [(2, "b \"…\"".to_string()), (2, "a".to_string())]);
    }

    #[test]
    fn the_example_is_the_first_quoted_part_of_the_first_line() {
        let stderr = "rename \"/a\" -> \"/b\" failed\nrename \"/c\" -> \"/d\" failed\n";
        let groups = group_lines(stderr);
        assert_eq!(groups[0].line, "rename \"…\" -> \"…\" failed");
        assert_eq!(groups[0].example.as_deref(), Some("/a"));
    }

    #[test]
    fn blank_lines_and_an_unclosed_quote_are_left_alone() {
        let groups = group_lines("\n  \nwhat \"is this\n");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].line, "what \"is this");
        assert_eq!(groups[0].example, None);
    }

    #[test]
    fn a_text_without_lines_has_no_groups() {
        assert!(group_lines("").is_empty());
    }
}
