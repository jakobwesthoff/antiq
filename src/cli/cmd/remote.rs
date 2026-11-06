// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `remote add`, `remote show` and `remote remove` commands.

use super::status::sanitize;
use crate::cli::output::Output;
use crate::cli::{Context, RemoteAddArgs, RemoteCommand, RemoteRemoveArgs};
use antiq::brake::BrakeInput;
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::id::RandomIds;
use antiq::ops::remote::{
    AddReport, AddRequest, RemoteOpError, RemoveReport, ShowReport, add, remove, show,
};
use antiq::remote::RemoteState;
use antiq::report;
use antiq::run::{self, Started};
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &RemoteCommand, out: &mut Output) -> anyhow::Result<u8> {
    match args {
        RemoteCommand::Add(add_args) => add_command(ctx, add_args, out),
        RemoteCommand::Show => show_command(ctx, out),
        RemoteCommand::Remove(remove_args) => remove_command(ctx, remove_args, out),
    }
}

fn find_collection(ctx: &Context) -> anyhow::Result<Collection> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")
}

fn print(out: &mut Output, lines: &Lines) -> anyhow::Result<()> {
    for line in &lines.stdout {
        out.line(line).context("print the result")?;
    }
    for line in &lines.stderr {
        out.note(line).context("print the notes")?;
    }
    Ok(())
}

/// The note a dry run prints when another run holds the lock: a dry run
/// takes none itself.
fn lock_note(started: &Started) -> Option<String> {
    match started {
        Started::DryRun {
            holder: Some(holder),
        } => Some(format!(
            "note: {}",
            sanitize(&report::lock_note(holder).message)
        )),
        _ => None,
    }
}

fn show_command(ctx: &Context, out: &mut Output) -> anyhow::Result<u8> {
    let collection = find_collection(ctx)?;
    let report = show(&collection)
        .map_err(antiq::Error::from)
        .context("look at the remote")?;
    if ctx.json {
        out.json(Some("remote show"), &report)
            .context("print the result")?;
        return Ok(0);
    }
    print(out, &render_show(&report))?;
    Ok(0)
}

fn remove_command(ctx: &Context, args: &RemoteRemoveArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = find_collection(ctx)?;
    let clock = SystemClock;
    let started = run::start(&collection, &clock, args.dry_run)
        .map_err(antiq::Error::from)
        .context("start the run")?;
    let mut interaction = ctx.interaction();
    let brake = BrakeInput {
        force: args.force,
        interaction: interaction.as_mut(),
    };
    let report = remove(&collection, &started, brake)
        .map_err(antiq::Error::from)
        .context("detach the remote")?;
    if ctx.json {
        out.json(Some("remote remove"), &report)
            .context("print the result")?;
        return Ok(0);
    }
    let mut lines = render_remove(&report);
    lines.stderr.extend(lock_note(&started));
    print(out, &lines)?;
    Ok(0)
}

fn add_command(ctx: &Context, args: &RemoteAddArgs, out: &mut Output) -> anyhow::Result<u8> {
    let collection = find_collection(ctx)?;
    let clock = SystemClock;
    let started = run::start(&collection, &clock, args.dry_run)
        .map_err(antiq::Error::from)
        .context("start the run")?;
    let request = AddRequest {
        url: &args.url,
        init: args.init,
        host_key_alias: args.host_key_alias.as_deref(),
    };
    let report = add(&collection, &request, &started, &clock, &RandomIds)
        .map_err(|error| match error {
            // A URL that does not fit and an option that does not apply to
            // it are flaws of the command line.
            error @ (RemoteOpError::Url(_) | RemoteOpError::HostKeyAliasNeedsSsh) => {
                antiq::Error::Usage(error.to_string())
            }
            error => error.into(),
        })
        .context("attach the remote")?;

    if ctx.json {
        out.json(Some("remote add"), &report)
            .context("print the result")?;
        return Ok(0);
    }
    let mut lines = render(&report);
    lines.stderr.extend(lock_note(&started));
    print(out, &lines)?;
    Ok(0)
}

/// What a run prints on a console.
#[derive(Debug, PartialEq, Eq)]
struct Lines {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

fn render(report: &AddReport) -> Lines {
    let mut stdout = Vec::new();
    if report.dry_run {
        stdout = report.plan_lines();
    } else {
        let verb = if report.created {
            "Created"
        } else {
            "Attached"
        };
        stdout.push(format!(
            "{verb} the {} remote at {}",
            report.kind, report.url
        ));
        if let Some(id) = report.id {
            stdout.push(format!("  id: {id}"));
        }
        match report.pulled {
            Some(1) => stdout.push("  metadata copied for 1 object".to_string()),
            Some(objects) if !report.created => {
                stdout.push(format!("  metadata copied for {objects} objects"));
            }
            _ => {}
        }
    }
    Lines {
        stdout,
        stderr: notes(&report.warnings),
    }
}

fn notes(warnings: &[antiq::report::Warning]) -> Vec<String> {
    warnings
        .iter()
        .map(|warning| format!("note: {}", warning.message))
        .collect()
}

fn render_show(report: &ShowReport) -> Lines {
    let (Some(kind), Some(url), Some(id), Some(state)) =
        (report.kind, &report.url, report.id, report.state)
    else {
        return Lines {
            stdout: vec!["No remote is attached.".to_string()],
            stderr: notes(&report.warnings),
        };
    };
    let word = match state {
        RemoteState::Reachable => "reachable",
        RemoteState::Away => "away",
        RemoteState::Broken => "broken",
        RemoteState::None => "none",
    };
    let state = match &report.reason {
        Some(reason) => format!("{word} ({reason})"),
        None => word.to_string(),
    };
    let mut stdout = vec![
        format!("remote: {kind} {url}"),
        format!("  id: {id}"),
        format!("  state: {state}"),
    ];
    let probed = report.probed.iter().flatten();
    for (name, label) in [
        ("rsync", "rsync"),
        ("remote_rsync", "remote rsync"),
        ("sha256", "sha256"),
        ("stat", "stat"),
    ] {
        if let Some((_, found)) = probed.clone().find(|(key, _)| *key == name) {
            stdout.push(format!("  {label}: {}", found.as_str().unwrap_or("none")));
        }
    }
    Lines {
        stdout,
        stderr: notes(&report.warnings),
    }
}

fn render_remove(report: &RemoveReport) -> Lines {
    let mut stdout = if report.dry_run {
        report.plan_lines()
    } else {
        vec![format!("Detached the remote at {}", report.url)]
    };
    if !report.remote_only.is_empty() {
        stdout.push(
            if report.dry_run {
                "These remote-only pieces would no longer be reachable:"
            } else {
                "These remote-only pieces can no longer be reached:"
            }
            .to_string(),
        );
        stdout.extend(
            report
                .remote_only
                .iter()
                .map(|piece| format!("  {} ({})", piece.path, piece.id)),
        );
    }
    Lines {
        stdout,
        stderr: notes(&report.warnings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiq::ops::remote::RemoteAction;
    use antiq::report::Warning;

    fn report() -> AddReport {
        AddReport {
            kind: "path",
            url: "/Volumes/nas/antiq".into(),
            id: Some("01BX5ZZKBKACTAV9WEVGEMMVRY".parse().expect("a ULID")),
            attached: true,
            created: false,
            pulled: Some(3),
            dry_run: false,
            actions: Vec::new(),
            warnings: vec![Warning::new("watch out")],
        }
    }

    fn texts(lines: &[String]) -> Vec<&str> {
        lines.iter().map(String::as_str).collect()
    }

    #[test]
    fn an_attached_remote_shows_its_id_and_the_copy() {
        let lines = render(&report());
        assert_eq!(
            texts(&lines.stdout),
            [
                "Attached the path remote at /Volumes/nas/antiq",
                "  id: 01BX5ZZKBKACTAV9WEVGEMMVRY",
                "  metadata copied for 3 objects",
            ]
        );
        assert_eq!(texts(&lines.stderr), ["note: watch out"]);
    }

    #[test]
    fn one_object_is_not_plural() {
        let lines = render(&AddReport {
            pulled: Some(1),
            ..report()
        });
        assert_eq!(lines.stdout[2], "  metadata copied for 1 object");
    }

    #[test]
    fn a_created_remote_says_so_and_has_nothing_copied() {
        let lines = render(&AddReport {
            created: true,
            pulled: Some(0),
            ..report()
        });
        assert_eq!(
            texts(&lines.stdout),
            [
                "Created the path remote at /Volumes/nas/antiq",
                "  id: 01BX5ZZKBKACTAV9WEVGEMMVRY",
            ]
        );
    }

    fn shown(state: RemoteState, reason: Option<&str>) -> ShowReport {
        ShowReport {
            attached: true,
            kind: Some("path"),
            url: Some("/Volumes/nas/antiq".into()),
            id: Some("01BX5ZZKBKACTAV9WEVGEMMVRY".parse().expect("a ULID")),
            state: Some(state),
            reason: reason.map(String::from),
            probed: Some(serde_json::Map::new()),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn the_state_of_a_remote_that_is_away_comes_with_its_reason() {
        let lines = render_show(&shown(
            RemoteState::Away,
            Some("/Volumes/nas does not exist"),
        ));
        assert_eq!(
            texts(&lines.stdout),
            [
                "remote: path /Volumes/nas/antiq",
                "  id: 01BX5ZZKBKACTAV9WEVGEMMVRY",
                "  state: away (/Volumes/nas does not exist)",
            ]
        );
    }

    #[test]
    fn a_broken_remote_is_called_broken() {
        let lines = render_show(&shown(RemoteState::Broken, Some("wrong marker")));
        assert_eq!(lines.stdout[2], "  state: broken (wrong marker)");
    }

    #[test]
    fn a_state_of_none_is_called_none() {
        let lines = render_show(&shown(RemoteState::None, None));
        assert_eq!(lines.stdout[2], "  state: none");
    }

    #[test]
    fn a_dry_run_prints_the_plan() {
        let lines = render(&AddReport {
            attached: false,
            pulled: None,
            dry_run: true,
            actions: vec![RemoteAction::CreateRemote {
                url: "/Volumes/nas/antiq".into(),
            }],
            ..report()
        });
        assert_eq!(
            texts(&lines.stdout),
            ["Would create the remote at /Volumes/nas/antiq"]
        );
    }
}
