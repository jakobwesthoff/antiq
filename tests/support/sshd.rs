// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A real `sshd` for the opt-in SSH tests: started as the user on
//! 127.0.0.1, with keys generated for the test, stopped when the guard
//! drops. Nothing outside the test's temp directory is read or written, and
//! the user's `~/.ssh/config` is never consulted.

use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Where sshd may live, in the order they are tried.
pub const SSHD_CANDIDATES: [&str; 3] = [
    "/usr/sbin/sshd",
    "/usr/local/sbin/sshd",
    "/opt/homebrew/sbin/sshd",
];

/// The first of `candidates` that exists, or a message naming all of them.
pub fn find_executable(candidates: &[&str]) -> Result<PathBuf, String> {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| {
            format!(
                "none of these executables exists: {}",
                candidates.join(", ")
            )
        })
}

/// How long to wait for the server to answer, polled in steps.
const START_CEILING: Duration = Duration::from_secs(10);
const POLL_STEP: Duration = Duration::from_millis(50);

pub struct Sshd {
    dir: TempDir,
    child: Child,
    port: u16,
}

impl Sshd {
    /// Starts a server and returns once it accepts connections.
    pub fn start() -> Result<Sshd, String> {
        let sshd = find_executable(&SSHD_CANDIDATES)?;
        let dir = tempfile::tempdir().map_err(|error| format!("temp dir: {error}"))?;
        let root = dir.path();
        for key in ["hostkey", "userkey", "wrongkey"] {
            keygen(&root.join(key))?;
        }
        std::fs::copy(root.join("userkey.pub"), root.join("authorized_keys"))
            .map_err(|error| format!("authorized_keys: {error}"))?;
        std::fs::set_permissions(
            root.join("authorized_keys"),
            std::fs::Permissions::from_mode(0o600),
        )
        .map_err(|error| format!("authorized_keys mode: {error}"))?;

        let port = free_port()?;
        let root_text = root.display();
        let user = std::env::var("USER").map_err(|_| "USER is not set".to_string())?;
        std::fs::write(
            root.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {root_text}/hostkey\n\
                 AuthorizedKeysFile {root_text}/authorized_keys\nPidFile {root_text}/pid\n\
                 StrictModes no\nUsePAM no\nPasswordAuthentication no\n"
            ),
        )
        .map_err(|error| format!("sshd_config: {error}"))?;
        std::fs::write(
            root.join("ssh_config"),
            format!(
                "Host loop\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n\
                 \x20 IdentityFile {root_text}/userkey\n  IdentitiesOnly yes\n\
                 \x20 UserKnownHostsFile {root_text}/known_hosts\n  StrictHostKeyChecking no\n\
                 Host wrongkey\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n\
                 \x20 IdentityFile {root_text}/wrongkey\n  IdentitiesOnly yes\n\
                 \x20 UserKnownHostsFile {root_text}/known_hosts\n  StrictHostKeyChecking no\n"
            ),
        )
        .map_err(|error| format!("ssh_config: {error}"))?;
        // A symlink to a checked-in script, not a generated executable:
        // a file that is written and then executed in a process with other
        // threads can fail to execute while another thread's child holds
        // it open for writing.
        let program = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-real.sh");
        std::os::unix::fs::symlink(program, root.join("ssh"))
            .map_err(|error| format!("ssh link: {error}"))?;

        let log = std::fs::File::create(root.join("sshd.log"))
            .map_err(|error| format!("sshd.log: {error}"))?;
        let mut child = Command::new(&sshd)
            .args(["-D", "-e", "-f"])
            .arg(root.join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", sshd.display()))?;

        let started = Instant::now();
        loop {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                let log = std::fs::read_to_string(root.join("sshd.log")).unwrap_or_default();
                return Err(format!("sshd ended with {status}: {log}"));
            }
            if started.elapsed() > START_CEILING {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("sshd did not answer on port {port} in time"));
            }
            std::thread::sleep(POLL_STEP);
        }
        Ok(Sshd { dir, child, port })
    }

    /// The `ssh` that reads this server's `ssh_config`.
    pub fn program(&self) -> PathBuf {
        self.dir.path().join("ssh")
    }

    /// A host name from the generated `ssh_config` that logs in.
    pub fn host(&self) -> &'static str {
        "loop"
    }

    /// A host name from the generated `ssh_config` whose key is refused.
    pub fn host_with_wrong_key(&self) -> &'static str {
        "wrongkey"
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The process id of the server's listener.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn keygen(path: &Path) -> Result<(), String> {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .output()
        .map_err(|error| format!("cannot run ssh-keygen: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// A port nothing listens on: bound through port 0, then released.
pub fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("cannot bind a port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("cannot read the port: {error}"))?
        .port();
    Ok(port)
}
