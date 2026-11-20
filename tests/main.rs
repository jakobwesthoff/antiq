// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The integration tests, built as one test binary so that a build after a
//! change links it once instead of once per file. Each file in `tests/` is a
//! module of it, declared below; `Cargo.toml` turns off the discovery of
//! the files as test binaries of their own.

// Tests print paths in assertion messages and in the commands they run.
#![allow(clippy::disallowed_methods)]

mod support;

mod archive;
mod cli;
mod console_errors;
mod degradation;
mod dryrun;
mod fake_rsync_fixture;
mod find;
mod forget;
mod fork;
mod grep;
mod hostile;
mod import;
mod init;
mod json_shapes;
mod live_progress;
mod message_names;
mod notice_order;
mod pin;
mod pin_screens;
mod push;
mod push_log;
mod push_progress;
mod readme;
mod rebuild;
mod run_folders;
mod scenarios;
mod skill;
mod ssh_shim_fixture;
mod start_directory;
mod status;
mod tmp_empty;
mod unearth;
mod unresolved;
mod usage_errors;
mod version_screen;
mod versions_fixture;
mod warnings;

/// A test file that no `mod` line names is never compiled, and nothing
/// reports it, since Cargo no longer builds it on its own.
#[test]
fn every_test_file_is_a_module_of_this_binary() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for (dir, declaring) in [("", "main.rs"), ("scenarios", "scenarios/mod.rs")] {
        let declared = std::fs::read_to_string(tests.join(declaring))
            .unwrap_or_else(|error| panic!("{declaring} reads: {error}"));
        for entry in std::fs::read_dir(tests.join(dir)).expect("the directory reads") {
            let path = entry.expect("an entry").path();
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if path.extension().is_some_and(|ext| ext == "rs") && !["main", "mod"].contains(&stem) {
                assert!(
                    declared.contains(&format!("\nmod {stem};\n")),
                    "{} is not declared in {declaring}",
                    path.display()
                );
            }
        }
    }
}
