// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The machine name.

/// Errors of the machine name.
#[derive(Debug, thiserror::Error)]
pub enum MachineError {
    #[error("cannot determine the machine name: {reason}")]
    Unavailable { reason: String },
}

impl crate::error::HasKind for MachineError {}

/// The name this machine gives itself: the ComputerName on macOS, the
/// hostname elsewhere.
pub fn detect() -> Result<String, MachineError> {
    let device =
        cfg!(target_os = "macos").then(|| whoami::devicename().map_err(|error| error.to_string()));
    choose(
        device,
        whoami::hostname().map_err(|error| error.to_string()),
    )
}

/// Picks between the results of asking the system. `device` is `None` where
/// the system has no separate device name to ask for.
fn choose(
    device: Option<Result<String, String>>,
    host: Result<String, String>,
) -> Result<String, MachineError> {
    // A device name that cannot be read or is empty is a failed lookup, not
    // a name: the hostname stands in for it.
    if let Some(Ok(name)) = device
        && !name.is_empty()
    {
        return Ok(name);
    }
    match host {
        Ok(name) if name.is_empty() => Err(MachineError::Unavailable {
            reason: "the hostname is empty".to_string(),
        }),
        Ok(name) => Ok(name),
        Err(reason) => Err(MachineError::Unavailable { reason }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unavailable(result: Result<String, MachineError>) -> String {
        match result.expect_err("no name is available") {
            MachineError::Unavailable { reason } => reason,
        }
    }

    #[test]
    fn the_device_name_wins_over_the_hostname() {
        let name = choose(Some(Ok("imperius".into())), Ok("imperius.local".into()));
        assert_eq!(name.expect("a name"), "imperius");
    }

    #[test]
    fn a_failing_device_name_falls_back_to_the_hostname() {
        let name = choose(
            Some(Err("no ComputerName".into())),
            Ok("imperius.local".into()),
        );
        assert_eq!(name.expect("a name"), "imperius.local");
    }

    #[test]
    fn an_empty_device_name_falls_back_to_the_hostname() {
        let name = choose(Some(Ok(String::new())), Ok("imperius.local".into()));
        assert_eq!(name.expect("a name"), "imperius.local");
    }

    #[test]
    fn without_a_device_name_the_hostname_is_used() {
        let name = choose(None, Ok("build-host".into()));
        assert_eq!(name.expect("a name"), "build-host");
    }

    #[test]
    fn no_device_name_and_no_hostname_is_unavailable() {
        let reason = unavailable(choose(
            Some(Err("no ComputerName".into())),
            Err("no host".into()),
        ));
        assert_eq!(reason, "no host");
    }

    #[test]
    fn an_empty_hostname_is_unavailable() {
        let reason = unavailable(choose(None, Ok(String::new())));
        assert_eq!(reason, "the hostname is empty");
    }

    #[test]
    fn the_detected_name_is_not_empty() {
        assert!(!detect().expect("a name").is_empty());
    }
}
