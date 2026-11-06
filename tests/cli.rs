// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command surface, the JSON envelope and the exit codes, seen from
//! outside the binary.

mod support;

use std::io;
use std::process::Stdio;
use support::cmd::Antiq;

const COMMANDS: [&str; 13] = [
    "init",
    "status",
    "archive",
    "unearth",
    "find",
    "fork",
    "pin",
    "unpin",
    "sync",
    "resolve",
    "remote",
    "reconcile",
    "import",
];

/// Runs `antiq --json <args>`.
/// A directory that is a collection, for the commands that look for one
/// before they fail.
fn collection_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir(root.path().join(".antiq")).expect("the directory");
    std::fs::write(
        root.path().join(".antiq/config.toml"),
        "[collection]\nid = \"01BX5ZZKBKACTAV9WEVGEMMVRZ\"\n",
    )
    .expect("the config");
    root
}

fn json_run(args: &[&str]) -> support::cmd::Run {
    Antiq::new().arg("--json").args(args).run()
}

#[test]
fn help_lists_the_thirteen_subcommands() {
    let run = Antiq::new().arg("--help").run();
    assert_eq!(run.code(), 0, "stderr: {}", run.stderr);
    let mut listed: Vec<&str> = run
        .stdout
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    listed.sort_unstable();
    let mut expected = COMMANDS.to_vec();
    expected.sort_unstable();
    assert_eq!(listed, expected, "full help:\n{}", run.stdout);
}

#[test]
fn the_help_subcommand_is_disabled() {
    let run = json_run(&["help"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert!(
        run.error_message().contains("'help'"),
        "message: {}",
        run.error_message()
    );
}

#[test]
fn the_remote_help_subcommand_is_disabled() {
    let run = json_run(&["remote", "help"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert!(
        run.error_message().contains("'help'"),
        "message: {}",
        run.error_message()
    );
}

// A flag a command does not take is a usage error. Each of these also
// has an accepting twin below, so a pass cannot come from a surface that
// rejects everything.

fn assert_usage_error(args: &[&str], message: &str) {
    let run = json_run(args);
    assert_eq!(run.code(), 2, "args: {args:?}, stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "usage");
    assert_eq!(run.error_message(), message);
}

#[test]
fn status_takes_no_dry_run() {
    assert_usage_error(
        &["status", "--dry-run"],
        "unexpected argument '--dry-run' found",
    );
}

#[test]
fn find_takes_no_force() {
    assert_usage_error(&["find", "--force"], "unexpected argument '--force' found");
}

#[test]
fn status_takes_no_if_reachable() {
    assert_usage_error(
        &["status", "--if-reachable"],
        "unexpected argument '--if-reachable' found",
    );
}

#[test]
fn an_answer_of_resolve_needs_a_piece() {
    assert_usage_error(
        &["resolve", "--mine"],
        "the following required arguments were not provided: <PIECE>",
    );
}

#[test]
fn resolve_takes_one_answer_at_most() {
    assert_usage_error(
        &["resolve", "piece", "--mine", "--theirs"],
        "the argument '--mine' cannot be used with '--theirs'",
    );
}

/// For a command that is implemented: its options parse, and outside a
/// collection it fails with the kind `not_a_collection`.
macro_rules! accepts_and_needs_a_collection {
    ($name:ident, $command:expr, [$($arg:expr),*]) => {
        #[test]
        fn $name() {
            let dir = tempfile::tempdir().expect("the temp dir is writable");
            let run = Antiq::new()
                .args(["--json", "-C"])
                .arg(dir.path())
                .args([$($arg),*])
                .run();
            assert_eq!(run.code(), 1, "stdout: {}, stderr: {}", run.stdout, run.stderr);
            assert_eq!(run.error_kind(), "not_a_collection");
            assert_eq!(run.json()["command"], $command);
        }
    };
}

#[test]
fn status_takes_nothing() {
    let dir = tempfile::tempdir().expect("the temp dir is writable");
    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .arg("status")
        .run();
    assert_eq!(
        run.code(),
        1,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "not_a_collection");
    assert_eq!(run.json()["command"], "status");
}

accepts_and_needs_a_collection!(
    unearth_takes_its_options,
    "unearth",
    [
        "unearth",
        "piece",
        "--to",
        "out",
        "--version",
        "3",
        "--dry-run",
        "--if-reachable"
    ]
);
accepts_and_needs_a_collection!(find_takes_no_query, "find", ["find"]);
accepts_and_needs_a_collection!(
    find_takes_a_query_and_print,
    "find",
    ["find", "q", "--print"]
);
accepts_and_needs_a_collection!(
    fork_takes_a_name_and_dry_run,
    "fork",
    ["fork", "piece", "--name", "copy", "--dry-run"]
);
accepts_and_needs_a_collection!(pin_takes_dry_run, "pin", ["pin", "piece", "--dry-run"]);
accepts_and_needs_a_collection!(
    unpin_takes_dry_run,
    "unpin",
    ["unpin", "piece", "--dry-run"]
);
accepts_and_needs_a_collection!(
    sync_takes_dry_run_and_if_reachable,
    "sync",
    ["sync", "--dry-run", "--if-reachable"]
);
accepts_and_needs_a_collection!(resolve_takes_nothing, "resolve", ["resolve"]);
accepts_and_needs_a_collection!(resolve_takes_a_piece, "resolve", ["resolve", "piece"]);
accepts_and_needs_a_collection!(
    resolve_takes_mine,
    "resolve",
    ["resolve", "piece", "--mine", "--force", "--dry-run"]
);
accepts_and_needs_a_collection!(
    resolve_takes_theirs,
    "resolve",
    ["resolve", "piece", "--theirs"]
);
accepts_and_needs_a_collection!(
    resolve_takes_fork,
    "resolve",
    ["resolve", "piece", "--fork"]
);
accepts_and_needs_a_collection!(
    resolve_takes_replace,
    "resolve",
    ["resolve", "piece", "--replace"]
);
accepts_and_needs_a_collection!(
    resolve_takes_skip,
    "resolve",
    ["resolve", "piece", "--skip"]
);
accepts_and_needs_a_collection!(
    resolve_takes_drop_superseded,
    "resolve",
    ["resolve", "piece", "--drop-superseded"]
);
/// `remote add` is implemented, so a command line it accepts runs and, away
/// from a collection, fails there.
#[test]
fn remote_add_takes_its_options() {
    let run = json_run(&[
        "remote",
        "add",
        "--init",
        "--host-key-alias",
        "nas",
        "ssh://nas/backup",
        "--dry-run",
    ]);
    assert_eq!(
        run.code(),
        1,
        "stdout: {}, stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.error_kind(), "not_a_collection");
    assert_eq!(run.json()["command"], "remote add");
}
accepts_and_needs_a_collection!(remote_show_takes_nothing, "remote show", ["remote", "show"]);
accepts_and_needs_a_collection!(
    remote_remove_takes_force_and_dry_run,
    "remote remove",
    ["remote", "remove", "--force", "--dry-run"]
);
accepts_and_needs_a_collection!(
    reconcile_takes_its_options,
    "reconcile",
    [
        "reconcile",
        "--prune",
        "--force",
        "--dry-run",
        "--if-reachable"
    ]
);
accepts_and_needs_a_collection!(
    import_takes_a_file_and_a_path,
    "import",
    ["import", "--path", "dir", "one.tar.zst", "--dry-run"]
);

#[test]
fn a_json_run_prints_one_not_implemented_envelope() {
    let root = collection_root();
    let run = json_run(&[
        "-C",
        root.path().to_str().expect("UTF-8"),
        "reconcile",
        "--prune",
    ]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.stderr, "");
    let json = run.json();
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(keys, ["format", "command", "error"]);
    assert_eq!(json["format"], 1);
    assert_eq!(json["command"], "reconcile");
    assert_eq!(json["error"]["kind"], "not_implemented");
    assert_eq!(
        json["error"]["message"],
        "reconcile --prune is not implemented"
    );
}

#[test]
fn a_plain_run_prints_its_error_to_stderr() {
    let root = collection_root();
    let run = Antiq::new()
        .arg("-C")
        .arg(root.path())
        .args(["reconcile", "--prune"])
        .run();
    assert_eq!(run.code(), 1);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "error: reconcile --prune is not implemented\n  while attempting to reconcile the index\n"
    );
}

#[test]
fn an_unknown_command_names_none_in_the_usage_envelope() {
    let run = json_run(&["bogus"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert!(run.json()["command"].is_null());
}

#[test]
fn a_missing_command_names_none_in_the_usage_envelope() {
    let run = json_run(&[]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert!(run.json()["command"].is_null());
}

#[test]
fn the_directory_option_does_not_name_the_command() {
    let run = json_run(&["-C", "status", "bogus"]);
    assert_eq!(run.code(), 2);
    assert!(run.json()["command"].is_null());
}

#[test]
fn a_missing_remote_subcommand_names_remote() {
    let run = json_run(&["remote"]);
    assert_eq!(run.code(), 2);
    assert_eq!(run.error_kind(), "usage");
    assert_eq!(run.json()["command"], "remote");
}

#[test]
fn help_and_version_exit_zero_under_json() {
    let help = json_run(&["--help"]);
    assert_eq!(help.code(), 0);
    assert!(help.stdout.contains("Usage:"), "stdout: {}", help.stdout);
    let version = json_run(&["--version"]);
    assert_eq!(version.code(), 0);
    assert!(
        version.stdout.starts_with("antiq "),
        "stdout: {}",
        version.stdout
    );
}

#[test]
fn the_global_options_work_after_the_subcommand() {
    let dir = tempfile::tempdir().expect("the temp dir is writable");
    let run = Antiq::new()
        .arg("status")
        .arg("-C")
        .arg(dir.path())
        .arg("--json")
        .run();
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
}

#[test]
fn a_closed_stdout_keeps_the_exit_code_and_does_not_panic() {
    let (reader, writer) = io::pipe().expect("a pipe can be made");
    drop(reader);
    let run = Antiq::new()
        .arg("--json")
        .arg("status")
        .stdout(Stdio::from(writer))
        .run();
    assert_eq!(run.code(), 1, "stderr: {}", run.stderr);
    assert!(!run.stderr.contains("panicked"), "stderr: {}", run.stderr);
}

#[test]
fn if_reachable_does_not_hide_an_error_that_is_not_an_away_remote() {
    let run = json_run(&["sync", "--if-reachable"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.error_kind(), "not_a_collection");
}

#[test]
fn a_child_is_the_leader_of_a_session_of_its_own() {
    // A signal to the process group named by the shell's own pid reaches
    // someone only if the shell leads that group, which `setsid` makes it
    // do. `kill` is a builtin of the shell, so no other executable is
    // needed.
    let group = Antiq::program("sh").args(["-c", "kill -0 -$$"]).run();
    assert!(
        group.status.success(),
        "the child does not lead its own process group; stderr: {}",
        group.stderr
    );

    let tty = Antiq::program("sh").args(["-c", ": < /dev/tty"]).run();
    assert!(!tty.status.success(), "the child can open /dev/tty");
}

#[test]
fn a_child_keeps_the_environment_of_the_test_run() {
    // CARGO_MANIFEST_DIR is set by cargo for every test run;
    // LLVM_PROFILE_FILE only under cargo llvm-cov, where spawned binaries
    // need it to report their coverage.
    let run = Antiq::program("env").run();
    for name in ["CARGO_MANIFEST_DIR", "LLVM_PROFILE_FILE"] {
        if let Some(value) = std::env::var_os(name) {
            let line = format!("{name}={}", value.to_string_lossy());
            assert!(
                run.stdout.lines().any(|candidate| candidate == line),
                "{name} did not reach the child"
            );
        }
    }
    assert!(run.stdout.contains("CARGO_MANIFEST_DIR="));
}
