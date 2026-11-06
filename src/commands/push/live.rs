// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The progress of the mirror as one item: the entries rsync lists, the
//! bytes it sends, and the operation it is at.

use super::observed::{Segment, progress_total, segment};
use crate::progress::{Counter, Event, Observer, Operation};

/// Follows the observed run of the mirror and tells the observer about it.
///
/// The whole collection is one run of rsync, so it is one item. Its entries
/// count every entry rsync lists, directories and unchanged ones too, as
/// rsync cannot say beforehand which ones change.
pub(super) struct Progress<'a> {
    observer: &'a dyn Observer,
    name: String,
    /// The entries listed while rsync stores, up to the first deletion.
    entries: Counter,
    /// The bytes of the files rsync sends.
    sent: Counter,
    /// The entries deleted, once rsync deletes.
    removed: Option<Counter>,
    /// The total of the latest progress update.
    reported_total: u64,
    started: bool,
}

impl<'a> Progress<'a> {
    pub(super) fn new(name: &str, observer: &'a dyn Observer) -> Progress<'a> {
        Progress {
            observer,
            name: name.to_string(),
            entries: Counter::new(),
            sent: Counter::new(),
            removed: None,
            reported_total: 0,
            started: false,
        }
    }

    /// One segment of the observed output of rsync.
    pub(super) fn segment(&mut self, bytes: &[u8]) {
        if let Some(total) = progress_total(bytes) {
            self.reported_total = total;
            self.show_total();
        }
        match segment(bytes) {
            Segment::Progress { .. } | Segment::Other => {}
            Segment::Entry(entry) if entry.deleting => self.remove(),
            Segment::Entry(entry) => {
                // Deletions come last, so an entry after one is not
                // counted.
                if self.removed.is_none() {
                    self.entries.add(1);
                    self.sent.add(entry.sent_bytes);
                    self.show_total();
                    self.start();
                }
            }
        }
    }

    /// The card opens with the first entry, in the operation of storing.
    fn start(&mut self) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        self.observer.event(Event::Started {
            name: &self.name,
            bytes: 0,
            stamp: None,
            siblings: &[],
        });
        self.observer.event(Event::Operation {
            name: &self.name,
            operation: Operation::Storing,
            counter: &self.entries,
        });
        self.observer.event(Event::Measured {
            name: &self.name,
            bytes: &self.sent,
        });
    }

    /// The first deleted entry moves the item to removing, with a counter
    /// of its own: rsync's total does not count deletions.
    fn remove(&mut self) {
        self.start();
        if let Some(removed) = &self.removed {
            removed.add(1);
            return;
        }
        let removed = self.removed.insert(Counter::new());
        removed.add(1);
        self.observer.event(Event::Operation {
            name: &self.name,
            operation: Operation::Removing,
            counter: removed,
        });
    }

    /// rsync's total is shown while it covers the entries seen. A total
    /// that lags behind them would read as a count past its end, so then
    /// the total is unknown.
    fn show_total(&self) {
        let total = if self.reported_total >= self.entries.done() {
            self.reported_total
        } else {
            0
        };
        self.entries.set_total(total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::recording::Recorder;

    fn feed(progress: &mut Progress, segments: &[&str]) {
        for text in segments {
            progress.segment(text.as_bytes());
        }
    }

    fn update(done: u64, total: u64) -> String {
        format!(
            "\r  1,234  50%  1.00MB/s  0:00:01 (xfr#1, to-chk={}/{total})\n",
            total - done
        )
    }

    #[test]
    fn the_item_opens_with_the_first_entry_and_only_then() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("nas:/antiq", &recorder);

        feed(
            &mut progress,
            &["sending incremental file list\n", &update(0, 5)],
        );
        assert_eq!(recorder.lines(), Vec::<String>::new());

        feed(&mut progress, &[".d          96 ./\n"]);
        assert_eq!(
            recorder.lines(),
            [
                "started nas:/antiq 0",
                "nas:/antiq Storing 1/5",
                "nas:/antiq sent 0"
            ]
        );

        feed(
            &mut progress,
            &[".f          5 a.txt\n", "cd+++++++++ 96 d/\n"],
        );
        assert_eq!(recorder.lines().len(), 3, "no second start");
        assert_eq!(recorder.lines()[1], "nas:/antiq Storing 3/5");
    }

    #[test]
    fn every_entry_counts_but_only_files_that_are_sent_add_bytes() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);

        feed(
            &mut progress,
            &[
                ".d          224 ./\n",
                ">f+++++++++ 40 a.bin\n",
                "<f.st...... 2 b.bin\n",
                "hf+++++++++ 40 c.bin => a.bin\n",
                "cd+++++++++ 96 sub/\n",
                ".f          900 same.bin\n",
                "cL+++++++++ 5 l -> a.bin\n",
            ],
        );

        assert_eq!(recorder.lines()[1], "t Storing 7/0");
        assert_eq!(recorder.lines()[2], "t sent 42");
        assert_eq!(recorder.lines_of("t"), recorder.lines());
    }

    #[test]
    fn the_total_follows_the_latest_update_while_it_covers_the_entries() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);
        feed(&mut progress, &[".d          1 ./\n"]);
        let total = |recorder: &Recorder| recorder.lines()[1].clone();

        feed(&mut progress, &["\r  1  1%  (xfr#1, ir-chk=9/12)\n"]);
        assert_eq!(total(&recorder), "t Storing 1/12");
        feed(&mut progress, &["\r  1  1%  (xfr#1, to-chk=2/20)\n"]);
        assert_eq!(total(&recorder), "t Storing 1/20");

        // The total lags behind the entries: the count has no end.
        feed(
            &mut progress,
            &[
                ".f          1 a\n",
                ".f          1 b\n",
                "\r  1  1%  (xfr#1, ir-chk=0/2)\n",
            ],
        );
        assert_eq!(total(&recorder), "t Storing 3/0");
        feed(&mut progress, &["\r  1  1%  (xfr#1, ir-chk=0/3)\n"]);
        assert_eq!(total(&recorder), "t Storing 3/3");
    }

    #[test]
    fn an_entry_glued_to_a_progress_update_counts_and_the_update_sets_the_total() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);
        feed(&mut progress, &[".d          1 ./\n"]);

        feed(
            &mut progress,
            &["\r      5 100%    0.00kB/s    0:00:00 (xfr#1, to-chk=3/6)>f+++++++++ 5 a/b.txt\n"],
        );

        assert_eq!(recorder.lines()[1], "t Storing 2/6");
        assert_eq!(recorder.lines()[2], "t sent 5");
    }

    #[test]
    fn a_deletion_glued_to_a_progress_update_opens_removing() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);
        feed(&mut progress, &[".d          1 ./\n"]);

        feed(
            &mut progress,
            &["\r      5 100%    0.00kB/s    0:00:00 (xfr#1, to-chk=0/1)*deleting   0 old\n"],
        );

        assert_eq!(recorder.lines()[3], "t Removing 1/0");
    }

    #[test]
    fn an_update_without_a_total_changes_nothing() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);
        feed(
            &mut progress,
            &[".d          1 ./\n", "\r  1  1%  (xfr#1, to-chk=0/1)\n"],
        );

        feed(&mut progress, &["\r  1  50%  1.00MB/s  0:00:01  "]);

        assert_eq!(recorder.lines()[1], "t Storing 1/1");
    }

    #[test]
    fn deletions_after_the_last_update_open_removing_and_leave_the_storing_count() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);
        feed(
            &mut progress,
            &[
                ".d          1 ./\n",
                ">f+++++++++ 4 a\n",
                "\r  4  100%  (xfr#1, to-chk=0/2)\n",
                "*deleting   0 gone/f\n",
                "*deleting   0 gone/\n",
                "*deleting   0 old\n",
            ],
        );

        let lines = recorder.lines();
        assert_eq!(lines[1], "t Storing 2/2");
        assert_eq!(lines[3], "t Removing 3/0");
        assert_eq!(lines.len(), 4, "{lines:?}");
    }

    #[test]
    fn an_entry_after_a_deletion_is_not_counted() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);
        feed(
            &mut progress,
            &[
                ".d          1 ./\n",
                "*deleting   0 old\n",
                ">f+++++++++ 4 late\n",
            ],
        );

        assert_eq!(recorder.lines()[1], "t Storing 1/0");
        assert_eq!(recorder.lines()[2], "t sent 0");
    }

    #[test]
    fn a_run_that_starts_with_a_deletion_opens_the_item_too() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);

        feed(&mut progress, &["*deleting   0 old\n"]);

        assert_eq!(
            recorder.lines(),
            ["started t 0", "t Storing 0/0", "t sent 0", "t Removing 1/0"]
        );
    }

    #[test]
    fn lines_that_name_no_entry_are_ignored() {
        let recorder = Recorder::new();
        let mut progress = Progress::new("t", &recorder);

        feed(
            &mut progress,
            &[
                "sending incremental file list\n",
                "IO error encountered -- skipping file deletion\n",
                "\n",
                ">f+++++++++ projects/a/no-length\n",
            ],
        );

        assert_eq!(recorder.lines(), Vec::<String>::new());
    }
}
