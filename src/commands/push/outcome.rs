// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a push tells: its report and the dry-run plan.

use super::summary::{Changes, Counts};
use crate::plan::{HeldDryRun, Plan, PlanAction, Step, serialize_repairs};
use crate::repair::Repair;
use crate::report::Warning;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

/// How a push ended, when it did not fail.
#[derive(Debug)]
pub enum PushOutcome {
    /// A dry run that found another run holding the lock.
    HeldDryRun(HeldDryRun),
    Report(Box<PushReport>),
}

/// An action of the plan a dry run prints, after the repairs.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushAction {
    /// The marker `.antiq-target` is sent to the target.
    WriteMarker,
    /// The mirror run, with the changes `rsync -n` itemized.
    Mirror {
        #[serde(flatten)]
        changes: Changes,
    },
}

impl PlanAction for PushAction {
    fn describe(&self) -> String {
        match self {
            PushAction::WriteMarker => "write the marker .antiq-target to the target".to_string(),
            PushAction::Mirror { .. } => "mirror the collection to the target".to_string(),
        }
    }
}

/// The report of a push.
#[derive(Debug)]
pub struct PushReport {
    pub dry_run: bool,
    /// The target cannot be reached; `reason` says why.
    pub reason: Option<String>,
    /// The `[target] url` as written.
    pub target: String,
    /// The marker was written (in a dry run: would be).
    pub initialized: bool,
    pub forced: bool,
    pub changes: Changes,
    /// The paths of the `file has vanished` lines.
    pub vanished: Vec<String>,
    /// The mirror ran a second time.
    pub second_pass: bool,
    /// How the push was cleared, such as a forced push.
    pub notes: Vec<String>,
    /// The repairs at the start of a real run that were done.
    pub repairs: Vec<Repair>,
    pub warnings: Vec<Warning>,
    /// The repairs and the actions of a dry run.
    pub plan: Option<Plan<Step<PushAction>>>,
}

impl PushReport {
    fn result(&self) -> &'static str {
        if self.reason.is_some() {
            "unreachable"
        } else {
            "pushed"
        }
    }

    fn to_value(&self) -> Value {
        let mut object = Map::new();
        if self.dry_run {
            object.insert("dry_run".into(), json!(true));
        }
        if let Some(plan) = &self.plan {
            object.insert(
                "actions".into(),
                serde_json::to_value(&plan.actions).expect("actions serialise"),
            );
        }
        object.insert("result".into(), json!(self.result()));
        object.insert("target".into(), json!(self.target));
        if let Some(reason) = &self.reason {
            object.insert("reason".into(), json!(reason));
        }
        if self.reason.is_none() && !self.dry_run {
            object.insert("initialized".into(), json!(self.initialized));
            object.insert("forced".into(), json!(self.forced));
            object.insert(
                "pieces".into(),
                serde_json::to_value(&self.changes.pieces).expect("pieces serialise"),
            );
            object.insert(
                "other".into(),
                serde_json::to_value(self.changes.other).expect("counts serialise"),
            );
            object.insert("vanished".into(), json!(self.vanished));
            object.insert("second_pass".into(), json!(self.second_pass));
            object.insert("notes".into(), json!(self.notes));
        }
        if !self.dry_run {
            object.insert(
                "repairs".into(),
                serialize_repairs(&self.repairs, serde_json::value::Serializer)
                    .expect("repairs serialise"),
            );
        }
        object.insert(
            "warnings".into(),
            serde_json::to_value(&self.warnings).expect("warnings serialise"),
        );
        Value::Object(object)
    }

    /// The lines the console prints to stdout. An unreachable target
    /// prints its reason to stderr instead, see [`PushReport::reason`].
    pub fn lines(&self) -> Vec<String> {
        if self.reason.is_some() {
            // The repairs of the dry run are real output. "Nothing to do."
            // would contradict the reason that goes to stderr.
            return self
                .plan
                .as_ref()
                .filter(|plan| !plan.is_empty())
                .map(|plan| plan.lines())
                .unwrap_or_default();
        }
        let mut lines = Vec::new();
        if let Some(plan) = &self.plan {
            lines.extend(plan.lines());
        } else if self.initialized {
            lines.push("Wrote the marker .antiq-target.".to_string());
        }
        let verb = if self.dry_run {
            "Would change"
        } else {
            "Changed"
        };
        if self.changes.is_empty() {
            lines.push(if self.dry_run {
                format!("The target {} is up to date.", self.target)
            } else {
                format!("Pushed to {}. Nothing changed.", self.target)
            });
        } else {
            let counts = |counts: &Counts| {
                format!(
                    "{} created, {} updated, {} deleted",
                    counts.created, counts.updated, counts.deleted
                )
            };
            lines.push(format!("{verb} on {}:", self.target));
            for piece in &self.changes.pieces {
                lines.push(format!("  {}: {}", piece.piece, counts(&piece.counts)));
            }
            if self.changes.other != Counts::default() {
                lines.push(format!("  other files: {}", counts(&self.changes.other)));
            }
        }
        for note in &self.notes {
            lines.push(format!("Note: {note}"));
        }
        if !self.vanished.is_empty() {
            lines.push(format!(
                "Note: {} files vanished while rsync ran: {}",
                self.vanished.len(),
                self.vanished.join(", ")
            ));
        }
        lines
    }
}

impl Serialize for PushReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::super::summary::PieceChanges;
    use super::*;
    use crate::relpath::RelPath;
    use crate::repair::Repair;

    fn changes() -> Changes {
        Changes {
            pieces: vec![PieceChanges {
                piece: RelPath::parse("work").expect("a path"),
                counts: Counts {
                    created: 2,
                    updated: 1,
                    deleted: 0,
                },
            }],
            other: Counts {
                created: 0,
                updated: 0,
                deleted: 3,
            },
        }
    }

    fn report() -> PushReport {
        PushReport {
            dry_run: false,
            reason: None,
            target: "backup@nas:/srv".into(),
            initialized: true,
            forced: false,
            changes: changes(),
            vanished: vec!["/c/x".into()],
            second_pass: true,
            notes: vec!["be careful".into()],
            repairs: Vec::new(),
            warnings: vec![Warning::new("w")],
            plan: None,
        }
    }

    fn keys(value: &Value) -> Vec<&str> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn a_real_push_reports_the_changes_in_a_fixed_key_order() {
        let value = serde_json::to_value(report()).expect("serialises");
        assert_eq!(
            keys(&value),
            [
                "result",
                "target",
                "initialized",
                "forced",
                "pieces",
                "other",
                "vanished",
                "second_pass",
                "notes",
                "repairs",
                "warnings"
            ]
        );
        assert_eq!(value["result"], "pushed");
        assert_eq!(
            value["pieces"],
            json!([{"piece": "work", "created": 2, "updated": 1, "deleted": 0}])
        );
        assert_eq!(
            value["other"],
            json!({"created": 0, "updated": 0, "deleted": 3})
        );
        assert_eq!(value["warnings"], json!([{"message": "w"}]));
    }

    #[test]
    fn an_unreachable_push_reports_the_reason_and_none_of_the_changes() {
        let mut unreachable = report();
        unreachable.reason = Some("share gone".into());
        let value = serde_json::to_value(unreachable).expect("serialises");
        assert_eq!(
            keys(&value),
            ["result", "target", "reason", "repairs", "warnings"]
        );
        assert_eq!(value["result"], "unreachable");
        assert_eq!(value["reason"], "share gone");
    }

    #[test]
    fn a_dry_run_lists_its_actions_after_dry_run() {
        let mut plan = Plan::after_repairs(Vec::new());
        plan.push_command(PushAction::WriteMarker);
        plan.push_command(PushAction::Mirror { changes: changes() });
        let mut dry = report();
        dry.dry_run = true;
        dry.plan = Some(plan);
        let value = serde_json::to_value(dry).expect("serialises");
        assert_eq!(
            keys(&value),
            ["dry_run", "actions", "result", "target", "warnings"]
        );
        assert_eq!(value["actions"][0], json!({"kind": "write_marker"}));
        assert_eq!(value["actions"][1]["kind"], "mirror");
        assert_eq!(value["actions"][1]["other"]["deleted"], 3);
        assert_eq!(value["actions"][1]["pieces"][0]["piece"], "work");
    }

    #[test]
    fn the_console_lists_the_changes_per_piece_and_the_notes() {
        let lines = report().lines();
        assert_eq!(lines[0], "Wrote the marker .antiq-target.");
        assert_eq!(lines[1], "Changed on backup@nas:/srv:");
        assert_eq!(lines[2], "  work: 2 created, 1 updated, 0 deleted");
        assert_eq!(lines[3], "  other files: 0 created, 0 updated, 3 deleted");
        assert_eq!(lines[4], "Note: be careful");
        assert!(lines[5].contains("1 files vanished") && lines[5].contains("/c/x"));
    }

    #[test]
    fn a_push_without_changes_says_so_and_a_dry_run_says_would() {
        let mut quiet = report();
        quiet.initialized = false;
        quiet.changes = Changes::default();
        quiet.notes.clear();
        quiet.vanished.clear();
        assert_eq!(
            quiet.lines(),
            ["Pushed to backup@nas:/srv. Nothing changed."]
        );

        quiet.dry_run = true;
        quiet.plan = Some(Plan::after_repairs(Vec::new()));
        assert!(quiet.lines().last().expect("a line").contains("up to date"));

        let mut dry = report();
        dry.dry_run = true;
        dry.plan = Some(Plan::after_repairs(Vec::new()));
        assert!(dry.lines().iter().any(|l| l.starts_with("Would change on")));
    }

    #[test]
    fn an_unreachable_push_prints_nothing_on_stdout_except_its_plan() {
        let mut unreachable = report();
        unreachable.reason = Some("gone".into());
        assert!(unreachable.lines().is_empty());
    }

    #[test]
    fn an_unreachable_dry_run_prints_its_repairs_and_never_nothing_to_do() {
        let mut unreachable = report();
        unreachable.dry_run = true;
        unreachable.reason = Some("gone".into());
        unreachable.plan = Some(Plan::after_repairs(Vec::new()));
        assert!(unreachable.lines().is_empty(), "{:?}", unreachable.lines());

        unreachable.plan = Some(Plan::after_repairs(vec![Repair::DeleteSidecars {
            piece: RelPath::parse("a/b").expect("a path"),
            version: "2026-11-04T090211".parse().expect("a stamp"),
        }]));
        let lines = unreachable.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Would "), "{lines:?}");
    }
}
