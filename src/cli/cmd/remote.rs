// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `remote add`, `remote show` and `remote remove` commands.

use crate::cli::output::Output;
use crate::cli::{Context, RemoteAddArgs, RemoteCommand};
use antiq::clock::SystemClock;
use antiq::collection::Collection;
use antiq::id::RandomIds;
use antiq::ops::remote::{AddReport, AddRequest, RemoteOpError, add};
use antiq::run::{self, Started};
use anyhow::Context as _;
use std::path::PathBuf;

pub fn run(ctx: &Context, args: &RemoteCommand, out: &mut Output) -> anyhow::Result<u8> {
    match args {
        RemoteCommand::Add(add_args) => add_command(ctx, add_args, out),
        other => super::not_implemented(other.name()),
    }
}

fn add_command(ctx: &Context, args: &RemoteAddArgs, out: &mut Output) -> anyhow::Result<u8> {
    let start = ctx.directory.clone().unwrap_or_else(|| PathBuf::from("."));
    let collection = Collection::find(&start)
        .map_err(antiq::Error::from)
        .context("find the collection")?;
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
    // A dry run takes no lock, so it says when another run holds it.
    if let Started::DryRun {
        holder: Some(holder),
    } = &started
    {
        lines.stderr.push(format!(
            "note: another run holds the lock (pid {}, machine {}, started {})",
            holder.pid, holder.machine, holder.started
        ));
    }
    for line in &lines.stdout {
        out.line(line).context("print the result")?;
    }
    for line in &lines.stderr {
        out.note(line).context("print the notes")?;
    }
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
    let stderr = report
        .warnings
        .iter()
        .map(|warning| format!("note: {}", warning.message))
        .collect();
    Lines { stdout, stderr }
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
