// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What a failed remote operation means for the exit code.

use crate::error::Error;
use crate::remote::RemoteError;

/// How the binary treats a failed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The error decides the exit code.
    Fail,
    /// The run ends with exit code 0 and prints what it would have printed.
    TreatAsSuccess,
}

/// Decides what `--if-reachable` makes of an error.
///
/// Only an away remote that has not been away for too long ends a run with
/// exit code 0. `overdue` is set by [`crate::remote::away::with_overdue`],
/// which the commands that take the flag apply to their remote result.
/// Errors of `remote add`, `remote show` and `remote remove` are not
/// looked at since those commands take no `--if-reachable`.
pub fn outcome(if_reachable: bool, error: &Error) -> Outcome {
    match error {
        Error::Remote(RemoteError::Away { overdue: false, .. }) if if_reachable => {
            Outcome::TreatAsSuccess
        }
        _ => Outcome::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::remote::RemoteOpError;

    fn away(overdue: bool) -> Error {
        RemoteError::Away {
            message: "no route to host".into(),
            overdue,
        }
        .into()
    }

    #[test]
    fn if_reachable_turns_a_fresh_away_remote_into_success() {
        assert_eq!(outcome(true, &away(false)), Outcome::TreatAsSuccess);
    }

    #[test]
    fn without_the_flag_an_away_remote_fails() {
        assert_eq!(outcome(false, &away(false)), Outcome::Fail);
        assert_eq!(outcome(false, &away(true)), Outcome::Fail);
    }

    #[test]
    fn an_overdue_away_remote_fails_even_with_the_flag() {
        assert_eq!(outcome(true, &away(true)), Outcome::Fail);
    }

    #[test]
    fn a_remote_op_away_fails_with_the_flag() {
        let error: Error = RemoteOpError::Remote(RemoteError::Away {
            message: "no route to host".into(),
            overdue: false,
        })
        .into();
        assert_eq!(outcome(true, &error), Outcome::Fail);
    }

    #[test]
    fn no_remote_and_unrelated_errors_fail_with_the_flag() {
        let no_remote: Error = RemoteError::NoRemote.into();
        assert_eq!(outcome(true, &no_remote), Outcome::Fail);
        assert_eq!(outcome(true, &Error::Usage("x".into())), Outcome::Fail);
    }

    #[test]
    fn a_broken_remote_fails_the_run() {
        let broken: Error = RemoteError::Broken("wrong marker".into()).into();
        assert_eq!(outcome(true, &broken), Outcome::Fail);
    }
}
