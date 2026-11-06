// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ordered actions a command intends to take.

use crate::brake::BrakeHold;
use serde::Serialize;

/// What a command is going to do, in order, and where the brake would hold
/// it. `A` is the action type of the command.
#[derive(Debug, Serialize)]
pub struct Plan<A> {
    pub actions: Vec<A>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brake: Option<BrakeHold>,
}

impl<A> Plan<A> {
    pub fn new() -> Self {
        Plan {
            actions: Vec::new(),
            brake: None,
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
    /// then `actions`, then `brake` where it would hold.
    pub fn dry_run(&self) -> DryRun<'_, A> {
        DryRun {
            dry_run: true,
            plan: self,
        }
    }

    /// The plan as the console prints it for a dry run: a `Would ...` line
    /// per action, or `Nothing to do.`, and the places the brake would
    /// hold.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .actions
            .iter()
            .map(|action| format!("Would {}", action.describe()))
            .collect();
        if lines.is_empty() {
            lines.push("Nothing to do.".to_string());
        }
        if let Some(hold) = &self.brake {
            lines.push(format!(
                "The brake would hold at {} of {}:",
                hold.planned, hold.set_size
            ));
            lines.extend(
                hold.items()
                    .iter()
                    .map(|item| format!("  {} ({})", item.path, item.id)),
            );
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brake::BrakeHold;
    use crate::reference::Candidate;
    use serde_json::json;

    #[test]
    fn a_plan_without_a_brake_serialises_its_actions_only() {
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
    fn a_default_plan_is_empty_and_has_no_brake() {
        let plan = Plan::<String>::default();
        assert!(plan.is_empty());
        assert!(plan.brake.is_none());
    }

    #[test]
    fn a_plan_with_a_brake_serialises_where_it_holds() {
        let mut plan = Plan::new();
        plan.push("delete p");
        plan.brake = Some(BrakeHold::new(
            1,
            4,
            vec![Candidate {
                id: "01ABC".into(),
                path: "p".into(),
            }],
        ));
        assert_eq!(
            serde_json::to_value(&plan).expect("plan serialises"),
            json!({
                "actions": ["delete p"],
                "brake": {
                    "planned": 1,
                    "set_size": 4,
                    "items": [{"id": "01ABC", "path": "p"}],
                },
            })
        );
    }

    #[derive(Serialize)]
    #[serde(transparent)]
    struct Step(&'static str);

    impl PlanAction for Step {
        fn describe(&self) -> String {
            self.0.to_string()
        }
    }

    fn held_plan() -> Plan<Step> {
        let mut plan = Plan::new();
        plan.push(Step("remove projects/a"));
        plan.brake = Some(BrakeHold::new(
            3,
            10,
            vec![
                Candidate {
                    id: "01ABC".into(),
                    path: "projects/a".into(),
                },
                Candidate {
                    id: "01ABD".into(),
                    path: "projects/b".into(),
                },
            ],
        ));
        plan
    }

    #[test]
    fn a_dry_run_in_json_says_so_before_the_actions() {
        let mut plan = Plan::new();
        plan.push(Step("archive projects/a"));
        plan.push(Step("archive projects/b"));
        let text = serde_json::to_string(&plan.dry_run()).expect("a dry run serialises");
        assert_eq!(
            text,
            r#"{"dry_run":true,"actions":["archive projects/a","archive projects/b"]}"#
        );
    }

    #[test]
    fn a_dry_run_in_json_carries_the_brake_only_when_it_holds() {
        let held = serde_json::to_value(held_plan().dry_run()).expect("serialises");
        assert_eq!(held["brake"]["planned"], 3);
        let free = serde_json::to_value(Plan::<Step>::new().dry_run()).expect("serialises");
        assert_eq!(free, json!({"dry_run": true, "actions": []}));
    }

    #[test]
    fn each_action_is_a_would_line() {
        let mut plan = Plan::new();
        plan.push(Step("archive projects/a"));
        plan.push(Step("archive projects/b"));
        assert_eq!(
            plan.lines(),
            ["Would archive projects/a", "Would archive projects/b"]
        );
    }

    #[test]
    fn an_empty_plan_has_nothing_to_do() {
        assert_eq!(Plan::<Step>::new().lines(), ["Nothing to do."]);
    }

    #[test]
    fn a_brake_that_would_hold_is_listed_after_the_actions() {
        assert_eq!(
            held_plan().lines(),
            [
                "Would remove projects/a",
                "The brake would hold at 3 of 10:",
                "  projects/a (01ABC)",
                "  projects/b (01ABD)",
            ]
        );
    }
}
