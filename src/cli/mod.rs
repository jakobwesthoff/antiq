// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command line: the arguments of every command, the dispatch to the
//! command modules, and how a failed run is reported.

pub mod cmd;
pub mod console;
mod error_block;
pub mod notice;
pub mod output;
pub mod table;
mod unresolved;
mod usage;

use self::console::block::{Block, Para};
use self::console::rich::Rich;
use self::output::{ErrorReport, Output};
use antiq::commands::forget::{self, Which};
use antiq::error::ErrorKind;
use anyhow::Context as _;
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::Map;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Your idle folders, kept like antiques.
#[derive(Debug, Parser)]
#[command(version, about, disable_help_subcommand = true)]
pub struct Cli {
    /// Run as if started in this directory
    #[arg(short = 'C', value_name = "PATH", global = true)]
    pub directory: Option<PathBuf>,

    /// Print the result as one JSON object and never ask a question
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Make a directory a collection
    Init(InitArgs),
    /// Show the pieces that have a state
    Status,
    /// Archive due pieces, or one piece now
    Archive(ArchiveArgs),
    /// Bring a version back and remove it
    Unearth(UnearthArgs),
    /// Extract a working copy of a version
    Fork(ForkArgs),
    /// Delete a version, or all of them
    Forget(ForgetArgs),
    /// Repair a damaged version
    Rebuild(RebuildArgs),
    /// Pack a folder as a version
    Import(ImportArgs),
    /// List the versions
    List(ListArgs),
    /// Search the paths of all archived files
    Grep(GrepArgs),
    /// Keep a piece from being archived
    Pin(PieceArgs),
    /// Let a piece be archived again
    Unpin(PieceArgs),
    /// Mirror the collection to the target
    Push(PushArgs),
}

impl Command {
    /// The name a JSON envelope carries.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Status => "status",
            Command::Archive(_) => "archive",
            Command::Unearth(_) => "unearth",
            Command::Fork(_) => "fork",
            Command::Forget(_) => "forget",
            Command::Rebuild(_) => "rebuild",
            Command::Import(_) => "import",
            Command::List(_) => "list",
            Command::Grep(_) => "grep",
            Command::Pin(_) => "pin",
            Command::Unpin(_) => "unpin",
            Command::Push(_) => "push",
        }
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Glob pattern that selects pieces; repeat for several
    #[arg(long = "pattern", value_name = "GLOB")]
    pub patterns: Vec<String>,
    /// Print the config that would be written and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ArchiveArgs {
    /// Path or search word of the piece to archive now
    pub piece: Option<OsString>,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct UnearthArgs {
    /// Path or search word of the piece
    pub piece: OsString,
    /// Version to bring back
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ForkArgs {
    /// Path or search word of the piece
    pub piece: OsString,
    /// Version to extract
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// Path of the working copy in the collection
    #[arg(long, value_name = "NEW PATH")]
    pub name: Option<OsString>,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// Path or search word of the piece
    pub piece: OsString,
    /// Version to delete
    #[arg(
        required_unless_present = "all_versions",
        conflicts_with = "all_versions"
    )]
    pub version: Option<String>,
    /// Delete every version of the piece
    #[arg(long)]
    pub all_versions: bool,
    /// Confirm `--all-versions`
    #[arg(long, requires = "all_versions", conflicts_with = "version")]
    pub force: bool,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RebuildArgs {
    /// Path or search word of the piece
    pub piece: OsString,
    /// Version to repair
    pub version: String,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Folder to pack
    pub folder: PathBuf,
    /// Path of the piece in the collection
    #[arg(long = "as", value_name = "PATH")]
    pub as_path: Option<PathBuf>,
    /// Time to give the version
    #[arg(long, value_name = "TIME")]
    pub time: Option<String>,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Fuzzy pattern that selects pieces
    pub query: Option<String>,
}

#[derive(Debug, Args)]
pub struct GrepArgs {
    /// Regular expression matched against archived paths
    pub pattern: String,
}

#[derive(Debug, Args)]
pub struct PieceArgs {
    /// Path or search word of the piece
    pub piece: OsString,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Write the target's marker on an empty directory (over ssh, also create its last missing level)
    #[arg(long, conflicts_with = "if_reachable")]
    pub init: bool,
    /// Push although the target's marker names another host or path
    #[arg(long, conflicts_with = "if_reachable")]
    pub force: bool,
    /// Exit 0 when the target cannot be reached
    #[arg(long)]
    pub if_reachable: bool,
    /// Print the plan and change nothing
    #[arg(short = 'n', long)]
    pub dry_run: bool,
}

/// What every command gets to know about the invocation.
#[derive(Debug)]
pub struct Context {
    pub json: bool,
    /// The directory the command runs in: the one `-C` names, else the
    /// current directory. It is absolute and free of links and `..`, which
    /// is the form the piece references expect.
    pub start: PathBuf,
    /// The `-C` the run was given, as written. A command line copied from
    /// the output has to carry it to find the same collection.
    pub directory: Option<PathBuf>,
}

/// Runs the command line `args` (starting with the program name), writes
/// its output through `out` and returns the exit code.
pub fn run(args: Vec<OsString>, out: &mut Output) -> u8 {
    match Cli::try_parse_from(&args) {
        Ok(cli) => {
            let result = dispatch(&cli, out);
            finish(&cli, result, out)
        }
        Err(error) => report_usage_error(&error, &args, out),
    }
}

/// The directory the run starts in: the one `-C` names, else the current
/// directory, resolved.
fn start_directory(cli: &Cli) -> anyhow::Result<PathBuf> {
    let given = cli.directory.as_deref().unwrap_or(Path::new("."));
    std::fs::canonicalize(given)
        .with_context(|| format!("resolve the directory {} to start from", given.display()))
}

fn dispatch(cli: &Cli, out: &mut Output) -> anyhow::Result<u8> {
    // The refusal needs nothing of the collection, so it comes before the
    // search for one: outside a collection it is still the error to fix.
    if let Command::Forget(args) = &cli.command {
        let which = match &args.version {
            Some(version) => Which::One(version),
            None => Which::All,
        };
        forget::require_force(&args.piece, which, args.force)
            .map_err(antiq::Error::from)
            .with_context(|| format!("forget all versions of {}", args.piece.to_string_lossy()))?;
    }
    let start = start_directory(cli)?;
    let ctx = Context {
        json: cli.json,
        start,
        directory: cli.directory.clone(),
    };
    match &cli.command {
        Command::Init(args) => cmd::init::run(&ctx, args, out),
        Command::Status => cmd::status::run(&ctx, out),
        Command::Archive(args) => cmd::archive::run(&ctx, args, out),
        Command::Unearth(args) => cmd::unearth::run(&ctx, args, out),
        Command::Fork(args) => cmd::fork::run(&ctx, args, out),
        Command::Forget(args) => cmd::forget::run(&ctx, args, out),
        Command::Rebuild(args) => cmd::rebuild::run(&ctx, args, out),
        Command::Import(args) => cmd::import::run(&ctx, args, out),
        Command::List(args) => cmd::list::run(&ctx, args, out),
        Command::Grep(args) => cmd::grep::run(&ctx, args, out),
        Command::Pin(args) => cmd::pin::pin(&ctx, args, out),
        Command::Unpin(args) => cmd::pin::unpin(&ctx, args, out),
        Command::Push(args) => cmd::push::run(&ctx, args, out),
    }
}

// =========================================================
// Reporting a failed run
// =========================================================

/// The error of a run in the three parts a report shows.
struct Description<'a> {
    /// The library error in the chain, if there is one.
    library_error: Option<&'a antiq::Error>,
    /// What went wrong: the library error and the errors below it, or the
    /// innermost error when no library error is in the chain.
    root: String,
    /// What the run was doing, innermost first.
    contexts: Vec<String>,
}

fn describe(error: &anyhow::Error) -> Description<'_> {
    let chain: Vec<&(dyn std::error::Error + 'static)> = error.chain().collect();
    let library_error = chain.iter().position(|link| link.is::<antiq::Error>());
    let (root, outer) = match library_error {
        Some(index) => {
            let root = antiq::error::join_distinct(chain[index..].iter().map(ToString::to_string));
            (root, &chain[..index])
        }
        None => {
            let (innermost, outer) = chain
                .split_last()
                .expect("an error chain holds at least the error itself");
            (innermost.to_string(), outer)
        }
    };
    Description {
        library_error: library_error.and_then(|index| chain[index].downcast_ref()),
        root,
        contexts: outer.iter().rev().map(ToString::to_string).collect(),
    }
}

/// Reports the failure of a command and returns the exit code.
fn finish(cli: &Cli, result: anyhow::Result<u8>, out: &mut Output) -> u8 {
    let error = match result {
        Ok(code) => return code,
        Err(error) => error,
    };
    let description = describe(&error);
    let (kind, details) = match description.library_error {
        Some(library_error) => (library_error.kind(), library_error.details()),
        None => (ErrorKind::Other, Map::new()),
    };
    // When the output itself fails there is nowhere left to report that.
    let _ = if cli.json {
        let explained = description
            .library_error
            .and_then(|error| antiq::error::explanation(error));
        out.json_error(
            Some(cli.command.name()),
            &ErrorReport {
                kind,
                code: explained
                    .and_then(|link| link.code())
                    .map(|code| code.to_string()),
                message: &description.root,
                fix: explained
                    .and_then(|link| link.help())
                    .map(|help| help.to_string()),
                details: &details,
            },
        )
    } else if let Some(parts) = out
        .stderr_env()
        .rich
        .then(|| unresolved::group(cli, &description, &out.stderr_env()))
        .flatten()
    {
        out.group(&parts)
    } else {
        out.blocks(&plain_report(cli, &description, kind))
    };
    kind.exit_code()
}

/// The blocks of an error that has no table: the error block, and the info
/// that nothing was done when a command that changes things refused its
/// command line.
///
/// Only the CLI knows the invocation, so the command that fixes a refused
/// `forget --all-versions` is added here to the help the library gives.
fn plain_report(cli: &Cli, description: &Description, kind: ErrorKind) -> Vec<Block> {
    let mut block = error_block::block_for(description);
    let refused_for_force = description
        .library_error
        .and_then(error_block::find::<forget::ForgetError>)
        .is_some_and(|error| matches!(error, forget::ForgetError::AllVersionsNeedForce { .. }));
    if refused_for_force {
        block = block.how(Para::Blank).how(
            Rich::new()
                .text("Run ")
                .cmd(&unresolved::forget_confirmation(cli))
                .text(" to confirm."),
        );
    }
    let mut blocks = vec![block];
    if kind == ErrorKind::Usage
        && let Some(action) = unresolved::action(&cli.command)
    {
        blocks.push(Block::info(
            Rich::new().text(&format!("Nothing was {}.", action.done)),
        ));
    }
    blocks
}

/// Reports an error of clap. Help and version are not errors: clap prints
/// them and they exit 0, also under `--json`. Every other error is a usage
/// error, which `--json` turns into an envelope on stdout. A bare `antiq`
/// exits 2 as the usage error it is, but what it prints is the help, which
/// asks for no error block.
fn report_usage_error(error: &clap::Error, args: &[OsString], out: &mut Output) -> u8 {
    let code = u8::try_from(error.exit_code()).unwrap_or(ErrorKind::Usage.exit_code());
    let shows_help =
        error.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand;
    if code == 0 || (shows_help && !asks_for_json(args)) {
        print_clap(error, out);
        return code;
    }
    if !asks_for_json(args) {
        if out.stderr_env().rich {
            let _ = out.blocks(&[usage::block_for(error, args)]);
        } else {
            print_clap(error, out);
        }
        return code;
    }
    let message = first_paragraph(&error.render().to_string());
    let command = guess_command(args);
    let _ = out.json_error(
        command.as_deref(),
        &ErrorReport {
            kind: ErrorKind::Usage,
            code: None,
            message: &message,
            fix: None,
            details: &Map::new(),
        },
    );
    code
}

/// Prints what clap rendered: help and version to stdout, an error to
/// stderr. A terminal gets clap's own styling, which follows `NO_COLOR`
/// and the terminal. A stream that is not a terminal gets the text without
/// any escape sequence, whatever `CLICOLOR_FORCE` says.
fn print_clap(error: &clap::Error, out: &mut Output) {
    let to_stderr = error.use_stderr();
    let rich = if to_stderr {
        out.stderr_env().rich
    } else {
        out.stdout_env().rich
    };
    // A closed terminal leaves nowhere to print to.
    if rich {
        let _ = error.print();
        return;
    }
    for line in error.render().to_string().lines() {
        let _ = if to_stderr {
            out.note(line)
        } else {
            out.line(line)
        };
    }
}

/// Whether `--json` is among the arguments. Arguments cannot be trusted
/// after a parse error, so this looks at the raw list.
fn asks_for_json(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json")
}

/// The lines of a rendered clap error up to the first blank line, without
/// the `error: ` prefix, joined into one line. What follows is the usage
/// text, which belongs on a console only.
fn first_paragraph(rendered: &str) -> String {
    let joined = rendered
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    joined
        .strip_prefix("error: ")
        .unwrap_or(&joined)
        .to_string()
}

/// `args` as the command line of a test.
#[cfg(test)]
fn os_args(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The command a failed command line was meant for, as far as the
/// arguments tell: the first positional argument, when it names a command.
fn guess_command(args: &[OsString]) -> Option<String> {
    let name = first_positional(args)?;
    Cli::command().find_subcommand(&name)?;
    Some(name)
}

/// The first argument that is neither a flag nor the value of `-C`. Only
/// global options can come before a command.
fn first_positional(args: &[OsString]) -> Option<String> {
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].to_str()?;
        match arg {
            "--" => return None,
            "-C" => index += 2,
            _ if arg.starts_with('-') => index += 1,
            _ => return Some(arg.to_string()),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::output::testing::{Capture, FailsOnFlush};
    use super::*;
    use antiq::lock::LockError;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Runs `antiq <args>` and returns exit code, stdout and stderr.
    fn run_captured(args: &[&str]) -> (u8, String, String) {
        let capture = Capture::default();
        let mut out = capture.output();
        let mut line = vec!["antiq"];
        line.extend_from_slice(args);
        let code = run(os(&line), &mut out);
        (code, capture.stdout(), capture.stderr())
    }

    fn parse(args: &[&str]) -> Cli {
        let mut line = vec!["antiq"];
        line.extend_from_slice(args);
        Cli::try_parse_from(line).expect("the arguments are valid")
    }

    fn finish_captured(cli: &Cli, result: anyhow::Result<u8>) -> (u8, String, String) {
        let capture = Capture::default();
        let code = finish(cli, result, &mut capture.output());
        (code, capture.stdout(), capture.stderr())
    }

    #[test]
    fn a_plain_run_reports_on_stderr() {
        let root = tempfile::tempdir().expect("a temp dir");
        let canonical = std::fs::canonicalize(root.path()).expect("the temp dir exists");
        let (code, stdout, stderr) =
            run_captured(&["-C", root.path().to_str().expect("UTF-8"), "status"]);
        assert_eq!(code, 1);
        assert_eq!(stdout, "");
        assert_eq!(
            stderr,
            format!(
                "error: `{}` is not inside an antiq collection, which has an `.antiq/` folder and none of its parent directories has one\n  \
                 fix: Change into your collection, or create one here with antiq init.\n  \
                 while attempting to find the antiq collection\n",
                canonical.display()
            )
        );
    }

    #[test]
    fn a_json_usage_error_is_enveloped() {
        let (code, stdout, stderr) = run_captured(&["--json", "status", "--dry-run"]);
        assert_eq!(code, 2);
        assert_eq!(
            stdout,
            "{\"format\":1,\"command\":\"status\",\"error\":\
             {\"kind\":\"usage\",\"message\":\"unexpected argument '--dry-run' found\"}}\n"
        );
        assert_eq!(stderr, "");
    }

    #[test]
    fn a_successful_command_keeps_its_exit_code() {
        let cli = parse(&["status"]);
        assert_eq!(
            finish_captured(&cli, Ok(0)),
            (0, String::new(), String::new())
        );
        assert_eq!(
            finish_captured(&cli, Ok(3)),
            (3, String::new(), String::new())
        );
    }

    #[test]
    fn the_json_error_carries_the_details_of_the_library_error() {
        let cli = parse(&["--json", "archive"]);
        let held: anyhow::Error = antiq::Error::from(LockError::Held {
            pid: 4242,
            started: "2026-11-03T10:00:00Z".parse().expect("valid literal"),
        })
        .into();
        let (code, stdout, _) = finish_captured(&cli, Err(held));
        assert_eq!(code, 1);
        assert!(
            stdout.ends_with(
                "\"error\":{\"kind\":\"lock_held\",\"code\":\"antiq::lock::held\",\
                 \"message\":\"another antiq run holds the lock (pid 4242, started 2026-11-03T10:00:00Z), and runs that change an antiq collection take turns\",\
                 \"fix\":\"Wait until that run ends, then run the command again.\",\
                 \"pid\":4242,\"started\":\"2026-11-03T10:00:00Z\"}}\n"
            ),
            "{stdout}"
        );
    }

    #[test]
    fn the_json_error_of_a_wrapped_error_carries_the_code_of_its_explanation() {
        let cli = parse(&["--json", "push"]);
        let wrapped: anyhow::Error = antiq::Error::from(antiq::commands::push::PushError::Run(
            antiq::run::RunError::Lock(LockError::HeldUnknown),
        ))
        .into();
        let (_, stdout, _) = finish_captured(&cli, Err(wrapped));
        assert!(
            stdout.contains(
                "\"kind\":\"lock_held\",\"code\":\"antiq::lock::held_unknown\",\"message\""
            ),
            "{stdout}"
        );
    }

    #[test]
    fn a_json_error_without_a_code_leaves_the_key_out() {
        let cli = parse(&["--json", "status"]);
        let error: anyhow::Error = antiq::Error::Usage("bad".into()).into();
        let (_, stdout, _) = finish_captured(&cli, Err(error));
        assert!(
            stdout.ends_with("\"error\":{\"kind\":\"usage\",\"message\":\"bad\"}}\n"),
            "{stdout}"
        );
    }

    /// An unearth-shaped chain: every layer's text embeds the one below.
    fn ambiguous_unearth() -> anyhow::Error {
        let reference = antiq::reference::ReferenceError::Ambiguous {
            reference: "scan".into(),
            candidates: vec![
                antiq::reference::Candidate {
                    path: "projects/scanner".into(),
                },
                antiq::reference::Candidate {
                    path: "tools/scan-old".into(),
                },
            ],
        };
        let restore = antiq::commands::restore::RestoreError::from(reference);
        let unearth = antiq::commands::unearth::UnearthError::from(restore);
        anyhow::Error::new(antiq::Error::from(unearth)).context("unearth the version")
    }

    #[test]
    fn a_plain_error_says_each_cause_once_and_lists_the_candidates() {
        let cli = parse(&["unearth", "scan"]);
        let (code, _, stderr) = finish_captured(&cli, Err(ambiguous_unearth()));
        assert_eq!(code, 1);
        assert_eq!(
            stderr,
            "error: no unique piece matches `scan`, as it fits the paths of 2 pieces\n  \
             fix: Name the piece by more of its path, for example projects/scanner.\n    \
             candidate: projects/scanner\n    \
             candidate: tools/scan-old\n  \
             while attempting to unearth the version\n"
        );
    }

    #[test]
    fn a_json_error_says_each_cause_once_and_keeps_its_candidates() {
        let cli = parse(&["--json", "unearth", "scan"]);
        let (code, stdout, stderr) = finish_captured(&cli, Err(ambiguous_unearth()));
        assert_eq!(code, 1);
        assert_eq!(stderr, "");
        assert!(
            stdout.ends_with(
                "\"error\":{\"kind\":\"ambiguous_reference\",\
                 \"code\":\"antiq::reference::ambiguous\",\
                 \"message\":\"cannot find the piece: no unique piece matches `scan`, as it fits the paths of 2 pieces\",\
                 \"fix\":\"Name the piece by more of its path, for example `projects/scanner`.\",\
                 \"candidates\":[{\"path\":\"projects/scanner\"},{\"path\":\"tools/scan-old\"}]}}\n"
            ),
            "{stdout}"
        );
    }

    #[test]
    fn a_plain_error_names_the_contexts_innermost_first() {
        let cli = parse(&["status"]);
        let error = Err::<(), _>(antiq::Error::Usage("bad input".into()))
            .context("read the config file")
            .context("start the run")
            .expect_err("the result is an error");
        let (code, _, stderr) = finish_captured(&cli, Err(error));
        assert_eq!(code, 2);
        assert_eq!(
            stderr,
            "error: bad input\n  \
             while attempting to read the config file\n  \
             while attempting to start the run\n"
        );
    }

    #[test]
    fn without_a_library_error_the_innermost_error_is_the_root() {
        #[derive(Debug, thiserror::Error)]
        #[error("cannot read {path}")]
        struct Wrapper {
            path: String,
            #[source]
            source: std::io::Error,
        }
        let error = anyhow::Error::new(Wrapper {
            path: "config.toml".into(),
            source: std::io::ErrorKind::PermissionDenied.into(),
        });
        let description = describe(&error);
        assert!(description.library_error.is_none());
        assert_eq!(description.root, "permission denied");
        assert_eq!(description.contexts, ["cannot read config.toml"]);
    }

    #[test]
    fn an_error_that_is_not_a_library_error_has_the_kind_other() {
        let cli = parse(&["--json", "status"]);
        let error = anyhow::Error::new(std::io::Error::other("disk on fire"));
        let (code, stdout, _) = finish_captured(&cli, Err(error));
        assert_eq!(code, 1);
        assert!(
            stdout.ends_with("\"error\":{\"kind\":\"other\",\"message\":\"disk on fire\"}}\n"),
            "{stdout}"
        );
    }

    #[test]
    fn a_broken_pipe_inside_an_error_is_an_ordinary_failure() {
        let cli = parse(&["status"]);
        let error = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert_eq!(finish_captured(&cli, Err(error)).0, 1);
    }

    #[test]
    fn a_closed_stdout_keeps_the_exit_code() {
        let (pipe, flushes) = FailsOnFlush::new(std::io::ErrorKind::BrokenPipe);
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()));
        let code = run(os(&["antiq", "--json", "status"]), &mut out);
        assert_eq!(code, 1);
        assert_eq!(*flushes.lock().expect("no test panics while counting"), 1);
    }

    #[test]
    fn json_is_found_in_the_arguments_up_to_a_double_dash() {
        assert!(asks_for_json(&os(&["antiq", "--json", "status"])));
        assert!(asks_for_json(&os(&["antiq", "status", "--json"])));
        assert!(!asks_for_json(&os(&["antiq", "status"])));
        assert!(!asks_for_json(&os(&["antiq", "status", "--", "--json"])));
    }

    #[test]
    fn the_message_of_a_usage_error_is_its_first_paragraph() {
        let rendered = "error: the following required arguments were not provided:\n  \
                        <PIECE>\n\nUsage: antiq pin <PIECE>\n\nFor more information, try '--help'.\n";
        assert_eq!(
            first_paragraph(rendered),
            "the following required arguments were not provided: <PIECE>"
        );
    }

    #[test]
    fn the_command_is_guessed_from_the_first_positional_argument() {
        let guess = |args: &[&str]| guess_command(&os(args));
        assert_eq!(
            guess(&["antiq", "--json", "status", "x"]).as_deref(),
            Some("status")
        );
        assert_eq!(guess(&["antiq", "-C", "status", "bogus"]), None);
        assert_eq!(guess(&["antiq", "-Cstatus", "--json", "bogus"]), None);
        assert_eq!(
            guess(&["antiq", "-C=x", "--json", "pin", "p"]).as_deref(),
            Some("pin")
        );
        assert_eq!(
            guess(&["antiq", "--json", "-C", "x", "status", "--dry-run"]).as_deref(),
            Some("status")
        );
        assert_eq!(guess(&["antiq", "--", "status"]), None);
        assert_eq!(guess(&["antiq"]), None);
    }

    #[test]
    fn dry_run_has_a_long_and_a_short_spelling() {
        for flag in ["-n", "--dry-run"] {
            let init = parse(&["init", flag]);
            assert!(
                matches!(&init.command, Command::Init(args) if args.dry_run),
                "{flag}"
            );
            let archive = parse(&["archive", flag]);
            assert!(
                matches!(&archive.command, Command::Archive(args) if args.dry_run),
                "{flag}"
            );
            let push = parse(&["push", flag]);
            assert!(
                matches!(&push.command, Command::Push(args) if args.dry_run),
                "{flag}"
            );
        }
    }

    #[test]
    fn init_takes_patterns_and_dry_run() {
        let cli = parse(&["init", "--pattern", "a*", "--pattern", "b*", "-n"]);
        let Command::Init(args) = cli.command else {
            panic!("the command is init");
        };
        assert_eq!(args.patterns, ["a*", "b*"]);
        assert!(args.dry_run);
    }

    #[test]
    fn the_arguments_of_every_command_reach_its_struct() {
        let cli = parse(&["archive", "projects/scanner"]);
        assert!(matches!(&cli.command, Command::Archive(args)
            if args.piece.as_deref() == Some(std::ffi::OsStr::new("projects/scanner"))));
        let cli = parse(&["unearth", "p", "--version", "v1"]);
        assert!(matches!(&cli.command, Command::Unearth(args)
            if args.version.as_deref() == Some("v1")));
        let cli = parse(&["fork", "p", "--version", "v1", "--name", "copy"]);
        assert!(matches!(&cli.command, Command::Fork(args)
            if args.version.as_deref() == Some("v1")
                && args.name.as_deref() == Some(std::ffi::OsStr::new("copy"))));
        let cli = parse(&["forget", "p", "v1"]);
        assert!(matches!(&cli.command, Command::Forget(args)
            if args.version.as_deref() == Some("v1") && !args.all_versions));
        let cli = parse(&["forget", "p", "--all-versions", "--force"]);
        assert!(matches!(&cli.command, Command::Forget(args)
            if args.version.is_none() && args.all_versions && args.force));
        let cli = parse(&["rebuild", "p", "v1"]);
        assert!(matches!(&cli.command, Command::Rebuild(args) if args.version == "v1"));
        let cli = parse(&["import", "dir", "--as", "proj", "--time", "2020-01-01"]);
        assert!(matches!(&cli.command, Command::Import(args)
            if args.folder == std::path::Path::new("dir")
                && args.as_path.as_deref() == Some(std::path::Path::new("proj"))
                && args.time.as_deref() == Some("2020-01-01")));
        let cli = parse(&["list", "scan"]);
        assert!(
            matches!(&cli.command, Command::List(args) if args.query.as_deref() == Some("scan"))
        );
        let cli = parse(&["grep", "main.rs"]);
        assert!(matches!(&cli.command, Command::Grep(args) if args.pattern == "main.rs"));
        let cli = parse(&["push", "--init", "--force"]);
        assert!(
            matches!(&cli.command, Command::Push(args) if args.init && args.force && !args.if_reachable)
        );
        let cli = parse(&["push", "--if-reachable"]);
        assert!(matches!(&cli.command, Command::Push(args) if args.if_reachable));
    }

    #[cfg(unix)]
    #[test]
    fn a_piece_that_is_not_utf8_parses() {
        use std::os::unix::ffi::OsStringExt;
        let piece = OsString::from_vec(b"p\xff".to_vec());
        let cli = Cli::try_parse_from([
            OsString::from("antiq"),
            OsString::from("pin"),
            piece.clone(),
        ])
        .expect("a piece reference is not required to be UTF-8");
        assert!(matches!(&cli.command, Command::Pin(args) if args.piece == piece));
    }
}
