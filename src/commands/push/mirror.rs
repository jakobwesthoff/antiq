// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Steps 5 to 8 of `push`: the marker and the mirror.

use super::PushError;
use super::live::{Forecast, Live};
use super::precheck::{Runner, Scratch};
use super::summary;
use crate::progress::Observer;
use crate::relpath::RelPath;
use crate::target::marker::{self, Marker};
use crate::target::rsync::{Options, Outcome, Rsync};
use crate::target::url::TargetUrl;
use std::cell::RefCell;
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
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    if outcome.code == Some(12) && lacks_fsync(&stderr) {
        return Err(PushError::MarkerRun {
            detail: LACKS_FSYNC.to_string(),
        });
    }
    let how = match outcome.code {
        Some(code) => format!("rsync ended with exit {code}"),
        None => "rsync was ended by a signal".to_string(),
    };
    Err(PushError::MarkerRun {
        detail: format!("{how}: {}", stderr.trim()),
    })
}

/// What the user is told when the rsync of the target refuses `--fsync`,
/// which the marker run and the mirror both pass.
const LACKS_FSYNC: &str = "the target's rsync (openrsync, or older than 3.2.4) lacks `--fsync`; set \
                           `[target] rsync_path` to a 3.2.4+ rsync there, or `fsync = false`";

/// Whether the error text of an exit 12 says that the target's rsync does
/// not know `--fsync`.
fn lacks_fsync(stderr: &str) -> bool {
    stderr.contains("unrecognized option `--fsync'") || stderr.contains("--fsync: unknown option")
}

const IO_ERROR_LINE: &str = "IO error encountered -- skipping file deletion";

/// What a finished mirror run tells beyond its itemized changes.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Finished {
    /// The paths of the `file has vanished` lines of exit 24.
    pub vanished: Vec<String>,
}

/// The first double-quoted text of `line`.
fn quoted(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}

/// Reads the exit status of a mirror run (step 8): 0 and 24 are success,
/// everything else is an error that carries rsync's text.
pub(super) fn classify(outcome: &Outcome) -> Result<Finished, PushError> {
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    let said = stderr.trim();
    let detail = match outcome.code {
        Some(0) => {
            return Ok(Finished {
                vanished: Vec::new(),
            });
        }
        Some(24) => {
            let vanished = stderr
                .lines()
                .filter(|line| line.contains("file has vanished"))
                .filter_map(|line| quoted(line).map(str::to_string))
                .collect();
            return Ok(Finished { vanished });
        }
        Some(12) if lacks_fsync(&stderr) => LACKS_FSYNC.to_string(),
        // With a directory unreadable, rsync deletes nothing, since it
        // cannot tell what the sender still has.
        Some(23) if stdout.lines().any(|line| line.trim() == IO_ERROR_LINE) => {
            let directories: Vec<&str> = stderr
                .lines()
                .filter(|line| line.contains("opendir") || line.contains("readdir"))
                .filter_map(quoted)
                .collect();
            format!(
                "rsync could not read parts of the collection (exit 23), so no deletion reached the target; it could not open {}: {said}",
                directories.join(", ")
            )
        }
        Some(code) => format!("rsync ended with exit {code}: {said}"),
        None => format!("rsync was ended by a signal: {said}"),
    };
    Err(PushError::Mirror { detail })
}

/// What the mirror needs to run.
pub(super) struct Mirror<'a> {
    pub rsync: &'a Rsync,
    pub options: &'a Options,
    pub collection: &'a Path,
    pub url: &'a TargetUrl,
}

/// What the runs of the mirror left.
#[derive(Debug)]
pub(super) struct Mirrored {
    pub vanished: Vec<String>,
    /// The output of every run that changed the target (or, in a dry run,
    /// would), in order.
    pub stdouts: Vec<Vec<u8>>,
    pub second_pass: bool,
}

/// Runs the mirror as a dry run that lists the sizes and tells the
/// observer which pieces will change. Nothing is returned when the dry
/// run fails, changes nothing or lists nothing that can be read: the
/// real run then goes on without progress, and says what is wrong itself.
pub(super) fn forecast<'a>(
    mirror: &Mirror,
    folders: &[RelPath],
    observer: &'a dyn Observer,
) -> Option<RefCell<Live<'a>>> {
    let call = mirror
        .rsync
        .mirror_counted(mirror.options, mirror.collection, mirror.url);
    let outcome = call.run().ok()?;
    if !matches!(outcome.code, Some(0 | 24)) {
        return None;
    }
    let found = Forecast::from_sized(&summary::parse_sized(&outcome.stdout), folders);
    if found.is_empty() {
        return None;
    }
    Some(RefCell::new(Live::new(found, folders.to_vec(), observer)))
}

/// Runs the mirror (step 6), and after deletions the second pass (step 7).
pub(super) fn run_mirror(
    mirror: &Mirror,
    run: Runner,
    dry_run: bool,
) -> Result<Mirrored, PushError> {
    let call = |dry_run: bool| {
        mirror
            .rsync
            .mirror(mirror.options, mirror.collection, mirror.url, dry_run)
    };
    let first = run(&call(dry_run)).map_err(PushError::Rsync)?;
    let mut vanished = Vec::new();
    let mut failure: Option<PushError> = None;
    let mut record = |outcome: &Outcome| match classify(outcome) {
        Ok(finished) => vanished.extend(finished.vanished),
        // The first failure is the one reported.
        Err(error) => {
            failure.get_or_insert(error);
        }
    };
    record(&first);
    let mut stdouts = vec![first.stdout.clone()];

    // `--delete-delay` deletes at the end, after rsync has seen the whole
    // tree, so a name that differs only in case from one it deletes can
    // have been skipped on a volume that treats them as one. Another run
    // finds what is left. A dry run cannot see the effect of the first
    // deletions, and a run that ended before its end deleted nothing.
    let deleted =
        !dry_run && summary::has_deletion(&first.stdout) && matches!(first.code, Some(0 | 23 | 24));
    let mut second_pass = false;
    if deleted {
        let check = run(&call(true)).map_err(PushError::Rsync)?;
        record(&check);
        if summary::has_itemized_line(&check.stdout) {
            let again = run(&call(false)).map_err(PushError::Rsync)?;
            record(&again);
            stdouts.push(again.stdout);
            second_pass = true;
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(Mirrored {
            vanished,
            stdouts,
            second_pass,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::rsync::{Call, Outcome, RsyncError};
    use crate::testing::fake_rsync::FakeRsync;
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
        _fake: FakeRsync,
        rsync: Rsync,
        url: TargetUrl,
        scratch: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let fake = FakeRsync::new(RSYNC_3_5_1);
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
    fn exit_12_over_fsync_names_the_two_ways_out_for_the_marker_run_too() {
        for stderr in [
            "openrsync: unrecognized option `--fsync'\n",
            "rsync: --fsync: unknown option\n",
        ] {
            let result = fixture().send(&|path, bytes| fs::write(path, bytes), &|_| {
                answer(Some(12), stderr)
            });
            match result {
                Err(PushError::MarkerRun { detail }) => {
                    assert!(detail.contains("[target] rsync_path"), "{detail}");
                    assert!(detail.contains("fsync = false"), "{detail}");
                }
                other => panic!("expected a marker run error, got {other:?}"),
            }
        }
        let result = fixture().send(&|path, bytes| fs::write(path, bytes), &|_| {
            answer(Some(12), "rsync: protocol broke\n")
        });
        match result {
            Err(PushError::MarkerRun { detail }) => {
                assert!(!detail.contains("fsync"), "{detail}");
                assert!(detail.contains("exit 12"), "{detail}");
            }
            other => panic!("expected a marker run error, got {other:?}"),
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

    fn outcome_of(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
        Outcome {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn mirror_error(outcome: Outcome) -> String {
        match classify(&outcome) {
            Err(PushError::Mirror { detail }) => detail,
            other => panic!("expected a mirror error, got {other:?}"),
        }
    }

    #[test]
    fn exit_0_is_success_and_exit_24_names_the_vanished_paths() {
        let done = classify(&outcome_of(Some(0), ">f+++++++++ a\n", "")).expect("success");
        assert_eq!(done.vanished, Vec::<String>::new());

        let stderr = "file has vanished: \"/srv/coll/a/tmp.1\"\n\
                      rsync warning: some files vanished before they could be transferred (code 24)\n\
                      file has vanished: \"/srv/coll/b\"\n";
        let done = classify(&outcome_of(Some(24), "", stderr)).expect("success");
        assert_eq!(done.vanished, ["/srv/coll/a/tmp.1", "/srv/coll/b"]);
    }

    #[test]
    fn exit_23_with_the_io_error_line_says_no_deletion_reached_the_target() {
        let stdout = format!("{IO_ERROR_LINE}\n");
        let stderr = "rsync: [sender] opendir \"/srv/coll/locked\" failed: Permission denied (13)\n\
                      rsync: [sender] readdir(\"/srv/coll/odd\"): No such file (2)\n\
                      rsync error: some files/attrs were not transferred (code 23) at main.c(1)\n";
        let detail = mirror_error(outcome_of(Some(23), &stdout, stderr));
        assert!(
            detail.contains("no deletion reached the target"),
            "{detail}"
        );
        assert!(detail.contains("/srv/coll/locked"), "{detail}");
        assert!(detail.contains("/srv/coll/odd"), "{detail}");
        assert!(detail.contains("exit 23"), "{detail}");
    }

    #[test]
    fn another_exit_23_30_and_a_signal_are_errors_with_rsyncs_text() {
        for (code, expected) in [
            (Some(23), "exit 23"),
            (Some(30), "exit 30"),
            (Some(11), "exit 11"),
            (None, "signal"),
        ] {
            let detail = mirror_error(outcome_of(code, "", "rsync: something broke\n"));
            assert!(detail.contains(expected), "{detail}");
            assert!(detail.contains("something broke"), "{detail}");
            assert!(!detail.contains("no deletion"), "{detail}");
        }
    }

    #[test]
    fn exit_12_over_fsync_names_the_two_ways_out() {
        for stderr in [
            "openrsync: unrecognized option `--fsync'\n",
            "rsync: --fsync: unknown option\n",
        ] {
            let detail = mirror_error(outcome_of(Some(12), "", stderr));
            assert!(detail.contains("[target] rsync_path"), "{detail}");
            assert!(detail.contains("fsync = false"), "{detail}");
        }
        let detail = mirror_error(outcome_of(Some(12), "", "rsync: protocol broke\n"));
        assert!(!detail.contains("fsync"), "{detail}");
        assert!(detail.contains("exit 12"), "{detail}");
    }

    // -----------------------------------------------------
    // The second pass
    // -----------------------------------------------------

    /// Runs the mirror against answers given in order, and returns the
    /// result with the argument lists of the calls.
    fn mirrored(
        answers: Vec<Outcome>,
        dry_run: bool,
    ) -> (Result<Mirrored, PushError>, Vec<Vec<String>>) {
        let fixture = fixture();
        let options = Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let calls: RefCell<Vec<Vec<String>>> = RefCell::new(Vec::new());
        let result = run_mirror(
            &Mirror {
                rsync: &fixture.rsync,
                options: &options,
                collection: Path::new("/c"),
                url: &fixture.url,
            },
            &|call| {
                let mut calls = calls.borrow_mut();
                calls.push(
                    call.args()
                        .iter()
                        .map(|arg| arg.to_string_lossy().into_owned())
                        .collect(),
                );
                match answers.get(calls.len() - 1) {
                    Some(outcome) => Ok(outcome.clone()),
                    None => panic!("an unexpected call {}", calls.len()),
                }
            },
            dry_run,
        );
        (result, calls.into_inner())
    }

    fn dry(call: &[String]) -> bool {
        call.contains(&"-n".to_string())
    }

    const DELETING: &str = "*deleting   gone.txt
";
    const CREATED: &str = ">f+++++++++ new.txt
";

    #[test]
    fn a_run_without_deletions_is_one_call() {
        let (result, calls) = mirrored(vec![outcome_of(Some(0), CREATED, "")], false);
        let done = result.expect("success");
        assert_eq!(calls.len(), 1);
        assert!(!done.second_pass);
        assert_eq!(done.stdouts, [CREATED.as_bytes()]);
    }

    #[test]
    fn deletions_are_followed_by_a_dry_run_that_ends_the_pass_when_it_is_quiet() {
        for code in [0, 23, 24] {
            let answers = vec![
                outcome_of(Some(code), DELETING, ""),
                outcome_of(
                    Some(0),
                    "sending incremental file list
",
                    "",
                ),
            ];
            let (result, calls) = mirrored(answers, false);
            assert_eq!(calls.len(), 2, "exit {code}");
            assert!(!dry(&calls[0]) && dry(&calls[1]), "exit {code}: {calls:?}");
            if code == 23 {
                assert!(result.is_err(), "the first failure stays");
            } else {
                assert!(!result.expect("success").second_pass);
            }
        }
    }

    #[test]
    fn a_dry_run_that_finds_changes_is_followed_by_the_mirror_once_more() {
        let answers = vec![
            outcome_of(Some(0), DELETING, ""),
            outcome_of(Some(0), CREATED, ""),
            outcome_of(Some(0), CREATED, ""),
        ];
        let (result, calls) = mirrored(answers, false);
        let done = result.expect("success");
        assert_eq!(calls.len(), 3);
        assert!(!dry(&calls[2]), "{calls:?}");
        assert_eq!(calls[0], calls[2]);
        assert!(done.second_pass);
        assert_eq!(done.stdouts.len(), 2, "the first run and the second mirror");
    }

    #[test]
    fn a_directory_line_in_the_dry_run_counts_as_a_change() {
        let answers = vec![
            outcome_of(Some(0), DELETING, ""),
            outcome_of(
                Some(0),
                "cd+++++++++ dir/
",
                "",
            ),
            outcome_of(Some(0), "", ""),
        ];
        let (result, calls) = mirrored(answers, false);
        assert!(result.expect("success").second_pass);
        assert_eq!(calls.len(), 3);
    }

    #[test]
    fn a_dry_run_of_the_push_never_runs_a_second_pass() {
        let (result, calls) = mirrored(vec![outcome_of(Some(0), DELETING, "")], true);
        assert!(!result.expect("success").second_pass);
        assert_eq!(calls.len(), 1);
        assert!(dry(&calls[0]));
    }

    #[test]
    fn no_second_pass_follows_a_run_that_did_not_get_to_delete() {
        for code in [Some(12), Some(30), Some(255), None] {
            let (result, calls) = mirrored(
                vec![outcome_of(
                    code, DELETING, "boom
",
                )],
                false,
            );
            assert!(result.is_err(), "{code:?}");
            assert_eq!(calls.len(), 1, "{code:?}");
        }
    }

    #[test]
    fn a_failing_dry_run_or_second_mirror_is_the_error_and_a_first_failure_stays() {
        let answers = vec![
            outcome_of(Some(0), DELETING, ""),
            outcome_of(
                Some(30),
                "",
                "Timeout in data send/receive
",
            ),
        ];
        let (result, calls) = mirrored(answers, false);
        assert_eq!(calls.len(), 2);
        match result {
            Err(PushError::Mirror { detail }) => assert!(detail.contains("exit 30"), "{detail}"),
            other => panic!("expected a mirror error, got {other:?}"),
        }

        let answers = vec![
            outcome_of(Some(0), DELETING, ""),
            outcome_of(Some(0), CREATED, ""),
            outcome_of(
                Some(11),
                "",
                "disk full
",
            ),
        ];
        match mirrored(answers, false).0 {
            Err(PushError::Mirror { detail }) => assert!(detail.contains("disk full"), "{detail}"),
            other => panic!("expected a mirror error, got {other:?}"),
        }

        let answers = vec![
            outcome_of(
                Some(23),
                DELETING,
                "first problem
",
            ),
            outcome_of(Some(0), CREATED, ""),
            outcome_of(
                Some(11),
                "",
                "second problem
",
            ),
        ];
        match mirrored(answers, false).0 {
            Err(PushError::Mirror { detail }) => {
                assert!(detail.contains("first problem"), "{detail}");
            }
            other => panic!("expected a mirror error, got {other:?}"),
        }
    }

    #[test]
    fn vanished_files_of_every_run_are_kept() {
        let answers = vec![
            outcome_of(Some(24), DELETING, "file has vanished: \"/c/a\"\n"),
            outcome_of(Some(0), CREATED, ""),
            outcome_of(Some(24), "", "file has vanished: \"/c/b\"\n"),
        ];
        let done = mirrored(answers, false).0.expect("success");
        assert_eq!(done.vanished, ["/c/a", "/c/b"]);
    }

    #[test]
    fn an_rsync_that_cannot_start_for_the_second_call_is_the_rsync_error() {
        let fixture = fixture();
        let options = Options {
            fsync: true,
            rsync_path: None,
            rsync_args: Vec::new(),
        };
        let count = RefCell::new(0);
        let result = run_mirror(
            &Mirror {
                rsync: &fixture.rsync,
                options: &options,
                collection: Path::new("/c"),
                url: &fixture.url,
            },
            &|_| {
                *count.borrow_mut() += 1;
                if *count.borrow() == 1 {
                    Ok(outcome_of(Some(0), DELETING, ""))
                } else {
                    Err(RsyncError::Spawn {
                        path: "rsync".into(),
                        cause: io::Error::other("gone"),
                    })
                }
            },
            false,
        );
        assert!(matches!(result, Err(PushError::Rsync(_))), "{result:?}");
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
