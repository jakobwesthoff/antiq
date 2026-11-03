// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Questions a command asks and the answers it gets.
//!
//! The library builds a [`Question`] as plain data and hands it to an
//! [`Interaction`]. The binary renders it on the terminal; without a
//! terminal nothing answers and the caller takes the question's default,
//! the non-destructive choice.

use std::collections::VecDeque;
use std::thread;

/// One answer a question offers. `id` is what the library matches on,
/// `label` is what the user reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

impl Choice {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Choice {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// A decision the user takes. `details` are lines under the title, such as
/// the list of what a replacement would lose. `default` is the index of the
/// non-destructive choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub title: String,
    pub details: Vec<String>,
    pub choices: Vec<Choice>,
    pub default: usize,
}

impl Question {
    pub fn default_choice(&self) -> &Choice {
        &self.choices[self.default]
    }
}

/// Whoever answers questions. `None` means nobody answered.
pub trait Interaction: Send {
    fn choose(&mut self, question: &Question) -> Option<Choice>;
}

/// Answers nothing, for runs without a terminal and for `--json`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NonInteractive;

impl Interaction for NonInteractive {
    fn choose(&mut self, _question: &Question) -> Option<Choice> {
        None
    }
}

/// Answers from a script and fails the test when the library asks
/// something else, or less than the script holds.
#[derive(Debug, Default)]
pub struct ScriptedInteraction {
    script: VecDeque<(String, Option<String>)>,
}

impl ScriptedInteraction {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next question has this title; answer it with the choice that
    /// has this id.
    pub fn expect(&mut self, title: &str, answer_id: &str) {
        self.script
            .push_back((title.to_string(), Some(answer_id.to_string())));
    }

    /// The next question has this title; nobody answers it.
    pub fn expect_no_answer(&mut self, title: &str) {
        self.script.push_back((title.to_string(), None));
    }
}

impl Interaction for ScriptedInteraction {
    fn choose(&mut self, question: &Question) -> Option<Choice> {
        let Some((title, answer_id)) = self.script.pop_front() else {
            panic!(
                "unexpected question {:?}: the script is empty",
                question.title
            );
        };
        assert_eq!(
            title, question.title,
            "expected question {title:?}, got {:?}",
            question.title
        );
        let answer_id = answer_id?;
        let choice = question
            .choices
            .iter()
            .find(|choice| choice.id == answer_id);
        Some(
            choice
                .unwrap_or_else(|| {
                    panic!(
                        "answer {answer_id:?} is not a choice of {:?}",
                        question.title
                    )
                })
                .clone(),
        )
    }
}

impl Drop for ScriptedInteraction {
    fn drop(&mut self) {
        if !thread::panicking() && !self.script.is_empty() {
            panic!("{} scripted answer was never asked for", self.script.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: ?Sized + Send>() {}

    fn replace_question() -> Question {
        Question {
            title: "Replace the archive?".into(),
            details: vec!["3 files would be lost".into()],
            choices: vec![
                Choice::new("skip", "Skip"),
                Choice::new("replace", "Replace"),
            ],
            default: 0,
        }
    }

    #[test]
    fn interactions_can_move_between_threads() {
        assert_send::<dyn Interaction>();
        assert_send::<NonInteractive>();
        assert_send::<ScriptedInteraction>();
    }

    #[test]
    fn the_default_choice_is_the_one_the_index_names() {
        assert_eq!(replace_question().default_choice().id, "skip");
    }

    #[test]
    fn a_non_interactive_run_answers_nothing() {
        assert_eq!(NonInteractive.choose(&replace_question()), None);
    }

    #[test]
    fn a_scripted_interaction_gives_the_scripted_answer() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect("Replace the archive?", "replace");
        assert_eq!(
            interaction.choose(&replace_question()),
            Some(Choice::new("replace", "Replace"))
        );
    }

    #[test]
    fn a_scripted_interaction_can_answer_nothing() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect_no_answer("Replace the archive?");
        assert_eq!(interaction.choose(&replace_question()), None);
    }

    #[test]
    #[should_panic(expected = "unexpected question \"Replace the archive?\"")]
    fn a_scripted_interaction_rejects_a_question_it_did_not_expect() {
        let mut interaction = ScriptedInteraction::new();
        interaction.choose(&replace_question());
    }

    #[test]
    #[should_panic(expected = "expected question \"Other?\"")]
    fn a_scripted_interaction_rejects_a_different_question() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect("Other?", "skip");
        interaction.choose(&replace_question());
    }

    #[test]
    #[should_panic(expected = "answer \"nope\" is not a choice")]
    fn a_scripted_interaction_rejects_an_answer_that_is_not_offered() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect("Replace the archive?", "nope");
        interaction.choose(&replace_question());
    }

    #[test]
    #[should_panic(expected = "1 scripted answer was never asked for")]
    fn a_scripted_interaction_rejects_a_leftover_answer() {
        let mut interaction = ScriptedInteraction::new();
        interaction.expect("Replace the archive?", "skip");
    }
}
