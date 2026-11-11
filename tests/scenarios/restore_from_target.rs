// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: restoring everything from the target with `rsync -aH` gives
//! a collection that antiq works in.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::outsider::same_file;
use crate::support::scenario::state::State;
use crate::support::scenario::target::{Target, real_rsync};
use crate::support::scenario::versions::Source;
use crate::support::scenario::world::{Setup, both_modes};
use serde_json::{Value, json};
use std::fs;

const STORED: &str = "2025-02-11T184007";

/// What the mirror leaves out and a copy back therefore lacks.
const NOT_MIRRORED: [&str; 3] = [".antiq/tmp", ".antiq/trash", ".antiq/doomed"];

#[test]
fn a_copy_back_from_the_target_is_a_working_collection() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .patterns(&["*/*"])
                .piece("projects/scanner", |piece| {
                    piece
                        .age_days(10)
                        .file("src/main.rs", "fn main() {}\n")
                        .hard_link("src/main.rs", "src/copy.rs")
                        .symlink("latest", "src/main.rs");
                })
                .piece("notes/daily", |piece| {
                    piece
                        .age_days(40)
                        .file("today.md", "fresh\n")
                        .hard_link("today.md", "also-today.md");
                })
                .version("old/client-site", STORED, &[("index.html", "<html>")])
                .finish();
            Setup::new(collection)
                .with_target(Target::new())
                .with_real_rsync()
        },
        |world| {
            // The due folder goes into the store, then everything goes to
            // the target.
            world.run(&["archive"]).expect_code(0);
            assert_eq!(world.collection().versions("notes/daily").len(), 1);
            world.run(&["push", "--init"]).expect_code(0);

            let target = world.target().expect("a target");
            let rsync = real_rsync();
            let copy =
                target.copy_back_using(rsync.to_str().expect("UTF-8"), world.collection().clock());

            // Nothing is lost on the way, and the hard links of the live
            // folder are links in the copy.
            let original = State::raw(&world.collection().root()).without(&NOT_MIRRORED);
            let restored = State::raw(&copy.root()).without(&[".antiq-target"]);
            if let Some(diff) = original.diff(&restored) {
                panic!("the copy back differs from the collection:\n{diff}");
            }
            assert!(same_file(
                &copy.path("projects/scanner/src/main.rs"),
                &copy.path("projects/scanner/src/copy.rs")
            ));
            assert!(!same_file(
                &copy.path("projects/scanner/src/main.rs"),
                &world.collection().path("projects/scanner/src/main.rs")
            ));

            // antiq works in the copy as it does in the original: the
            // commands answer alike, and none of them touches the
            // original or the target.
            world.assert_unchanged(|| {
                for args in [vec!["find"], vec!["status"]] {
                    let here = world.run(&args);
                    let there = world.run_as(&copy, &args);
                    here.expect_code(0);
                    there.expect_code(0);
                    assert_eq!(there.stdout, here.stdout, "{args:?} differs in the copy");
                    assert_eq!(there.stderr, here.stderr, "{args:?} differs in the copy");
                }

                // The commands that write take the lock and use the
                // folders a mirror leaves out, which a copy back lacks.
                let first = world.run_as(&copy, &["pin", "projects/scanner"]);
                first.expect_code(0);
                let created = ["tmp", "trash", "doomed"];
                first.json_only(|json| {
                    let expected: Vec<_> = created
                        .iter()
                        .map(|folder| {
                            json!({"kind": "repair", "repair": "create_folder", "folder": folder})
                        })
                        .collect();
                    assert_eq!(json["repairs"], Value::Array(expected));
                });
                first.text_only(|_| {
                    let expected: String = created
                        .iter()
                        .map(|folder| {
                            format!("note: repaired: create the folder .antiq/{folder}\n")
                        })
                        .collect();
                    assert_eq!(first.stderr, expected);
                });
                for folder in NOT_MIRRORED {
                    assert!(copy.path(folder).is_dir(), "{folder} is not recreated");
                }
                assert!(copy.path("projects/scanner/.antiq-pin").exists());
                world
                    .run_as(&copy, &["unpin", "projects/scanner"])
                    .expect_code(0);
                assert!(!copy.path("projects/scanner/.antiq-pin").exists());

                world
                    .run_as(&copy, &["archive", "projects/scanner"])
                    .expect_code(0);
                assert!(!copy.path("projects/scanner").exists());
                assert_eq!(copy.versions("projects/scanner").len(), 1);

                // `unearth` brings folders back from versions that were
                // copied and from the one made in the copy, hard links
                // included.
                world
                    .run_as(&copy, &["unearth", "notes/daily"])
                    .expect_code(0);
                assert_eq!(
                    fs::read_to_string(copy.path("notes/daily/today.md")).expect("restored"),
                    "fresh\n"
                );
                assert!(same_file(
                    &copy.path("notes/daily/today.md"),
                    &copy.path("notes/daily/also-today.md")
                ));
                assert!(copy.path("notes/daily/.antiq-restored").is_file());
                assert!(copy.versions("notes/daily").is_empty());

                world
                    .run_as(&copy, &["unearth", "projects/scanner"])
                    .expect_code(0);
                assert!(same_file(
                    &copy.path("projects/scanner/src/main.rs"),
                    &copy.path("projects/scanner/src/copy.rs")
                ));

                // `fork` extracts a copied version as a working copy and
                // leaves the version in place. `import` packs a folder from
                // outside as a new version.
                world
                    .run_as(
                        &copy,
                        &["fork", "old/client-site", "--name", "old/client-site-edit"],
                    )
                    .expect_code(0);
                assert_eq!(
                    fs::read_to_string(copy.path("old/client-site-edit/index.html"))
                        .expect("forked"),
                    "<html>"
                );
                assert!(copy.path("old/client-site-edit/.antiq-restored").is_file());
                assert_eq!(copy.versions("old/client-site").len(), 1);

                let source = Source::new(world.collection().clock(), |piece| {
                    piece.file("page.txt", "imported\n");
                });
                let source_folder = source.path();
                let source_folder = source_folder.to_str().expect("UTF-8");
                world
                    .run_as(&copy, &["import", source_folder, "--as", "tools/imported"])
                    .expect_code(0);
                assert_eq!(copy.versions("tools/imported").len(), 1);
                world
                    .run_as(&copy, &["unearth", "tools/imported"])
                    .expect_code(0);
                assert_eq!(
                    fs::read_to_string(copy.path("tools/imported/page.txt")).expect("imported"),
                    "imported\n"
                );

                world
                    .run_as(&copy, &["forget", "old/client-site", STORED])
                    .expect_code(0);
                assert!(copy.versions("old/client-site").is_empty());

                // A version whose listing did not come back is repaired.
                let again = target
                    .copy_back_using(rsync.to_str().expect("UTF-8"), world.collection().clock());
                let stamp = again.versions("notes/daily").remove(0);
                again.damage_remove_listing("notes/daily", &stamp);
                world
                    .run_as(&again, &["rebuild", "notes/daily", &stamp])
                    .expect_code(0);
                assert!(again.version_files("notes/daily", &stamp).listing.is_file());
            });
        },
    );
}
