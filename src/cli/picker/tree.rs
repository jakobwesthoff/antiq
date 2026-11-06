// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tree of a piece's contents as the picker lists it under the piece.
//!
//! The builder is pure: it gets the entries of a listing, which of them
//! match the query, and how many lines fit, and returns the lines to draw.

use std::collections::HashMap;

/// One entry of a listing, in display form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The path below the piece, components separated by `/`.
    pub path: String,
    /// Whether the entry is a directory.
    pub dir: bool,
    /// The size in bytes of a file; ignored for a directory.
    pub size: u64,
}

/// One line of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeLine {
    /// How many shown directories lie above the line.
    pub depth: usize,
    /// The name, `/`-terminated for a directory; a chain of single-child
    /// directories is one line.
    pub text: String,
    /// Whether the line is a directory.
    pub dir: bool,
    /// The size of a file, or the sum of the file sizes below a directory.
    pub size: u64,
    /// Char positions in `text` to highlight, ascending.
    pub highlights: Vec<u32>,
}

/// The lines to draw and how many entries they leave out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tree {
    pub lines: Vec<TreeLine>,
    /// The entries counted in `matched` that no line stands for.
    pub more: usize,
    /// The entries the tree is about: the topmost matches, or every entry
    /// without a filter.
    pub matched: usize,
}

/// One path component of the listing, listed or only implied by a deeper
/// path.
struct Node {
    name: String,
    dir: bool,
    /// The index of the listing entry with exactly this path, if any.
    entry: Option<usize>,
    /// The sum of the file sizes at or below this node.
    size: u64,
    children: Vec<usize>,
    /// Whether the node is, or leads to, a match.
    visible: bool,
}

/// The nodes of the entries' paths; node 0 is the root above everything.
fn nodes(entries: &[Member]) -> Vec<Node> {
    let node = |name: &str| Node {
        name: name.to_string(),
        dir: true,
        entry: None,
        size: 0,
        children: Vec::new(),
        visible: false,
    };
    let mut nodes = vec![node("")];
    let mut child_of: HashMap<(usize, String), usize> = HashMap::new();
    for (index, member) in entries.iter().enumerate() {
        let mut at = 0;
        for name in member.path.split('/').filter(|name| !name.is_empty()) {
            at = match child_of.get(&(at, name.to_string())) {
                Some(&child) => child,
                None => {
                    nodes.push(node(name));
                    let child = nodes.len() - 1;
                    nodes[at].children.push(child);
                    child_of.insert((at, name.to_string()), child);
                    child
                }
            };
        }
        // An entry is a directory by its type; a file's parents are
        // directories whether or not the listing has entries for them.
        nodes[at].dir = member.dir;
        nodes[at].entry = Some(index);
        if !member.dir {
            nodes[at].size = member.size;
        }
    }
    nodes
}

/// Build the tree of `entries`.
///
/// `matched` says which entries match the query, or is `None` for the whole
/// tree. A matching directory is a line of its own that stands for
/// everything below it. A directory that only leads to matches shows, with
/// the matches below it. A directory whose only entry is another directory is
/// joined with it into one line (`data/raw/`), which has the size of both.
///
/// At most `cap` lines come out, the last of them left to the caller's cut
/// line when the tree does not fit: with a cut, `cap - 1` lines remain.
/// `highlight` marks the matched characters of a line's text, and runs for
/// the lines that are kept only.
pub fn tree(
    entries: &[Member],
    matched: Option<&[bool]>,
    cap: usize,
    mut highlight: impl FnMut(&str) -> Vec<u32>,
) -> Tree {
    let Walk { mut lines, total } = walk(entries, matched, cap + 1);
    if lines.len() > cap {
        lines.truncate(cap.saturating_sub(1));
    }
    let shown: usize = lines.iter().map(|(_, counted)| counted).sum();
    Tree {
        lines: lines
            .into_iter()
            .map(|(mut line, _)| {
                line.highlights = highlight(&line.text);
                line
            })
            .collect(),
        more: total - shown,
        matched: total,
    }
}

/// What the walk over the tree found.
struct Walk {
    /// The first `limit` lines with the number of entries each counts.
    lines: Vec<(TreeLine, usize)>,
    /// The entries the whole tree counts.
    total: usize,
}

/// Walk the tree depth first and collect its lines, stopping after `limit`
/// of them: a listing of many thousand members costs no more than the lines
/// that fit. The entries are counted over the whole tree regardless.
fn walk(entries: &[Member], matched: Option<&[bool]>, limit: usize) -> Walk {
    let mut nodes = nodes(entries);
    let is_match = |node: &Node| match (matched, node.entry) {
        (Some(matched), Some(entry)) => matched[entry],
        _ => false,
    };

    // Children are created after their parent, so walking the nodes backwards
    // sums every size and visibility into the parent before it is read.
    let mut parent = vec![0; nodes.len()];
    for (at, node) in nodes.iter().enumerate() {
        for &child in &node.children {
            parent[child] = at;
        }
    }
    for at in (0..nodes.len()).rev() {
        nodes[at].visible = matched.is_none() || nodes[at].visible || is_match(&nodes[at]);
        if at > 0 {
            let (size, visible) = (nodes[at].size, nodes[at].visible);
            nodes[parent[at]].size += size;
            nodes[parent[at]].visible |= visible;
        }
    }

    // What a line counts: the matching entries it stands for, or with no
    // filter every entry among the nodes it joins.
    let counts = |node: &Node| match matched {
        None => usize::from(node.entry.is_some()),
        Some(_) => usize::from(is_match(node)),
    };
    // A matching directory ends the descent, so what lies below it adds
    // nothing to the total.
    let leaf = |node: &Node| node.dir && matched.is_some() && is_match(node);

    let mut walker = Walker {
        nodes: &nodes,
        leaf: &leaf,
        counts: &counts,
        limit,
        lines: Vec::new(),
        total: 0,
    };
    for &top in &nodes[0].children {
        walker.count(top);
    }
    for &top in &nodes[0].children {
        walker.line(top, 0);
    }
    Walk {
        lines: walker.lines,
        total: walker.total,
    }
}

/// The depth-first walk over the nodes that show.
struct Walker<'a> {
    nodes: &'a [Node],
    leaf: &'a dyn Fn(&Node) -> bool,
    counts: &'a dyn Fn(&Node) -> usize,
    /// The number of lines after which the walk stops.
    limit: usize,
    /// The lines with the number of entries each counts.
    lines: Vec<(TreeLine, usize)>,
    /// The entries the whole tree counts.
    total: usize,
}

impl Walker<'_> {
    /// Add what `at` and the nodes below it count to the total.
    fn count(&mut self, at: usize) {
        let node = &self.nodes[at];
        if !node.visible {
            return;
        }
        self.total += (self.counts)(node);
        if (self.leaf)(node) {
            return;
        }
        for &child in &node.children {
            self.count(child);
        }
    }

    /// Add the line of `at` and the lines below it, `depth` directories deep.
    fn line(&mut self, at: usize, depth: usize) {
        if self.lines.len() >= self.limit || !self.nodes[at].visible {
            return;
        }
        let mut last = &self.nodes[at];
        let mut text = last.name.clone();
        let mut counted = (self.counts)(last);
        if last.dir {
            text.push('/');
        }
        // Join the directory with its only child when that is a directory:
        // a directory with any other entry in the listing, shown or not,
        // keeps its own line.
        while last.dir && !(self.leaf)(last) {
            let &[only] = last.children.as_slice() else {
                break;
            };
            if !self.nodes[only].dir {
                break;
            }
            last = &self.nodes[only];
            text.push_str(&last.name);
            text.push('/');
            counted += (self.counts)(last);
        }

        self.lines.push((
            TreeLine {
                depth,
                text,
                dir: last.dir,
                size: last.size,
                highlights: Vec::new(),
            },
            counted,
        ));
        if last.dir && !(self.leaf)(last) {
            for &child in &last.children {
                self.line(child, depth + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> Member {
        Member {
            path: path.to_string(),
            dir: false,
            size,
        }
    }

    fn dir(path: &str) -> Member {
        Member {
            path: path.to_string(),
            dir: true,
            size: 0,
        }
    }

    fn nothing(_: &str) -> Vec<u32> {
        Vec::new()
    }

    /// The lines as `(depth, text, size)`.
    fn lines(tree: &Tree) -> Vec<(usize, &str, u64)> {
        tree.lines
            .iter()
            .map(|line| (line.depth, line.text.as_str(), line.size))
            .collect()
    }

    #[test]
    fn the_whole_tree_lists_children_in_listing_order() {
        let entries = [
            dir("src"),
            file("src/main.rs", 10),
            file("src/lib.rs", 20),
            file("README", 5),
        ];
        let tree = tree(&entries, None, 10, nothing);
        assert_eq!(
            lines(&tree),
            [
                (0, "src/", 30),
                (1, "main.rs", 10),
                (1, "lib.rs", 20),
                (0, "README", 5)
            ]
        );
        assert_eq!((tree.more, tree.matched), (0, 4));
    }

    #[test]
    fn a_chain_without_branching_is_one_line() {
        let entries = [dir("data"), dir("data/raw"), file("data/raw/a.csv", 7)];
        let tree = tree(&entries, None, 10, nothing);
        assert_eq!(lines(&tree), [(0, "data/raw/", 7), (1, "a.csv", 7)]);
    }

    #[test]
    fn a_directory_with_one_file_is_not_merged_with_it() {
        let entries = [dir("docs"), file("docs/guide.txt", 3)];
        let tree = tree(&entries, None, 10, nothing);
        assert_eq!(lines(&tree), [(0, "docs/", 3), (1, "guide.txt", 3)]);
    }

    #[test]
    fn a_branching_directory_is_not_merged() {
        let entries = [file("data/raw/a", 1), file("data/tmp/b", 2)];
        let tree = tree(&entries, None, 10, nothing);
        assert_eq!(
            lines(&tree),
            [
                (0, "data/", 3),
                (1, "raw/", 1),
                (2, "a", 1),
                (1, "tmp/", 2),
                (2, "b", 2)
            ]
        );
    }

    #[test]
    fn a_matched_directory_is_a_leaf_with_the_size_below_it() {
        let entries = [
            dir("scanner"),
            file("scanner/a.rs", 3),
            file("scanner/b.rs", 4),
        ];
        let tree = tree(&entries, Some(&[true, false, false]), 10, nothing);
        assert_eq!(lines(&tree), [(0, "scanner/", 7)]);
        assert_eq!((tree.more, tree.matched), (0, 1));
    }

    #[test]
    fn what_matches_below_a_matched_directory_is_not_counted() {
        let entries = [dir("scanner"), file("scanner/a.rs", 3)];
        let tree = tree(&entries, Some(&[true, true]), 10, nothing);
        assert_eq!(lines(&tree), [(0, "scanner/", 3)]);
        assert_eq!(tree.matched, 1);
    }

    #[test]
    fn the_ancestors_of_a_match_show_and_the_siblings_that_missed_do_not() {
        let entries = [
            dir("src"),
            file("src/a.rs", 3),
            file("src/b.rs", 4),
            file("top.txt", 9),
        ];
        let tree = tree(&entries, Some(&[false, true, false, false]), 10, nothing);
        assert_eq!(lines(&tree), [(0, "src/", 7), (1, "a.rs", 3)]);
        assert_eq!((tree.more, tree.matched), (0, 1));
    }

    #[test]
    fn a_cap_cuts_and_counts_what_is_left_out() {
        let entries = [
            file("a", 1),
            file("b", 1),
            dir("x"),
            file("x/1", 1),
            file("x/2", 1),
            file("z", 1),
        ];
        let all = [true; 6];
        let tree = tree(&entries, Some(&all), 3, nothing);
        assert_eq!(lines(&tree), [(0, "a", 1), (0, "b", 1)]);
        assert_eq!(
            (tree.more, tree.matched),
            (2, 4),
            "x stands for its members, z is the other"
        );
    }

    #[test]
    fn a_cap_that_fits_every_line_cuts_nothing() {
        let entries = [file("a", 1), file("b", 1)];
        let tree = tree(&entries, None, 2, nothing);
        assert_eq!(lines(&tree).len(), 2);
        assert_eq!(tree.more, 0);
    }

    #[test]
    fn a_cap_of_zero_or_one_shows_no_line() {
        let entries = [file("a", 1), file("b", 1)];
        for cap in [0, 1] {
            let tree = tree(&entries, None, cap, nothing);
            assert!(tree.lines.is_empty(), "cap {cap}");
            assert_eq!((tree.more, tree.matched), (2, 2), "cap {cap}");
        }
    }

    #[test]
    fn a_merged_chain_counts_every_directory_it_merges_without_a_filter() {
        let entries = [
            dir("data"),
            dir("data/raw"),
            file("data/raw/a", 1),
            file("data/raw/b", 1),
        ];
        let tree = tree(&entries, None, 2, nothing);
        assert_eq!(lines(&tree), [(0, "data/raw/", 2)]);
        assert_eq!((tree.more, tree.matched), (2, 4));
    }

    #[test]
    fn a_listing_out_of_order_still_groups_children_under_their_directory() {
        let entries = [file("b/x", 1), file("a/y", 1), file("b/z", 1)];
        let tree = tree(&entries, None, 10, nothing);
        assert_eq!(
            lines(&tree),
            [
                (0, "b/", 2),
                (1, "x", 1),
                (1, "z", 1),
                (0, "a/", 1),
                (1, "y", 1)
            ]
        );
    }

    #[test]
    fn a_match_without_listed_parents_still_gets_its_ancestors() {
        let entries = [file("deep/er/file", 4)];
        let tree = tree(&entries, Some(&[true]), 10, nothing);
        assert_eq!(lines(&tree), [(0, "deep/er/", 4), (1, "file", 4)]);
        assert_eq!((tree.more, tree.matched), (0, 1));
    }

    #[test]
    fn a_directory_entry_has_no_size_of_its_own() {
        let mut parent = dir("d");
        parent.size = 999;
        let entries = [parent, file("d/a", 1), file("d/b", 2)];
        let tree = tree(&entries, None, 10, nothing);
        assert_eq!(tree.lines[0].size, 3);
    }

    #[test]
    fn only_the_lines_shown_are_highlighted() {
        let entries = [
            file("a.rs", 1),
            file("b.rs", 1),
            file("c.rs", 1),
            file("d.rs", 1),
        ];
        let mut asked = Vec::new();
        let tree = tree(&entries, None, 3, |text| {
            asked.push(text.to_string());
            vec![0]
        });
        assert_eq!(asked, ["a.rs", "b.rs"]);
        assert_eq!(tree.lines[0].highlights, [0]);
    }

    #[test]
    fn the_walk_stops_at_its_limit_however_long_the_listing_is() {
        let entries: Vec<Member> = (0..1000).map(|n| file(&format!("f{n}"), 1)).collect();
        let walked = walk(&entries, None, 4);
        assert_eq!(walked.lines.len(), 4);
        assert_eq!(walked.total, 1000, "the entries are counted in full");
    }

    #[test]
    fn a_directory_with_a_sibling_that_did_not_match_is_not_merged() {
        let entries = [
            dir("src"),
            dir("src/scanner"),
            file("src/scanner/lib.rs", 61),
            file("src/util.rs", 27),
        ];
        let tree = tree(&entries, Some(&[false, true, false, false]), 10, nothing);
        assert_eq!(lines(&tree), [(0, "src/", 88), (1, "scanner/", 61)]);
    }
}
