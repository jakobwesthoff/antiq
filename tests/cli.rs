// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command surface, the JSON envelope and the exit codes, seen from
//! outside the binary.

mod support;

use std::fs;
use std::io;
use std::process::Stdio;
use support::cmd::Antiq;

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

/// `archive` is built: outside a collection it fails with `not_a_collection`.
mod archive {
    use super::*;

    #[test]
    fn needs_a_collection() {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args(["archive", "piece", "-n"])
            .run();
        assert_eq!(
            run.code(),
            1,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        assert_eq!(run.error_kind(), "not_a_collection");
        assert_eq!(run.json()["command"], "archive");
    }
}

/// `import` is built: outside a collection it fails with `not_a_collection`.
mod import {
    use super::*;

    #[test]
    fn needs_a_collection() {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args([
                "import",
                "dir",
                "--as",
                "proj",
                "--time",
                "2020-01-01",
                "-n",
            ])
            .run();
        assert_eq!(
            run.code(),
            1,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        assert_eq!(run.error_kind(), "not_a_collection");
        assert_eq!(run.json()["command"], "import");
    }
}

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
fn rebuild_needs_a_collection() {
    for args in [
        &["rebuild", "piece", "v"][..],
        &["rebuild", "piece", "v", "-n"],
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
        assert_eq!(run.json()["command"], "rebuild");
    }
}

#[test]
fn fork_needs_a_collection() {
    for args in [
        &["fork", "piece", "--version", "v", "--name", "copy"][..],
        &["fork", "piece", "-n"],
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
        assert_eq!(run.json()["command"], "fork");
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

#[test]
fn pin_and_unpin_need_a_collection() {
    for command in ["pin", "unpin"] {
        let dir = tempfile::tempdir().expect("the temp dir is writable");
        let run = Antiq::new()
            .args(["--json", "-C"])
            .arg(dir.path())
            .args([command, "piece", "-n"])
            .run();
        assert_eq!(
            run.code(),
            1,
            "stdout: {}, stderr: {}",
            run.stdout,
            run.stderr
        );
        assert_eq!(run.error_kind(), "not_a_collection", "{command}");
        assert_eq!(run.json()["command"], command);
    }
}

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

is_not_a_command!(reconcile_is_not_a_command, "reconcile", ["reconcile"]);

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
        format!(
            "{} is not inside a collection",
            fs::canonicalize(dir.path())
                .expect("the dir exists")
                .display()
        )
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
            "error: {} is not inside an antiq collection\n  \
                 why: antiq works in a directory that holds an antiq collection, which has an .antiq/ folder. None of the parent directories has one.\n  \
                 fix: Change into your collection, or create one here with antiq init.\n  \
                 while attempting to find the antiq collection\n",
            fs::canonicalize(dir.path())
                .expect("the dir exists")
                .display()
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

#[test]
fn a_test_run_does_not_inherit_colour_and_locale_settings_of_the_developer() {
    let mut command = std::process::Command::new("antiq");
    support::cmd::make_hermetic(&mut command);
    let envs: std::collections::HashMap<_, _> = command
        .get_envs()
        .map(|(key, value)| (key.to_string_lossy().into_owned(), value.is_some()))
        .collect();
    for gone in [
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "CI",
        "LC_CTYPE",
        "LANG",
    ] {
        assert_eq!(envs.get(gone), Some(&false), "{gone} is removed");
    }
    assert_eq!(envs.get("LC_ALL"), Some(&true), "LC_ALL is set");
    assert_eq!(envs.get("TERM"), Some(&true), "TERM is set");
}

#[test]
fn forget_all_versions_without_force_keeps_its_json_message_byte_for_byte() {
    let run = json_run(&["forget", "piece", "--all-versions"]);
    assert_eq!(run.code(), 2);
    assert_eq!(
        run.stdout,
        "{\"format\":1,\"command\":\"forget\",\"error\":{\"kind\":\"usage\",\
         \"message\":\"the following required arguments were not provided: --force\"}}\n"
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn forget_all_versions_without_force_gets_its_own_error_on_a_pipe() {
    let run = Antiq::new()
        .args(["forget", "tools/backup-script", "--all-versions"])
        .run();
    assert_eq!(run.code(), 2);
    assert_eq!(run.stdout, "");
    assert_eq!(
        run.stderr,
        "error: tools/backup-script: --all-versions needs --force\n  \
         why: --all-versions deletes every stored version of the piece, damaged and incomplete ones included, and that cannot be undone.\n  \
         fix: Add --force to confirm: antiq forget tools/backup-script --all-versions --force. Check the effect first with -n.\n  \
         while attempting to forget all versions of tools/backup-script\n"
    );
}

#[test]
fn the_forget_confirmation_keeps_the_directory_and_quotes_the_piece() {
    let run = Antiq::new()
        .args([
            "-C",
            "/tmp/my collection",
            "forget",
            "my piece",
            "--all-versions",
        ])
        .run();
    assert_eq!(run.code(), 2);
    assert!(
        run.stderr.contains(
            "fix: Add --force to confirm: antiq -C '/tmp/my collection' forget 'my piece' --all-versions --force. "
        ),
        "{:?}",
        run.stderr
    );
}
