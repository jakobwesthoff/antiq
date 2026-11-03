// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What every command reports back.

use serde::Serialize;

/// Something the user should know that did not stop the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub message: String,
}

impl Warning {
    pub fn new(message: impl Into<String>) -> Self {
        Warning {
            message: message.into(),
        }
    }
}

/// Implemented by the report of every command. The binary wraps the
/// serialised report in the JSON envelope and takes the exit code from
/// [`CommandReport::partial`].
pub trait CommandReport: Serialize {
    /// Whether some objects were skipped (exit code 3).
    fn partial(&self) -> bool;

    fn warnings(&self) -> &[Warning];
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use serde_json::json;

    #[derive(Serialize)]
    struct ExampleReport {
        skipped: usize,
        warnings: Vec<Warning>,
    }

    impl CommandReport for ExampleReport {
        fn partial(&self) -> bool {
            self.skipped > 0
        }

        fn warnings(&self) -> &[Warning] {
            &self.warnings
        }
    }

    #[test]
    fn a_report_says_whether_it_is_partial_and_lists_its_warnings() {
        let report = ExampleReport {
            skipped: 1,
            warnings: vec![Warning::new("a piece needs a decision")],
        };
        assert!(report.partial());
        assert_eq!(report.warnings()[0].message, "a piece needs a decision");
        assert_eq!(
            serde_json::to_value(&report).expect("report serialises"),
            json!({
                "skipped": 1,
                "warnings": [{"message": "a piece needs a decision"}],
            })
        );
    }
}
