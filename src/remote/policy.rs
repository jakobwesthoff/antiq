// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a failed remote operation means for the exit code.

use crate::error::Error;

/// How the binary treats a failed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The error decides the exit code.
    Fail,
    /// The run ends with exit code 0 and prints what it would have printed.
    TreatAsSuccess,
}

/// Decides what `--if-reachable` makes of an error.
pub fn outcome(_if_reachable: bool, _error: &Error) -> Outcome {
    Outcome::Fail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RemoteError;

    fn away(overdue: bool) -> Error {
        RemoteError::Away {
            message: "no route to host".into(),
            overdue,
        }
        .into()
    }

    #[test]
    fn an_away_remote_fails_the_run_with_or_without_if_reachable() {
        assert_eq!(outcome(false, &away(false)), Outcome::Fail);
        assert_eq!(outcome(true, &away(false)), Outcome::Fail);
        assert_eq!(outcome(true, &away(true)), Outcome::Fail);
    }

    #[test]
    fn a_broken_remote_fails_the_run() {
        let broken: Error = RemoteError::Broken("wrong marker".into()).into();
        assert_eq!(outcome(true, &broken), Outcome::Fail);
    }
}
