// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The JSON envelope of the spec ("JSON output"), checked on every
//! `--json` run.

use serde_json::Value;

/// The error kinds of the spec's table.
const KINDS: [&str; 10] = [
    "usage",
    "not_a_collection",
    "ambiguous_reference",
    "unknown_reference",
    "outside_collection",
    "lock_held",
    "target_unreachable",
    "no_target",
    "format_too_new",
    "other",
];

/// The command a command line names: the first argument that is neither
/// `--json` nor `-C <path>`.
pub fn command_of(args: &[String]) -> Option<String> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => {}
            "-C" => {
                rest.next();
            }
            other if other.starts_with('-') => {}
            other => return Some(other.to_string()),
        }
    }
    None
}

/// Checks stdout and the exit code of a `--json` run of `args`.
pub fn check(args: &[String], stdout: &str, code: i32) -> Result<(), String> {
    // `--help` and `--version` print text, also under `--json`.
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return Ok(());
    }
    let body = stdout
        .strip_suffix('\n')
        .ok_or_else(|| format!("stdout does not end in a newline: {stdout:?}"))?;
    if body.contains('\n') {
        return Err(format!("stdout is more than one line: {stdout:?}"));
    }
    let value: Value =
        serde_json::from_str(body).map_err(|error| format!("stdout is not JSON ({error})"))?;
    let object = value.as_object().ok_or("stdout is not a JSON object")?;
    let keys: Vec<&String> = object.keys().collect();
    if object.get("format") != Some(&Value::from(1))
        || keys.first().map(|key| key.as_str()) != Some("format")
    {
        return Err("the first key is not format: 1".to_string());
    }
    if keys.get(1).map(|key| key.as_str()) != Some("command") {
        return Err("command is not the second key".to_string());
    }

    let error = object.get("error");
    match (&object["command"], command_of(args)) {
        (Value::String(named), Some(ran)) if *named == ran => {}
        (Value::Null, _) if error.is_some_and(|e| e["kind"] == "usage") => {}
        (Value::Null, _) => return Err("a null command is only for a usage error".to_string()),
        (named, ran) => return Err(format!("command {named} does not match {ran:?}")),
    }

    if let Some(error) = error {
        check_error(error, code)?;
    } else if matches!(code, 1 | 2 | 4) && !refused_piece_of_archive(object, code) {
        return Err(format!("exit {code} comes without an error object"));
    }

    if let Some(position) = keys.iter().position(|key| key.as_str() == "dry_run") {
        if object["dry_run"] != Value::Bool(true) {
            return Err("dry_run is present but not true".to_string());
        }
        // `lock_held` may stand between the two, as the spec shows.
        let actions = keys.iter().position(|key| key.as_str() == "actions");
        if actions.is_none_or(|actions| actions < position) || !object["actions"].is_array() {
            return Err("actions does not follow dry_run as a list".to_string());
        }
    }
    Ok(())
}

/// `archive <piece>` exits 1 when that piece is refused and prints its
/// report, which lists the piece under `refused` and carries no `error`.
fn refused_piece_of_archive(object: &serde_json::Map<String, Value>, code: i32) -> bool {
    code == 1
        && object["command"] == "archive"
        && object
            .get("refused")
            .and_then(Value::as_array)
            .is_some_and(|refused| !refused.is_empty())
}

fn check_error(error: &Value, code: i32) -> Result<(), String> {
    let kind = error["kind"].as_str().unwrap_or_default();
    if !KINDS.contains(&kind) {
        return Err(format!("unknown error kind {kind:?}"));
    }
    if !error["message"].is_string() {
        return Err("the error has no message".to_string());
    }
    // `push --if-reachable` exits 0 with the reason; the spec does not
    // fix the shape of that output.
    let allowed: &[i32] = match kind {
        "usage" => &[2],
        "target_unreachable" => &[4, 0],
        _ => &[1],
    };
    if !allowed.contains(&code) {
        return Err(format!(
            "kind {kind} comes with exit {code}, not {allowed:?}"
        ));
    }
    if kind == "lock_held" && error.get("pid").is_some() != error.get("started").is_some() {
        return Err("lock_held carries pid and started together or neither".to_string());
    }
    if kind == "ambiguous_reference" {
        let listed = error["candidates"].as_array().is_some_and(|candidates| {
            candidates
                .iter()
                .all(|candidate| candidate["path"].is_string())
        });
        if !listed {
            return Err("candidates is not a list of {path}".to_string());
        }
    }
    Ok(())
}
