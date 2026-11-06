// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What `push_observed` tells an observer while a stand-in rsync answers.

mod support;

use antiq::clock::SystemClock;
use antiq::commands::push::{PushOutcome, PushRequest, push_observed};
use antiq::external::RunLog;
use antiq::progress::{Event, Observer};
use std::fs;
use std::sync::Mutex;
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;

const RSYNC_3_2_4: &str = "rsync  version 3.2.4  protocol version 31\n";

/// One event as a line of text, made when the lines are asked for so that
/// a counter reads as the run left it.
type Line = Box<dyn Fn() -> String + Send>;

#[derive(Default)]
struct Lines(Mutex<Vec<Line>>);

impl Observer for Lines {
    fn event(&self, event: Event<'_>) {
        let line: Line = match event {
            Event::Planned { items } => {
                let names: Vec<String> = items.iter().map(|item| item.name.clone()).collect();
                Box::new(move || format!("planned {}", names.join(" ")))
            }
            Event::Started { name, bytes, .. } => {
                let name = name.to_string();
                let bytes = bytes.map_or("-".to_string(), |bytes| bytes.to_string());
                Box::new(move || format!("started {name} {bytes}"))
            }
            Event::Operation {
                name,
                operation,
                counter,
            } => {
                let (name, counter) = (name.to_string(), counter.clone());
                Box::new(move || {
                    format!(
                        "{name} {operation:?} {}/{}",
                        counter.done(),
                        counter.total()
                    )
                })
            }
            Event::Measured { name, bytes } => {
                let (name, bytes) = (name.to_string(), bytes.clone());
                Box::new(move || format!("{name} sent {}", bytes.done()))
            }
            Event::Finished {
                name,
                bytes,
                outcome,
            } => {
                let (name, outcome) = (name.to_string(), outcome.clone());
                Box::new(move || format!("finished {name} {bytes} {outcome:?}"))
            }
            Event::Dropped { name } => {
                let name = name.to_string();
                Box::new(move || format!("dropped {name}"))
            }
            Event::Paused { name } => {
                let name = name.to_string();
                Box::new(move || format!("paused {name}"))
            }
        };
        self.0.lock().expect("no panic holds the lock").push(line);
    }
}

impl Lines {
    fn lines(&self) -> Vec<String> {
        let lines = self.0.lock().expect("no panic holds the lock");
        lines.iter().map(|line| line()).collect()
    }
}

struct World {
    collection: Collection,
    fake: FakeRsync,
    target: Target,
}

impl World {
    /// The name the push gives its one item: the target as configured.
    fn item(&self) -> String {
        self.target
            .path()
            .to_str()
            .expect("a UTF-8 path")
            .to_string()
    }
}

fn world() -> World {
    world_of(&["projects/a"])
}

fn world_of(pieces: &[&str]) -> World {
    let mut builder = Collection::build(&Clock::now());
    for name in pieces {
        builder.piece(name, |piece| {
            piece.age_days(3).file("a.txt", "a");
        });
    }
    let collection = builder.finish();
    let target = Target::new();
    target.marker(&collection);
    let path = collection.path(".antiq/config.toml");
    let fake = FakeRsync::new(RSYNC_3_2_4);
    let rsync = serde_json::to_string(fake.path().to_str().expect("a UTF-8 path")).expect("quotes");
    let url = serde_json::to_string(target.path().to_str().expect("a UTF-8 path")).expect("quotes");
    let mut text = fs::read_to_string(&path).expect("the config exists");
    text.push_str(&format!("\n[target]\nurl = {url}\nrsync = {rsync}\n"));
    fs::write(&path, text).expect("the config can be written");
    World {
        collection,
        fake,
        target,
    }
}

fn push_result(
    world: &World,
    dry_run: bool,
    observer: &Lines,
) -> Result<PushOutcome, antiq::commands::push::PushError> {
    let collection =
        antiq::collection::Collection::find(&world.collection.root()).expect("a collection");
    let request = PushRequest {
        init: false,
        on_mismatch: antiq::commands::push::Mismatch::Refuse,
        if_reachable: false,
        dry_run,
    };
    push_observed(
        &collection,
        &request,
        &SystemClock,
        &RunLog::discard(),
        observer,
    )
}

fn push_with(world: &World, dry_run: bool, observer: &Lines) -> PushOutcome {
    push_result(world, dry_run, observer).expect("the push ends")
}

fn transfers(fake: &FakeRsync) -> Vec<Vec<String>> {
    fake.calls()
        .into_iter()
        .filter(|call| call[0] == "transfer")
        .collect()
}

#[test]
fn a_real_push_follows_its_one_run_and_makes_no_dry_run() {
    let world = world();
    world.fake.answer(
        1,
        ".d          96 ./\n\
         >f+++++++++ 5 projects/a/a.txt\n\
         \r              5 100%    0.00kB/s    0:00:00 (xfr#1, to-chk=0/3)\n",
        "",
        0,
    );
    let seen = Lines::default();

    let outcome = push_with(&world, false, &seen);

    let item = world.item();
    assert_eq!(
        seen.lines(),
        [
            format!("started {item} 0"),
            format!("{item} Storing 2/3"),
            format!("{item} sent 5"),
        ]
    );
    let calls = transfers(&world.fake);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(!calls[0].contains(&"-n".to_string()));
    assert!(calls[0].contains(&"-ii".to_string()));
    assert!(calls[0].contains(&"--out-format=%i %l %n%L".to_string()));
    assert!(!calls[0].contains(&"--itemize-changes".to_string()));
    let PushOutcome::Report(report) = outcome else {
        panic!("a report");
    };
    assert_eq!(report.changes.pieces[0].counts.created, 1);
}

#[test]
fn a_push_that_changes_nothing_still_shows_the_entries_and_reports_no_change() {
    let world = world();
    world.fake.answer(
        1,
        ".d          96 ./\n.f          1 projects/a/a.txt\n\r  0 0% (xfr#0, to-chk=0/2)\n",
        "",
        0,
    );
    let seen = Lines::default();

    let outcome = push_with(&world, false, &seen);

    let item = world.item();
    assert_eq!(
        seen.lines(),
        [
            format!("started {item} 0"),
            format!("{item} Storing 2/2"),
            format!("{item} sent 0"),
        ]
    );
    let PushOutcome::Report(report) = outcome else {
        panic!("a report");
    };
    assert!(report.changes.is_empty());
}

#[test]
fn a_dry_run_makes_one_plain_call_and_tells_nothing() {
    let world = world();
    world
        .fake
        .answer_all(">f+++++++++ projects/a/a.txt\n", "", 0);
    let seen = Lines::default();

    push_with(&world, true, &seen);

    assert_eq!(seen.lines(), Vec::<String>::new());
    let calls = transfers(&world.fake);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(&"--itemize-changes".to_string()));
}

#[test]
fn deletions_are_followed_in_the_first_run_only_and_the_checks_after_it_tell_nothing() {
    let world = world();
    world.fake.answer(
        1,
        ".d          96 ./\n*deleting   0 projects/a/old.txt\n",
        "",
        0,
    );
    world
        .fake
        .answer(2, "*deleting   projects/a/old.txt\n", "", 0);
    world.fake.answer(3, "", "", 0);
    let seen = Lines::default();

    push_with(&world, false, &seen);

    let item = world.item();
    assert_eq!(
        seen.lines(),
        [
            format!("started {item} 0"),
            format!("{item} Storing 1/0"),
            format!("{item} sent 0"),
            format!("{item} Removing 1/0"),
        ]
    );
    assert_eq!(transfers(&world.fake).len(), 3);
}

#[test]
fn a_deletion_that_follows_a_progress_update_on_its_line_still_starts_the_second_pass() {
    let world = world();
    // rsync leaves a progress update without a newline, and the line that
    // follows it, when a warning went to stderr in between.
    world.fake.answer(
        1,
        ".d          96 ./\n\
         \r      0   0%    0.00kB/s    0:00:00 (xfr#0, to-chk=0/1)*deleting   0 projects/a/old.txt\n",
        "",
        0,
    );
    world
        .fake
        .answer(2, "*deleting   projects/a/old.txt\n", "", 0);
    world.fake.answer(3, "", "", 0);
    let seen = Lines::default();

    let PushOutcome::Report(report) = push_with(&world, false, &seen) else {
        panic!("a report");
    };

    assert_eq!(transfers(&world.fake).len(), 3);
    assert_eq!(report.changes.pieces[0].counts.deleted, 1);
}

#[test]
fn a_check_that_fails_after_the_first_run_is_the_error_of_the_push() {
    let world = world();
    world
        .fake
        .answer(1, "*deleting   0 projects/a/old.txt\n", "", 0);
    world.fake.answer(2, "", "rsync: error\n", 30);
    let seen = Lines::default();

    let result = push_result(&world, false, &seen);

    assert!(result.is_err(), "{result:?}");
}

#[test]
fn a_report_counts_the_changes_of_the_second_run_too() {
    let world = world();
    world.fake.answer(
        1,
        ".d          96 ./\n*deleting   0 projects/a/old.txt\n",
        "",
        0,
    );
    world
        .fake
        .answer(2, ">f+++++++++ projects/a/b.txt\n", "", 0);
    world
        .fake
        .answer(3, ">f+++++++++ projects/a/b.txt\n", "", 0);
    let seen = Lines::default();

    let PushOutcome::Report(report) = push_with(&world, false, &seen) else {
        panic!("a report");
    };

    let counts = report.changes.pieces[0].counts;
    assert_eq!((counts.created, counts.deleted), (1, 1));
    assert!(report.second_pass);
}
