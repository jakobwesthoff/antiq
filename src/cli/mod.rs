// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command line: the arguments of every command, the dispatch to the
//! command modules, and how a failed run is reported.

pub mod cmd;
pub mod output;
pub mod tty;

use self::output::Output;
use self::tty::TtyInteraction;
use antiq::error::ErrorKind;
use antiq::interact::{Interaction, NonInteractive};
use antiq::remote::policy::{self, Outcome};
use clap::{ArgGroup, Args, CommandFactory, Parser, Subcommand};
use serde_json::Map;
use std::ffi::OsString;
use std::path::PathBuf;

/// Archive what you no longer touch, find it when you need it.
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
    /// Show the pieces by state
    Status,
    /// Archive idle pieces and remove their folders
    Archive(DryRunArgs),
    /// Bring an archived piece back
    Unearth(UnearthArgs),
    /// List and search all pieces, and open the picker
    Find(FindArgs),
    /// Give an unearthed piece a new ID
    Fork(ForkArgs),
    /// Keep a piece from being archived
    Pin(PieceArgs),
    /// Let a piece be archived again
    Unpin(PieceArgs),
    /// Exchange backups and archives with the remote
    Sync(SyncArgs),
    /// Answer pending decisions
    Resolve(ResolveArgs),
    /// Attach, show and detach the remote
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Bring the index in line with the stored metadata
    Reconcile(ReconcileArgs),
    /// Register existing compressed tar archives as archived pieces
    Import(ImportArgs),
}

impl Command {
    /// The name a JSON envelope carries: the words of the command line,
    /// joined by one space.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Init(_) => "init",
            Command::Status => "status",
            Command::Archive(_) => "archive",
            Command::Unearth(_) => "unearth",
            Command::Find(_) => "find",
            Command::Fork(_) => "fork",
            Command::Pin(_) => "pin",
            Command::Unpin(_) => "unpin",
            Command::Sync(_) => "sync",
            Command::Resolve(_) => "resolve",
            Command::Remote { command } => command.name(),
            Command::Reconcile(_) => "reconcile",
            Command::Import(_) => "import",
        }
    }

    /// Whether the command was started with `--if-reachable`.
    pub fn if_reachable(&self) -> bool {
        match self {
            Command::Unearth(args) => args.if_reachable,
            Command::Sync(args) => args.if_reachable,
            Command::Reconcile(args) => args.if_reachable,
            _ => false,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Attach a remote
    Add(RemoteAddArgs),
    /// Show the attached remote
    Show,
    /// Detach the remote
    Remove(RemoteRemoveArgs),
}

impl RemoteCommand {
    pub fn name(&self) -> &'static str {
        match self {
            RemoteCommand::Add(_) => "remote add",
            RemoteCommand::Show => "remote show",
            RemoteCommand::Remove(_) => "remote remove",
        }
    }
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Glob pattern that selects pieces; repeat for several
    #[arg(long = "pattern", value_name = "GLOB")]
    pub patterns: Vec<String>,
    /// Create the collection without asking
    #[arg(long)]
    pub yes: bool,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct DryRunArgs {
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct UnearthArgs {
    /// Path or ULID of the piece
    pub piece: OsString,
    /// Unearth into this directory
    #[arg(long, value_name = "PATH")]
    pub to: Option<PathBuf>,
    /// Payload version of the archive to unearth
    #[arg(long, value_name = "N")]
    pub version: Option<u32>,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Exit 0 when the remote is away
    #[arg(long)]
    pub if_reachable: bool,
}

#[derive(Debug, Args)]
pub struct FindArgs {
    /// What to search for; without it every piece is listed
    pub query: Option<String>,
    /// Print the path of the picked piece
    #[arg(long)]
    pub print: bool,
}

#[derive(Debug, Args)]
pub struct ForkArgs {
    /// Path or ULID of the piece
    pub piece: OsString,
    /// Name of the forked piece
    #[arg(long, value_name = "NAME")]
    pub name: Option<OsString>,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct PieceArgs {
    /// Path or ULID of the piece
    pub piece: OsString,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Exit 0 when the remote is away
    #[arg(long)]
    pub if_reachable: bool,
}

/// One decision at most is answered per run, and only for a named piece.
#[derive(Debug, Args)]
#[command(group = ArgGroup::new("answer").multiple(false))]
pub struct ResolveArgs {
    /// Path or ULID of the piece; without it every pending decision is
    /// gone through
    pub piece: Option<OsString>,
    /// Keep this machine's version
    #[arg(long, requires = "piece", group = "answer")]
    pub mine: bool,
    /// Take the other machine's version
    #[arg(long, requires = "piece", group = "answer")]
    pub theirs: bool,
    /// Keep both as separate pieces
    #[arg(long, requires = "piece", group = "answer")]
    pub fork: bool,
    /// Replace the archive although content is lost
    #[arg(long, requires = "piece", group = "answer")]
    pub replace: bool,
    /// Leave the piece as it is
    #[arg(long, requires = "piece", group = "answer")]
    pub skip: bool,
    /// Delete the superseded archive
    #[arg(long, requires = "piece", group = "answer")]
    pub drop_superseded: bool,
    /// Go on although the brake holds
    #[arg(long)]
    pub force: bool,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RemoteAddArgs {
    /// ssh://[user@]host[:port]/path or an absolute local path
    pub url: String,
    /// Create the remote root instead of attaching an existing one
    #[arg(long)]
    pub init: bool,
    /// Name under which the host key is looked up
    #[arg(long, value_name = "ALIAS")]
    pub host_key_alias: Option<String>,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RemoteRemoveArgs {
    /// Go on although the brake holds
    #[arg(long)]
    pub force: bool,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ReconcileArgs {
    /// Drop entries that are missing on the remote
    #[arg(long)]
    pub prune: bool,
    /// Go on although the brake holds
    #[arg(long)]
    pub force: bool,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
    /// Exit 0 when the remote is away
    #[arg(long)]
    pub if_reachable: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Directory the archives are copied from, instead of the current one
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
    /// Archives to register
    #[arg(required = true, value_name = "FILE")]
    pub files: Vec<PathBuf>,
    /// Print the plan and change nothing
    #[arg(long)]
    pub dry_run: bool,
}

/// What every command gets to know about the invocation.
#[derive(Debug)]
#[allow(dead_code, reason = "each command reads the fields it needs")]
pub struct Context {
    pub json: bool,
    pub directory: Option<PathBuf>,
}

#[allow(
    dead_code,
    reason = "each command asks through the interaction it needs"
)]
impl Context {
    /// Whom a command asks. `--json` never asks. Otherwise the controlling
    /// terminal answers, also when the output is redirected; without one
    /// nobody does.
    pub fn interaction(&self) -> Box<dyn Interaction> {
        if self.json {
            return Box::new(NonInteractive);
        }
        match TtyInteraction::open() {
            Ok(tty) => Box::new(tty),
            Err(_) => Box::new(NonInteractive),
        }
    }
}

/// Runs the command line `args` (starting with the program name), writes
/// its output through `out` and returns the exit code.
pub fn run(args: Vec<OsString>, out: &mut Output) -> u8 {
    match Cli::try_parse_from(&args) {
        Ok(cli) => {
            let result = dispatch(&cli, out);
            finish(&cli, result, out, policy::outcome)
        }
        Err(error) => report_usage_error(&error, &args, out),
    }
}

fn dispatch(cli: &Cli, out: &mut Output) -> anyhow::Result<u8> {
    let ctx = Context {
        json: cli.json,
        directory: cli.directory.clone(),
    };
    match &cli.command {
        Command::Init(args) => cmd::init::run(&ctx, args, out),
        Command::Status => cmd::status::run(&ctx, out),
        Command::Archive(args) => cmd::archive::run(&ctx, args, out),
        Command::Unearth(args) => cmd::unearth::run(&ctx, args, out),
        Command::Find(args) => cmd::find::run(&ctx, args, out),
        Command::Fork(args) => cmd::fork::run(&ctx, args, out),
        Command::Pin(args) => cmd::pin::run(&ctx, args, out),
        Command::Unpin(args) => cmd::unpin::run(&ctx, args, out),
        Command::Sync(args) => cmd::sync::run(&ctx, args, out),
        Command::Resolve(args) => cmd::resolve::run(&ctx, args, out),
        Command::Remote { command } => cmd::remote::run(&ctx, command, out),
        Command::Reconcile(args) => cmd::reconcile::run(&ctx, args, out),
        Command::Import(args) => cmd::import::run(&ctx, args, out),
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
            let root = chain[index..]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(": ");
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
fn finish(
    cli: &Cli,
    result: anyhow::Result<u8>,
    out: &mut Output,
    decide: fn(bool, &antiq::Error) -> Outcome,
) -> u8 {
    let error = match result {
        Ok(code) => return code,
        Err(error) => error,
    };
    let description = describe(&error);
    let (kind, details, outcome) = match description.library_error {
        Some(library_error) => (
            library_error.kind(),
            library_error.details(),
            decide(cli.command.if_reachable(), library_error),
        ),
        None => (ErrorKind::Other, Map::new(), Outcome::Fail),
    };
    // When the output itself fails there is nowhere left to report that.
    let _ = if cli.json {
        out.json_error(Some(cli.command.name()), kind, &description.root, &details)
    } else {
        out.plain_error(&description.root, &description.contexts)
    };
    match outcome {
        Outcome::TreatAsSuccess => 0,
        Outcome::Fail => kind.exit_code(),
    }
}

/// Reports an error of clap. Help and version are not errors: clap prints
/// them and they exit 0, also under `--json`. Every other error is a usage
/// error, which `--json` turns into an envelope on stdout.
fn report_usage_error(error: &clap::Error, args: &[OsString], out: &mut Output) -> u8 {
    let code = u8::try_from(error.exit_code()).unwrap_or(ErrorKind::Usage.exit_code());
    if code == 0 || !asks_for_json(args) {
        // A closed terminal leaves nowhere to print to.
        let _ = error.print();
        return code;
    }
    let message = first_paragraph(&error.render().to_string());
    let command = guess_command(args);
    let _ = out.json_error(command.as_deref(), ErrorKind::Usage, &message, &Map::new());
    code
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

/// The command a failed command line was meant for, as far as the
/// arguments tell: `remote` when the nested command is missing or unknown.
fn guess_command(args: &[OsString]) -> Option<String> {
    let root = Cli::command();
    let (index, name) = next_positional(args, 1)?;
    let command = root.find_subcommand(&name)?;
    if command.has_subcommands()
        && let Some((_, nested)) = next_positional(args, index + 1)
        && command.find_subcommand(&nested).is_some()
    {
        return Some(format!("{name} {nested}"));
    }
    Some(name)
}

/// The first argument from `from` on that is neither a flag nor the value
/// of `-C`, with its index. Only global options can come before a command.
fn next_positional(args: &[OsString], from: usize) -> Option<(usize, String)> {
    let mut index = from;
    while index < args.len() {
        let arg = args[index].to_str()?;
        match arg {
            "--" => return None,
            "-C" => index += 2,
            _ if arg.starts_with('-') => index += 1,
            _ => return Some((index, arg.to_string())),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::output::testing::{Capture, FailsOnFlush};
    use super::*;
    use antiq::lock::LockError;
    use antiq::remote::RemoteError;
    use anyhow::Context as _;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Runs `antiq <args>` and returns exit code, stdout and stderr.
    fn run_captured(args: &[&str]) -> (u8, String, String) {
        let capture = Capture::default();
        let mut out = capture.output(false);
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
        let code = finish(cli, result, &mut capture.output(false), policy::outcome);
        (code, capture.stdout(), capture.stderr())
    }

    fn question() -> antiq::interact::Question {
        antiq::interact::Question {
            title: "Replace the archive?".into(),
            details: vec![],
            choices: vec![antiq::interact::Choice::new("skip", "Skip")],
            default: 0,
        }
    }

    #[test]
    fn json_never_asks() {
        let ctx = Context {
            json: true,
            directory: None,
        };
        assert_eq!(ctx.interaction().choose(&question()), None);
    }

    #[test]
    fn without_json_the_terminal_is_asked_if_there_is_one() {
        let ctx = Context {
            json: false,
            directory: None,
        };
        // Asking would wait for a keypress where a terminal exists, so
        // this only checks that either kind of interaction is made.
        let _interaction = ctx.interaction();
    }

    #[test]
    fn a_command_without_an_implementation_reports_not_implemented() {
        let (code, stdout, stderr) = run_captured(&["--json", "sync"]);
        assert_eq!(code, 1);
        assert_eq!(
            stdout,
            "{\"format\":1,\"command\":\"sync\",\"error\":\
             {\"kind\":\"not_implemented\",\"message\":\"sync is not implemented\"}}\n"
        );
        assert_eq!(stderr, "");
    }

    #[test]
    fn the_command_name_of_a_nested_command_has_all_its_words() {
        let (_, stdout, _) = run_captured(&["--json", "remote", "remove"]);
        assert!(stdout.contains("\"command\":\"remote remove\""), "{stdout}");
    }

    #[test]
    fn a_plain_run_reports_on_stderr() {
        let (code, stdout, stderr) = run_captured(&["sync"]);
        assert_eq!(code, 1);
        assert_eq!(stdout, "");
        assert_eq!(stderr, "error: sync is not implemented\n");
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
    fn the_exit_code_comes_from_the_kind_of_the_library_error() {
        let cli = parse(&["status"]);
        let away: anyhow::Error = antiq::Error::from(RemoteError::Away {
            message: "no route to host".into(),
            overdue: false,
        })
        .into();
        assert_eq!(finish_captured(&cli, Err(away)).0, 4);
        let broken: anyhow::Error =
            antiq::Error::from(RemoteError::Broken("wrong marker".into())).into();
        assert_eq!(finish_captured(&cli, Err(broken)).0, 1);
    }

    #[test]
    fn if_reachable_ends_a_run_with_an_away_remote_with_zero() {
        let cli = parse(&["sync", "--if-reachable"]);
        let away: anyhow::Error = antiq::Error::from(RemoteError::Away {
            message: "no route to host".into(),
            overdue: false,
        })
        .into();
        assert_eq!(finish_captured(&cli, Err(away)).0, 0);
    }

    #[test]
    fn if_reachable_leaves_an_overdue_away_remote_at_exit_four() {
        let cli = parse(&["sync", "--if-reachable"]);
        let away: anyhow::Error = antiq::Error::from(RemoteError::Away {
            message: "no route to host".into(),
            overdue: true,
        })
        .into();
        assert_eq!(finish_captured(&cli, Err(away)).0, 4);
    }

    #[test]
    fn an_away_remote_without_the_flag_exits_with_four() {
        let cli = parse(&["sync"]);
        let away: anyhow::Error = antiq::Error::from(RemoteError::Away {
            message: "no route to host".into(),
            overdue: false,
        })
        .into();
        assert_eq!(finish_captured(&cli, Err(away)).0, 4);
    }

    #[test]
    fn a_policy_can_end_a_failed_run_with_zero_and_still_report_the_error() {
        let cli = parse(&["--json", "sync", "--if-reachable"]);
        let away: anyhow::Error = antiq::Error::from(RemoteError::Away {
            message: "no route to host".into(),
            overdue: false,
        })
        .into();
        let capture = Capture::default();
        let code = finish(
            &cli,
            Err(away),
            &mut capture.output(false),
            |if_reachable, _| {
                assert!(
                    if_reachable,
                    "the flag of the command line reaches the policy"
                );
                Outcome::TreatAsSuccess
            },
        );
        assert_eq!(code, 0);
        let stdout = capture.stdout();
        assert!(stdout.contains("\"kind\":\"remote_away\""), "{stdout}");
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
                "\"error\":{\"kind\":\"lock_held\",\
                 \"message\":\"another run holds the lock (pid 4242, started 2026-11-03T10:00:00Z)\",\
                 \"pid\":4242,\"started\":\"2026-11-03T10:00:00Z\"}}\n"
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
        let mut out = Output::new(Box::new(pipe), Box::new(Vec::new()), false);
        let code = run(os(&["antiq", "--json", "status"]), &mut out);
        assert_eq!(code, 1);
        assert_eq!(*flushes.lock().expect("no test panics while counting"), 1);
    }

    #[test]
    fn json_is_found_in_the_arguments_up_to_a_double_dash() {
        assert!(asks_for_json(&os(&["antiq", "--json", "status"])));
        assert!(asks_for_json(&os(&["antiq", "status", "--json"])));
        assert!(!asks_for_json(&os(&["antiq", "status"])));
        assert!(!asks_for_json(&os(&["antiq", "find", "--", "--json"])));
    }

    #[test]
    fn the_message_of_a_usage_error_is_its_first_paragraph() {
        let rendered = "error: the following required arguments were not provided:\n  \
                        <PIECE>\n\nUsage: antiq resolve --mine <PIECE>\n\nFor more information, try '--help'.\n";
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
        assert_eq!(guess(&["antiq", "remote"]).as_deref(), Some("remote"));
        assert_eq!(
            guess(&["antiq", "remote", "help"]).as_deref(),
            Some("remote")
        );
        assert_eq!(
            guess(&["antiq", "-Cx", "remote", "bogus"]).as_deref(),
            Some("remote")
        );
        assert_eq!(
            guess(&["antiq", "remote", "add", "--bogus"]).as_deref(),
            Some("remote add")
        );
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
