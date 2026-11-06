// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What `push_observed` tells an observer while a stand-in rsync answers.

mod support;

use antiq::clock::SystemClock;
use antiq::commands::push::{PushOutcome, PushRequest, push_observed};
use antiq::progress::{Event, Observer};
use std::fs;
use std::sync::Mutex;
use support::fake_rsync::FakeRsync;
use support::scenario::collection::{Clock, Collection};
use support::scenario::target::Target;

const RSYNC_3_2_4: &str = "rsync  version 3.2.4  protocol version 31\n";

/// The events as lines of text, counters read when the line is taken.
#[derive(Default)]
struct Lines(Mutex<Vec<String>>);

impl Observer for Lines {
    fn event(&self, event: Event<'_>) {
        let line = match event {
            Event::Planned { items } => {
                let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
                format!("planned {}", names.join(" "))
            }
            Event::Started { name, bytes, .. } => format!("started {name} {bytes}"),
            Event::Operation {
                name,
                operation,
                counter,
            } => format!("{name} {operation:?} total {}", counter.total()),
            Event::Finished {
                name,
                bytes,
                outcome,
            } => format!("finished {name} {bytes} {outcome:?}"),
            Event::Dropped { name } => format!("dropped {name}"),
            Event::Paused { name } => format!("paused {name}"),
        };
        self.0.lock().expect("no panic holds the lock").push(line);
    }
}

impl Lines {
    fn lines(&self) -> Vec<String> {
        self.0.lock().expect("no panic holds the lock").clone()
    }
}

struct World {
    collection: Collection,
    fake: FakeRsync,
    _target: Target,
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
        _target: target,
    }
}

fn push_with(world: &World, dry_run: bool, observer: &Lines) -> PushOutcome {
    let collection =
        antiq::collection::Collection::find(&world.collection.root()).expect("a collection");
    let request = PushRequest {
        init: false,
        force: false,
        if_reachable: false,
        dry_run,
    };
    push_observed(&collection, &request, &SystemClock, observer).expect("the push ends")
}

fn transfers(fake: &FakeRsync) -> Vec<Vec<String>> {
    fake.calls()
        .into_iter()
        .filter(|call| call[0] == "transfer")
        .collect()
}

#[test]
fn a_real_push_lists_the_sizes_first_and_follows_the_real_run() {
    let world = world();
    world
        .fake
        .answer(1, ">f+++++++++ 5 projects/a/a.txt\n", "", 0);
    world
        .fake
        .answer(2, ">f+++++++++ projects/a/a.txt\n", "", 0);
    let seen = Lines::default();

    push_with(&world, false, &seen);

    assert_eq!(
        seen.lines(),
        [
            "planned projects",
            "started projects 5",
            "projects Storing total 1",
            "finished projects 5 Changes { created: 1, updated: 0, deleted: 0 }",
        ]
    );
    let calls = transfers(&world.fake);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].contains(&"-n".to_string()));
    assert!(calls[0].contains(&"--out-format=%i %l %n%L".to_string()));
    assert!(!calls[1].contains(&"-n".to_string()));
    assert!(calls[1].contains(&"--itemize-changes".to_string()));
}

#[test]
fn a_listing_that_cannot_be_read_gives_no_events_and_the_push_goes_on() {
    let world = world();
    world
        .fake
        .answer(1, ">f+++++++++ many projects/a/a.txt\n", "", 0);
    world
        .fake
        .answer(2, ">f+++++++++ projects/a/a.txt\n", "", 0);
    let seen = Lines::default();

    let outcome = push_with(&world, false, &seen);

    assert_eq!(seen.lines(), Vec::<String>::new());
    let PushOutcome::Report(report) = outcome else {
        panic!("a report");
    };
    assert_eq!(report.changes.pieces[0].counts.created, 1);
}

#[test]
fn a_failing_listing_gives_no_events_and_the_push_goes_on() {
    let world = world();
    world.fake.answer(1, "", "rsync: error\n", 23);
    world
        .fake
        .answer(2, ">f+++++++++ projects/a/a.txt\n", "", 0);
    let seen = Lines::default();

    push_with(&world, false, &seen);

    assert_eq!(seen.lines(), Vec::<String>::new());
    assert_eq!(transfers(&world.fake).len(), 2);
}

#[test]
fn a_dry_run_makes_one_call_and_tells_nothing() {
    let world = world();
    world
        .fake
        .answer_all(">f+++++++++ projects/a/a.txt\n", "", 0);
    let seen = Lines::default();

    push_with(&world, true, &seen);

    assert_eq!(seen.lines(), Vec::<String>::new());
    assert_eq!(transfers(&world.fake).len(), 1);
}

#[test]
fn deletions_are_followed_in_the_first_pass_only_and_the_checks_after_it_tell_nothing() {
    let world = world();
    world
        .fake
        .answer(1, "*deleting   0 projects/a/old.txt\n", "", 0);
    world
        .fake
        .answer(2, "*deleting   projects/a/old.txt\n", "", 0);
    world.fake.answer(3, "", "", 0);
    let seen = Lines::default();

    push_with(&world, false, &seen);

    assert_eq!(
        seen.lines(),
        [
            "planned projects",
            "started projects 0",
            "projects Removing total 1",
            "finished projects 0 Changes { created: 0, updated: 0, deleted: 1 }",
        ]
    );
    assert_eq!(transfers(&world.fake).len(), 3);
}

#[test]
fn a_real_pass_that_differs_from_the_listing_reports_what_it_did_and_keeps_the_cards_whole() {
    let world = world_of(&["alpha", "beta"]);
    // The listing expects one file and one deletion for alpha, one file for
    // beta. The real pass sends a second file for alpha while it waits for
    // its deletions, a second one for beta after it finished, and a second
    // deletion for alpha after it finished. A card never reopens for those.
    world.fake.answer(
        1,
        ">f+++++++++ 5 alpha/x\n*deleting   0 alpha/gone\n>f+++++++++ 7 beta/z\n",
        "",
        0,
    );
    world.fake.answer(
        2,
        ">f+++++++++ alpha/x\n\
         >f+++++++++ beta/z\n\
         >f+++++++++ alpha/y\n\
         *deleting   alpha/gone\n\
         >f+++++++++ beta/w\n\
         *deleting   alpha/more\n",
        "",
        0,
    );
    world.fake.answer(3, "", "", 0);
    let seen = Lines::default();

    push_with(&world, false, &seen);

    assert_eq!(
        seen.lines(),
        [
            "planned alpha beta",
            "started alpha 5",
            "alpha Storing total 1",
            "paused alpha",
            "started beta 7",
            "beta Storing total 1",
            "finished beta 7 Changes { created: 1, updated: 0, deleted: 0 }",
            "alpha Removing total 1",
            "finished alpha 5 Changes { created: 1, updated: 0, deleted: 1 }",
        ]
    );
}
