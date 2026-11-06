// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A reference that names no single piece or version, reported on a
//! terminal: the error, the table of what it may have meant, and the info
//! that nothing was done.

use super::cmd::list::{Marking, RowLook, table, version_labels};
use super::console::block::{Block, Para};
use super::console::env::Env;
use super::console::render::Part;
use super::console::rich::Rich;
use super::console::symbols::Symbols;
use super::console::theme;
use super::output::Style;
use super::table::{WIDE_FROM, mid, pad, pad_left, painted, sanitize, spaces};
use super::{Cli, Command, Description, error_block};
use antiq::commands::list::{Row, list};
use antiq::error::explanation;
use antiq::query::Query;
use antiq::reference::{Candidate, ReferenceError};
use antiq::versions::naming::{VersionLookupError, system_zone};
use jiff::tz::TimeZone;
use miette::Diagnostic;
use std::path::Path;
use unicode_width::UnicodeWidthStr;

/// What did not resolve, as the library reports it.
enum Failure<'a> {
    Ambiguous {
        reference: &'a str,
        candidates: &'a [Candidate],
    },
    /// A search word that fits one piece, for a command that acts only on a
    /// path.
    Inexact {
        reference: &'a str,
        candidates: &'a [Candidate],
    },
    Unknown {
        closest: &'a [Candidate],
    },
    Version {
        piece: &'a str,
        input: &'a str,
        stamps: &'a [String],
    },
}

/// Where the pieces a command can mean come from, which decides how the
/// table lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// The pieces of the collection, which may have no stored version.
    Live,
    /// The pieces of the store, which all have versions.
    Store,
}

/// The command in the words the texts need.
#[derive(Debug, Clone, Copy)]
pub(super) struct Action {
    /// As in "find the piece to unearth".
    verb: &'static str,
    /// As in "Nothing was unearthed."
    pub(super) done: &'static str,
    source: Source,
}

pub(super) fn action(command: &Command) -> Option<Action> {
    let (verb, done, source) = match command {
        Command::Archive(_) => ("archive", "archived", Source::Live),
        Command::Pin(_) => ("pin", "pinned", Source::Live),
        Command::Unpin(_) => ("unpin", "unpinned", Source::Live),
        Command::Unearth(_) => ("unearth", "unearthed", Source::Store),
        Command::Fork(_) => ("fork", "forked", Source::Store),
        Command::Forget(_) => ("forget", "deleted", Source::Store),
        Command::Rebuild(_) => ("rebuild", "rebuilt", Source::Store),
        _ => return None,
    };
    Some(Action { verb, done, source })
}

/// The report of an unresolved reference on a terminal, or `None` when the
/// error is something else.
///
/// The listing of the collection is read again for the table. When that
/// fails the error is reported without a table, with the matches in its
/// text.
pub(super) fn group(cli: &Cli, description: &Description, env: &Env) -> Option<Vec<Part>> {
    let error = description.library_error?;
    let action = action(&cli.command)?;
    let reference = error_block::find::<ReferenceError>(error);
    let version = error_block::find::<VersionLookupError>(error);
    let explained = explanation(error)?;
    let failure = match (reference, version) {
        (
            Some(ReferenceError::Ambiguous {
                reference,
                candidates,
            }),
            _,
        ) => Failure::Ambiguous {
            reference,
            candidates,
        },
        (
            Some(ReferenceError::Inexact {
                reference,
                candidates,
                ..
            }),
            _,
        ) => Failure::Inexact {
            reference,
            candidates,
        },
        (Some(ReferenceError::Unknown { closest, .. }), _) => Failure::Unknown { closest },
        (
            _,
            Some(VersionLookupError::Ambiguous {
                piece,
                input,
                stamps,
            }),
        ) => Failure::Version {
            piece,
            input,
            stamps,
        },
        _ => return None,
    };
    let rows = super::start_directory(cli)
        .ok()
        .and_then(|start| all_rows(&start));
    let command = |piece: Option<&str>, version: Option<&str>| command_line(cli, piece, version);
    Some(build(
        &failure,
        explained,
        action,
        rows.as_deref(),
        &command,
        env,
        &system_zone(),
    ))
}

/// Every row of the listing of the collection around `start`.
fn all_rows(start: &Path) -> Option<Vec<Row>> {
    let collection = antiq::collection::Collection::find(start).ok()?;
    Some(list(&collection, None).ok()?.versions)
}

/// The parts of the report: the error as `explained` words it, the table
/// when there are `rows` to build it from, the info, and, for closest
/// pieces without a table, the hint that suggests a command.
fn build(
    failure: &Failure,
    explained: &dyn Diagnostic,
    action: Action,
    rows: Option<&[Row]>,
    command: &dyn Fn(Option<&str>, Option<&str>) -> String,
    env: &Env,
    zone: &TimeZone,
) -> Vec<Part> {
    let style = Style {
        width: env.width,
        color: env.color,
        terminal: true,
        utf8: true,
    };
    let mut parts = Vec::new();
    let mut hint = None;
    let table_lines: Option<Vec<String>>;
    let block;
    match failure {
        Failure::Ambiguous {
            reference,
            candidates,
        } => {
            let listing = piece_table(candidates, reference, action, rows, style, zone);
            table_lines = listing.map(|lines| {
                let mut shown = vec![bold(
                    &format!("The {} pieces that match `{reference}`", candidates.len()),
                    style,
                )];
                shown.extend(lines);
                shown
            });
            block = ambiguous_piece(explained, candidates, action, table_lines.is_some());
        }
        Failure::Unknown { closest } => {
            table_lines = if closest.is_empty() {
                None
            } else {
                piece_table(closest, "", action, rows, style, zone).map(|lines| {
                    let mut shown = vec![bold("The closest pieces", style)];
                    shown.extend(lines);
                    shown
                })
            };
            block = unknown_piece(explained, action);
            // The table names the closest pieces; the hint stands in for it
            // when the listing could not be read.
            if table_lines.is_none() {
                hint = closest.first().map(|nearest| {
                    Block::hint(
                        Rich::new()
                            .text("Did you mean ")
                            .path(&nearest.path)
                            .text("? Run ")
                            .cmd(&command(Some(&nearest.path), None))
                            .text("."),
                    )
                });
            }
        }
        Failure::Inexact {
            reference,
            candidates,
        } => {
            let listing = piece_table(candidates, reference, action, rows, style, zone);
            table_lines = listing.map(|lines| {
                let mut shown = vec![bold(
                    &format!("The piece that matches `{reference}`"),
                    style,
                )];
                shown.extend(lines);
                shown
            });
            block = ambiguous_piece(explained, candidates, action, table_lines.is_some());
        }
        Failure::Version {
            piece,
            input,
            stamps,
        } => {
            let versions: Vec<Row> = rows
                .unwrap_or_default()
                .iter()
                .filter(|row| row.piece.to_string() == *piece)
                .cloned()
                .collect();
            table_lines = (!versions.is_empty()).then(|| {
                let mut shown = vec![bold(
                    &format!(
                        "The versions of {}, the {} matches marked",
                        sanitize(piece),
                        stamps.len()
                    ),
                    style,
                )];
                shown.extend(version_table(&versions, input, stamps, style, zone, env));
                shown
            });
            let example = stamps.first().map(|stamp| command(None, Some(stamp)));
            block = ambiguous_version(explained, example, action);
        }
    }
    parts.push(Part::Block(block));
    if let Some(lines) = table_lines {
        parts.push(Part::Terminal(lines));
    }
    parts.push(Part::Block(Block::info(
        Rich::new().text(&format!("Nothing was {}.", action.done)),
    )));
    parts.extend(hint.map(Part::Block));
    parts
}

fn bold(text: &str, style: Style) -> String {
    painted(style, text, theme::BOLD, &[])
}

// =========================================================
// The error blocks
// =========================================================

fn ambiguous_piece(
    explained: &dyn Diagnostic,
    candidates: &[Candidate],
    action: Action,
    listed_below: bool,
) -> Block {
    let mut block = error_block::explained_block(explained);
    // Without a table the matches would not be shown at all on a terminal.
    if !listed_below {
        block = block
            .how(Para::Blank)
            .how(Rich::new().text("These pieces match:"));
        for candidate in candidates {
            block = block.how(Para::pre(&format!("  {}", candidate.path)));
        }
    }
    block.doing(&format!("find the piece to {}", action.verb))
}

fn unknown_piece(explained: &dyn Diagnostic, action: Action) -> Block {
    error_block::explained_block(explained).doing(&format!("find the piece to {}", action.verb))
}

/// The command to copy is built from the invocation, which the library
/// does not know, so it follows the plain help of the error.
fn ambiguous_version(explained: &dyn Diagnostic, example: Option<String>, action: Action) -> Block {
    let mut block = error_block::explained_block(explained);
    if let Some(example) = example {
        block = block.how(
            Rich::new()
                .text("Run ")
                .cmd(&example)
                .text(" to name the newest match."),
        );
    }
    block.doing(&format!("find the version to {}", action.verb))
}

// =========================================================
// The tables
// =========================================================

/// The table of `pieces`, the matched letters of `word` marked: the list
/// table with every version for a command on the store, a count of the
/// versions for a command on the live pieces. `None` without a listing.
fn piece_table(
    pieces: &[Candidate],
    word: &str,
    action: Action,
    rows: Option<&[Row]>,
    style: Style,
    zone: &TimeZone,
) -> Option<Vec<String>> {
    let rows = rows?;
    let mut query = Query::parse(word);
    let mut matched_of = |path: &str| {
        if word.is_empty() {
            Vec::new()
        } else {
            query.matched(path)
        }
    };
    let mut named: Vec<(String, Vec<usize>)> = pieces
        .iter()
        .map(|piece| (piece.path.clone(), Vec::new()))
        .collect();
    for (path, matched) in &mut named {
        *matched = matched_of(path);
    }
    let versions: Vec<Row> = rows
        .iter()
        .filter(|row| named.iter().any(|(path, _)| *path == row.piece.to_string()))
        .cloned()
        .collect();
    let every_piece_has_versions = named
        .iter()
        .all(|(path, _)| versions.iter().any(|row| row.piece.to_string() == *path));
    if action.source == Source::Store && every_piece_has_versions {
        let looks: Vec<RowLook> = versions
            .iter()
            .map(|row| RowLook {
                piece_matched: named
                    .iter()
                    .find(|(path, _)| *path == row.piece.to_string())
                    .map(|(_, matched)| matched.clone())
                    .unwrap_or_default(),
                ..RowLook::default()
            })
            .collect();
        let marking = Marking {
            looks: &looks,
            marker: None,
        };
        return Some(table(&versions, style, zone, Some(&marking)));
    }
    let counted: Vec<(String, usize, Vec<usize>)> = named
        .into_iter()
        .map(|(path, matched)| {
            let count = versions
                .iter()
                .filter(|row| row.piece.to_string() == path)
                .count();
            (path, count, matched)
        })
        .collect();
    Some(count_table(&counted, style))
}

/// The pieces with the number of their versions, `0` for a piece that
/// holds none.
fn count_table(pieces: &[(String, usize, Vec<usize>)], style: Style) -> Vec<String> {
    const GAP: &str = "   ";
    let header = "VERSIONS";
    let names: Vec<String> = pieces.iter().map(|(path, _, _)| sanitize(path)).collect();
    let longest = names
        .iter()
        .map(|name| name.width())
        .chain([5])
        .max()
        .expect("the header is a cell");
    let piece_width = if style.width >= WIDE_FROM {
        longest
    } else {
        longest.min(style.width.saturating_sub(header.len() + GAP.len()).max(8))
    };
    let mut lines = vec![painted(
        style,
        &format!("{}{GAP}{header}", pad("PIECE", piece_width)),
        theme::DIM,
        &[],
    )];
    for ((_, count, matched), name) in pieces.iter().zip(&names) {
        let shown = mid(name, piece_width, style.symbols().ellipsis);
        let matched = if shown == *name {
            matched.as_slice()
        } else {
            &[]
        };
        lines.push(format!(
            "{}{}{GAP}{}",
            painted(style, &shown, anstyle::Style::new(), matched),
            spaces(piece_width.saturating_sub(shown.width())),
            pad_left(&count.to_string(), header.len())
        ));
    }
    lines
}

/// All versions of the piece, the ones named by `stamps` marked and the
/// start of their time that `input` spelled in red.
fn version_table(
    versions: &[Row],
    input: &str,
    stamps: &[String],
    style: Style,
    zone: &TimeZone,
    env: &Env,
) -> Vec<String> {
    let labels = version_labels(versions, zone);
    // An input with a `T` is a prefix of the stamp, which the table does not
    // show; the marker alone points at the version then.
    let spelled = if input.contains('T') {
        0
    } else {
        input.chars().count()
    };
    let looks: Vec<RowLook> = versions
        .iter()
        .zip(&labels)
        .map(|(row, label)| {
            let chosen = stamps.contains(&row.stamp.to_string());
            RowLook {
                chosen,
                piece_matched: Vec::new(),
                label_matched: if chosen {
                    spelled.min(label.chars().count())
                } else {
                    0
                },
            }
        })
        .collect();
    let symbols = Symbols::for_env(env);
    let marking = Marking {
        looks: &looks,
        marker: Some(symbols.matched),
    };
    table(versions, style, zone, Some(&marking))
}

// =========================================================
// Commands to copy
// =========================================================

/// A word of a command line, in single quotes when a shell would split or
/// expand it.
pub(super) fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:@%+=,".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// `antiq` followed by `words`, quoted for a shell, with the `-C` of the
/// run when it had one. The command can be pasted from anywhere.
pub(super) fn pasteable_line(directory: Option<&std::path::Path>, words: &[String]) -> String {
    let mut line = vec!["antiq".to_string()];
    if let Some(directory) = directory {
        line.push("-C".to_string());
        line.push(shell_word(&directory.to_string_lossy()));
    }
    line.extend(words.iter().map(|word| shell_word(word)));
    line.join(" ")
}

/// The command line the user ran, with the piece and the version replaced
/// by the given ones where there are any.
fn command_line(cli: &Cli, piece: Option<&str>, version: Option<&str>) -> String {
    command_line_for(cli, piece, version, false)
}

/// The `forget --all-versions` the user ran, with `--force` added: the
/// command that confirms what the refusal asks for.
pub(super) fn forget_confirmation(cli: &Cli) -> String {
    command_line_for(cli, None, None, true)
}

/// `command_line`, optionally with `--force` on an `--all-versions` forget.
/// A `-C` the run was given stays, as the copied command has to find the
/// same collection from wherever it is pasted.
fn command_line_for(cli: &Cli, piece: Option<&str>, version: Option<&str>, force: bool) -> String {
    let own = |piece_arg: &std::ffi::OsStr| piece_arg.to_string_lossy().into_owned();
    let mut words: Vec<String> = vec!["antiq".to_string()];
    let mut push = |word: &str| words.push(shell_word(word));
    if let Some(directory) = &cli.directory {
        push("-C");
        push(&own(directory.as_os_str()));
    }
    match &cli.command {
        Command::Archive(args) => {
            push("archive");
            push(piece.unwrap_or(&own(args.piece.as_deref().unwrap_or_default())));
            if args.dry_run {
                push("-n");
            }
        }
        Command::Pin(args) | Command::Unpin(args) => {
            push(if matches!(cli.command, Command::Pin(_)) {
                "pin"
            } else {
                "unpin"
            });
            push(piece.unwrap_or(&own(&args.piece)));
            if args.dry_run {
                push("-n");
            }
        }
        Command::Unearth(args) => {
            push("unearth");
            push(piece.unwrap_or(&own(&args.piece)));
            if let Some(version) = version.or(args.version.as_deref()) {
                push("--version");
                push(version);
            }
            if args.dry_run {
                push("-n");
            }
        }
        Command::Fork(args) => {
            push("fork");
            push(piece.unwrap_or(&own(&args.piece)));
            if let Some(version) = version.or(args.version.as_deref()) {
                push("--version");
                push(version);
            }
            if let Some(name) = &args.name {
                push("--name");
                push(&own(name));
            }
            if args.dry_run {
                push("-n");
            }
        }
        Command::Forget(args) => {
            push("forget");
            push(piece.unwrap_or(&own(&args.piece)));
            if args.all_versions {
                push("--all-versions");
                if args.force || force {
                    push("--force");
                }
            } else if let Some(version) = version.or(args.version.as_deref()) {
                push(version);
            }
            if args.dry_run {
                push("-n");
            }
        }
        Command::Rebuild(args) => {
            push("rebuild");
            push(piece.unwrap_or(&own(&args.piece)));
            push(version.unwrap_or(&args.version));
            if args.dry_run {
                push("-n");
            }
        }
        _ => {}
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::console::render::render_parts;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        let mut line = vec!["antiq"];
        line.extend_from_slice(args);
        Cli::try_parse_from(line).expect("the arguments are valid")
    }

    fn candidates(paths: &[&str]) -> Vec<Candidate> {
        paths
            .iter()
            .map(|path| Candidate {
                path: path.to_string(),
            })
            .collect()
    }

    fn plain_env() -> Env {
        Env::new(true, false, true, 100)
    }

    fn no_command(_: Option<&str>, _: Option<&str>) -> String {
        String::new()
    }

    #[test]
    fn without_a_listing_the_matches_stand_in_the_text_of_the_error() {
        let pieces = candidates(&["projects/scanner", "tools/scan-old"]);
        let failure = Failure::Ambiguous {
            reference: "scan",
            candidates: &pieces,
        };
        let action = action(&cli(&["pin", "scan"]).command).expect("pin is an action");
        let error = ReferenceError::Ambiguous {
            reference: "scan".into(),
            candidates: pieces.clone(),
        };
        let parts = build(
            &failure,
            &error,
            action,
            None,
            &no_command,
            &plain_env(),
            &TimeZone::UTC,
        );
        assert_eq!(
            render_parts(&parts, &plain_env()),
            [
                " ✖ ERROR",
                "▌ No unique piece matches scan, as it fits the paths of 2 pieces",
                "▌",
                "▌ How to fix it",
                "▌   Name the piece by more of its path, for example projects/scanner.",
                "▌",
                "▌   These pieces match:",
                "▌     projects/scanner",
                "▌     tools/scan-old",
                "▌",
                "▌ What antiq was doing",
                "▌   Attempting to find the piece to pin.",
                "",
                " ℹ INFO  Nothing was pinned.",
            ]
        );
    }

    #[test]
    fn the_command_to_copy_swaps_the_piece_and_keeps_the_rest() {
        let swap = |args: &[&str], piece: Option<&str>, version: Option<&str>| {
            command_line(&cli(args), piece, version)
        };
        assert_eq!(
            swap(&["unearth", "scnaer", "-n"], Some("projects/scanner"), None),
            "antiq unearth projects/scanner -n"
        );
        assert_eq!(
            swap(
                &["unearth", "projects/scanner", "--version", "2025-06"],
                None,
                Some("2025-06-18T154509")
            ),
            "antiq unearth projects/scanner --version 2025-06-18T154509"
        );
        assert_eq!(
            swap(&["forget", "scan", "2025"], Some("tools/old"), None),
            "antiq forget tools/old 2025"
        );
        assert_eq!(
            swap(
                &["forget", "scan", "--all-versions", "--force"],
                Some("a"),
                None
            ),
            "antiq forget a --all-versions --force"
        );
        assert_eq!(
            swap(
                &["rebuild", "scan", "2025"],
                None,
                Some("2025-06-18T154509")
            ),
            "antiq rebuild scan 2025-06-18T154509"
        );
    }

    #[test]
    fn the_command_to_copy_covers_the_commands_on_live_pieces_and_fork() {
        let swap = |args: &[&str], piece: Option<&str>, version: Option<&str>| {
            command_line(&cli(args), piece, version)
        };
        assert_eq!(
            swap(&["archive", "scnaer", "-n"], Some("projects/scanner"), None),
            "antiq archive projects/scanner -n"
        );
        assert_eq!(
            swap(&["pin", "scnaer"], Some("projects/scanner"), None),
            "antiq pin projects/scanner"
        );
        assert_eq!(
            swap(&["unpin", "scnaer", "-n"], Some("projects/scanner"), None),
            "antiq unpin projects/scanner -n"
        );
        assert_eq!(
            swap(
                &[
                    "fork",
                    "scan",
                    "--version",
                    "2025",
                    "--name",
                    "my copy",
                    "-n"
                ],
                Some("projects/scanner"),
                Some("2025-06-18T154509")
            ),
            "antiq fork projects/scanner --version 2025-06-18T154509 --name 'my copy' -n"
        );
        assert_eq!(
            swap(&["rebuild", "scan", "2025", "-n"], None, None),
            "antiq rebuild scan 2025 -n"
        );
    }

    #[test]
    fn without_a_listing_the_stamps_of_an_ambiguous_version_stand_in_the_error() {
        let stamps: Vec<String> = (10..21)
            .map(|day| format!("2025-06-{day}T101500"))
            .collect();
        let failure = Failure::Version {
            piece: "projects/scanner",
            input: "2025-06",
            stamps: &stamps,
        };
        let action = action(&cli(&["fork", "scanner"]).command).expect("fork is an action");
        let error = VersionLookupError::Ambiguous {
            piece: "projects/scanner".into(),
            input: "2025-06".into(),
            stamps: stamps.clone(),
        };
        let parts = build(
            &failure,
            &error,
            action,
            None,
            &no_command,
            &plain_env(),
            &TimeZone::UTC,
        );
        let lines = render_parts(&parts, &plain_env());
        assert_eq!(lines[0], " ✖ ERROR");
        assert!(
            lines[1].starts_with(
                "▌ 2025-06 names no single version of projects/scanner, as 11 versions match it:"
            ),
            "{lines:?}"
        );
        assert!(
            lines.contains(
                &"▌   Name one version by more of its stamp, for example 2025-06-10T101500."
                    .to_string()
            ),
            "{lines:?}"
        );
        assert_eq!(lines[lines.len() - 1], " ℹ INFO  Nothing was forked.");
    }

    #[test]
    fn a_word_a_shell_would_split_is_quoted() {
        assert_eq!(shell_word("projects/scanner"), "projects/scanner");
        assert_eq!(shell_word("my docs"), "'my docs'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
        assert_eq!(shell_word(""), "''");
    }

    #[test]
    fn the_count_table_cuts_a_long_name_on_a_narrow_terminal() {
        let long = format!("projects/{}", "x".repeat(60));
        let pieces = vec![(long, 3, Vec::new())];
        let narrow = Style {
            width: 40,
            color: false,
            terminal: true,
            utf8: true,
        };
        let lines = count_table(&pieces, narrow);
        assert_eq!(lines[0], format!("PIECE{}VERSIONS", " ".repeat(27)));
        assert_eq!(lines[1].width(), 40);
        assert!(lines[1].contains('…'), "{:?}", lines[1]);
        assert!(lines[1].ends_with("       3"), "{:?}", lines[1]);
    }

    #[test]
    fn the_header_of_the_count_table_is_dim() {
        let coloured = Style {
            width: 80,
            color: true,
            terminal: true,
            utf8: true,
        };
        let lines = count_table(&[("projects/scanner".into(), 2, Vec::new())], coloured);
        assert_eq!(lines[0], "\x1b[2mPIECE              VERSIONS\x1b[0m");
        assert_eq!(lines[1], "projects/scanner          2");
    }

    fn row(piece: &str, stamp: &str) -> Row {
        let stamp: antiq::versions::stamp::Stamp = stamp.parse().expect("a stamp");
        Row {
            piece: antiq::relpath::RelPath::parse(piece).expect("a piece"),
            stamp,
            version: stamp.time(),
            state: antiq::commands::list::State::Ok,
            archive_bytes: Some(10),
            bytes: Some(20),
            files: Some(1),
            damage: Vec::new(),
        }
    }

    #[test]
    fn a_store_piece_without_a_version_turns_the_table_into_a_count() {
        // `forget` also names folders that hold sidecars only; they have no
        // row, and a list table would leave them out of the two it announces.
        let pieces = candidates(&["projects/gone", "projects/scanner"]);
        let rows = [row("projects/scanner", "2026-11-04T090211")];
        let action = action(&cli(&["forget", "p", "v"]).command).expect("forget is an action");
        let style = Style {
            width: 80,
            color: false,
            terminal: true,
            utf8: true,
        };
        let lines = piece_table(
            &pieces,
            "projects",
            action,
            Some(&rows),
            style,
            &TimeZone::UTC,
        )
        .expect("there is a listing");
        assert_eq!(
            lines,
            [
                "PIECE              VERSIONS",
                "projects/gone             0",
                "projects/scanner          1",
            ]
        );
    }

    #[test]
    fn every_command_that_takes_a_piece_names_its_verb_and_where_it_looks() {
        let words = |args: &[&str]| {
            let action = action(&cli(args).command).expect("an action");
            (action.verb, action.done, action.source)
        };
        assert_eq!(
            words(&["archive", "p"]),
            ("archive", "archived", Source::Live)
        );
        assert_eq!(words(&["unpin", "p"]), ("unpin", "unpinned", Source::Live));
        assert_eq!(
            words(&["rebuild", "p", "v"]),
            ("rebuild", "rebuilt", Source::Store)
        );
        assert!(action(&cli(&["list"]).command).is_none());
    }

    #[test]
    fn the_command_to_copy_keeps_the_dry_run_of_forget_and_stays_bare_elsewhere() {
        assert_eq!(
            command_line(&cli(&["forget", "scan", "2025", "-n"]), Some("a"), None),
            "antiq forget a 2025 -n"
        );
        assert_eq!(command_line(&cli(&["list"]), None, None), "antiq");
    }

    #[test]
    fn the_command_to_copy_keeps_the_directory_of_the_run_quoted() {
        assert_eq!(
            command_line(
                &cli(&["-C", "/tmp/my collection", "pin", "scan"]),
                Some("projects/scanner"),
                None
            ),
            "antiq -C '/tmp/my collection' pin projects/scanner"
        );
        assert_eq!(
            command_line(&cli(&["-C", "/data", "unpin", "scan"]), None, None),
            "antiq -C /data unpin scan"
        );
    }

    #[test]
    fn the_confirmation_of_forget_all_versions_quotes_the_piece_and_keeps_the_directory() {
        assert_eq!(
            forget_confirmation(&cli(&[
                "-C",
                "/data",
                "forget",
                "my piece",
                "--all-versions"
            ])),
            "antiq -C /data forget 'my piece' --all-versions --force"
        );
    }

    #[test]
    fn the_count_table_keeps_a_long_name_whole_on_a_wide_terminal() {
        let long = format!("projects/{}", "x".repeat(60));
        let wide = Style {
            width: WIDE_FROM,
            color: false,
            terminal: true,
            utf8: true,
        };
        let lines = count_table(&[(long.clone(), 3, Vec::new())], wide);
        assert!(lines[1].starts_with(&long), "{:?}", lines[1]);
    }

    #[test]
    fn a_version_spelled_with_a_t_is_marked_but_not_highlighted() {
        let versions = [row("projects/scanner", "2026-11-04T090211")];
        let style = Style {
            width: 100,
            color: true,
            terminal: true,
            utf8: true,
        };
        let env = Env::new(true, true, true, 100);
        let lines = version_table(
            &versions,
            "2026-11-04T09",
            &["2026-11-04T090211".to_string()],
            style,
            &TimeZone::UTC,
            &env,
        );
        // The only red is the marker.
        assert!(lines[1].contains('▸'), "{lines:?}");
        assert_eq!(lines[1].matches("\x1b[31m").count(), 1, "{lines:?}");
    }
}
