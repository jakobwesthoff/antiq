// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: a damaged version through `status`, `push`, `rebuild` and
//! `push` again.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::outsider::shasum_check;
use crate::support::scenario::target::Target;
use crate::support::scenario::world::{Setup, both_modes};
use serde_json::json;
use std::fs;

const SOUND: &str = "2025-02-11T184007";
const DAMAGED: &str = "2025-03-01T080000";

#[test]
fn a_damaged_version_stops_push_until_rebuild_repairs_it() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("tools/scan-old", |piece| {
                    piece.age_days(10).file("scan.sh", "#!/bin/sh\n");
                })
                .version("tools/scan-old", SOUND, &[("scan.sh", "#!/bin/sh\n")])
                .version(
                    "tools/scan-old",
                    DAMAGED,
                    &[("scan.sh", "#!/bin/sh\n# 2\n")],
                )
                .finish();
            collection.damage_remove_listing("tools/scan-old", DAMAGED);
            let target = Target::new();
            target.marker(&collection);
            Setup::new(collection).with_target(target).with_real_rsync()
        },
        |world| {
            let files = world.collection().version_files("tools/scan-old", DAMAGED);
            let listing_name = files
                .listing
                .file_name()
                .expect("a file name")
                .to_str()
                .expect("UTF-8")
                .to_string();

            // `status` reports the damage and says what is wrong with it.
            let status = world.run(&["status"]);
            status.expect_code(0);
            status.text_only(|out| {
                assert!(
                    out.contains(
                        "! 1 damaged version: tools/scan-old 2025-03-01 21:00:00 (no listing)\n"
                    ),
                    "{out}"
                );
            });
            status.json_only(|json| {
                assert_eq!(
                    json["damaged_versions"],
                    json!([{
                        "piece": "tools/scan-old",
                        "version": DAMAGED,
                        "reasons": ["no listing"],
                    }])
                );
            });

            // `push` stops, names the command that repairs the version and
            // sends nothing.
            let command = format!("antiq rebuild tools/scan-old {DAMAGED}");
            world.assert_unchanged(|| {
                let refused = world.run(&["push"]);
                refused.expect_code(1);
                let text = format!("{}{}", refused.stdout, refused.stderr);
                assert!(text.contains(&command), "{text}");
            });

            // `rebuild` writes the missing listing and says on what the
            // archive was checked.
            let rebuilt = world.run(&["rebuild", "tools/scan-old", DAMAGED]);
            rebuilt.expect_code(0);
            rebuilt.text_only(|out| {
                assert_eq!(
                    out,
                    format!("Rebuilt version {DAMAGED} of tools/scan-old: wrote {listing_name}\n")
                );
            });
            rebuilt.text_only(|_| {
                assert_eq!(
                    rebuilt.stderr,
                    "note: the archive is the one checksummed when it was stored\n"
                );
            });
            rebuilt.keys(&[
                "format", "command", "piece", "stamp", "version", "basis", "written", "repairs",
                "warnings",
            ]);
            rebuilt.json_only(|json| {
                assert_eq!(json["piece"], "tools/scan-old");
                assert_eq!(json["stamp"], DAMAGED);
                assert_eq!(json["version"], "2025-03-01T08:00:00Z");
                assert_eq!(json["basis"], "checksum_file");
                assert_eq!(json["written"], json!([listing_name]));
            });

            // The listing is back, says it was rebuilt and describes the
            // archive; the checksum file still verifies it.
            let listing = zstd::decode_all(
                fs::read(&files.listing)
                    .expect("the listing was written")
                    .as_slice(),
            )
            .expect("the listing decodes");
            let listing = String::from_utf8(listing).expect("the listing is text");
            for expected in [
                "# piece         tools/scan-old\n",
                "# files         1\n",
                "# bytes         14\n",
            ] {
                assert!(listing.contains(expected), "{expected:?} in {listing}");
            }
            assert!(listing.contains("# rebuilt"), "{listing}");
            assert!(listing.contains("scan.sh\n"), "{listing}");
            shasum_check(
                files.checksum.parent().expect("a store folder"),
                &files.checksum,
            );

            let status = world.run(&["status"]);
            status.expect_code(0);
            status.text_only(|out| assert!(!out.contains('!'), "{out}"));
            status.json_only(|json| assert_eq!(json["damaged_versions"], json!([])));

            // The next push goes through and the target holds the
            // repaired version.
            let pushed = world.run(&["push"]);
            pushed.expect_code(0);
            let target = world.target().expect("a target").path();
            let sent = target
                .join(".antiq/pieces/tools/scan-old")
                .join(&listing_name);
            assert_eq!(
                fs::read(&sent).expect("the listing arrived"),
                fs::read(&files.listing).expect("the listing is there")
            );
        },
    );
}
