// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: what `status` shows of a collection that needs a look.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Setup, both_modes};
use serde_json::json;
use std::fs;

const SOUND: &str = "2025-02-11T184007";
const NO_LISTING: &str = "2025-03-01T080000";

#[test]
fn status_shows_every_state_and_notes_what_needs_a_look() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .exclude(&["media/camera-import/card"])
                .piece("experiments/rust-gpu", |piece| {
                    piece.age_days(33).file("src/main.rs", "fn main() {}\n");
                })
                .piece("old/client-site", |piece| {
                    piece.age_days(141).file("index.html", "<html>");
                })
                .piece("tools/scan-old", |piece| {
                    piece.age_days(28).file("scan.sh", "#!/bin/sh\n");
                })
                .piece("projects/antiq", |piece| {
                    piece
                        .age_days(120)
                        .file("src/main.rs", "fn main() {}\n")
                        .pinned();
                })
                .piece("media/camera-import", |piece| {
                    piece
                        .age_days(52)
                        .file("one.jpg", "x")
                        .file("card/two.jpg", "y");
                })
                .piece("notes/daily", |piece| {
                    piece
                        .age_days(5)
                        .file("today.md", "fresh\n")
                        .file_aged("plan.md", "later\n", -10);
                })
                .version("tools/scan-old", SOUND, &[("scan.sh", "#!/bin/sh\n")])
                .version("tools/scan-old", NO_LISTING, &[("scan.sh", "#!/bin/sh\n")])
                .finish();
            collection.damage_remove_listing("tools/scan-old", NO_LISTING);
            Setup::new(collection)
        },
        |world| {
            let sound_bytes = fs::metadata(
                world
                    .collection()
                    .version_files("tools/scan-old", SOUND)
                    .archive,
            )
            .expect("the archive exists")
            .len();
            let future_time =
                jiff::Timestamp::from_second(world.collection().clock().days_ago(-10))
                    .expect("the time is representable");
            let future = future_time
                .to_zoned(jiff::tz::TimeZone::get("Pacific/Auckland").expect("a known zone"))
                .strftime("%Y-%m-%d")
                .to_string();

            // `status` writes nothing, below `.antiq/` or anywhere else.
            world.assert_unchanged(|| {
                let status = world.run(&["status"]);
                status.expect_code(0);
                assert_eq!(status.stderr, "");
                status.text_only(|out| {
                    // The version without a listing is counted, and adds no
                    // size, as it records none.
                    let expected = format!(
                        "\
6 folders \u{b7} 2 versions ({sound_bytes} B) \u{b7} archive after 30 days idle

STATE    PIECE                 IDLE  WHAT
due      experiments/rust-gpu   33d  archives next run
refused  media/camera-import    52d  exclude entry: media/camera-import/card
due      old/client-site       141d  archives next run
pinned   projects/antiq        120d  kept by .antiq-pin
soon     tools/scan-old         28d  archives in 2 days

! 1 file with a time in the future: notes/daily/plan.md ({future})
! 1 damaged version: tools/scan-old 2025-03-01 21:00:00 (no listing)
"
                    );
                    assert_eq!(out, expected);
                });
                status.json_only(|json| {
                    status.keys(&[
                        "format",
                        "command",
                        "folders",
                        "versions",
                        "archive_bytes",
                        "idle_days",
                        "soon_days",
                        "pieces",
                        "future_times",
                        "damaged_versions",
                        "incomplete_versions",
                        "newer_format_versions",
                        "recovered",
                        "trash",
                        "doomed",
                        "spelling",
                        "unreadable_directories",
                        "unparsable_files",
                        "warnings",
                        "store_error",
                    ]);
                    assert_eq!(json["folders"], 6);
                    assert_eq!(json["versions"], 2);
                    assert_eq!(json["archive_bytes"], sound_bytes);
                    assert_eq!(json["idle_days"], 30);
                    assert_eq!(json["soon_days"], 3);

                    let pieces = json["pieces"].as_array().expect("a list of pieces");
                    let states: Vec<(&str, &str)> = pieces
                        .iter()
                        .map(|piece| {
                            (
                                piece["piece"].as_str().expect("a piece"),
                                piece["state"].as_str().expect("a state"),
                            )
                        })
                        .collect();
                    assert_eq!(
                        states,
                        [
                            ("experiments/rust-gpu", "due"),
                            ("media/camera-import", "refused"),
                            ("old/client-site", "due"),
                            ("projects/antiq", "pinned"),
                            ("tools/scan-old", "soon"),
                        ]
                    );
                    let idle: Vec<u64> = pieces
                        .iter()
                        .map(|piece| piece["idle_for_days"].as_u64().expect("whole days"))
                        .collect();
                    assert_eq!(idle, [33, 52, 141, 120, 28]);
                    assert_eq!(pieces[4]["due_in_days"], 2);
                    assert_eq!(pieces[3]["reasons"], json!([]));
                    assert!(pieces[3].get("due_in_days").is_none());
                    assert_eq!(
                        pieces[1]["reasons"],
                        json!([{
                            "kind": "exclude_entry",
                            "path": "media/camera-import/card",
                            "text": "exclude entry: media/camera-import/card",
                        }])
                    );

                    let future_times = json["future_times"].as_array().expect("a list");
                    assert_eq!(future_times.len(), 1);
                    assert_eq!(future_times[0]["path"], "notes/daily/plan.md");
                    assert_eq!(
                        future_times[0]["time"],
                        future_time
                            .strftime("%Y-%m-%dT%H:%M:%SZ")
                            .to_string()
                            .as_str()
                    );
                    assert_eq!(
                        json["damaged_versions"],
                        json!([{
                            "piece": "tools/scan-old",
                            "version": NO_LISTING,
                            "reasons": ["no listing"],
                        }])
                    );
                    for key in [
                        "incomplete_versions",
                        "newer_format_versions",
                        "recovered",
                        "trash",
                        "doomed",
                        "spelling",
                        "unreadable_directories",
                        "unparsable_files",
                        "warnings",
                    ] {
                        assert_eq!(json[key], json!([]), "{key}");
                    }
                    assert_eq!(json["store_error"], json!(null));
                });
            });
        },
    );
}
