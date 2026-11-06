// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The brake on bulk deletions.

use crate::error::{ErrorKind, HasKind};
use crate::interact::{Choice, Interaction, Question};
use crate::reference::Candidate;
use serde::Serialize;

/// Where the brake holds: how many deletions are planned out of how many
/// objects, and the first few of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrakeHold {
    pub planned: usize,
    pub set_size: usize,
    items: Vec<Candidate>,
}

impl BrakeHold {
    /// A report names at most this many items.
    pub const MAX_ITEMS: usize = 10;

    pub fn new(planned: usize, set_size: usize, mut items: Vec<Candidate>) -> Self {
        items.truncate(Self::MAX_ITEMS);
        BrakeHold {
            planned,
            set_size,
            items,
        }
    }

    pub fn items(&self) -> &[Candidate] {
        &self.items
    }

    /// The items as a person reads them: one line each with path and ID,
    /// and a last line counting those the hold does not list.
    pub fn item_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .items
            .iter()
            .map(|item| format!("{} ({})", item.path, item.id))
            .collect();
        if self.planned > self.items.len() {
            lines.push(format!("and {} more", self.planned - self.items.len()));
        }
        lines
    }
}

/// What a step needs to pass the brake: whether `--force` was given and
/// whom to ask.
pub struct BrakeInput<'a> {
    pub force: bool,
    pub interaction: &'a mut dyn Interaction,
}

/// Errors of the brake.
#[derive(Debug, thiserror::Error)]
pub enum BrakeError {
    #[error("the brake holds {} of {} pieces", .0.planned, .0.set_size)]
    Held(BrakeHold),
}

impl HasKind for BrakeError {
    fn kind(&self) -> ErrorKind {
        match self {
            BrakeError::Held(_) => ErrorKind::Brake,
        }
    }
}

/// The brake holds when the planned removals exceed `percent` of the
/// affected set. Nothing planned never holds, 0 percent holds on any
/// removal and 100 percent never holds as the planned removals are part
/// of the set.
pub fn evaluate(percent: u32, planned: Vec<Candidate>, set_size: usize) -> Option<BrakeHold> {
    // Multiplying instead of dividing keeps the comparison exact: the
    // share is exceeded only when it is strictly larger.
    let exceeds = planned.len() as u128 * 100 > u128::from(percent) * set_size as u128;
    exceeds.then(|| BrakeHold::new(planned.len(), set_size, planned))
}

/// Lets a step past the brake. A step the brake does not hold, or that
/// runs with `--force`, passes. Otherwise the user is asked, and the step
/// stops unless the answer is `go`. Without a terminal nobody answers, so
/// the step stops. `title` words the question for the step.
pub fn gate(
    hold: Option<BrakeHold>,
    force: bool,
    title: &dyn Fn(&BrakeHold) -> String,
    interaction: &mut dyn Interaction,
) -> Result<(), BrakeError> {
    let Some(hold) = hold else {
        return Ok(());
    };
    if force {
        return Ok(());
    }
    let question = Question {
        title: title(&hold),
        details: hold.item_lines(),
        choices: vec![Choice::new("stop", "Stop"), Choice::new("go", "Go on")],
        default: 0,
    };
    match interaction.choose(&question) {
        Some(choice) if choice.id == "go" => Ok(()),
        _ => Err(BrakeError::Held(hold)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interact::{NonInteractive, ScriptedInteraction};

    fn candidates(count: usize) -> Vec<Candidate> {
        (0..count)
            .map(|number| Candidate {
                id: format!("01ARZ3NDEKTSV4RRFFQ69G5F{number:02}"),
                path: format!("projects/p{number}"),
            })
            .collect()
    }

    #[test]
    fn three_of_ten_hold_the_default_brake() {
        let hold = evaluate(20, candidates(3), 10).expect("30 percent exceeds 20");
        assert_eq!((hold.planned, hold.set_size), (3, 10));
        assert_eq!(hold.items().len(), 3);
    }

    #[test]
    fn exactly_the_share_does_not_hold() {
        assert_eq!(evaluate(20, candidates(2), 10), None);
    }

    #[test]
    fn zero_percent_holds_on_any_removal() {
        assert!(evaluate(0, candidates(1), 10).is_some());
    }

    #[test]
    fn a_hundred_percent_never_holds() {
        assert_eq!(evaluate(100, candidates(10), 10), None);
    }

    #[test]
    fn nothing_planned_never_holds() {
        assert_eq!(evaluate(0, Vec::new(), 10), None);
        assert_eq!(evaluate(0, Vec::new(), 0), None);
    }

    #[test]
    fn a_hold_names_ten_items_at_most_and_counts_all() {
        let hold = evaluate(20, candidates(12), 40).expect("12 of 40 exceeds 20 percent");
        assert_eq!(hold.planned, 12);
        assert_eq!(hold.items().len(), BrakeHold::MAX_ITEMS);
    }

    fn title(hold: &BrakeHold) -> String {
        format!("Drop {} of {} entries?", hold.planned, hold.set_size)
    }

    fn held() -> Option<BrakeHold> {
        evaluate(20, candidates(3), 10)
    }

    fn held_error(result: Result<(), BrakeError>) -> BrakeHold {
        match result {
            Err(BrakeError::Held(hold)) => hold,
            Ok(()) => panic!("the brake let the step pass"),
        }
    }

    #[test]
    fn a_step_the_brake_does_not_hold_passes_without_a_question() {
        let mut interaction = ScriptedInteraction::new();
        gate(None, false, &title, &mut interaction).expect("passes");
    }

    #[test]
    fn force_passes_a_held_step_without_a_question() {
        let mut interaction = ScriptedInteraction::new();
        gate(held(), true, &title, &mut interaction).expect("passes");
    }

    #[test]
    fn without_a_terminal_a_held_step_stops() {
        let hold = held_error(gate(held(), false, &title, &mut NonInteractive));
        assert_eq!((hold.planned, hold.set_size), (3, 10));
    }

    #[test]
    fn the_answer_go_lets_a_held_step_pass() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect("Drop 3 of 10 entries?", "go");
        gate(held(), false, &title, &mut interaction).expect("passes");
    }

    #[test]
    fn the_answer_stop_stops_the_step() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect("Drop 3 of 10 entries?", "stop");
        held_error(gate(held(), false, &title, &mut interaction));
    }

    #[test]
    fn the_question_counts_the_items_it_does_not_list() {
        struct Capture(Option<Question>);
        impl Interaction for Capture {
            fn choose(&mut self, question: &Question) -> Option<Choice> {
                self.0 = Some(question.clone());
                None
            }
        }
        let mut capture = Capture(None);
        let hold = evaluate(20, candidates(12), 40);
        held_error(gate(hold, false, &title, &mut capture));
        let details = capture.0.expect("the user was asked").details;
        assert_eq!(details.len(), BrakeHold::MAX_ITEMS + 1);
        assert_eq!(details.last().map(String::as_str), Some("and 2 more"));
    }

    #[test]
    fn the_question_lists_the_items_and_defaults_to_stop() {
        struct Capture(Option<Question>);
        impl Interaction for Capture {
            fn choose(&mut self, question: &Question) -> Option<Choice> {
                self.0 = Some(question.clone());
                None
            }
        }
        let mut capture = Capture(None);
        held_error(gate(held(), false, &title, &mut capture));
        let question = capture.0.expect("the user was asked");
        assert_eq!(question.title, "Drop 3 of 10 entries?");
        assert_eq!(question.details.len(), 3);
        assert!(question.details[0].contains("projects/p0"));
        assert_eq!(question.default_choice().id, "stop");
    }
}
