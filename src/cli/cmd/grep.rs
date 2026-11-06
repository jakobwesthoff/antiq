// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `grep` command: searches the paths of all archived files.

use crate::cli::output::Output;
use crate::cli::table::sanitize;
use crate::cli::{Context, GrepArgs};
use antiq::collection::Collection;
use antiq::commands::grep::{GrepReport, PieceHits, compile, grep};
use antiq::{lock, report};
use anyhow::Context as _;
use serde::Serialize;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &GrepArgs, out: &mut Output) -> anyhow::Result<u8> {
    // The pattern is part of the command line, so a bad one is a usage
    // error also where no collection can be found.
    let pattern = compile(&args.pattern)
        .map_err(antiq::Error::from)
        .context("compile the pattern")?;
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
    let found = grep(&collection.pieces_dir(), &pattern)
        .map_err(antiq::Error::from)
        .context("search the listings")?;
    let note = lock_note(&collection);

    if ctx.json {
        out.json(Some("grep"), &json_report(&args.pattern, &found, note))
            .context("print the result")?;
    } else {
        if let Some(note) = &note {
            out.note(&format!("note: {note}"))
                .context("print the notes")?;
        }
        for skipped in &found.skipped {
            out.note(&format!(
                "warning: cannot search version {} of {}: {}",
                skipped.stamp,
                sanitize(&skipped.piece.to_string()),
                skipped.reason
            ))
            .context("print the warnings")?;
        }
        for line in pipe_lines(&found) {
            out.line_os(OsStr::from_bytes(&line))
                .context("print the hits")?;
        }
    }
    if found.pieces.is_empty() {
        out.note(&format!(
            "no archived path matches {}",
            sanitize(&args.pattern)
        ))
        .context("print the notes")?;
    }
    Ok(0)
}

/// What the run lock says, for a command that takes no lock: who holds it,
/// or that the lock file cannot be read.
fn lock_note(collection: &Collection) -> Option<String> {
    match lock::holder(&collection.lock_path()) {
        Ok(Some(holder)) => Some(report::lock_note(&holder).message),
        Ok(None) => None,
        Err(error) => Some(format!("cannot read the lock file: {error}")),
    }
}

// =========================================================
// Pipe lines
// =========================================================

/// One line `piece<TAB>stamp<TAB>path` per hit and version, the piece and
/// the path as the raw bytes they are, and `-` for the path of a piece
/// whose name matches.
fn pipe_lines(found: &GrepReport) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    for hits in &found.pieces {
        let line = |stamp: String, path: &[u8]| {
            let mut line = hits.piece.as_bytes().to_vec();
            line.push(b'\t');
            line.extend_from_slice(stamp.as_bytes());
            line.push(b'\t');
            line.extend_from_slice(path);
            line
        };
        if hits.name_match {
            lines.extend(
                hits.versions
                    .iter()
                    .map(|version| line(version.stamp.to_string(), b"-")),
            );
        }
        for file in &hits.files {
            lines.extend(
                file.versions
                    .iter()
                    .map(|stamp| line(stamp.to_string(), &file.path)),
            );
        }
    }
    lines
}

// =========================================================
// JSON
// =========================================================

#[derive(Serialize)]
struct JsonReport<'a> {
    pattern: &'a str,
    pieces: Vec<JsonPiece>,
    skipped: Vec<JsonSkipped<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

#[derive(Serialize)]
struct JsonPiece {
    piece: String,
    name_match: bool,
    versions: Vec<JsonVersion>,
    files: Vec<JsonFile>,
}

#[derive(Serialize)]
struct JsonVersion {
    stamp: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<u64>,
}

#[derive(Serialize)]
struct JsonFile {
    path: String,
    versions: Vec<String>,
}

#[derive(Serialize)]
struct JsonSkipped<'a> {
    piece: String,
    stamp: String,
    reason: &'a str,
}

/// The report as the object of `--json`: pieces and paths in the escaped
/// form, versions by stamp and by their time.
fn json_report<'a>(
    pattern: &'a str,
    found: &'a GrepReport,
    note: Option<String>,
) -> JsonReport<'a> {
    let piece = |hits: &PieceHits| JsonPiece {
        piece: hits.piece.to_string(),
        name_match: hits.name_match,
        versions: hits
            .versions
            .iter()
            .map(|version| JsonVersion {
                stamp: version.stamp.to_string(),
                version: version.stamp.time().to_string(),
                files: version.files,
            })
            .collect(),
        files: hits
            .files
            .iter()
            .map(|file| JsonFile {
                path: antiq::relpath::escape(&file.path),
                versions: file.versions.iter().map(ToString::to_string).collect(),
            })
            .collect(),
    };
    JsonReport {
        pattern,
        pieces: found.pieces.iter().map(piece).collect(),
        skipped: found
            .skipped
            .iter()
            .map(|skipped| JsonSkipped {
                piece: skipped.piece.to_string(),
                stamp: skipped.stamp.to_string(),
                reason: &skipped.reason,
            })
            .collect(),
        note,
    }
}
