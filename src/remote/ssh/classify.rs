// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Classifying ssh failures.

/// Why ssh failed, as far as the remote is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The host cannot be reached from here right now.
    Away(String),
    /// The host answers but refuses, or the failure is not recognised.
    Broken(String),
}

/// A problem with the control socket or the master connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxProblem {
    NoMaster,
    StaleSocket,
    PathTooLong,
    AlreadyExists,
    SessionRefused,
}

impl std::fmt::Display for MuxProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MuxProblem::NoMaster => "no master connection is running",
            MuxProblem::StaleSocket => "the control socket has no master behind it",
            MuxProblem::PathTooLong => "the control path is too long for a unix socket",
            MuxProblem::AlreadyExists => "something already exists at the control path",
            MuxProblem::SessionRefused => "the server refused another session on the connection",
        })
    }
}

// =========================================================
// Which lines count
// =========================================================

/// Lines that say something about the control socket, the master or a
/// proxy, never about the remote. A refused control socket reads
/// "Connection refused" just like a refused host, so these lines are
/// dropped before a command's failure is decided. A proxy command or jump
/// host adds the `UNKNOWN port 65535` lines after the line that tells what
/// went wrong, or on their own.
const NOISE_PREFIXES: [&str; 6] = [
    "Control socket ",
    "ControlSocket ",
    "mux_client_",
    "unix_listener:",
    "Connection closed by UNKNOWN port",
    "Connection to UNKNOWN port",
];

const NO_MESSAGE: &str = "ssh failed with exit status 255 and no message";

fn lines(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn is_noise(line: &str) -> bool {
    NOISE_PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

// =========================================================
// The shapes of ssh's own lines
// =========================================================

/// A line of the form `ssh: connect to host <h> port <p>: <reason>`. The
/// line is printed only when the TCP connect itself failed, so any reason
/// means the host never answered. Other texts that happen to say
/// "Connection refused" do not have this form.
fn is_connect_failure(line: &str) -> bool {
    line.strip_prefix("ssh: connect to host ")
        .and_then(|rest| rest.split_once(" port "))
        .is_some_and(|(_, after_port)| after_port.contains(": "))
}

fn is_away_line(line: &str) -> bool {
    is_connect_failure(line)
        || line.starts_with("ssh: Could not resolve hostname ")
        || (line.starts_with("Timeout, server ") && line.ends_with(" not responding."))
        || line == "Connection timed out during banner exchange"
}

fn is_broken_line(line: &str) -> bool {
    // The destination before the colon is what ssh resolved from the
    // user's config, not necessarily what antiq passed.
    let denied = line
        .split_once(": Permission denied (")
        .is_some_and(|(destination, rest)| !destination.contains(' ') && rest.ends_with(")."));
    denied
        || line == "Host key verification failed."
        || line.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || line.starts_with("kex_exchange_identification: ")
        || line.starts_with("Connection closed by ")
        || line.starts_with("Connection reset by ")
        || line.starts_with("Too many authentication failures")
}

/// Classifies the stderr of a command that ssh ended with exit status 255.
///
/// Call it only for status 255: any other status is the remote command's
/// own, and the remote command can print anything. A broken line wins over
/// an away line so that an authentication problem is never reported as
/// "away", which `--if-reachable` turns into success. A text without a
/// recognised line is broken.
pub fn command_failure(stderr: &[u8]) -> Failure {
    let lines: Vec<String> = lines(stderr)
        .into_iter()
        .filter(|line| !is_noise(line))
        .collect();
    if let Some(line) = lines.iter().find(|line| is_broken_line(line)) {
        return Failure::Broken(line.clone());
    }
    if let Some(line) = lines.iter().find(|line| is_away_line(line)) {
        return Failure::Away(line.clone());
    }
    if lines.is_empty() {
        return Failure::Broken(NO_MESSAGE.to_string());
    }
    Failure::Broken(lines.join("; "))
}

// =========================================================
// Control socket and master problems
// =========================================================

/// Finds the control socket or master problem in ssh's stderr, or in the
/// log of a master.
pub fn mux_problem(stderr: &[u8]) -> Option<MuxProblem> {
    lines(stderr).iter().find_map(|line| {
        if line.starts_with("Control socket connect(") {
            if line.ends_with(": No such file or directory") {
                Some(MuxProblem::NoMaster)
            } else if line.ends_with(": Connection refused") {
                Some(MuxProblem::StaleSocket)
            } else {
                None
            }
        } else if line.starts_with("unix_listener: path ")
            && line.ends_with(" too long for Unix domain socket")
        {
            Some(MuxProblem::PathTooLong)
        } else if line.starts_with("ControlSocket ")
            && line.ends_with(" already exists, disabling multiplexing")
        {
            Some(MuxProblem::AlreadyExists)
        } else if is_session_refused(line) {
            Some(MuxProblem::SessionRefused)
        } else {
            None
        }
    })
}

fn is_session_refused(line: &str) -> bool {
    line == "mux_client_request_session: session request failed: Session open refused by peer"
}

/// Whether the server refused another session on the master connection
/// (`MaxSessions` of sshd). ssh then runs the command over a direct
/// connection, so the command itself succeeds.
pub fn session_refused(stderr: &[u8]) -> bool {
    lines(stderr).iter().any(|line| is_session_refused(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../../../tests/fixtures/ssh-texts/", $name, ".txt")).as_bytes()
        };
    }

    fn away(stderr: &[u8]) -> String {
        match command_failure(stderr) {
            Failure::Away(line) => line,
            other => panic!("expected away, got {other:?}"),
        }
    }

    fn broken(stderr: &[u8]) -> String {
        match command_failure(stderr) {
            Failure::Broken(message) => message,
            other => panic!("expected broken, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_connection_is_away() {
        assert!(away(fixture!("connect-refused")).contains("Connection refused"));
    }

    #[test]
    fn an_unresolvable_name_is_away() {
        assert!(away(fixture!("resolve")).contains("Could not resolve hostname"));
    }

    #[test]
    fn a_connect_timeout_is_away() {
        assert!(away(fixture!("timeout")).contains("Operation timed out"));
    }

    #[test]
    fn any_reason_after_the_connect_prefix_is_away() {
        let line = b"ssh: connect to host nas port 22: Network is down\n";
        assert!(away(line).contains("Network is down"));
    }

    #[test]
    fn a_keepalive_timeout_is_away() {
        assert!(away(fixture!("keepalive-timeout")).contains("not responding"));
    }

    #[test]
    fn a_jump_host_that_refuses_is_away() {
        assert!(away(fixture!("proxy-jump-refused")).contains("Connection refused"));
    }

    #[test]
    fn a_jump_host_that_does_not_resolve_is_away() {
        assert!(away(fixture!("proxy-jump-resolve")).contains("Could not resolve"));
    }

    #[test]
    fn a_banner_timeout_through_a_proxy_is_away() {
        assert!(away(fixture!("proxy-command-banner-timeout")).contains("banner exchange"));
    }

    #[test]
    fn a_refused_key_is_broken() {
        assert!(broken(fixture!("permission-denied")).contains("Permission denied"));
    }

    #[test]
    fn a_permission_denied_line_names_the_resolved_destination() {
        let line = b"someone@192.168.1.5: Permission denied (publickey).\n";
        assert!(broken(line).contains("Permission denied"));
    }

    #[test]
    fn an_unknown_host_key_is_broken() {
        assert!(broken(fixture!("host-key-unknown")).contains("Host key verification failed"));
    }

    #[test]
    fn a_changed_host_key_is_broken() {
        assert!(broken(fixture!("host-key-changed")).contains("HOST IDENTIFICATION HAS CHANGED"));
    }

    #[test]
    fn a_connection_closed_by_the_server_is_broken() {
        let line = b"Connection closed by 192.168.1.5 port 22\n";
        assert!(broken(line).contains("Connection closed by 192.168.1.5"));
    }

    #[test]
    fn a_bare_proxy_close_has_no_deciding_line() {
        assert_eq!(
            broken(fixture!("proxy-command-closed")),
            "ssh failed with exit status 255 and no message"
        );
    }

    #[test]
    fn connection_refused_from_the_control_socket_is_not_away() {
        let message = broken(fixture!("control-socket-stale"));
        assert_eq!(message, "ssh failed with exit status 255 and no message");
    }

    #[test]
    fn connection_refused_from_a_remote_tool_is_not_away() {
        let text = b"curl: (7) Failed to connect to nas port 80: Connection refused\n";
        assert!(broken(text).contains("curl: (7)"));
    }

    #[test]
    fn mux_noise_before_a_real_line_does_not_hide_it() {
        let mut text = fixture!("control-socket-stale").to_vec();
        text.extend_from_slice(fixture!("timeout"));
        assert!(away(&text).contains("Operation timed out"));
    }

    #[test]
    fn a_broken_line_wins_over_an_away_line() {
        let mut text = fixture!("timeout").to_vec();
        text.extend_from_slice(fixture!("permission-denied"));
        assert!(broken(&text).contains("Permission denied"));
    }

    #[test]
    fn an_empty_stderr_is_broken_without_a_message() {
        assert_eq!(
            broken(b""),
            "ssh failed with exit status 255 and no message"
        );
    }

    #[test]
    fn an_unrecognised_line_is_broken_and_kept() {
        assert!(broken(b"something odd happened\n").contains("something odd happened"));
    }

    fn problem(stderr: &[u8]) -> Option<MuxProblem> {
        mux_problem(stderr)
    }

    #[test]
    fn a_missing_socket_is_no_master() {
        assert_eq!(
            problem(fixture!("control-socket-missing")),
            Some(MuxProblem::NoMaster)
        );
    }

    #[test]
    fn a_refusing_socket_is_stale() {
        assert_eq!(
            problem(fixture!("control-socket-stale")),
            Some(MuxProblem::StaleSocket)
        );
    }

    #[test]
    fn a_path_too_long_is_recognised() {
        assert_eq!(
            problem(fixture!("path-too-long")),
            Some(MuxProblem::PathTooLong)
        );
    }

    #[test]
    fn an_existing_socket_path_is_recognised() {
        assert_eq!(
            problem(fixture!("already-exists")),
            Some(MuxProblem::AlreadyExists)
        );
    }

    #[test]
    fn a_refused_session_is_recognised() {
        assert_eq!(
            problem(fixture!("session-refused")),
            Some(MuxProblem::SessionRefused)
        );
    }

    #[test]
    fn another_control_socket_error_is_no_known_problem() {
        let text = b"Control socket connect(/tmp/x): Permission denied\n";
        assert_eq!(problem(text), None);
    }

    #[test]
    fn an_ordinary_ssh_line_is_no_mux_problem() {
        assert_eq!(problem(fixture!("connect-refused")), None);
    }

    #[test]
    fn a_refused_session_is_found_among_other_lines() {
        let mut text = b"warning: something\n".to_vec();
        text.extend_from_slice(fixture!("session-refused"));
        assert!(session_refused(&text));
    }

    #[test]
    fn other_text_is_no_refused_session() {
        assert!(!session_refused(fixture!("control-socket-stale")));
    }

    #[test]
    fn each_mux_problem_has_words_for_a_warning() {
        let all = [
            MuxProblem::NoMaster,
            MuxProblem::StaleSocket,
            MuxProblem::PathTooLong,
            MuxProblem::AlreadyExists,
            MuxProblem::SessionRefused,
        ];
        for problem in all {
            assert!(!problem.to_string().is_empty(), "{problem:?}");
        }
    }
}
