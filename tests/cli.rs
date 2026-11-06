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

const COMMANDS: [&str; 13] = [
    "init", "status", "archive", "unearth", "fork", "forget", "rebuild", "import", "list", "grep",
    "pin", "unpin", "push",
];

/// Runs `antiq --json <args>`.
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
fn init_takes_no_yes() {
    // A dry run in a temporary directory, so that a build that still
    // accepts the flag creates nothing.
    let dir = tempfile::tempdir().expect("the temp dir is writable");
    let run = Antiq::new()
        .args(["--json", "-C"])
        .arg(dir.path())
        .args(["init", "--yes", "-n"])
        .run();
    assert_eq!(run.code(), 2, "stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "usage");
    assert_eq!(run.error_message(), "unexpected argument '--yes' found");
}

/// For a command line: its arguments parse, outside a collection the run
/// fails with the kind `not_a_collection`, and inside one with the kind
/// `not_implemented`.
macro_rules! command_line {
    ($name:ident, $command:expr, [$($arg:expr),*]) => {
        mod $name {
            use super::*;

            #[test]
            fn needs_a_collection() {
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

            #[test]
            fn is_not_implemented() {
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
        }
    };
}

/// `status` is built: outside a collection it fails with `not_a_collection`.
mod status {
    use super::*;

    #[test]
    fn needs_a_collection() {
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
}

command_line!(archive_everything, "archive", ["archive", "-n"]);
command_line!(
    archive_one_piece,
    "archive",
    ["archive", "piece", "--dry-run"]
);
command_line!(
    unearth,
    "unearth",
    ["unearth", "piece", "--version", "2026-11-04T090211", "-n"]
);
command_line!(
    fork,
    "fork",
    ["fork", "piece", "--version", "v", "--name", "copy", "-n"]
);
command_line!(rebuild, "rebuild", ["rebuild", "piece", "v", "-n"]);
command_line!(
    import,
    "import",
    [
        "import",
        "dir",
        "--as",
        "proj",
        "--time",
        "2020-01-01",
        "-n"
    ]
);

#[test]
fn forget_needs_a_collection() {
    for args in [
        &["forget", "piece", "v"][..],
        &["forget", "piece", "--all-versions", "--force"],
    ] {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args(args)
            .run();
        assert_eq!(
            run.code(),
            1,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        assert_eq!(run.error_kind(), "not_a_collection", "{args:?}");
        assert_eq!(run.json()["command"], "forget");
    }
}

#[test]
fn list_needs_a_collection() {
    for args in [&["list"][..], &["list", "query"]] {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args(args)
            .run();
        assert_eq!(
            run.code(),
            1,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        assert_eq!(run.error_kind(), "not_a_collection", "{args:?}");
        assert_eq!(run.json()["command"], "list");
    }
}
command_line!(pin, "pin", ["pin", "piece", "-n"]);
command_line!(unpin, "unpin", ["unpin", "piece", "-n"]);
command_line!(push, "push", ["push"]);
command_line!(push_with_init, "push", ["push", "--init", "--dry-run"]);
command_line!(push_with_force, "push", ["push", "--force"]);
command_line!(push_if_reachable, "push", ["push", "--if-reachable", "-n"]);

/// A command line that does not parse: exit 2, kind `usage`, and the
/// message contains `fragment`.
fn assert_usage(args: &[&str], fragment: &str) {
    let run = json_run(args);
    assert_eq!(run.code(), 2, "args: {args:?}, stdout: {}", run.stdout);
    assert_eq!(run.error_kind(), "usage");
    assert!(
        run.error_message().contains(fragment),
        "args: {args:?}, message: {}",
        run.error_message()
    );
}

#[test]
fn forget_needs_a_version_or_all_versions() {
    assert_usage(&["forget", "piece"], "required arguments were not provided");
}

#[test]
fn forget_takes_a_version_or_all_versions_not_both() {
    assert_usage(
        &["forget", "piece", "v", "--all-versions", "--force"],
        "cannot be used with",
    );
}

#[test]
fn forget_all_versions_needs_force() {
    assert_usage(
        &["forget", "piece", "--all-versions"],
        "required arguments were not provided",
    );
}

#[test]
fn forget_force_alone_is_refused() {
    assert_usage(
        &["forget", "piece", "--force"],
        "required arguments were not provided",
    );
}

#[test]
fn forget_force_is_refused_with_a_version() {
    assert_usage(&["forget", "piece", "v", "--force"], "cannot be used with");
}

#[test]
fn forget_all_versions_is_refused_with_a_version() {
    assert_usage(
        &["forget", "piece", "v", "--all-versions"],
        "cannot be used with",
    );
}

#[test]
fn push_init_is_refused_with_if_reachable() {
    assert_usage(&["push", "--init", "--if-reachable"], "cannot be used with");
}

#[test]
fn push_force_is_refused_with_if_reachable() {
    assert_usage(
        &["push", "--force", "--if-reachable"],
        "cannot be used with",
    );
}

#[test]
fn unearth_has_no_to() {
    assert_usage(
        &["unearth", "piece", "--to", "out"],
        "unexpected argument '--to'",
    );
}

#[test]
fn rebuild_needs_a_version() {
    assert_usage(
        &["rebuild", "piece"],
        "required arguments were not provided",
    );
}

#[test]
fn import_needs_a_folder() {
    assert_usage(&["import"], "required arguments were not provided");
}

#[test]
fn list_takes_one_query() {
    assert_usage(&["list", "a", "b"], "unexpected argument 'b'");
}

#[test]
fn grep_needs_a_pattern() {
    assert_usage(&["grep"], "required arguments were not provided");
}

#[test]
fn list_and_grep_and_status_take_no_dry_run() {
    assert_usage(&["list", "-n"], "unexpected argument '-n'");
    assert_usage(&["grep", "-n", "x"], "unexpected argument '-n'");
    assert_usage(&["status", "-n"], "unexpected argument '-n'");
}

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
