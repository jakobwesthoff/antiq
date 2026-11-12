// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: a collection that an older antiq made has no README. Looking
//! at it changes nothing, the first run that takes the lock writes the
//! README and says so, and the next run finds nothing to do.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Setup, both_modes};
use antiq::collection::README;
use serde_json::{Value, json};
use std::fs;

#[test]
fn an_older_collection_gets_its_readme_with_the_first_run_that_takes_the_lock() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("tools/scan-old", |piece| {
                    piece.age_days(40).file("scan.sh", "#!/bin/sh\n");
                })
                .finish();
            fs::remove_file(collection.path(".antiq/README.md")).expect("the fixture has a README");
            Setup::new(collection)
        },
        |world| {
            let readme = world.collection().path(".antiq/README.md");

            // Looking at the collection leaves the README missing.
            world.assert_unchanged(|| {
                let status = world.run(&["status"]);
                status.expect_code(0);
                assert!(!status.plain().contains("README"), "{}", status.plain());
            });
            assert!(!readme.exists());

            // A dry run lists the write before the work.
            world.assert_unchanged(|| {
                let plan = world.run(&["archive", "-n"]);
                plan.expect_code(0);
                plan.text_only(|out| {
                    assert_eq!(
                        out,
                        "Would write .antiq/README.md\nWould archive tools/scan-old\n"
                    );
                });
                plan.json_only(|json| {
                    assert_eq!(
                        json["actions"][0],
                        json!({"kind": "repair", "repair": "write_readme", "replaces": false})
                    );
                    assert_eq!(json["actions"][1]["kind"], "archive");
                });
            });

            let archive = world.run(&["archive"]);
            archive.expect_code(0);
            archive.text_only(|_| {
                assert!(
                    archive.stderr.contains("repaired: write .antiq/README.md"),
                    "{}",
                    archive.stderr
                );
            });
            archive.json_only(|json| {
                assert_eq!(
                    json["repairs"],
                    json!([{"kind": "repair", "repair": "write_readme", "replaces": false}])
                );
            });
            assert_eq!(fs::read_to_string(&readme).expect("it exists"), README);

            // Nothing is left to do, and the run after it repairs nothing.
            let again = world.run(&["archive", "-n"]);
            again.expect_code(0);
            again.text_only(|out| assert_eq!(out, "Nothing to do.\n"));
            again.json_only(|json| assert_eq!(json["actions"], Value::Array(vec![])));
            let status = world.run(&["status"]);
            status.expect_code(0);
        },
    );
}
