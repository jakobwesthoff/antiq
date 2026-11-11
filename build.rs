// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Embeds the terminal renderings of Archie's emblem.
//!
//! Every file in `assets/emblem/terminal/` becomes one entry of a table the
//! crate includes, so a new rendering needs no change in the code. A file
//! whose name does not follow the pattern fails the build.

// The build script prints paths for cargo and cannot use the library.
#![allow(clippy::disallowed_methods)]

#[path = "src/cli/emblem/name.rs"]
#[allow(dead_code)] // the crate uses what the build script does not
mod name;

use std::fmt::Write as _;
use std::path::PathBuf;

const DIRECTORY: &str = "assets/emblem/terminal";

fn main() {
    println!("cargo:rerun-if-changed={DIRECTORY}");
    println!("cargo:rerun-if-changed=src/cli/emblem/name.rs");

    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let directory = root.join(DIRECTORY);
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", directory.display()))
        .map(|entry| entry.expect("a directory entry is readable").path())
        .filter(|path| path.is_file())
        .collect();
    // A fixed order keeps the generated table, and so the build, reproducible.
    files.sort();

    let mut table = String::from("&[\n");
    for path in &files {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| panic!("{} has no UTF-8 name", path.display()));
        let parsed = name::parse(file_name).unwrap_or_else(|message| {
            panic!("{DIRECTORY}/{message}");
        });
        // Each file is tracked on its own too: cargo does not look inside a
        // watched directory for changed contents.
        println!("cargo:rerun-if-changed={}", path.display());
        writeln!(
            table,
            "    Embedded {{ ratio_hundredths: {}, glyphs: Glyphs::{:?}, text: include_str!({:?}) }},",
            parsed.ratio_hundredths,
            parsed.glyphs,
            path.display().to_string()
        )
        .expect("writing to a String cannot fail");
    }
    table.push(']');

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets it"));
    std::fs::write(out.join("emblem_table.rs"), table).expect("OUT_DIR is writable");
}
