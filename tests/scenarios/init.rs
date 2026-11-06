// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Scenario: a directory with folders in it becomes a collection.

use crate::support::scenario::collection::Collection;
use crate::support::scenario::world::{Setup, both_modes};
use antiq::config::Config;

#[test]
fn init_writes_the_defaults_names_the_due_count_and_refuses_a_second_run() {
    both_modes(
        |clock| {
            let collection = Collection::build(clock)
                .uninitialised()
                .piece("projects", |piece| {
                    piece.age_days(40).file("src/main.rs", "fn main() {}\n");
                })
                .piece("old", |piece| {
                    piece.age_days(90).file("notes.txt", "kept\n");
                })
                .piece("notes", |piece| {
                    piece.age_days(5).file("today.txt", "fresh\n");
                })
                .finish();
            Setup::new(collection)
        },
        |world| {
            // `projects` and `old` are idle for longer than the default 30
            // days, `notes` is not.
            let created = world.run(&["init"]);
            created.expect_code(0);
            created.text_only(|out| {
                assert_eq!(
                    out.lines().last(),
                    Some("The next archive would archive 2 folders."),
                    "{out:?}"
                );
            });
            created.json_only(|json| {
                let keys: Vec<&str> = json
                    .as_object()
                    .expect("the output is an object")
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    keys,
                    [
                        "format", "command", "created", "id", "path", "patterns", "due", "warnings"
                    ]
                );
                assert_eq!(json["created"], true);
                assert_eq!(json["id"].as_str().map(str::len), Some(26));
                let root = world.collection().root();
                assert_eq!(json["path"], root.to_str().expect("the path is UTF-8"));
                assert_eq!(json["patterns"], serde_json::json!(["*"]));
                assert_eq!(json["due"], 2);
                assert_eq!(json["warnings"], serde_json::json!([]));
            });

            let path = world.collection().path(".antiq/config.toml");
            let config = Config::read(&path).expect("the written config parses");
            let defaults = &config.collection;
            assert_eq!(defaults.patterns, ["*"]);
            assert!(defaults.exclude.is_empty());
            assert_eq!(defaults.ignore, [".DS_Store"]);
            assert!(defaults.timeless.is_empty());
            assert_eq!(defaults.idle_days, 30);
            assert_eq!(defaults.soon_days, 3);
            assert_eq!(config.target.url, None);
            created.json_only(|json| {
                assert_eq!(json["id"], defaults.id.to_string().as_str());
            });
            // Every key but the ID is a comment at its default, so that a
            // changed default reaches collections that never set it.
            let text = std::fs::read_to_string(&path).expect("the config is readable");
            let active: Vec<&str> = text
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
                .collect();
            assert_eq!(active.len(), 2, "{active:?}");
            assert_eq!(active[0], "[collection]");
            assert!(active[1].starts_with("id = \""), "{active:?}");

            world.assert_unchanged(|| {
                let again = world.run(&["init"]);
                again.expect_code(1);
                again.text_only(|_| {
                    assert!(
                        again.stderr.contains("is already a collection"),
                        "{:?}",
                        again.stderr
                    );
                });
                again.json_only(|json| {
                    assert_eq!(again.error_kind().as_deref(), Some("other"));
                    let message = json["error"]["message"].as_str().expect("a message");
                    assert!(message.contains("is already a collection"), "{message}");
                });
            });
        },
    );
}
