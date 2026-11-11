// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `antiq unearth` seen from outside the binary, on stores the scenario
//! harness writes by hand. The harness runs every command under
//! `TZ=Pacific/Auckland`, which is +13:00 in November 2026 and +12:00 in
//! September 2026.

mod support;

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use support::cmd::{Antiq, hold_lock, snapshot};
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::world::{Mode, Setup, World};

use serde_json::Value;

const NEW: &str = "2026-11-04T090211";
const OLD: &str = "2026-09-03T121500";

fn world(mode: Mode, build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*", "tools/*"]);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), mode)
}

fn scanner(builder: &mut CollectionBuilder) {
    builder
        .version(
            "projects/scanner",
            OLD,
            &[("src/main.rs", "fn main() {}\n")],
        )
        .version(
            "projects/scanner",
            NEW,
            &[("src/main.rs", "fn main() {}\n"), ("README", "scan\n")],
        )
        .version("tools/scan-old", OLD, &[("run.sh", "echo\n")]);
}

fn marker(world: &World) -> String {
    fs::read_to_string(world.collection().path("projects/scanner/.antiq-restored"))
        .expect("the marker exists")
}

#[test]
fn a_piece_by_path_comes_back_and_its_newest_version_goes() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let run = world.run(&["unearth", "projects/scanner"]);
        run.expect_code(0)
            .keys(&[
                "format",
                "command",
                "unearthed",
                "piece",
                "stamp",
                "version",
                "path",
                "bytes",
                "repairs",
                "warnings",
            ])
            .text_only(|out| {
                assert_eq!(out, format!("Unearthed projects/scanner (version {NEW})\n"));
            })
            .json_only(|json| {
                assert_eq!(json["command"], "unearth");
                assert_eq!(json["unearthed"], true);
                assert_eq!(json["stamp"], NEW);
                assert_eq!(json["version"], "2026-11-04T09:02:11Z");
                assert_eq!(json["path"], "projects/scanner");
            });
        assert!(world.collection().path("projects/scanner/README").exists());
        assert!(marker(&world).starts_with("restored "));
        assert!(!marker(&world).contains("version"), "{}", marker(&world));
        let list = world.run(&["find"]);
        list.text_only(|out| {
            assert!(out.contains(OLD) && !out.contains(NEW), "{out:?}");
        });
    }
}

#[test]
fn a_search_word_that_fits_one_piece_is_offered_and_nothing_is_unearthed() {
    let world = world(Mode::Json, scanner);
    let before = world.collection().versions("tools/scan-old");
    let run = world.run(&["unearth", "scan-old"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("inexact_reference"));
    run.json_only(|json| {
        assert_eq!(
            json["error"]["candidates"],
            serde_json::json!([{"path": "tools/scan-old"}])
        );
    });
    assert!(!world.collection().path("tools/scan-old").exists());
    assert_eq!(world.collection().versions("tools/scan-old"), before);

    world.run(&["unearth", "tools/scan-old"]).expect_code(0);
    assert!(world.collection().path("tools/scan-old/run.sh").exists());
}

#[test]
fn a_version_is_named_by_its_local_date() {
    let world = world(Mode::Text, scanner);
    // 12:15:00 UTC on September 3rd is 00:15 on the 4th at +12:00.
    world
        .run(&["unearth", "projects/scanner", "--version", "2026-09-04"])
        .expect_code(0);
    assert!(
        world
            .collection()
            .path("projects/scanner/src/main.rs")
            .exists()
    );
    assert!(!world.collection().path("projects/scanner/README").exists());
}

#[test]
fn a_dry_run_names_the_version_the_destination_and_the_checks_and_changes_nothing() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let before = snapshot(&world.collection().root());
        let run = world.run(&["unearth", "projects/scanner", "-n"]);
        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "actions", "warnings"])
            .text_only(|out| {
                let lines: Vec<&str> = out.lines().collect();
                assert_eq!(
                    lines[0],
                    format!("Would unearth projects/scanner (version {NEW})")
                );
                assert_eq!(lines.len(), 6, "{out:?}");
                assert!(lines[1].starts_with("  check "), "{out:?}");
                assert!(
                    lines[3].starts_with("  check there is room for "),
                    "{out:?}"
                );
                assert!(lines[4].contains("(not run by -n)"), "{out:?}");
                assert!(lines[5].contains("(not run by -n)"), "{out:?}");
            })
            .json_only(|json| {
                let action = &json["actions"][0];
                assert_eq!(action["kind"], "unearth");
                assert_eq!(action["stamp"], NEW);
                assert_eq!(action["path"], "projects/scanner");
                assert_eq!(
                    action["checks"],
                    serde_json::json!([
                        "version",
                        "destination",
                        "free_space",
                        "member_names",
                        "extracted_tree"
                    ])
                );
            });
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn a_held_lock_stops_a_real_run_and_is_named_by_a_dry_run() {
    let world = world(Mode::Json, scanner);
    let _lock = hold_lock(&world.collection().root());
    let run = world.run(&["unearth", "projects/scanner"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("lock_held"));
    run.json_only(|json| {
        assert!(json["error"]["pid"].is_u64(), "{json}");
        assert!(json["error"]["started"].is_string(), "{json}");
    });
}

#[test]
fn a_dry_run_with_the_lock_held_names_the_holder() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, scanner);
        let _lock = hold_lock(&world.collection().root());
        let before = snapshot(&world.collection().root());

        let run = world.run(&["unearth", "projects/scanner", "-n"]);

        run.expect_code(0)
            .keys(&["format", "command", "dry_run", "lock_held", "actions"]);
        run.text_only(|out| {
            let line = format!("The lock is held by pid {}, started ", std::process::id());
            assert!(out.starts_with(&line), "{out:?}");
            assert_eq!(out.lines().count(), 1);
        });
        run.json_only(|json| {
            assert_eq!(json["dry_run"], true);
            assert_eq!(json["lock_held"]["pid"], std::process::id());
            assert!(json["lock_held"]["started"].is_string());
            assert_eq!(json["actions"], Value::Array(Vec::new()));
        });
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn a_damaged_version_is_refused_with_the_three_remedies() {
    let world = world(Mode::Text, scanner);
    world
        .collection()
        .damage_truncate_archive("projects/scanner", NEW);
    let run = world.run(&["unearth", "projects/scanner"]);
    run.expect_code(1);
    for word in ["fork", "rebuild", "copied back"] {
        assert!(run.stderr.contains(word), "{word}: {:?}", run.stderr);
    }
    assert!(!world.collection().path("projects").exists());
}

#[test]
fn an_ambiguous_word_lists_its_candidates() {
    let world = world(Mode::Json, scanner);
    let run = world.run(&["unearth", "scan"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("ambiguous_reference"));
    run.json_only(|json| {
        assert_eq!(
            json["error"]["candidates"].as_array().map(Vec::len),
            Some(2)
        );
    });
}

#[test]
fn an_unknown_piece_and_an_unknown_version_are_refused() {
    let world = world(Mode::Json, scanner);
    let run = world.run(&["unearth", "nothing-like-it"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("unknown_reference"));
    let run = world.run(&["unearth", "projects/scanner", "--version", "1999"]);
    run.expect_code(1);
    run.json_only(|json| {
        assert!(
            json["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(NEW)),
            "{json}"
        );
    });
    let run = world.run(&["unearth", "projects/scanner", "--version", "2026"]);
    run.expect_code(1);
}

#[test]
fn an_empty_piece_is_a_usage_error() {
    let world = world(Mode::Json, scanner);
    let run = world.run(&["unearth", ""]);
    run.expect_code(2);
    assert_eq!(run.error_kind().as_deref(), Some("usage"));
}

#[test]
fn a_taken_destination_is_refused() {
    let world = world(Mode::Text, scanner);
    fs::create_dir_all(world.collection().path("projects/scanner")).expect("takes the place");
    let run = world.run(&["unearth", "projects/scanner"]);
    run.expect_code(1);
    assert!(run.stderr.contains("it is taken"), "{:?}", run.stderr);
}

/// The listing of the only version says its file is far larger than any
/// volume has room for.
fn oversize_listing(world: &World) {
    world.oversize_listing("projects/scanner", NEW);
}

#[test]
fn a_dry_run_that_finds_no_room_names_the_skip_and_exits_3() {
    for mode in [Mode::Text, Mode::Json] {
        let world = world(mode, |b| {
            b.version("projects/scanner", NEW, &[("a.txt", "hello\n")]);
        });
        oversize_listing(&world);
        let before = snapshot(&world.collection().root());

        let run = world.run(&["unearth", "projects/scanner", "-n"]);

        run.expect_code(3);
        run.text_only(|out| {
            assert_eq!(
                out,
                format!(
                    "Would skip projects/scanner (version {NEW}): 9000 TB needed, {} free\n",
                    out.rsplit_once(", ")
                        .and_then(|(_, rest)| rest.strip_suffix(" free\n"))
                        .expect("the free space ends the line")
                )
            );
        });
        run.json_only(|json| {
            let skipped = &json["actions"][0]["skipped"];
            assert_eq!(skipped["reason"], "no_space");
            assert_eq!(skipped["needed"], 9_000_000_000_000_000_u64);
            assert!(skipped["available"].is_u64(), "{json}");
        });
        assert_eq!(snapshot(&world.collection().root()), before);
    }
}

#[test]
fn a_version_that_does_not_fit_is_skipped_with_exit_3_and_nothing_changes() {
    let world = world(Mode::Json, |b| {
        b.version("projects/scanner", NEW, &[("a.txt", "hello\n")]);
    });
    oversize_listing(&world);
    let before = snapshot(&world.collection().root());

    let run = world.run(&["unearth", "projects/scanner"]);
    let mut after = snapshot(&world.collection().root());
    // A real run leaves the lock file behind.
    after.remove(".antiq/lock");

    run.expect_code(3).json_only(|json| {
        assert_eq!(json["unearthed"], false);
        assert_eq!(json["skipped"]["reason"], "no_space");
        assert_eq!(json["skipped"]["needed"], 9_000_000_000_000_000_u64);
    });
    assert_eq!(after, before);
    assert!(
        fs::read_dir(world.collection().path(".antiq/tmp"))
            .expect("lists")
            .all(|item| item.expect("item").file_name() == "pack"),
        "nothing is left in tmp/"
    );
}

#[test]
fn a_listing_of_a_newer_format_is_refused_with_its_own_kind() {
    let world = world(Mode::Json, scanner);
    fs::write(
        world
            .collection()
            .version_files("projects/scanner", NEW)
            .listing,
        zstd::stream::encode_all(&b"# antiq listing 99\n#\n"[..], 3).expect("compresses"),
    )
    .expect("writes");
    let run = world.run(&["unearth", "projects/scanner"]);
    run.expect_code(1);
    assert_eq!(run.error_kind().as_deref(), Some("format_too_new"));
}

#[test]
fn a_terminal_names_the_version_by_its_local_time() {
    let world = world(Mode::Text, scanner);
    let dry = world.run_tty(&["unearth", "projects/scanner", "-n"]);
    dry.expect_code(0);
    // 09:02:11 UTC is 22:02:11 at +13:00.
    let plain = dry.plain();
    assert!(
        plain.contains("○ would unearth  projects/scanner (version 2026-11-04 22:02:11)"),
        "{plain:?}"
    );
    assert_eq!(
        plain.lines().nth(1),
        Some("Checks the real run makes first:")
    );
    assert_eq!(
        plain.lines().nth(2),
        Some("  the version 2026-11-04 22:02:11 is sound"),
        "{:?}",
        dry.stdout
    );
    let run = world.run_tty_sized(
        &["unearth", "projects/scanner"],
        120,
        24,
        &[("NO_COLOR", "1")],
    );
    run.expect_code(0);
    assert!(
        run.stdout
            .contains("projects/scanner version 2026-11-04 22:02:11"),
        "{:?}",
        run.stdout
    );
}

#[test]
fn the_offset_tells_apart_versions_that_render_alike_on_a_terminal() {
    // New Zealand's summer time ends at 03:00 on 2026-04-05, so 02:30 happens twice.
    let world = world(Mode::Text, |builder| {
        builder
            .version("projects/notes", "2026-04-04T133000", &[("a.txt", "a")])
            .version("projects/notes", "2026-04-04T143000", &[("a.txt", "a")]);
    });
    let run = world.run_tty_sized(
        &["unearth", "projects/notes"],
        120,
        24,
        &[("NO_COLOR", "1")],
    );
    run.expect_code(0);
    assert!(
        run.stdout.contains("version 2026-04-05 02:30:00 +12:00"),
        "{:?}",
        run.stdout
    );
}

#[test]
fn a_text_run_that_does_not_fit_says_so_on_stderr() {
    let world = world(Mode::Text, |b| {
        b.version("projects/scanner", NEW, &[("a.txt", "hello\n")]);
    });
    oversize_listing(&world);
    let run = world.run(&["unearth", "projects/scanner"]);
    run.expect_code(3);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr
            .starts_with("Skipped projects/scanner: 9000 TB needed, "),
        "{:?}",
        run.stderr
    );
}

fn after_a_died_unearth(builder: &mut CollectionBuilder) {
    scanner(builder);
    builder
        .piece("projects/died", |piece| {
            piece.file("a.txt", "a").restored(3, Some(NEW));
        })
        .version("projects/died", NEW, &[("a.txt", "a")]);
}

#[test]
fn the_repairs_of_the_start_are_told_before_and_after_the_run() {
    let world = world(Mode::Text, after_a_died_unearth);
    let dry = world.run(&["unearth", "tools/scan-old", "-n"]);
    dry.expect_code(0);
    let lines: Vec<&str> = dry.stdout.lines().collect();
    assert!(
        lines[0].starts_with("Would delete version "),
        "{:?}",
        dry.stdout
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("Would unearth tools/scan-old")),
        "{:?}",
        dry.stdout
    );

    let run = world.run(&["unearth", "tools/scan-old"]);
    run.expect_code(0);
    assert!(
        run.stderr.starts_with("note: repaired: delete version "),
        "{:?}",
        run.stderr
    );
    assert!(
        run.stdout.starts_with("Unearthed tools/scan-old"),
        "{:?}",
        run.stdout
    );
}

#[test]
fn a_refusal_a_repair_may_lift_is_a_note_of_the_dry_run() {
    let world = world(Mode::Text, after_a_died_unearth);
    let dry = world.run(&["unearth", "projects/died", "-n"]);
    dry.expect_code(0);
    assert!(
        dry.stderr
            .contains("warning: cannot be judged before the repairs run"),
        "{:?}",
        dry.stderr
    );
}

#[test]
fn a_dry_run_judges_the_repairs_by_the_path_the_run_would_resolve() {
    let world = world(Mode::Text, after_a_died_unearth);
    let projects = world.collection().path("projects");

    let from_below = world.run_in(&projects, &["unearth", "died", "-n"]);
    from_below.expect_code(0);
    assert!(
        from_below
            .stderr
            .contains("warning: cannot be judged before the repairs run"),
        "{:?}",
        from_below.stderr
    );

    // The dry run comes first: a real run repairs the store.
    let dry = world.run_in(&projects, &["unearth", "projects/died", "-n"]);
    let real = world.run_in(&projects, &["unearth", "projects/died"]);
    real.expect_code(1);
    dry.expect_code(1);
}

/// What a shell makes of `$'...'`: the bytes between the quotes, with the
/// escapes `\\`, `\'` and `\xHH` resolved.
fn bytes_of_dollar_quote(quoted: &str) -> Vec<u8> {
    let inner = quoted
        .strip_prefix("$'")
        .and_then(|rest| rest.strip_suffix('\''))
        .expect("a $'...' word");
    let raw = inner.as_bytes();
    let mut bytes = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        if raw[at] != b'\\' {
            bytes.push(raw[at]);
            at += 1;
        } else if raw[at + 1] == b'x' {
            let hex = std::str::from_utf8(&raw[at + 2..at + 4]).expect("two hex digits");
            bytes.push(u8::from_str_radix(hex, 16).expect("hex digits"));
            at += 4;
        } else {
            bytes.push(raw[at + 1]);
            at += 2;
        }
    }
    bytes
}

#[test]
fn the_path_offered_for_a_name_that_is_not_utf8_reaches_the_piece() {
    let world = world(Mode::Json, |_| {});
    let root = world.collection().root();
    let name = OsString::from_vec(b"d\xff".to_vec());
    let folder = world.collection().path("projects").join(&name);
    if let Err(error) = fs::create_dir_all(&folder) {
        // A volume that keeps names as UTF-8 (APFS) has no such piece.
        assert_eq!(
            error.raw_os_error(),
            Some(rustix::io::Errno::ILSEQ.raw_os_error())
        );
        return;
    }
    fs::write(folder.join("a"), "1\n").expect("a file is written");
    let given = OsString::from_vec(b"projects/d\xff".to_vec());
    let antiq = || Antiq::new().env("TZ", "UTC").arg("-C").arg(&root);
    let archived = antiq().arg("archive").arg(&given).run();
    assert_eq!(archived.status.code(), Some(0), "{}", archived.stderr);
    assert!(!folder.exists(), "the folder is archived away");

    let word = antiq().args(["--json", "unearth", "projects/d"]).run();
    assert_eq!(word.status.code(), Some(1), "{}", word.stderr);
    let json: Value = serde_json::from_str(&word.stdout).expect("a JSON object");
    assert_eq!(json["error"]["kind"], "inexact_reference");
    let fix = json["error"]["fix"].as_str().expect("a text");
    let example = fix.split('`').nth(1).expect("the path is a code span");
    assert_eq!(example, "$'projects/d\\xff'");

    let named = antiq()
        .arg("unearth")
        .arg(OsString::from_vec(bytes_of_dollar_quote(example)))
        .run();
    assert_eq!(named.status.code(), Some(0), "{}", named.stderr);
    assert!(folder.join("a").exists());
}

#[test]
fn an_argument_that_is_not_utf8_reaches_the_reference_as_its_bytes() {
    let world = world(Mode::Json, scanner);
    let given = OsString::from_vec(b"projects/d\xff".to_vec());
    let run = Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(world.collection().root())
        .args(["--json", "unearth"])
        .arg(&given)
        .run();
    assert_eq!(run.status.code(), Some(1), "{}", run.stderr);
    let json: Value = serde_json::from_str(&run.stdout).expect("a JSON object");
    assert_eq!(json["error"]["kind"], "unknown_reference");
    let message = json["error"]["message"].as_str().expect("a text");
    assert!(message.contains("`projects/d%FF`"), "{message}");
}
