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
}
