// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Where a command starts: the directory `-C` names, or the current
//! directory of the process without it. Every command runs through the
//! binary with the process directory set, and its output must equal the
//! output of the same command with an absolute `-C`.

mod support;

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, Run};
use support::scenario::collection::{Clock, Collection};

const STORED: &str = "2026-09-03T121500";

/// A command line for one command. `piece` is the path of the piece the
/// command works on, relative to the collection root; a case spells it
/// relative to its own start directory.
struct Command {
    name: &'static str,
    flags: &'static [&'static str],
    piece: Option<&'static str>,
    tail: &'static [&'static str],
    /// The exit code of the command with an absolute `-C`.
    code: i32,
}

const fn command(
    name: &'static str,
    flags: &'static [&'static str],
    piece: Option<&'static str>,
    tail: &'static [&'static str],
    code: i32,
) -> Command {
    Command {
        name,
        flags,
        piece,
        tail,
        code,
    }
}

/// Dry runs where there is one, so that one collection serves every case.
const COMMANDS: &[Command] = &[
    command("unearth", &["-n"], Some("tools/scan-old"), &[], 0),
    command("fork", &["-n"], Some("tools/scan-old"), &[], 0),
    command("forget", &["-n"], Some("tools/scan-old"), &[STORED], 0),
    // The version is sound, so there is nothing to rebuild; the error is
    // raised after the piece reference resolved.
    command("rebuild", &["-n"], Some("tools/scan-old"), &[STORED], 1),
    command("archive", &["-n"], Some("tools/old-live"), &[], 0),
    command("pin", &["-n"], Some("tools/old-live"), &[], 0),
    command("unpin", &["-n"], Some("tools/pinned"), &[], 0),
    command("status", &[], None, &[], 0),
    command("list", &[], None, &[], 0),
    command("grep", &[], None, &["main"], 0),
    // No target is configured, which is an error that needs the collection.
    command("push", &["-n"], None, &[], 1),
];

fn collection() -> Collection {
    Collection::build(&Clock::now())
        .patterns(&["tools/*"])
        .version(
            "tools/scan-old",
            STORED,
            &[("src/main.rs", "fn main() {}\n")],
        )
        .piece("tools/old-live", |piece| {
            piece.age_days(40).file("a.txt", "a");
        })
        .piece("tools/pinned", |piece| {
            piece.age_days(40).file("a.txt", "a").pinned();
        })
        .finish()
}

fn argv(command: &Command, piece_ref: Option<&str>) -> Vec<String> {
    let mut args = vec![command.name.to_string()];
    args.extend(command.flags.iter().map(ToString::to_string));
    if let Some(piece_ref) = piece_ref {
        args.push(piece_ref.to_string());
    }
    args.extend(command.tail.iter().map(ToString::to_string));
    args
}

fn run(cwd: &Path, global: &[&str], args: &[String]) -> Run {
    Antiq::new()
        .env("TZ", "UTC")
        .current_dir(cwd)
        .args(global)
        .args(args)
        .run()
}

/// The command with an absolute `-C`: the output the other spellings must
/// reproduce.
fn baseline(collection: &Collection, command: &Command) -> Run {
    let root = collection.root();
    let global = ["-C", root.to_str().expect("the path is UTF-8")];
    let run = run(&root, &global, &argv(command, command.piece));
    assert_eq!(
        run.code(),
        command.code,
        "{} with an absolute -C\nstdout: {:?}\nstderr: {:?}",
        command.name,
        run.stdout,
        run.stderr
    );
    run
}

/// Runs every command from `cwd` with `global` and the piece spelled by
/// `spell`, and compares it with the baseline.
fn check_all(
    collection: &Collection,
    cwd: &Path,
    global: &[&str],
    spell: impl Fn(&'static str) -> String,
) {
    let mut differing = Vec::new();
    for command in COMMANDS {
        let expected = baseline(collection, command);
        let piece_ref = command.piece.map(&spell);
        let actual = run(cwd, global, &argv(command, piece_ref.as_deref()));
        if (actual.code(), &actual.stdout, &actual.stderr)
            != (expected.code(), &expected.stdout, &expected.stderr)
        {
            differing.push(format!(
                "{} {piece_ref:?}: exit {}, stdout {:?}, stderr {:?}",
                command.name,
                actual.code(),
                actual.stdout,
                actual.stderr
            ));
        }
    }
    assert!(
        differing.is_empty(),
        "from {} with {global:?}, these differ from the absolute -C run:\n{}",
        cwd.display(),
        differing.join("\n")
    );
}

fn root_relative(piece: &'static str) -> String {
    piece.to_string()
}

fn from_tools(piece: &'static str) -> String {
    format!("../{piece}")
}

#[test]
fn no_c_starts_in_the_current_directory() {
    let collection = collection();
    check_all(&collection, &collection.root(), &[], root_relative);
}

#[test]
fn c_dot_starts_in_the_current_directory() {
    let collection = collection();
    check_all(&collection, &collection.root(), &["-C", "."], root_relative);
}

#[test]
fn a_relative_c_is_taken_against_the_current_directory() {
    let collection = collection();
    let root = collection.root();
    let name = root.file_name().expect("the root has a name");
    let parent = root.parent().expect("the root has a parent");
    check_all(
        &collection,
        parent,
        &["-C", name.to_str().expect("the name is UTF-8")],
        root_relative,
    );
}

#[test]
fn a_subdirectory_finds_the_collection_above_and_resolves_paths_from_itself() {
    let collection = collection();
    check_all(&collection, &collection.path("tools"), &[], from_tools);
}

#[test]
fn a_path_inside_the_subdirectory_names_the_piece() {
    let collection = collection();
    let tools = collection.path("tools");
    for (name, piece) in [
        ("unearth", "scan-old"),
        ("fork", "scan-old"),
        ("forget", "scan-old"),
        ("rebuild", "scan-old"),
        ("archive", "old-live"),
        ("pin", "old-live"),
        ("unpin", "pinned"),
    ] {
        let command = COMMANDS
            .iter()
            .find(|command| command.name == name)
            .expect("the command is in the table");
        let expected = baseline(&collection, command);
        let actual = run(&tools, &[], &argv(command, Some(&format!("./{piece}"))));
        assert_eq!(
            (actual.code(), &actual.stdout, &actual.stderr),
            (expected.code(), &expected.stdout, &expected.stderr),
            "{name} with ./{piece} from tools"
        );
    }
}

/// A directory that is a link to `<root>/tools`, in a directory of its own.
fn link_to_tools(collection: &Collection) -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("the temp dir is writable");
    let link = home.path().join("link");
    symlink(collection.path("tools"), &link).expect("the link is made");
    (home, link)
}

#[test]
fn a_c_through_a_link_resolves_the_dot_dot_of_a_path_in_the_real_directory() {
    let collection = collection();
    let (home, _link) = link_to_tools(&collection);
    check_all(&collection, home.path(), &["-C", "link"], from_tools);
}

#[test]
fn a_c_that_goes_up_from_a_link_names_the_real_parent() {
    let collection = collection();
    let (home, _link) = link_to_tools(&collection);
    check_all(&collection, home.path(), &["-C", "link/.."], root_relative);
}

#[test]
fn outside_a_collection_every_command_but_init_says_so() {
    let outside = tempfile::tempdir().expect("the temp dir is writable");
    for command in COMMANDS {
        let run = run(outside.path(), &["--json"], &argv(command, command.piece));
        assert_eq!(run.code(), 1, "{}: {:?}", command.name, run.stdout);
        assert_eq!(run.error_kind(), "not_a_collection", "{}", command.name);
    }
}

#[test]
fn init_makes_the_current_directory_a_collection_without_c() {
    let home = tempfile::tempdir().expect("the temp dir is writable");
    let bare = home.path().join("bare");
    fs::create_dir(&bare).expect("the directory is made");

    let run = run(&bare, &[], &["init".to_string()]);
    assert_eq!(run.code(), 0, "{:?}", run.stderr);
    assert!(bare.join(".antiq").is_dir());
}

#[test]
fn init_with_a_relative_c_makes_that_directory_a_collection() {
    let home = tempfile::tempdir().expect("the temp dir is writable");
    fs::create_dir(home.path().join("bare")).expect("the directory is made");

    let run = run(home.path(), &["-C", "bare"], &["init".to_string()]);
    assert_eq!(run.code(), 0, "{:?}", run.stderr);
    assert!(home.path().join("bare/.antiq").is_dir());
}

#[test]
fn a_c_that_does_not_exist_is_an_error_naming_the_path() {
    let home = tempfile::tempdir().expect("the temp dir is writable");
    let run = run(home.path(), &["-C", "missing"], &["status".to_string()]);
    assert_eq!(run.code(), 1);
    assert!(run.stderr.contains("missing"), "{:?}", run.stderr);
}

/// A collection `coll` made by `init`, next to a folder `inbox/data` to
/// import.
fn collection_and_inbox() -> (tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().expect("the temp dir is writable");
    let root = home.path().join("coll");
    fs::create_dir(&root).expect("the collection directory is made");
    fs::create_dir_all(home.path().join("inbox/data")).expect("the folder is made");
    fs::write(home.path().join("inbox/data/x.txt"), "x").expect("the file is written");
    let init = run(&root, &[], &["init".to_string()]);
    assert_eq!(init.code(), 0, "{:?}", init.stderr);
    let root = fs::canonicalize(root).expect("the root exists");
    (home, root)
}

#[test]
fn import_takes_its_folder_from_the_start_directory() {
    let (home, root) = collection_and_inbox();
    let args: Vec<String> = ["import", "-n", "../inbox/data"].map(String::from).to_vec();
    let absolute = root.to_str().expect("the path is UTF-8");
    let expected = run(&root, &["-C", absolute], &args);
    assert_eq!(expected.code(), 0, "{:?}", expected.stderr);

    for (cwd, global) in [
        (root.clone(), vec![]),
        (root.clone(), vec!["-C", "."]),
        (home.path().to_path_buf(), vec!["-C", "coll"]),
    ] {
        let actual = run(&cwd, &global, &args);
        assert_eq!(
            (actual.code(), &actual.stdout, &actual.stderr),
            (expected.code(), &expected.stdout, &expected.stderr),
            "import from {} with {global:?}",
            cwd.display()
        );
    }
}
