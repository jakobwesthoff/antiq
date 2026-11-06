// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The JSON shapes every command shares, seen from outside the binary: a
//! real run has no `dry_run` and no `actions`, and lists the repairs it did
//! as `{"kind": "repair", "repair": ...}`; a dry run has `"dry_run": true`,
//! then `actions`, and every action has a `kind`. Each case plants a
//! leftover in `tmp/`, so that the repair `delete_temporary` is part of the
//! run.

mod support;

use serde_json::Value;
use std::fs;
use std::path::Path;
use support::cmd::Antiq;
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection, CollectionBuilder};
use support::scenario::target::Target;
use support::scenario::world::{Mode, Outcome, Setup, World};
use support::tree::Tree;

const PIECE: &str = "projects/scanner";
const STAMP: &str = "2026-11-04T090211";
const OLD_STAMP: &str = "2026-09-03T121500";

fn world(build: impl FnOnce(&mut CollectionBuilder)) -> World {
    let clock = Clock::now();
    let mut builder = Collection::build(&clock);
    builder.patterns(&["projects/*"]).idle_days(30);
    build(&mut builder);
    World::new(Setup::new(builder.finish()), Mode::Json)
}

fn two_versions(builder: &mut CollectionBuilder) {
    builder
        .version(PIECE, OLD_STAMP, &[("README", "old\n")])
        .version(PIECE, STAMP, &[("README", "scan\n"), ("src/a", "a\n")]);
}

fn due_piece(builder: &mut CollectionBuilder) {
    builder.piece("projects/due", |piece| {
        piece.age_days(40).file("data", "x");
    });
}

/// The leftover that makes `delete_temporary` a repair of the next run.
fn plant_leftover(world: &World) {
    let leftover = world.collection().path(".antiq/tmp/leftover");
    fs::write(&leftover, "x").expect("the leftover is written");
}

fn is_a_delete_temporary(repair: &Value) -> bool {
    repair["kind"] == "repair" && repair["repair"] == "delete_temporary"
}

/// A real run: no `dry_run`, no `actions`, and the repairs it did are
/// tagged with their kind.
fn assert_real_shape(json: &Value) {
    let object = json.as_object().expect("an object");
    assert!(!object.contains_key("dry_run"), "{json}");
    assert!(!object.contains_key("actions"), "{json}");
    let repairs = json["repairs"].as_array().expect("a list of repairs");
    assert!(
        repairs.iter().any(is_a_delete_temporary),
        "the repairs are {repairs:?}"
    );
}

/// A dry run: `dry_run` is true, `actions` follows, every action has a
/// `kind` and none an `action`, and the repair is among them.
fn assert_dry_shape(json: &Value) {
    assert_eq!(json["dry_run"], true, "{json}");
    let actions = json["actions"].as_array().expect("a list of actions");
    for action in actions {
        let object = action.as_object().expect("an action is an object");
        assert!(
            object.get("kind").is_some_and(Value::is_string),
            "no kind in {action}"
        );
        assert!(!object.contains_key("action"), "{action}");
    }
    assert!(actions.iter().any(is_a_delete_temporary), "{actions:?}");
}

fn json_of(run: Outcome) -> Value {
    run.expect_code(0);
    let mut json = Value::Null;
    run.json_only(|value| json = value.clone());
    json
}

fn real(world: &World, args: &[&str]) -> Value {
    plant_leftover(world);
    json_of(world.run(args))
}

fn dry(world: &World, args: &[&str]) -> Value {
    plant_leftover(world);
    let mut all = args.to_vec();
    all.push("-n");
    json_of(world.run(&all))
}

// =========================================================
// unearth and fork
// =========================================================

#[test]
fn a_real_unearth_tags_its_repairs_with_kind() {
    let world = world(two_versions);
    assert_real_shape(&real(&world, &["unearth", PIECE]));
}

#[test]
fn a_dry_unearth_names_its_action_by_kind() {
    let world = world(two_versions);
    let json = dry(&world, &["unearth", PIECE]);
    assert_dry_shape(&json);
    assert_eq!(json["actions"][1]["kind"], "unearth");
}

#[test]
fn a_real_fork_tags_its_repairs_with_kind() {
    let world = world(two_versions);
    assert_real_shape(&real(&world, &["fork", PIECE]));
}

#[test]
fn a_dry_fork_names_its_action_by_kind() {
    let world = world(two_versions);
    let json = dry(&world, &["fork", PIECE]);
    assert_dry_shape(&json);
    assert_eq!(json["actions"][1]["kind"], "fork");
}

// =========================================================
// The commands that already follow the rule
// =========================================================

#[test]
fn archive_follows_the_rule() {
    assert_real_shape(&real(&world(due_piece), &["archive"]));
    assert_dry_shape(&dry(&world(due_piece), &["archive"]));
}

#[test]
fn forget_follows_the_rule() {
    assert_real_shape(&real(&world(two_versions), &["forget", PIECE, OLD_STAMP]));
    assert_dry_shape(&dry(&world(two_versions), &["forget", PIECE, OLD_STAMP]));
}

#[test]
fn pin_and_unpin_follow_the_rule() {
    assert_real_shape(&real(&world(due_piece), &["pin", "projects/due"]));
    assert_dry_shape(&dry(&world(due_piece), &["pin", "projects/due"]));

    let pinned = world(due_piece);
    pinned.run(&["pin", "projects/due"]).expect_code(0);
    assert_real_shape(&real(&pinned, &["unpin", "projects/due"]));

    let pinned = world(due_piece);
    pinned.run(&["pin", "projects/due"]).expect_code(0);
    assert_dry_shape(&dry(&pinned, &["unpin", "projects/due"]));
}

fn without_listing(world: &World) {
    let files = world.collection().version_files(PIECE, STAMP);
    fs::remove_file(files.listing).expect("the listing is removed");
}

#[test]
fn rebuild_follows_the_rule() {
    let world = world(two_versions);
    without_listing(&world);
    assert_real_shape(&real(&world, &["rebuild", PIECE, STAMP]));

    let world = self::world(two_versions);
    without_listing(&world);
    assert_dry_shape(&dry(&world, &["rebuild", PIECE, STAMP]));
}

#[test]
fn import_follows_the_rule() {
    let mut source = Tree::new();
    source.file("data/a.txt", "alpha");
    let path = source.path("data");
    let path = path.to_str().expect("a UTF-8 path");

    assert_real_shape(&real(
        &world(due_piece),
        &["import", path, "--as", "projects/data"],
    ));
    assert_dry_shape(&dry(
        &world(due_piece),
        &["import", path, "--as", "projects/data"],
    ));
}

// =========================================================
// push
// =========================================================

const RSYNC_3_2_4: &str = "rsync  version 3.2.4  protocol version 31\n";

const ITEMIZED: &str = ">f+++++++++ projects/a/new.txt\n\
    >f+++++++++ loose.txt\n";

/// A collection that pushes to the folder `url` through a fake rsync.
fn targeted(url: &Path) -> (Collection, FakeRsync) {
    let collection = Collection::build(&Clock::now())
        .piece("projects/a", |piece| {
            piece.age_days(3).file("a.txt", "a");
        })
        .finish();
    let fake = FakeRsync::new(RSYNC_3_2_4);
    let url = serde_json::to_string(url.to_str().expect("a UTF-8 path")).expect("quotes");
    let rsync = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path")).expect("quotes");
    let config = collection.path(".antiq/config.toml");
    let mut text = fs::read_to_string(&config).expect("the config exists");
    text.push_str(&format!("\n[target]\nurl = {url}\nrsync = {rsync}\n"));
    fs::write(&config, text).expect("the config is written");
    (collection, fake)
}

fn push(collection: &Collection, args: &[&str]) -> Value {
    let run = Antiq::new()
        .env("TZ", "UTC")
        .arg("-C")
        .arg(collection.root())
        .arg("--json")
        .arg("push")
        .args(args)
        .run();
    assert_eq!(run.code(), 0, "{}", run.stdout);
    run.json()
}

#[test]
fn a_real_push_has_no_dry_run_key() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    target.marker(&collection);
    fake.answer_all(ITEMIZED, "", 0);

    let json = push(&collection, &[]);

    assert_eq!(json["result"], "pushed");
    assert!(json.get("dry_run").is_none(), "{json}");
}

#[test]
fn an_unreachable_real_push_has_no_dry_run_key() {
    let gone = Target::new();
    let (collection, _fake) = targeted(&gone.path().join("not-there"));

    let json = push(&collection, &["--if-reachable"]);

    assert_eq!(json["result"], "unreachable");
    assert!(json.get("dry_run").is_none(), "{json}");
}

#[test]
fn a_dry_push_names_its_actions_by_kind() {
    let target = Target::new();
    let (collection, fake) = targeted(&target.path());
    fake.answer_all(ITEMIZED, "", 0);

    let json = push(&collection, &["--init", "-n"]);

    assert_eq!(json["dry_run"], true);
    let kinds: Vec<&Value> = json["actions"]
        .as_array()
        .expect("a list of actions")
        .iter()
        .map(|action| {
            assert!(action.get("action").is_none(), "{action}");
            &action["kind"]
        })
        .collect();
    assert_eq!(kinds, ["write_marker", "mirror"]);
}

#[test]
fn an_unreachable_dry_push_keeps_dry_run() {
    let gone = Target::new();
    let (collection, _fake) = targeted(&gone.path().join("not-there"));

    let json = push(&collection, &["--if-reachable", "-n"]);

    assert_eq!(json["result"], "unreachable");
    assert_eq!(json["dry_run"], true, "{json}");
}
