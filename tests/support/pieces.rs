// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pieces made through the binary, for tests that need an unearthed one.

use super::cmd::Antiq;
use super::tree::TempCollection;
use serde_json::Value;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch, `days` before now.
pub fn days_ago(days: i64) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_secs() as i64;
    now - days * 86_400
}

/// `antiq -C <root> <args>`, its JSON output when `--json` is among them.
pub fn run_json(temp: &TempCollection, args: &[&str]) -> Value {
    let run = Antiq::new()
        .arg("-C")
        .arg(temp.root())
        .arg("--json")
        .args(args)
        .run();
    run.json()
}

/// Writes the piece `name` with `files`, all 40 days old, archives it,
/// unearths it again and sets the time it was unearthed to 40 days ago, so
/// that the next `archive` takes it. Returns the ID of the object.
pub fn unearthed(temp: &mut TempCollection, name: &str, files: &[(&str, &str)]) -> String {
    for (file, content) in files {
        temp.tree()
            .file_with(&format!("{name}/{file}"), content, 0o644, days_ago(40));
    }
    let archived = run_json(temp, &["archive"]);
    let id = archived["archived"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("the piece is archived: {archived}"))
        .to_string();
    let unearth = run_json(temp, &["unearth", &id]);
    assert_eq!(unearth["unearthed"], true, "{unearth}");

    let entry = temp.root().join(format!(".antiq/tracking/{id}.json"));
    let mut document: Value =
        serde_json::from_slice(&fs::read(&entry).expect("the entry is there")).expect("json");
    let past = jiff::Timestamp::from_second(days_ago(40)).expect("a time");
    document["unearthed"] = Value::String(past.to_string());
    fs::write(&entry, serde_json::to_vec(&document).expect("json")).expect("written");
    id
}
