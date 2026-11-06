// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ordered actions a command intends to take.

use serde::Serialize;

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

#[cfg(test)]
mod tests {
    use super::*;
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
    struct Step(&'static str);

    impl PlanAction for Step {
        fn describe(&self) -> String {
            self.0.to_string()
        }
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
    fn an_empty_dry_run_in_json_has_no_actions() {
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
}
