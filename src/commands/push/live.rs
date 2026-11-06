// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The progress of the mirror: what a dry run said the pieces will change,
//! and the itemized lines of the real run that show how far it is.

use super::summary::{Change, SizedItem, item_of_line, owner};
use crate::progress::{Counter, Event, Observer, Operation, Outcome, Planned};
use crate::relpath::RelPath;

/// What the dry run found for one piece.
struct Expected {
    folder: RelPath,
    bytes: u64,
    /// Files created or updated, in the store folder and the live folder.
    adds: u64,
    deletes: u64,
    /// Some of the adds are in the live folder, where the card opens.
    live_adds: bool,
}

/// What the dry run found, per piece in path order.
pub(super) struct Forecast {
    pieces: Vec<Expected>,
}

impl Forecast {
    pub(super) fn from_sized(items: &[SizedItem], folders: &[RelPath]) -> Forecast {
        let mut pieces: Vec<Expected> = Vec::new();
        for sized in items {
            let Some(found) = owner(&sized.item.path, folders) else {
                continue;
            };
            let folder = &folders[found.index];
            let at = match pieces.iter().position(|piece| &piece.folder == folder) {
                Some(at) => at,
                None => {
                    pieces.push(Expected {
                        folder: folder.clone(),
                        bytes: 0,
                        adds: 0,
                        deletes: 0,
                        live_adds: false,
                    });
                    pieces.len() - 1
                }
            };
            let piece = &mut pieces[at];
            piece.bytes += sized.bytes;
            match sized.item.change {
                Change::Created | Change::Updated => {
                    piece.adds += 1;
                    piece.live_adds |= !found.in_store;
                }
                Change::Deleted => piece.deletes += 1,
            }
        }
        pieces.sort_by(|a, b| a.folder.as_bytes().cmp(b.folder.as_bytes()));
        Forecast { pieces }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }
}

/// Where a piece is in the real run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No card is open: the piece has not started, or only its files in the
    /// store folder went by.
    Waiting,
    /// Its files are being sent.
    Adding,
    /// The files are sent, the deletions are still to come.
    Paused,
    /// Its deletions are being made.
    Deleting,
    Done,
}

struct Track {
    phase: Phase,
    /// The observer has been told the piece started.
    started: bool,
    seen_adds: u64,
    created: u64,
    updated: u64,
    seen_deletes: u64,
    counter: Counter,
    /// What the counter shows now.
    shown: u64,
}

/// Follows the real run piece by piece.
///
/// rsync names a file before it sends it, so the piece that got the latest
/// line has one file in transit, which is not counted as done yet. The
/// piece of the line before it is complete once a line of another piece
/// shows up and its files and deletions all went by.
pub(super) struct Live<'a> {
    observer: &'a dyn Observer,
    forecast: Forecast,
    folders: Vec<RelPath>,
    tracks: Vec<Track>,
    latest: Option<usize>,
}

impl<'a> Live<'a> {
    /// Tells the observer which pieces the run will change.
    pub(super) fn new(
        forecast: Forecast,
        folders: Vec<RelPath>,
        observer: &'a dyn Observer,
    ) -> Live<'a> {
        let planned: Vec<Planned> = forecast
            .pieces
            .iter()
            .map(|piece| Planned {
                name: piece.folder.to_string(),
                bytes: piece.bytes,
            })
            .collect();
        observer.event(Event::Planned { items: &planned });
        let tracks = forecast
            .pieces
            .iter()
            .map(|_| Track {
                phase: Phase::Waiting,
                started: false,
                seen_adds: 0,
                created: 0,
                updated: 0,
                seen_deletes: 0,
                counter: Counter::new(),
                shown: 0,
            })
            .collect();
        Live {
            observer,
            forecast,
            folders,
            tracks,
            latest: None,
        }
    }

    /// One line of the standard output of the real run.
    pub(super) fn line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let Some(item) = item_of_line(line) else {
            return;
        };
        let Some(found) = owner(&item.path, &self.folders) else {
            return;
        };
        let folder = &self.folders[found.index];
        let Some(at) = self
            .forecast
            .pieces
            .iter()
            .position(|piece| &piece.folder == folder)
        else {
            return;
        };
        for other in (0..self.tracks.len()).filter(|other| *other != at) {
            self.settle(other);
        }
        match item.change {
            Change::Created | Change::Updated => self.add(at, item.change, found.in_store),
            Change::Deleted => self.delete(at),
        }
        self.latest = Some(at);
        for index in 0..self.tracks.len() {
            self.sync(index, Some(index) == self.latest);
        }
    }

    /// The real run ended well: whatever is still open finishes, and a
    /// planned piece that never showed up is dropped.
    pub(super) fn finish(&mut self) {
        for index in 0..self.tracks.len() {
            match self.tracks[index].phase {
                Phase::Adding | Phase::Paused | Phase::Deleting => self.complete(index),
                Phase::Waiting => {
                    self.observer.event(Event::Dropped {
                        name: &self.forecast.pieces[index].folder.to_string(),
                    });
                    self.tracks[index].phase = Phase::Done;
                }
                Phase::Done => {}
            }
        }
    }

    fn add(&mut self, at: usize, change: Change, in_store: bool) {
        let live_adds = self.forecast.pieces[at].live_adds;
        let track = &mut self.tracks[at];
        match track.phase {
            Phase::Done | Phase::Paused | Phase::Deleting => return,
            Phase::Waiting | Phase::Adding => {}
        }
        track.seen_adds += 1;
        match change {
            Change::Created => track.created += 1,
            _ => track.updated += 1,
        }
        // The files of the store folder come first and the card opens with
        // the files of the live folder, when the piece has any.
        if track.phase == Phase::Waiting && !(in_store && live_adds) {
            self.open(at, Phase::Adding);
        }
    }

    fn delete(&mut self, at: usize) {
        let track = &mut self.tracks[at];
        if track.phase == Phase::Done {
            return;
        }
        track.seen_deletes += 1;
        if track.phase != Phase::Deleting {
            self.open(at, Phase::Deleting);
        }
    }

    /// Opens the card of a piece for the operation of `phase`, the first
    /// time with the start of the piece.
    fn open(&mut self, at: usize, phase: Phase) {
        let expected = &self.forecast.pieces[at];
        let name = expected.folder.to_string();
        let track = &mut self.tracks[at];
        if !track.started {
            track.started = true;
            self.observer.event(Event::Started {
                name: &name,
                bytes: expected.bytes,
                stamp: None,
            });
        }
        let (operation, total) = match phase {
            Phase::Deleting => (Operation::Removing, expected.deletes),
            _ => (Operation::Storing, expected.adds),
        };
        track.counter = Counter::new();
        track.counter.set_total(total);
        track.shown = 0;
        track.phase = phase;
        self.observer.event(Event::Operation {
            name: &name,
            operation,
            counter: &track.counter,
        });
    }

    /// Ends the stretch of a piece that is not the one of the latest line,
    /// when everything it was to do went by.
    fn settle(&mut self, at: usize) {
        let (adds, deletes) = {
            let expected = &self.forecast.pieces[at];
            (expected.adds, expected.deletes)
        };
        let track = &self.tracks[at];
        match track.phase {
            Phase::Adding if track.seen_adds >= adds => {
                if deletes > 0 {
                    self.fill(at);
                    let name = self.forecast.pieces[at].folder.to_string();
                    self.observer.event(Event::Paused { name: &name });
                    self.tracks[at].phase = Phase::Paused;
                } else {
                    self.complete(at);
                }
            }
            Phase::Deleting if track.seen_deletes >= deletes => self.complete(at),
            _ => {}
        }
    }

    /// Brings the counter of an open card to the lines seen, less the file
    /// in transit for the piece of the latest line, and never past its total.
    fn sync(&mut self, at: usize, latest: bool) {
        let track = &mut self.tracks[at];
        let seen = match track.phase {
            Phase::Adding => track.seen_adds,
            Phase::Deleting => track.seen_deletes,
            _ => return,
        };
        let wanted = if latest { seen.saturating_sub(1) } else { seen };
        let wanted = wanted.min(track.counter.total());
        if wanted > track.shown {
            track.counter.add(wanted - track.shown);
            track.shown = wanted;
        }
    }

    /// Puts the counter of the open stretch of a piece at the lines seen.
    fn fill(&mut self, at: usize) {
        self.sync(at, false);
    }

    fn complete(&mut self, at: usize) {
        self.fill(at);
        let expected = &self.forecast.pieces[at];
        let track = &mut self.tracks[at];
        track.phase = Phase::Done;
        let created = track.created.min(expected.adds);
        let outcome = Outcome::Changes {
            created,
            updated: track.updated.min(expected.adds - created),
            deleted: track.seen_deletes.min(expected.deletes),
        };
        self.observer.event(Event::Finished {
            name: &expected.folder.to_string(),
            bytes: expected.bytes,
            outcome: &outcome,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::push::summary::parse_sized;
    use crate::testing::recording::Recorder;

    fn path(text: &str) -> RelPath {
        RelPath::parse(text).expect("a valid path")
    }

    fn folders() -> Vec<RelPath> {
        vec![path("projects/a"), path("projects/b")]
    }

    fn forecast(plan: &str) -> Forecast {
        Forecast::from_sized(&parse_sized(plan.as_bytes()), &folders())
    }

    fn feed(live: &mut Live, lines: &[&str]) {
        for line in lines {
            live.line(format!("{line}\n").as_bytes());
        }
    }

    #[test]
    fn the_forecast_gives_each_piece_its_bytes_and_counts_in_path_order() {
        let found = forecast(
            ">f+++++++++ 5 projects/b/z\n\
             >f+++++++++ 10 projects/a/x\n\
             >f.st...... 20 projects/a/y\n\
             *deleting   0 projects/a/gone\n\
             >f+++++++++ 7 elsewhere/q\n",
        );
        let summary: Vec<(String, u64, u64, u64, bool)> = found
            .pieces
            .iter()
            .map(|piece| {
                (
                    piece.folder.to_string(),
                    piece.bytes,
                    piece.adds,
                    piece.deletes,
                    piece.live_adds,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("projects/a".to_string(), 30, 2, 1, true),
                ("projects/b".to_string(), 5, 1, 0, true)
            ]
        );
    }

    #[test]
    fn pieces_are_announced_and_each_card_closes_when_the_next_piece_starts() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(
                ">f+++++++++ 10 projects/a/x\n\
                 >f+++++++++ 20 projects/a/y\n\
                 >f+++++++++ 5 projects/b/z\n",
            ),
            folders(),
            &recorder,
        );
        assert_eq!(recorder.lines(), ["planned projects/a projects/b"]);

        // rsync names a file before it sends it, so the file just named is
        // not counted as done yet.
        feed(&mut live, &[">f+++++++++ projects/a/x"]);
        assert_eq!(
            recorder.lines()[1..],
            ["started projects/a 30", "projects/a Storing 0/2"]
        );
        feed(&mut live, &[">f+++++++++ projects/a/y"]);
        assert_eq!(recorder.lines()[2], "projects/a Storing 1/2");

        feed(&mut live, &[">f+++++++++ projects/b/z"]);
        let lines = recorder.lines();
        assert_eq!(lines[2], "projects/a Storing 2/2");
        assert_eq!(
            lines[3..],
            [
                "finished projects/a 30 Changes { created: 2, updated: 0, deleted: 0 }",
                "started projects/b 5",
                "projects/b Storing 0/1",
            ]
        );

        live.finish();
        let lines = recorder.lines();
        assert_eq!(lines[5], "projects/b Storing 1/1");
        assert_eq!(
            lines.last().map(String::as_str),
            Some("finished projects/b 5 Changes { created: 1, updated: 0, deleted: 0 }")
        );
    }

    #[test]
    fn the_lines_of_the_store_folder_open_no_card_and_the_live_folder_does() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(
                ">f+++++++++ 100 .antiq/pieces/projects/a/v.tar\n\
                 >f+++++++++ 5 projects/a/x\n",
            ),
            folders(),
            &recorder,
        );

        feed(&mut live, &[">f+++++++++ .antiq/pieces/projects/a/v.tar"]);
        assert_eq!(recorder.lines(), ["planned projects/a"]);

        feed(&mut live, &[">f+++++++++ projects/a/x"]);
        live.finish();
        assert_eq!(
            recorder.lines()[1..],
            [
                "started projects/a 105",
                "projects/a Storing 2/2",
                "finished projects/a 105 Changes { created: 2, updated: 0, deleted: 0 }",
            ]
        );
    }

    #[test]
    fn a_piece_with_only_store_lines_opens_at_the_first_of_them() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(">f+++++++++ 100 .antiq/pieces/projects/a/v.tar\n"),
            folders(),
            &recorder,
        );

        feed(&mut live, &[">f+++++++++ .antiq/pieces/projects/a/v.tar"]);

        assert_eq!(recorder.lines()[1], "started projects/a 100");
    }

    #[test]
    fn a_piece_with_deletions_pauses_after_its_files_and_reopens_for_the_deletions() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(
                ">f+++++++++ 10 projects/a/x\n\
                 *deleting   0 projects/a/gone\n\
                 >f+++++++++ 5 projects/b/z\n",
            ),
            folders(),
            &recorder,
        );

        feed(
            &mut live,
            &[
                ">f+++++++++ projects/a/x",
                ">f+++++++++ projects/b/z",
                "*deleting   projects/a/gone",
            ],
        );
        live.finish();

        assert_eq!(
            recorder.lines_of("projects/a"),
            [
                "started projects/a 10",
                "projects/a Storing 1/1",
                "paused projects/a",
                "projects/a Removing 1/1",
                "finished projects/a 10 Changes { created: 1, updated: 0, deleted: 1 }",
            ]
        );
    }

    #[test]
    fn counts_stop_at_the_forecast_when_the_real_run_finds_more() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(">f+++++++++ 10 projects/a/x\n"),
            folders(),
            &recorder,
        );

        feed(
            &mut live,
            &[
                ">f+++++++++ projects/a/x",
                ">f+++++++++ projects/a/y",
                ">f.st...... projects/a/z",
            ],
        );
        live.finish();

        assert_eq!(
            recorder.lines()[2..],
            [
                "projects/a Storing 1/1",
                "finished projects/a 10 Changes { created: 1, updated: 0, deleted: 0 }",
            ]
        );
    }

    #[test]
    fn what_is_open_when_the_run_ends_finishes_and_a_piece_that_never_came_is_dropped() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(
                ">f+++++++++ 10 projects/a/x\n\
                 >f+++++++++ 5 projects/b/z\n",
            ),
            folders(),
            &recorder,
        );

        feed(&mut live, &[">f+++++++++ projects/a/x"]);
        live.finish();

        let lines = recorder.lines();
        assert_eq!(
            lines[lines.len() - 2],
            "finished projects/a 10 Changes { created: 1, updated: 0, deleted: 0 }"
        );
        assert_eq!(lines.last().map(String::as_str), Some("dropped projects/b"));
    }

    #[test]
    fn lines_that_belong_to_no_planned_piece_and_other_text_are_ignored() {
        let recorder = Recorder::new();
        let mut live = Live::new(
            forecast(">f+++++++++ 10 projects/a/x\n"),
            folders(),
            &recorder,
        );

        feed(
            &mut live,
            &[
                "sending incremental file list",
                ">f+++++++++ elsewhere/q",
                ">f+++++++++ projects/b/z",
                "cd+++++++++ projects/a/",
            ],
        );

        assert_eq!(recorder.lines(), ["planned projects/a"]);
    }
}
