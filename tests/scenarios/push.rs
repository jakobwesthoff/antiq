// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenarios: mirroring the collection to a path target with `push`.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::outsider::same_file;
use crate::support::scenario::state::State;
use crate::support::scenario::target::Target;
use crate::support::scenario::world::{Outcome, Setup, World, both_modes};
use serde_json::json;
use std::fs;
use std::os::unix::fs::MetadataExt;

const STORED: &str = "2025-02-11T184007";

/// What a mirror leaves out: the scratch areas and the lock of the
/// collection, which `push` excludes.
const NOT_MIRRORED: [&str; 3] = [".antiq/tmp", ".antiq/trash", ".antiq/doomed"];

const REAL_KEYS: [&str; 13] = [
    "format",
    "command",
    "result",
    "target",
    "initialized",
    "forced",
    "pieces",
    "other",
    "vanished",
    "second_pass",
    "notes",
    "repairs",
    "warnings",
];

/// A collection of two live pieces and a stored version, with an empty
/// target and the located rsync.
fn pushing_setup(clock: &crate::support::scenario::collection::Clock) -> Setup {
    let collection = Collection::build(clock)
        .patterns(&["*/*"])
        .piece("projects/scanner", |piece| {
            piece
                .age_days(10)
                .file("src/main.rs", "fn main() {}\n")
                .hard_link("src/main.rs", "src/copy.rs")
                .symlink("latest", "src/main.rs")
                .file("notes.txt", "to do\n");
        })
        .piece("notes/daily", |piece| {
            piece.age_days(5).file("today.md", "fresh\n");
        })
        .version("old/client-site", STORED, &[("index.html", "<html>")])
        .outside("README.md", "the collection\n")
        .finish();
    Setup::new(collection)
        .with_target(Target::new())
        .with_real_rsync()
}

/// The collection and its mirror differ in nothing a push is meant to
/// keep: the mirror lacks the scratch areas and has the marker.
fn assert_mirrors(world: &World) {
    let collection = State::raw(&world.collection().root()).without(&NOT_MIRRORED);
    let target = State::raw(&world.target().expect("a target").path()).without(&[".antiq-target"]);
    if let Some(diff) = collection.diff(&target) {
        panic!("the target is not a copy of the collection:\n{diff}");
    }
}

/// Everything a run printed, for a refusal whose text goes to stderr in
/// text mode and into the error object in JSON mode.
fn said(outcome: &Outcome) -> String {
    format!("{}{}", outcome.stdout, outcome.stderr)
}

/// The marker file's bytes as `push --init` writes them for this world.
fn expected_marker(world: &World) -> Vec<u8> {
    let probe = Target::new();
    probe.marker(world.collection());
    fs::read(probe.path().join(".antiq-target")).expect("the marker was written")
}

#[test]
fn a_terminal_gets_the_claim_of_the_target_after_the_lines_of_the_pieces() {
    let clock = crate::support::scenario::collection::Clock::now();
    let world = World::new(
        pushing_setup(&clock),
        crate::support::scenario::world::Mode::Text,
    );
    let shown = world
        .target()
        .expect("a target")
        .path()
        .display()
        .to_string();
    // The archive of the stored version is compressed from a tree with the
    // times of this run, so its size changes with the clock.
    let stored_size: u64 = fs::read_dir(world.collection().path(".antiq/pieces/old/client-site"))
        .expect("the piece has a store folder")
        .map(|entry| entry.expect("an entry").metadata().expect("metadata").len())
        .sum();
    let run = world.run_tty_sized(&["push", "--init"], 80, 24, &[("NO_COLOR", "1")]);
    run.expect_code(0);
    let mut screen = crate::support::screen::Screen::new(80, 24);
    screen.feed(run.stdout.as_bytes());
    // The seconds of the times depend on the machine; the tests allow one.
    let lines: Vec<String> = screen
        .lines()
        .into_iter()
        .map(|line| match line.strip_suffix("0:01") {
            Some(rest) => format!("{rest}0:00"),
            None => line,
        })
        .collect();
    let (summary, rest) = (&lines[6], &lines[7..]);
    assert_eq!(
        lines[..6],
        [
            format!("Pushing to {shown}"),
            String::new(),
            format!(
                "  ✔ old/client-site                          {stored_size:>5} B             3 created  0:00"
            ),
            "  ✔ notes/daily                                  6 B             1 created  0:00"
                .to_string(),
            "  ✔ projects/scanner                            43 B             4 created  0:00"
                .to_string(),
            String::new(),
        ]
    );
    assert!(
        summary.starts_with(&format!(
            "✔ Claimed {shown} · 3 pieces pushed · 10 files changed   took 0:0"
        )),
        "{summary}"
    );
    assert_eq!(
        rest,
        [
            "The marker file .antiq-target on the target now records this antiq collection",
            "and host, so another antiq collection cannot push there by mistake.",
        ]
    );
}

#[test]
fn a_push_dry_run_on_a_terminal_tags_its_actions_and_ends_with_the_footer() {
    let clock = crate::support::scenario::collection::Clock::now();
    let world = World::new(
        pushing_setup(&clock),
        crate::support::scenario::world::Mode::Text,
    );
    let run = world.run_tty_env(&["push", "--init", "-n"], &[("NO_COLOR", "1")]);
    run.expect_code(0);
    let (head, rest) = run
        .stdout
        .split_once("\n  ")
        .expect("a piece line follows the change");
    assert_eq!(
        head.split('\n').collect::<Vec<_>>()[..2],
        [
            "○ would write   the marker .antiq-target to the target",
            "○ would mirror  the collection to the target"
        ]
    );
    assert!(
        rest.ends_with(
            "\nDry run: nothing was pushed. Run again without -n to push to the target.\n"
        ),
        "{:?}",
        run.stdout
    );
}

#[test]
fn the_zeros_of_the_claim_table_are_dim() {
    // Files outside every piece have no card, so a claim that sends only
    // those keeps the table of the report.
    let clock = crate::support::scenario::collection::Clock::now();
    let collection = Collection::build(&clock)
        .patterns(&["*/*"])
        .outside("README.md", "the collection\n")
        .finish();
    let world = World::new(
        Setup::new(collection)
            .with_target(Target::new())
            .with_real_rsync(),
        crate::support::scenario::world::Mode::Text,
    );
    let run = world.run_tty_env(&["push", "--init"], &[]);
    run.expect_code(0);
    assert!(
        run.stdout
            .contains("other files         2         \x1b[2m0\x1b[0m"),
        "{:?}",
        run.stdout
    );
}

#[test]
fn push_makes_a_copy_and_a_second_push_removes_what_is_gone() {
    both_modes(pushing_setup, |world| {
        let target = world.target().expect("a target").path();
        let shown = target.display().to_string();
        let marker_path = target.join(".antiq-target");

        // An empty directory without a marker is not used until `--init`
        // says it is the right one.
        world.assert_unchanged(|| {
            let refused = world.run(&["push"]);
            refused.expect_code(4);
            assert!(said(&refused).contains("push --init"), "{}", said(&refused));
            refused.json_only(|json| assert_eq!(json["error"]["kind"], "target_unreachable"));
        });

        let init = world.run(&["push", "--init"]);
        init.expect_code(0);
        init.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "\
Claimed the push target {shown} for this antiq collection.
Changed on {shown}:
  notes/daily: 1 created, 0 updated, 0 deleted
  old/client-site: 3 created, 0 updated, 0 deleted
  projects/scanner: 4 created, 0 updated, 0 deleted
  other files: 2 created, 0 updated, 0 deleted
"
                )
            );
        });
        init.keys(&REAL_KEYS);
        init.json_only(|json| {
            assert_eq!(json["result"], "pushed");
            assert_eq!(json["target"], shown.as_str());
            assert_eq!(json["initialized"], true);
            assert_eq!(json["forced"], false);
            assert_eq!(
                json["pieces"],
                json!([
                    {"piece": "notes/daily", "created": 1, "updated": 0, "deleted": 0},
                    {"piece": "old/client-site", "created": 3, "updated": 0, "deleted": 0},
                    {"piece": "projects/scanner", "created": 4, "updated": 0, "deleted": 0},
                ])
            );
            assert_eq!(
                json["other"],
                json!({"created": 2, "updated": 0, "deleted": 0})
            );
            assert_eq!(json["vanished"], json!([]));
            assert_eq!(json["second_pass"], false);
        });

        // The target is a copy, hard links included, and none of the
        // scratch areas travelled.
        assert_mirrors(world);
        assert!(same_file(
            &target.join("projects/scanner/src/main.rs"),
            &target.join("projects/scanner/src/copy.rs")
        ));
        for left_out in [".antiq/tmp", ".antiq/trash", ".antiq/doomed", ".antiq/lock"] {
            assert!(!target.join(left_out).exists(), "{left_out} was mirrored");
        }

        // The marker is the line the spec gives, readable by its owner.
        assert_eq!(
            fs::read(&marker_path).expect("the marker exists"),
            expected_marker(world)
        );
        let mode = fs::metadata(&marker_path)
            .expect("the marker exists")
            .mode();
        assert_eq!(mode & 0o600, 0o600);
        assert_eq!(mode & 0o111, 0);

        // Nothing changed, so nothing is sent.
        let again = world.run(&["push"]);
        again.expect_code(0);
        again.text_only(|out| assert_eq!(out, format!("Pushed to {shown}. Nothing changed.\n")));
        again.keys(&REAL_KEYS);
        again.json_only(|json| {
            assert_eq!(json["pieces"], json!([]));
            assert_eq!(
                json["other"],
                json!({"created": 0, "updated": 0, "deleted": 0})
            );
            assert_eq!(json["initialized"], false);
        });

        // `--init` is for an empty directory, whoever's marker is there.
        world.assert_unchanged(|| {
            let refused = world.run(&["push", "--init"]);
            refused.expect_code(1);
            assert!(said(&refused).contains("--init"), "{}", said(&refused));
        });

        // What the collection loses, the next push removes on the target.
        let root = world.collection().root();
        fs::remove_file(root.join("projects/scanner/notes.txt")).expect("the file is there");
        fs::write(root.join("projects/scanner/new.txt"), "fresh\n").expect("written");
        fs::remove_dir_all(root.join("notes/daily")).expect("the folder is there");
        fs::remove_file(root.join("README.md")).expect("the file is there");
        let pruned = world.run(&["push"]);
        pruned.expect_code(0);
        pruned.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "\
Changed on {shown}:
  projects/scanner: 1 created, 0 updated, 1 deleted
  other files: 0 created, 0 updated, 2 deleted
"
                )
            );
        });
        pruned.json_only(|json| {
            assert_eq!(
                json["pieces"],
                json!([{"piece": "projects/scanner", "created": 1, "updated": 0, "deleted": 1}])
            );
            assert_eq!(
                json["other"],
                json!({"created": 0, "updated": 0, "deleted": 2})
            );
            assert_eq!(json["second_pass"], false);
        });
        assert_mirrors(world);
        assert!(!target.join("projects/scanner/notes.txt").exists());
        assert!(!target.join("notes/daily").exists());
        assert_eq!(
            fs::read(&marker_path).expect("the marker survived"),
            expected_marker(world)
        );
    });
}

/// Whether the volume below `near` treats names that differ only in case
/// as one name. The probe is a directory of its own, so the target stays
/// empty.
fn folds_case(near: &std::path::Path) -> bool {
    let probe = tempfile::tempdir_in(near.parent().expect("the target has a parent"))
        .expect("the probe directory is made");
    fs::write(probe.path().join("probe"), "x").expect("the probe file is written");
    probe.path().join("PROBE").exists()
}

#[test]
fn push_mirrors_a_folder_whose_name_changes_only_in_case() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/Scanner", |piece| {
                    piece
                        .age_days(10)
                        .file("src/main.rs", "fn main() {}\n")
                        .file("notes.txt", "to do\n");
                })
                .finish();
            Setup::new(collection)
                .with_target(Target::new())
                .with_real_rsync()
        },
        |world| {
            let target = world.target().expect("a target").path();
            world.run(&["push", "--init"]).expect_code(0);
            assert_mirrors(world);

            let projects = world.collection().path("projects");
            fs::rename(projects.join("Scanner"), projects.join("scanner"))
                .expect("the folder is renamed");

            let renamed = world.run(&["push"]);
            renamed.expect_code(0);
            renamed.keys(&REAL_KEYS);
            // A volume that folds case makes rsync skip the new spelling
            // while it deletes the old one, which the second pass repairs.
            renamed.json_only(|json| {
                assert_eq!(json["second_pass"], folds_case(&target));
            });

            let names: Vec<String> = fs::read_dir(target.join("projects"))
                .expect("the target lists")
                .map(|entry| {
                    entry
                        .expect("an entry")
                        .file_name()
                        .into_string()
                        .expect("UTF-8")
                })
                .collect();
            assert_eq!(names, vec!["scanner".to_string()]);
            assert_mirrors(world);
            assert_eq!(
                fs::read_to_string(target.join("projects/scanner/src/main.rs")).expect("copied"),
                "fn main() {}\n"
            );
        },
    );
}

#[test]
fn a_scheduled_run_with_the_target_missing_archives_and_exits_0() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/scanner", |piece| {
                    piece.age_days(40).file("src/main.rs", "fn main() {}\n");
                })
                .finish();
            let target = Target::new();
            target.marker(&collection);
            Setup::new(collection).with_target(target).with_real_rsync()
        },
        |world| {
            let target = world.target().expect("a target");
            target.remove();

            // The scheduler runs the two commands one after the other.
            let archived = world.run(&["archive"]);
            archived.expect_code(0);
            assert_eq!(world.collection().versions("projects/scanner").len(), 1);
            assert!(!world.collection().path("projects/scanner").exists());

            let pushed = world.run(&["push", "--if-reachable"]);
            pushed.expect_code(0);
            pushed.text_only(|out| {
                assert_eq!(out, "");
                assert!(
                    pushed.stderr.starts_with("note: "),
                    "stderr: {:?}",
                    pushed.stderr
                );
            });
            pushed.keys(&[
                "format", "command", "result", "target", "reason", "repairs", "warnings",
            ]);
            pushed.json_only(|json| {
                assert_eq!(json["result"], "unreachable");
                assert_eq!(json["target"], target.path().to_str().expect("UTF-8"));
                let reason = json["reason"].as_str().expect("a reason");
                assert!(!reason.is_empty());
            });

            // Without the flag the same situation is an error.
            let failed = world.run(&["push"]);
            failed.expect_code(4);
            failed.json_only(|json| assert_eq!(json["error"]["kind"], "target_unreachable"));

            // antiq never creates the missing directory.
            assert!(!target.path().exists());

            // `--init` and `--force` are a statement about a target that
            // is there, so they do not combine with a flag that tolerates
            // its absence.
            for flag in ["--init", "--force"] {
                let refused = world.run(&["push", flag, "--if-reachable"]);
                refused.expect_code(2);
                refused.json_only(|json| assert_eq!(json["error"]["kind"], "usage"));
            }
            assert!(!target.path().exists());
        },
    );
}

#[test]
fn a_copy_used_elsewhere_is_refused_and_force_pushes_without_taking_the_target_over() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/scanner", |piece| {
                    piece.age_days(10).file("src/main.rs", "fn main() {}\n");
                })
                .finish();
            Setup::new(collection)
                .with_target(Target::new())
                .with_real_rsync()
        },
        |world| {
            let target = world.target().expect("a target");
            let marker = target.path().join(".antiq-target");
            let id = world.collection().id();
            let machine = target.machine_name();
            let root = world.collection().root();
            let root = root.to_str().expect("UTF-8");
            world.run(&["push", "--init"]).expect_code(0);

            // The marker of the same collection, written by another machine
            // and by this one from another path.
            let elsewhere = [
                ("Other Mac", root, "Other Mac", machine.as_str()),
                (
                    machine.as_str(),
                    "/elsewhere/projects",
                    "/elsewhere/projects",
                    root,
                ),
            ];
            for (round, (host, path, named, current)) in elsewhere.into_iter().enumerate() {
                target.marker_with(&id, host, path);
                let before = fs::read(&marker).expect("the marker exists");

                world.assert_unchanged(|| {
                    let refused = world.run(&["push"]);
                    refused.expect_code(1);
                    let text = said(&refused);
                    assert!(text.contains(named), "the refusal names {named}: {text}");
                    assert!(
                        text.contains(current),
                        "the refusal names {current}: {text}"
                    );
                });

                // The forced push mirrors whatever the collection holds now
                // and leaves the marker as it was.
                let name = format!("round-{round}.txt");
                fs::write(
                    world.collection().path(&format!("projects/scanner/{name}")),
                    "x",
                )
                .expect("written");
                let forced = world.run(&["push", "--force"]);
                forced.expect_code(0);
                forced.keys(&REAL_KEYS);
                forced.json_only(|json| {
                    assert_eq!(json["forced"], true);
                    assert_eq!(json["initialized"], false);
                });
                assert!(target.path().join("projects/scanner").join(&name).is_file());
                assert_eq!(fs::read(&marker).expect("the marker exists"), before);
            }

            // Another collection's marker stays a refusal under `--force`.
            target.marker_with("01JBXOTHERCOLLECTION00000", &machine, root);
            world.assert_unchanged(|| {
                world.run(&["push", "--force"]).expect_code(1);
            });

            // After the user edits the marker by hand, the push is plain.
            target.marker(world.collection());
            let plain = world.run(&["push"]);
            plain.expect_code(0);
            plain.json_only(|json| {
                assert_eq!(json["forced"], false);
                assert_eq!(json["notes"], serde_json::json!([]));
            });
            assert_mirrors(world);
        },
    );
}
