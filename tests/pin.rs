// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq pin` and `antiq unpin` seen from outside the binary, on
//! collections the scenario harness writes by hand. Every case runs in text
//! mode and in JSON mode.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use support::cmd::{Antiq, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

fn world(mode: Mode, build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

/// `projects/alpha`, and two pieces whose names both contain `scan`.
fn three_pieces(builder: &mut CollectionBuilder) {
    builder.patterns(&["projects/*", "tools/*"]);
    for piece in ["projects/alpha", "projects/scanner", "tools/scan-old"] {
        builder.piece(piece, |piece| {
            piece.age_days(40).file("data", "x");
        });
    }
}

fn both(script: impl Fn(&World)) {
    for mode in [Mode::Text, Mode::Json] {
        script(&world(mode, three_pieces));
    }
}

fn pin_file(world: &World, piece: &str) -> PathBuf {
    world.collection().path(&format!("{piece}/.antiq-pin"))
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// Makes `path` read-only, and writable again when dropped, also after a
/// panic, so the temporary directory can always be removed.
struct ReadOnly(PathBuf);

impl ReadOnly {
    fn make(path: &Path) -> ReadOnly {
        fs::set_permissions(path, fs::Permissions::from_mode(0o555)).expect("read-only");
        assert!(
            fs::write(path.join("probe"), "").is_err(),
            "the test needs a user that a read-only directory stops"
        );
        ReadOnly(path.to_path_buf())
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

// =========================================================
// Pinning and unpinning
// =========================================================

#[test]
fn pinning_by_path_writes_an_empty_pin_file() {
    both(|world| {
        let run = world.run(&["pin", "projects/alpha"]);
        run.expect_code(0).keys(&[
            "format", "command", "piece", "pinned", "changed", "repairs", "warnings",
        ]);
        run.text_only(|out| assert_eq!(out, "Pinned projects/alpha\n"));
        run.json_only(|json| {
            assert_eq!(json["command"], "pin");
            assert_eq!(json["piece"], "projects/alpha");
            assert_eq!(json["pinned"], true);
            assert_eq!(json["changed"], true);
            assert_eq!(json["repairs"], Value::Array(Vec::new()));
            assert_eq!(json["warnings"], Value::Array(Vec::new()));
        });
        assert_eq!(run.stderr, "");
        assert_eq!(
            fs::read(pin_file(world, "projects/alpha")).expect("the pin"),
            b""
        );
        assert!(!pin_file(world, "projects/scanner").exists());
    });
}

#[test]
fn pinning_by_search_word_takes_the_one_piece_it_matches() {
    both(|world| {
        let run = world.run(&["pin", "alph"]);
        run.expect_code(0);
        run.text_only(|out| assert_eq!(out, "Pinned projects/alpha\n"));
        assert!(pin_file(world, "projects/alpha").exists());
    });
}

#[test]
fn pinning_twice_says_so_and_leaves_the_pin() {
    both(|world| {
        world.run(&["pin", "projects/alpha"]).expect_code(0);
        let run = world.run(&["pin", "projects/alpha"]);
        run.expect_code(0);
        run.text_only(|out| assert_eq!(out, "projects/alpha is already pinned\n"));
        run.json_only(|json| {
            assert_eq!(json["pinned"], true);
            assert_eq!(json["changed"], false);
        });
        assert!(pin_file(world, "projects/alpha").exists());
    });
}

#[test]
fn unpinning_removes_the_pin_file_and_unpinning_again_says_so() {
    both(|world| {
        world.run(&["pin", "projects/alpha"]).expect_code(0);

        let run = world.run(&["unpin", "projects/alpha"]);
        run.expect_code(0).keys(&[
            "format", "command", "piece", "pinned", "changed", "repairs", "warnings",
        ]);
        run.text_only(|out| assert_eq!(out, "Unpinned projects/alpha\n"));
        run.json_only(|json| {
            assert_eq!(json["command"], "unpin");
            assert_eq!(json["pinned"], false);
            assert_eq!(json["changed"], true);
        });
        assert!(!pin_file(world, "projects/alpha").exists());

        let run = world.run(&["unpin", "projects/alpha"]);
        run.expect_code(0);
        run.text_only(|out| assert_eq!(out, "projects/alpha is not pinned\n"));
        run.json_only(|json| assert_eq!(json["changed"], false));
    });
}

#[test]
fn a_symlink_pin_is_removed_and_its_target_stays() {
    both(|world| {
        let outside = world.collection().path("outside");
        fs::write(&outside, "keep").expect("the target");
        std::os::unix::fs::symlink("../../outside", pin_file(world, "projects/alpha"))
            .expect("the symlink");

        world.run(&["unpin", "projects/alpha"]).expect_code(0);

        assert!(fs::symlink_metadata(pin_file(world, "projects/alpha")).is_err());
        assert_eq!(fs::read(&outside).expect("the target stays"), b"keep");
    });
}

#[test]
fn a_top_without_owner_write_is_pinned_and_unpinned_and_keeps_its_mode() {
    both(|world| {
        let top = world.collection().path("projects/alpha");
        let _restore = ReadOnly::make(&top);

        world.run(&["pin", "projects/alpha"]).expect_code(0);
        assert!(pin_file(world, "projects/alpha").exists());
        world.run(&["unpin", "projects/alpha"]).expect_code(0);

        assert!(!pin_file(world, "projects/alpha").exists());
        let mode = fs::metadata(&top).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o7777, 0o555);
    });
}

#[test]
fn a_relative_reference_starts_at_the_directory_of_the_collection_lookup() {
    both(|world| {
        let projects = world.collection().path("projects");
        let run = Antiq::new()
            .args(["-C"])
            .arg(&projects)
            .args(["pin", "alpha"])
            .run();
        assert_eq!(run.code(), 0, "{}", run.stderr);
        assert!(pin_file(world, "projects/alpha").exists());
    });
}

// =========================================================
// Which piece is meant
// =========================================================

fn refused(
    world: &World,
    args: &[&str],
    code: i32,
    kind: &str,
) -> support::scenario::world::Outcome {
    let before = snapshot(&world.collection().root());
    let run = world.run(args);
    run.expect_code(code);
    assert_eq!(
        run.error_kind().as_deref().unwrap_or(kind),
        kind,
        "{args:?}\nstderr: {:?}",
        run.stderr
    );
    run.text_only(|out| assert_eq!(out, "", "{args:?}"));
    let mut after = snapshot(&world.collection().root());
    let mut before = before;
    before.remove(".antiq/lock");
    after.remove(".antiq/lock");
    assert_eq!(after, before, "{args:?} changed the collection");
    run
}

#[test]
fn a_reference_to_no_live_piece_is_an_unknown_reference() {
    both(|world| {
        fs::create_dir(world.collection().path("docs")).expect("a folder no pattern selects");
        for command in ["pin", "unpin"] {
            for given in ["projects/gone", "docs", "zzzzzz"] {
                let run = refused(world, &[command, given], 1, "unknown_reference");
                run.text_only(|_| assert!(run.stderr.contains("error:"), "{:?}", run.stderr));
            }
        }
    });
}

#[test]
fn an_ambiguous_word_lists_the_candidates() {
    both(|world| {
        for command in ["pin", "unpin"] {
            let run = refused(world, &[command, "scan"], 1, "ambiguous_reference");
            run.json_only(|json| {
                assert_eq!(
                    json["error"]["candidates"],
                    serde_json::json!([{"path": "projects/scanner"}, {"path": "tools/scan-old"}])
                );
            });
            run.text_only(|_| ());
        }
    });
}

#[test]
fn a_path_outside_the_collection_is_refused() {
    both(|world| {
        for command in ["pin", "unpin"] {
            refused(world, &[command, "/"], 1, "outside_collection");
        }
    });
}

#[test]
fn an_empty_or_missing_piece_is_a_usage_error() {
    both(|world| {
        for command in ["pin", "unpin"] {
            refused(world, &[command, ""], 2, "usage");
            refused(world, &[command], 2, "usage");
            refused(world, &[command, "-n", ""], 2, "usage");
        }
    });
}

#[test]
fn a_directory_with_the_pin_name_is_refused_and_kept() {
    both(|world| {
        let directory = pin_file(world, "projects/alpha");
        fs::create_dir(&directory).expect("the directory");
        fs::write(directory.join("keep"), "x").expect("a file in it");
        for command in ["pin", "unpin"] {
            for flags in [&[][..], &["-n"][..]] {
                let mut args = vec![command, "projects/alpha"];
                args.extend_from_slice(flags);
                let run = refused(world, &args, 1, "other");
                assert!(
                    run.stderr.contains(".antiq-pin") || run.stdout.contains(".antiq-pin"),
                    "stdout {:?} stderr {:?}",
                    run.stdout,
                    run.stderr
                );
            }
        }
        assert!(directory.join("keep").exists());
    });
}

// =========================================================
// The run
// =========================================================

#[test]
fn the_repairs_run_first_and_are_reported() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_pieces);
        let leftover = world.collection().path(".antiq/tmp/leftover");
        fs::write(&leftover, "x").expect("the leftover");

        let run = world.run(&["pin", "projects/alpha"]);

        run.expect_code(0);
        assert!(!leftover.exists());
        run.text_only(|_| {
            assert_eq!(run.stderr, "note: repaired: delete .antiq/tmp/leftover\n");
        });
        run.json_only(|json| {
            assert_eq!(
                json["repairs"],
                serde_json::json!([
                    {"kind": "repair", "repair": "delete_temporary", "entry": "leftover"}
                ])
            );
        });
    }
}

#[test]
fn a_held_lock_fails_the_run_and_changes_nothing() {
    both(|world| {
        let _lock = hold_lock(&world.collection().root());
        for command in ["pin", "unpin"] {
            let run = refused(world, &[command, "projects/alpha"], 1, "lock_held");
            run.json_only(|json| {
                assert_eq!(json["error"]["pid"], std::process::id());
                assert!(json["error"]["started"].is_string());
            });
            run.text_only(|_| {
                assert!(
                    run.stderr.contains("holds the lock"),
                    "the error names the held lock: {:?}",
                    run.stderr
                );
            });
        }
    });
}

#[test]
fn the_notes_on_the_settings_come_with_a_real_run_and_a_dry_run() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_pieces);
        let config = world.collection().path(".antiq/config.toml");
        let mut text = fs::read_to_string(&config).expect("the config");
        text.push_str("colour = \"red\"\n");
        fs::write(&config, text).expect("the config is written");
        let note = "unknown setting `colour` under [collection], which antiq ignores";

        for args in [
            &["pin", "projects/alpha", "-n"][..],
            &["pin", "projects/alpha"][..],
        ] {
            let run = world.run(args);
            run.expect_code(0);
            run.text_only(|_| assert_eq!(run.stderr, format!("warning: {note}\n")));
            run.json_only(|json| {
                assert_eq!(
                    json["warnings"],
                    serde_json::json!([{
                        "code": "antiq::config::unknown_key",
                        "message": note,
                        "fix": "Correct the name or remove the line in `.antiq/config.toml`."
                    }])
                );
            });
        }
    }
}

// =========================================================
// Dry run
// =========================================================

#[test]
fn a_dry_run_says_what_it_would_do_and_changes_nothing() {
    both(|world| {
        world.assert_unchanged(|| {
            let run = world.run(&["pin", "projects/alpha", "-n"]);
            run.expect_code(0).keys(&[
                "format", "command", "dry_run", "piece", "pinned", "changed", "actions", "warnings",
            ]);
            run.text_only(|out| assert_eq!(out, "Would pin projects/alpha\n"));
            run.json_only(|json| {
                assert_eq!(json["dry_run"], true);
                assert_eq!(json["piece"], "projects/alpha");
                assert_eq!(json["pinned"], true);
                assert_eq!(json["changed"], true);
                assert_eq!(
                    json["actions"],
                    serde_json::json!([{"kind": "pin", "piece": "projects/alpha"}])
                );
            });
            assert_eq!(run.stderr, "");
        });
        assert!(!pin_file(world, "projects/alpha").exists());

        world.run(&["pin", "projects/alpha"]).expect_code(0);
        world.assert_unchanged(|| {
            let run = world.run(&["unpin", "alph", "-n"]);
            run.expect_code(0);
            run.text_only(|out| assert_eq!(out, "Would unpin projects/alpha\n"));
            run.json_only(|json| {
                assert_eq!(json["command"], "unpin");
                assert_eq!(json["pinned"], false);
                assert_eq!(
                    json["actions"],
                    serde_json::json!([{"kind": "unpin", "piece": "projects/alpha"}])
                );
            });
        });
        assert!(pin_file(world, "projects/alpha").exists());
    });
}

#[test]
fn a_dry_run_of_the_state_that_already_holds_says_so() {
    both(|world| {
        world.assert_unchanged(|| {
            let run = world.run(&["unpin", "projects/alpha", "-n"]);
            run.expect_code(0);
            run.text_only(|out| assert_eq!(out, "projects/alpha is not pinned\n"));
            run.json_only(|json| {
                assert_eq!(json["changed"], false);
                assert_eq!(json["actions"], Value::Array(Vec::new()));
            });
        });
        world.run(&["pin", "projects/alpha"]).expect_code(0);
        world.assert_unchanged(|| {
            let run = world.run(&["pin", "projects/alpha", "-n"]);
            run.expect_code(0);
            run.text_only(|out| assert_eq!(out, "projects/alpha is already pinned\n"));
            run.json_only(|json| {
                assert_eq!(json["pinned"], true);
                assert_eq!(json["changed"], false);
            });
        });
    });
}

#[test]
fn a_dry_run_lists_the_repairs_first_and_does_not_do_them() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_pieces);
        let leftover = world.collection().path(".antiq/tmp/leftover");
        fs::write(&leftover, "x").expect("the leftover");

        world.assert_unchanged(|| {
            let run = world.run(&["pin", "projects/alpha", "-n"]);
            run.expect_code(0);
            run.text_only(|out| {
                assert_eq!(
                    out,
                    "Would delete .antiq/tmp/leftover\nWould pin projects/alpha\n"
                );
            });
            run.json_only(|json| {
                let actions = json["actions"].as_array().expect("a list");
                assert_eq!(actions.len(), 2);
                assert_eq!(actions[0]["kind"], "repair");
                assert_eq!(actions[0]["repair"], "delete_temporary");
                assert_eq!(actions[1]["kind"], "pin");
                assert_eq!(keys(&actions[1]), ["kind", "piece"]);
            });
        });
        assert!(leftover.exists());
    }
}

#[test]
fn a_dry_run_assumes_a_folder_in_trash_was_given_back() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, |builder| {
            three_pieces(builder);
            builder.patterns(&["projects/*", "tools/*", "gamma"]);
        });
        let stored = world
            .collection()
            .path(".antiq/trash/2026-11-04T090211/gamma");
        fs::create_dir_all(&stored).expect("the trashed folder");
        fs::write(stored.join("data"), "x").expect("a file");

        world.assert_unchanged(|| {
            let run = world.run(&["pin", "gamma", "-n"]);
            run.expect_code(0);
            run.text_only(|out| assert!(out.ends_with("Would pin gamma\n"), "{out:?}"));
            run.json_only(|json| {
                let actions = json["actions"].as_array().expect("a list");
                assert_eq!(actions[0]["repair"], "give_back");
                assert_eq!(
                    actions.last().expect("an action"),
                    &serde_json::json!({"kind": "pin", "piece": "gamma"})
                );
            });
        });

        world.run(&["pin", "gamma"]).expect_code(0);
        assert!(pin_file(&world, "gamma").exists());
    }
}

#[test]
fn a_dry_run_that_finds_the_lock_held_names_the_holder_and_lists_no_repairs() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, three_pieces);
        fs::write(world.collection().path(".antiq/tmp/leftover"), "x").expect("written");
        let _lock = hold_lock(&world.collection().root());
        let before = snapshot(&world.collection().root());

        let run = world.run(&["pin", "projects/alpha", "-n"]);

        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "lock_held", "actions"]);
        run.text_only(|out| {
            let line = format!("The lock is held by pid {}, started ", std::process::id());
            assert!(out.starts_with(&line), "{out:?}");
            assert!(out.ends_with(". No repairs are listed.\n"), "{out:?}");
            assert_eq!(out.lines().count(), 1);
        });
        run.json_only(|json| {
            assert_eq!(json["dry_run"], true);
            assert_eq!(json["lock_held"]["pid"], std::process::id());
            assert!(json["lock_held"]["started"].is_string());
            assert_eq!(json["actions"], Value::Array(Vec::new()));
        });
        assert_eq!(run.stderr, "");
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn a_dry_run_refuses_what_a_real_run_refuses() {
    both(|world| {
        world.assert_unchanged(|| {
            world.run(&["pin", "projects/gone", "-n"]).expect_code(1);
            world.run(&["unpin", "scan", "-n"]).expect_code(1);
            world.run(&["pin", "", "-n"]).expect_code(2);
        });
    });
}
