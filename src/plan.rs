// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ordered actions a command intends to take.

use crate::clock::whole_seconds;
use crate::lock::Holder;
use crate::repair::Repair;
use jiff::Timestamp;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

/// What a command is going to do, in order. `A` is the action type of the
/// command.
#[derive(Debug, Serialize)]
pub struct Plan<A> {
    pub actions: Vec<A>,
}

impl<A> Plan<A> {
    pub fn new() -> Self {
        Plan {
            actions: Vec::new(),
        }
    }

    pub fn push(&mut self, action: A) {
        self.actions.push(action);
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

impl<A> Default for Plan<A> {
    fn default() -> Self {
        Self::new()
    }
}

/// An action of a plan, as a dry run names it.
pub trait PlanAction: Serialize {
    /// What the action does, in the imperative and without a leading
    /// "would": `archive projects/a`.
    fn describe(&self) -> String;
}

/// A plan as `--dry-run` prints it in JSON.
#[derive(Debug, Serialize)]
pub struct DryRun<'a, A> {
    dry_run: bool,
    #[serde(flatten)]
    plan: &'a Plan<A>,
}

impl<A: PlanAction> Plan<A> {
    /// The plan as the JSON payload of a dry run: `dry_run: true` first,
    /// then `actions`.
    pub fn dry_run(&self) -> DryRun<'_, A> {
        DryRun {
            dry_run: true,
            plan: self,
        }
    }

    /// The plan as the console prints it for a dry run: a `Would ...` line
    /// per action, or `Nothing to do.`.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .actions
            .iter()
            .map(|action| format!("Would {}", action.describe()))
            .collect();
        if lines.is_empty() {
            lines.push("Nothing to do.".to_string());
        }
        lines
    }
}

// =========================================================
// Repairs in front of a command's own actions
// =========================================================

/// One action of a dry run of a state-changing command: a repair of the
/// start of the run, or an action of the command.
#[derive(Debug)]
pub enum Step<A> {
    Repair(Repair),
    Command(A),
}

/// A repair as every command's JSON lists it: `{"kind": "repair",
/// "repair": ...}`.
#[derive(Serialize)]
struct Tagged<'a> {
    kind: &'static str,
    #[serde(flatten)]
    repair: &'a Repair,
}

impl<'a> Tagged<'a> {
    fn new(repair: &'a Repair) -> Self {
        Tagged {
            kind: "repair",
            repair,
        }
    }
}

/// Serialises a list of repairs that a real run did, for the reports whose
/// `repairs` field holds bare [`Repair`]s.
pub fn serialize_repairs<S: Serializer>(
    repairs: &[Repair],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(repairs.iter().map(Tagged::new))
}

impl<A: Serialize> Serialize for Step<A> {
    /// A repair is `{"kind": "repair", "repair": ...}`; a command action
    /// serialises as it is.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Step::Repair(repair) => Tagged::new(repair).serialize(serializer),
            Step::Command(action) => action.serialize(serializer),
        }
    }
}

impl<A: PlanAction> PlanAction for Step<A> {
    fn describe(&self) -> String {
        match self {
            Step::Repair(repair) => repair.describe(),
            Step::Command(action) => action.describe(),
        }
    }
}

impl<A> Plan<Step<A>> {
    /// A plan that starts with the repairs a run would do first.
    pub fn after_repairs(repairs: Vec<Repair>) -> Self {
        Plan {
            actions: repairs.into_iter().map(Step::Repair).collect(),
        }
    }

    /// Adds an action of the command behind the repairs and the actions
    /// added so far.
    pub fn push_command(&mut self, action: A) {
        self.actions.push(Step::Command(action));
    }
}

/// The holder of the run lock as a dry run names it in its JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LockHeld {
    pub pid: u32,
    /// In whole seconds.
    pub started: Timestamp,
}

impl LockHeld {
    pub fn of(holder: &Holder) -> LockHeld {
        LockHeld {
            pid: holder.pid,
            started: whole_seconds(holder.started),
        }
    }
}

/// The dry run of a command whose run lock is held by another run: the
/// repairs are not listed, since the other run is doing them.
#[derive(Debug)]
pub struct HeldDryRun {
    pid: u32,
    started: Timestamp,
}

impl HeldDryRun {
    /// The dry run that names the holder of the lock instead of listing
    /// anything.
    pub fn new(holder: &Holder) -> HeldDryRun {
        HeldDryRun {
            pid: holder.pid,
            started: holder.started,
        }
    }

    /// The line the console prints.
    pub fn lines(&self) -> Vec<String> {
        vec![format!(
            "The lock is held by pid {}, started {}. No repairs are listed.",
            self.pid, self.started
        )]
    }
}

impl Serialize for HeldDryRun {
    /// `{"dry_run": true, "lock_held": {"pid", "started"}, "actions": []}`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Holding {
            pid: u32,
            started: Timestamp,
        }
        let mut state = serializer.serialize_struct("HeldDryRun", 3)?;
        state.serialize_field("dry_run", &true)?;
        state.serialize_field(
            "lock_held",
            &Holding {
                pid: self.pid,
                started: self.started,
            },
        )?;
        state.serialize_field("actions", &Vec::<()>::new())?;
        state.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relpath::RelPath;
    use serde_json::json;

    #[test]
    fn a_plan_serialises_its_actions_only() {
        let mut plan = Plan::new();
        assert!(plan.is_empty());
        plan.push("archive projects/a");
        plan.push("archive projects/b");
        assert_eq!(plan.len(), 2);
        assert_eq!(
            serde_json::to_value(&plan).expect("plan serialises"),
            json!({"actions": ["archive projects/a", "archive projects/b"]})
        );
    }

    #[test]
    fn a_default_plan_is_empty() {
        let plan = Plan::<String>::default();
        assert!(plan.is_empty());
    }

    #[derive(Serialize)]
    #[serde(transparent)]
    struct Line(&'static str);

    impl PlanAction for Line {
        fn describe(&self) -> String {
            self.0.to_string()
        }
    }

    #[test]
    fn a_dry_run_in_json_says_so_before_the_actions() {
        let mut plan = Plan::new();
        plan.push(Line("archive projects/a"));
        plan.push(Line("archive projects/b"));
        let text = serde_json::to_string(&plan.dry_run()).expect("a dry run serialises");
        assert_eq!(
            text,
            r#"{"dry_run":true,"actions":["archive projects/a","archive projects/b"]}"#
        );
    }

    #[test]
    fn an_empty_dry_run_in_json_has_no_actions() {
        let free = serde_json::to_value(Plan::<Line>::new().dry_run()).expect("serialises");
        assert_eq!(free, json!({"dry_run": true, "actions": []}));
    }

    #[test]
    fn each_action_is_a_would_line() {
        let mut plan = Plan::new();
        plan.push(Line("archive projects/a"));
        plan.push(Line("archive projects/b"));
        assert_eq!(
            plan.lines(),
            ["Would archive projects/a", "Would archive projects/b"]
        );
    }

    #[test]
    fn an_empty_plan_has_nothing_to_do() {
        assert_eq!(Plan::<Line>::new().lines(), ["Nothing to do."]);
    }

    fn delete_sidecars() -> Repair {
        Repair::DeleteSidecars {
            piece: RelPath::parse("a/b").expect("a path"),
            version: "2026-11-04T090211".parse().expect("a stamp"),
        }
    }

    #[test]
    fn repairs_come_first_in_json_with_their_kind() {
        let mut plan = Plan::after_repairs(vec![delete_sidecars()]);
        plan.push_command(Line("archive projects/a"));
        let text = serde_json::to_string(&plan.dry_run()).expect("serialises");
        assert_eq!(
            text,
            concat!(
                r#"{"dry_run":true,"actions":["#,
                r#"{"kind":"repair","repair":"delete_sidecars","piece":"a/b","version":"2026-11-04T090211"},"#,
                r#""archive projects/a"]}"#
            )
        );
    }

    #[test]
    fn repairs_are_would_lines_before_the_commands_own() {
        let mut plan = Plan::after_repairs(vec![delete_sidecars()]);
        plan.push_command(Line("archive projects/a"));
        assert_eq!(
            plan.lines(),
            [
                "Would delete the sidecars of 2026-11-04T090211 in .antiq/pieces/a/b (the archive is not there)",
                "Would archive projects/a"
            ]
        );
    }

    #[test]
    fn a_dry_run_with_a_held_lock_names_the_holder_and_lists_no_actions() {
        let holder = Holder {
            pid: 4711,
            machine: "imperius".to_string(),
            started: "2026-11-04T09:02:11Z".parse().expect("a time"),
        };
        let held = HeldDryRun::new(&holder);
        assert_eq!(
            serde_json::to_string(&held).expect("serialises"),
            r#"{"dry_run":true,"lock_held":{"pid":4711,"started":"2026-11-04T09:02:11Z"},"actions":[]}"#
        );
        assert_eq!(
            held.lines(),
            ["The lock is held by pid 4711, started 2026-11-04T09:02:11Z. No repairs are listed."]
        );
    }
}
