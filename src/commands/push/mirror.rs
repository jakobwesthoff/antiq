// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Steps 5 to 8 of `push`: the marker and the mirror.

use super::PushError;
use super::precheck::{Runner, Scratch};
use crate::target::marker::{self, Marker};
use crate::target::rsync::{Options, Rsync};
use crate::target::url::TargetUrl;
use std::io;
use std::path::Path;

/// What sending the marker needs.
pub(super) struct Sender<'a> {
    pub rsync: &'a Rsync,
    pub options: &'a Options,
    pub url: &'a TargetUrl,
    /// The directory in which the marker gets a directory of its own.
    pub scratch: &'a Path,
    pub write: &'a dyn Fn(&Path, &[u8]) -> io::Result<()>,
    /// Called right before anything is sent; an error stops the send.
    pub before: &'a dyn Fn() -> Result<(), PushError>,
}

/// Sends the marker to the target with a run of its own: a directory in
/// `.antiq/tmp/` that holds only the marker, so that the target's root keeps
/// its mode and time (see [`Rsync::send_marker`]).
pub(super) fn send_marker(sender: &Sender, marker: &Marker, run: Runner) -> Result<(), PushError> {
    (sender.before)()?;
    let scratch = Scratch::create(sender.scratch)?;
    (sender.write)(&scratch.path().join(marker::FILE_NAME), &marker.to_bytes()).map_err(
        |source| PushError::Scratch {
            path: scratch.path().to_path_buf(),
            source,
        },
    )?;

    let call = sender
        .rsync
        .send_marker(sender.options, scratch.path(), sender.url);
    let outcome = run(&call).map_err(PushError::Rsync)?;
    if outcome.code == Some(0) {
        return Ok(());
    }
    let how = match outcome.code {
        Some(code) => format!("rsync ended with exit {code}"),
        None => "rsync was ended by a signal".to_string(),
    };
    Err(PushError::MarkerRun {
        detail: format!("{how}: {}", String::from_utf8_lossy(&outcome.stderr).trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::fake::Fake;
    use crate::target::rsync::{Call, Outcome, RsyncError};
    use std::cell::RefCell;
    use std::fs;

    const RSYNC_3_5_1: &str =
        "rsync  version 3.5.1  protocol version 33\nCopyright (C) 1996-2026\n";

    fn marker() -> Marker {
        Marker::new(
            "01BX5ZZKBKACTAV9WEVGEMMVRY".parse().expect("a ULID"),
            "Studio".into(),
            "/srv/coll".into(),
        )
    }

    struct Fixture {
        _fake: Fake,
        rsync: Rsync,
        url: TargetUrl,
        scratch: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let fake = Fake::new(RSYNC_3_5_1);
        let rsync = Rsync::probe(&fake.path()).expect("the stand-in passes");
        Fixture {
            _fake: fake,
            rsync,
            url: TargetUrl::parse("backup@nas:/srv/antiq").expect("a target"),
            scratch: tempfile::tempdir().expect("a temp dir"),
        }
    }

    impl Fixture {
        fn send(
            &self,
            write: &dyn Fn(&Path, &[u8]) -> io::Result<()>,
            run: Runner,
        ) -> Result<(), PushError> {
            let options = Options {
                fsync: true,
                rsync_path: None,
                rsync_args: Vec::new(),
            };
            send_marker(
                &Sender {
                    rsync: &self.rsync,
                    options: &options,
                    url: &self.url,
                    scratch: self.scratch.path(),
                    write,
                    before: &|| Ok(()),
                },
                &marker(),
                run,
            )
        }
    }

    fn answer(code: Option<i32>, stderr: &str) -> Result<Outcome, RsyncError> {
        Ok(Outcome {
            code,
            stdout: Vec::new(),
            stderr: stderr.into(),
        })
    }

    #[test]
    fn the_marker_is_sent_from_a_directory_that_holds_only_the_marker() {
        let fixture = fixture();
        type Seen = (Vec<String>, Vec<(String, Vec<u8>)>);
        let seen: RefCell<Option<Seen>> = RefCell::new(None);

        let result = fixture.send(&|path, bytes| fs::write(path, bytes), &|call: &Call| {
            let args: Vec<String> = call
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let from = args[args.len() - 2].clone();
            let files = fs::read_dir(&from)
                .expect("the directory exists during the run")
                .map(|entry| {
                    let entry = entry.expect("an entry");
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        fs::read(entry.path()).expect("readable"),
                    )
                })
                .collect();
            *seen.borrow_mut() = Some((args, files));
            answer(Some(0), "")
        });

        result.expect("sent");
        let (args, files) = seen.into_inner().expect("rsync ran");
        assert_eq!(files, [(".antiq-target".to_string(), marker().to_bytes())]);
        assert!(args.contains(&"-r".to_string()), "{args:?}");
        assert!(args.contains(&"--chmod=F644".to_string()), "{args:?}");
        assert_eq!(args.last().expect("a target"), "backup@nas:/srv/antiq/");
        let from = &args[args.len() - 2];
        assert!(
            from.starts_with(fixture.scratch.path().to_str().expect("UTF-8")),
            "{from}"
        );
        assert!(!Path::new(from).exists(), "the directory is removed again");
    }

    #[test]
    fn a_failed_run_is_an_error_with_rsyncs_text_and_leaves_no_directory() {
        for (code, text) in [(Some(1), "exit 1"), (Some(23), "exit 23"), (None, "signal")] {
            let fixture = fixture();
            let result = fixture.send(&|path, bytes| fs::write(path, bytes), &|_| {
                answer(
                    code,
                    "rsync: mkdir \"/srv/antiq/a/b\" failed: No such file\n",
                )
            });
            match result {
                Err(PushError::MarkerRun { detail }) => {
                    assert!(detail.contains(text), "{detail}");
                    assert!(detail.contains("mkdir"), "{detail}");
                }
                other => panic!("expected a marker run error, got {other:?}"),
            }
            assert_eq!(
                fs::read_dir(fixture.scratch.path())
                    .expect("exists")
                    .count(),
                0
            );
        }
    }

    #[test]
    fn an_rsync_that_cannot_start_is_the_rsync_error() {
        let fixture = fixture();
        let result = fixture.send(&|path, bytes| fs::write(path, bytes), &|_| {
            Err(RsyncError::Spawn {
                path: "rsync".into(),
                cause: io::Error::other("gone"),
            })
        });
        assert!(matches!(result, Err(PushError::Rsync(_))), "{result:?}");
    }

    #[test]
    fn a_marker_that_cannot_be_written_is_a_scratch_error_and_runs_nothing() {
        let fixture = fixture();
        let ran = RefCell::new(false);
        let result = fixture.send(&|_, _| Err(io::Error::other("disk full")), &|_| {
            *ran.borrow_mut() = true;
            answer(Some(0), "")
        });
        assert!(
            matches!(result, Err(PushError::Scratch { .. })),
            "{result:?}"
        );
        assert!(!*ran.borrow());
        assert_eq!(
            fs::read_dir(fixture.scratch.path())
                .expect("exists")
                .count(),
            0
        );
    }

    #[test]
    fn a_failed_check_before_the_send_stops_it_before_anything_is_made() {
        let fixture = fixture();
        let options = Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let ran = RefCell::new(false);
        let result = send_marker(
            &Sender {
                rsync: &fixture.rsync,
                options: &options,
                url: &fixture.url,
                scratch: fixture.scratch.path(),
                write: &|path, bytes| fs::write(path, bytes),
                before: &|| {
                    Err(PushError::Target {
                        problem: "moved".into(),
                    })
                },
            },
            &marker(),
            &|_| {
                *ran.borrow_mut() = true;
                answer(Some(0), "")
            },
        );
        assert!(
            matches!(result, Err(PushError::Target { .. })),
            "{result:?}"
        );
        assert!(!*ran.borrow());
        assert_eq!(
            fs::read_dir(fixture.scratch.path())
                .expect("exists")
                .count(),
            0
        );
    }

    #[test]
    fn a_scratch_directory_that_cannot_be_made_is_a_scratch_error() {
        let fixture = fixture();
        let temp = tempfile::tempdir().expect("a temp dir");
        let file = temp.path().join("file");
        fs::write(&file, "x").expect("written");
        let blocked = file.join("below");
        let options = Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let result = send_marker(
            &Sender {
                rsync: &fixture.rsync,
                options: &options,
                url: &fixture.url,
                scratch: &blocked,
                write: &|path, bytes| fs::write(path, bytes),
                before: &|| Ok(()),
            },
            &marker(),
            &|_| answer(Some(0), ""),
        );
        assert!(
            matches!(result, Err(PushError::Scratch { .. })),
            "{result:?}"
        );
    }
}
