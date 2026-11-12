// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `.antiq/README.md` seen from outside the binary.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

use antiq::collection::README;
use support::cmd::Antiq;

/// The commands the README shows are the lines of its code blocks that
/// start with `antiq `, cut where the description of the command begins:
/// at the first run of two spaces.
fn commands_of_the_readme() -> Vec<Vec<&'static str>> {
    README
        .lines()
        .filter(|line| line.starts_with("antiq "))
        .map(|line| {
            let command = line.split("  ").next().expect("a line has a first part");
            command.split_whitespace().skip(1).collect()
        })
        .collect()
}

#[test]
fn every_command_the_readme_shows_exists() {
    let commands = commands_of_the_readme();
    assert!(!commands.is_empty(), "the README shows no antiq command");
    for words in commands {
        let name = words.first().expect("a command line names a command");
        let run = Antiq::new().args([*name, "--help"]).run();
        assert_eq!(run.code(), 0, "`antiq {name} --help`: {}", run.stderr);
    }
}
