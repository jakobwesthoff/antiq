// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command surface, the JSON envelope and the exit codes, seen from
//! outside the binary.

mod support;

use std::io;
use std::process::Stdio;
use support::cmd::Antiq;
use support::tree::TempCollection;

const COMMANDS: [&str; 8] = [
    "init", "status", "archive", "unearth", "fork", "pin", "unpin", "import",
];

/// Runs `antiq --json <args>`.
fn json_run(args: &[&str]) -> support::cmd::Run {
    Antiq::new().arg("--json").args(args).run()
}

#[test]
fn help_lists_the_eight_subcommands() {
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

/// For a command: its options parse, and outside a collection it fails
/// with the kind `not_a_collection`.
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
        "--dry-run"
    ]
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
    import_takes_a_file_and_a_path,
    "import",
    ["import", "--path", "dir", "one.tar.zst", "--dry-run"]
);

/// For a command without an implementation: inside a collection its
/// options parse and the run fails with the kind `not_implemented`.
macro_rules! is_not_implemented {
    ($name:ident, $command:expr, [$($arg:expr),*]) => {
        #[test]
        fn $name() {
            let collection = TempCollection::minimal();
            let run = Antiq::new()
                .args(["--json", "-C"])
                .arg(collection.root())
                .args([$($arg),*])
                .run();
            assert_eq!(run.code(), 1, "stdout: {}, stderr: {}", run.stdout, run.stderr);
            assert_eq!(run.error_kind(), "not_implemented");
            assert_eq!(run.error_message(), concat!($command, " is not implemented"));
            assert_eq!(run.json()["command"], $command);
        }
    };
}

is_not_implemented!(status_is_not_implemented, "status", ["status"]);
is_not_implemented!(
    archive_is_not_implemented,
    "archive",
    ["archive", "--dry-run"]
);
is_not_implemented!(
    unearth_is_not_implemented,
    "unearth",
    [
        "unearth",
        "piece",
        "--to",
        "out",
        "--version",
        "3",
        "--dry-run"
    ]
);
is_not_implemented!(
    fork_is_not_implemented,
    "fork",
    ["fork", "piece", "--name", "copy", "--dry-run"]
);
is_not_implemented!(
    import_is_not_implemented,
    "import",
    ["import", "--path", "dir", "one.tar.zst", "--dry-run"]
);
is_not_implemented!(pin_is_not_implemented, "pin", ["pin", "piece", "--dry-run"]);
is_not_implemented!(
    unpin_is_not_implemented,
    "unpin",
    ["unpin", "piece", "--dry-run"]
);
accepts_and_needs_a_collection!(archive_takes_dry_run, "archive", ["archive", "--dry-run"]);

/// A name that is not a command is a usage error that names no command.
macro_rules! is_not_a_command {
    ($name:ident, $arg0:literal, [$($arg:expr),*]) => {
        #[test]
        fn $name() {
            let dir = tempfile::tempdir().expect("the temp dir is writable");
            let run = Antiq::new()
                .args(["--json", "-C"])
                .arg(dir.path())
                .args([$($arg),*])
                .run();
            assert_eq!(run.code(), 2, "stdout: {}, stderr: {}", run.stdout, run.stderr);
            assert_eq!(run.error_kind(), "usage");
            assert!(
                run.error_message().contains(concat!("unrecognized subcommand '", $arg0, "'")),
                "message: {}",
                run.error_message()
            );
            assert!(run.json()["command"].is_null(), "stdout: {}", run.stdout);
        }
    };
}

is_not_a_command!(find_is_not_a_command, "find", ["find"]);
is_not_a_command!(sync_is_not_a_command, "sync", ["sync"]);
is_not_a_command!(reconcile_is_not_a_command, "reconcile", ["reconcile"]);
is_not_a_command!(resolve_is_not_a_command, "resolve", ["resolve"]);
is_not_a_command!(remote_is_not_a_command, "remote", ["remote", "show"]);

#[test]
fn a_json_run_prints_one_error_envelope() {
    let dir = tempfile::tempdir().expect("the temp dir is writable");
    let run = json_run(&["-C", dir.path().to_str().expect("UTF-8"), "status"]);
    assert_eq!(run.code(), 1);
    assert_eq!(run.stderr, "");
    let json = run.json();
    let keys: Vec<&String> = json.as_object().expect("an object").keys().collect();
    assert_eq!(keys, ["format", "command", "error"]);
    assert_eq!(json["format"], 1);
    assert_eq!(json["command"], "status");
    assert_eq!(json["error"]["kind"], "not_a_collection");
    assert_eq!(
        json["error"]["message"],
        format!("{} is not inside a collection", dir.path().display())
    );
}

#[test]
fn a_plain_run_prints_its_error_to_stderr() {
    let dir = tempfile::tempdir().expect("the temp dir is writable");
    let run = Antiq::new().arg("-C").arg(dir.path()).arg("status").run();
    assert_eq!(run.code(), 1);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        format!(
            "error: {} is not inside a collection\n  while attempting to find the collection\n",
            dir.path().display()
        )
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
