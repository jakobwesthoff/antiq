// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The controlling terminal. Questions are asked on `/dev/tty`, not on
//! stdin and stdout, so they work while the output is redirected.

use antiq::interact::{Choice, Interaction, Question};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};

/// Asks questions by writing to `output` and reading lines from `input`.
/// On the real terminal both are `/dev/tty`.
pub struct TtyInteraction<R, W> {
    input: R,
    output: W,
}

impl<R, W> TtyInteraction<R, W> {
    pub fn new(input: R, output: W) -> Self {
        TtyInteraction { input, output }
    }
}

impl TtyInteraction<BufReader<File>, File> {
    /// Opens the controlling terminal. Fails when there is none.
    pub fn open() -> io::Result<Self> {
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        Ok(TtyInteraction::new(BufReader::new(tty.try_clone()?), tty))
    }
}

impl<R: BufRead + Send, W: Write + Send> Interaction for TtyInteraction<R, W> {
    /// A terminal that cannot be written to or read from answers nothing,
    /// like no terminal at all.
    fn choose(&mut self, question: &Question) -> Option<Choice> {
        ask(&mut self.input, &mut self.output, question)
            .ok()
            .flatten()
    }
}

/// Shows the question and reads until an answer is a choice. An empty line
/// takes the question's default; the end of input answers nothing.
fn ask(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &Question,
) -> io::Result<Option<Choice>> {
    writeln!(output, "{}", question.title)?;
    for detail in &question.details {
        writeln!(output, "  {detail}")?;
    }
    for (index, choice) in question.choices.iter().enumerate() {
        let marker = if index == question.default {
            " (default)"
        } else {
            ""
        };
        writeln!(output, "  {}) {}{marker}", index + 1, choice.label)?;
    }
    let count = question.choices.len();
    loop {
        let default = question.default + 1;
        write!(output, "Choose [1-{count}, Enter for {default}]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(Some(question.default_choice().clone()));
        }
        match answer.parse::<usize>() {
            Ok(number) if (1..=count).contains(&number) => {
                return Ok(Some(question.choices[number - 1].clone()));
            }
            _ => writeln!(output, "'{answer}' is not one of the choices.")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn question() -> Question {
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

    fn answer(input: &str) -> (Option<Choice>, String) {
        let mut output = Vec::new();
        let choice = ask(&mut Cursor::new(input.as_bytes()), &mut output, &question())
            .expect("memory does not fail");
        (
            choice,
            String::from_utf8(output).expect("the prompt is UTF-8"),
        )
    }

    #[test]
    fn a_question_is_shown_with_its_details_and_numbered_choices() {
        let (_, shown) = answer("1\n");
        assert_eq!(
            shown,
            "Replace the archive?\n  3 files would be lost\n  1) Skip (default)\n  2) Replace\nChoose [1-2, Enter for 1]: "
        );
    }

    #[test]
    fn an_empty_answer_takes_the_default() {
        assert_eq!(answer("\n").0, Some(Choice::new("skip", "Skip")));
    }

    #[test]
    fn a_number_picks_a_choice() {
        assert_eq!(answer("2\n").0, Some(Choice::new("replace", "Replace")));
    }

    #[test]
    fn a_bad_answer_asks_again() {
        let (choice, shown) = answer("3\nx\n0\n2\n");
        assert_eq!(choice, Some(Choice::new("replace", "Replace")));
        assert_eq!(shown.matches("is not one of the choices").count(), 3);
        assert!(shown.contains("'x' is not one of the choices."));
    }

    #[test]
    fn the_end_of_input_answers_nothing() {
        assert_eq!(answer("").0, None);
    }

    #[test]
    fn the_terminal_answers_through_the_interaction_trait() {
        let mut tty = TtyInteraction::new(Cursor::new(b"2\n".to_vec()), Vec::new());
        assert_eq!(
            tty.choose(&question()),
            Some(Choice::new("replace", "Replace"))
        );
    }

    #[test]
    fn a_terminal_that_fails_answers_nothing() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let mut tty = TtyInteraction::new(Cursor::new(b"2\n".to_vec()), Broken);
        assert_eq!(tty.choose(&question()), None);
    }
}
