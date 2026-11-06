// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: a pin keeps a due folder out of `archive`, and unpinning
//! lets the next run take it.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Setup, both_modes};
use serde_json::Value;
use std::fs;

#[test]
fn a_pinned_folder_is_kept_until_it_is_unpinned() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("old/client-site", |piece| {
                    piece.age_days(141).file("index.html", "<html>");
                })
                .piece("tools/scan-old", |piece| {
                    piece.age_days(40).file("scan.sh", "#!/bin/sh\n");
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            let collection = world.collection();
            let pin_file = collection.path("old/client-site/.antiq-pin");

            let pin = world.run(&["pin", "old/client-site"]);
            pin.expect_code(0);
            assert_eq!(pin.stderr, "");
            pin.text_only(|out| assert_eq!(out, "Pinned old/client-site\n"));
            pin.json_only(|json| {
                pin.keys(&[
                    "format", "command", "piece", "pinned", "changed", "repairs", "warnings",
                ]);
                assert_eq!(json["piece"], "old/client-site");
                assert_eq!(json["pinned"], true);
                assert_eq!(json["changed"], true);
                assert_eq!(json["repairs"], Value::Array(vec![]));
                assert_eq!(json["warnings"], Value::Array(vec![]));
            });
            assert!(
                fs::symlink_metadata(&pin_file)
                    .expect("the pin file exists")
                    .is_file()
            );

            // The pinned folder is not archived although it is due.
            let archive = world.run(&["archive"]);
            archive.expect_code(0);
            assert_eq!(archive.stderr, "");
            archive.text_only(|out| {
                assert_eq!(
                    out,
                    "archived tools/scan-old\nArchived 1, skipped 0, refused 0.\n"
                );
            });
            archive.json_only(|json| {
                let archived = json["archived"].as_array().expect("a list");
                assert_eq!(archived.len(), 1);
                assert_eq!(archived[0]["piece"], "tools/scan-old");
                assert_eq!(json["refused"], Value::Array(vec![]));
            });
            assert!(collection.path("old/client-site/index.html").exists());
            assert!(collection.versions("old/client-site").is_empty());

            let unpin = world.run(&["unpin", "old/client-site"]);
            unpin.expect_code(0);
            assert_eq!(unpin.stderr, "");
            unpin.text_only(|out| assert_eq!(out, "Unpinned old/client-site\n"));
            unpin.json_only(|json| {
                unpin.keys(&[
                    "format", "command", "piece", "pinned", "changed", "repairs", "warnings",
                ]);
                assert_eq!(json["pinned"], false);
                assert_eq!(json["changed"], true);
            });
            assert!(!pin_file.exists());

            // Now the next run takes it.
            let again = world.run(&["archive"]);
            again.expect_code(0);
            assert_eq!(again.stderr, "");
            again.text_only(|out| {
                assert_eq!(
                    out,
                    "archived old/client-site\nArchived 1, skipped 0, refused 0.\n"
                );
            });
            again.json_only(|json| {
                assert_eq!(json["archived"][0]["piece"], "old/client-site");
            });
            assert_eq!(collection.versions("old/client-site").len(), 1);
            assert!(!collection.path("old/client-site").exists());
        },
    );
}
