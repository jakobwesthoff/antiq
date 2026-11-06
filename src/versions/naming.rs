// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Matching a `--version` argument against the versions of a piece.

use super::stamp::Stamp;
use jiff::tz::TimeZone;

/// A version of a piece as far as naming it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub stamp: Stamp,
    /// Sidecars without an archive: such a version is named by its stamp
    /// only.
    pub incomplete: bool,
}

/// What an input names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Found(Stamp),
    NoMatch,
    /// Several versions match; their stamps in order.
    Several(Vec<Stamp>),
}

/// The time of `stamp` in `zone` with seconds, `2026-11-04 10:02:11`.
pub fn local_text(stamp: &Stamp, zone: &TimeZone) -> String {
    stamp
        .time()
        .to_zoned(zone.clone())
        .strftime("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// The zone the machine is in now: `TZ` when it is set, the system's zone
/// otherwise.
pub fn system_zone() -> TimeZone {
    TimeZone::system()
}

/// How text output names each of `stamps`: its local time, with the UTC
/// offset added where another of `stamps` renders alike.
pub fn local_labels(stamps: &[Stamp], zone: &TimeZone) -> Vec<String> {
    let texts: Vec<String> = stamps.iter().map(|stamp| local_text(stamp, zone)).collect();
    stamps
        .iter()
        .zip(&texts)
        .map(|(stamp, text)| {
            if texts.iter().filter(|other| *other == text).count() > 1 {
                format!("{text} {}", offset_text(stamp, zone))
            } else {
                text.clone()
            }
        })
        .collect()
}

/// The UTC offset of `zone` at `stamp`, `+02:00`.
fn offset_text(stamp: &Stamp, zone: &TimeZone) -> String {
    stamp
        .time()
        .to_zoned(zone.clone())
        .strftime("%:z")
        .to_string()
}

/// Finds the version `input` names among `versions`.
///
/// The spelling of the input decides what it is matched against, and the
/// two are never mixed. An input with a `T` is a prefix of a version
/// stamp. Any other input is a prefix of the version's time in `zone`, and
/// an incomplete version has no such time to match. Where text output adds
/// the UTC offset to the time ([`local_labels`]), the input may carry it
/// or leave it out. A version whose text equals the input wins over the
/// versions it is a prefix of.
pub fn find_version(input: &str, versions: &[Candidate], zone: &TimeZone) -> Lookup {
    let texts: Vec<(Stamp, Vec<String>)> = if input.contains('T') {
        versions
            .iter()
            .map(|version| (version.stamp, vec![version.stamp.to_string()]))
            .collect()
    } else {
        let complete: Vec<Stamp> = versions
            .iter()
            .filter(|version| !version.incomplete)
            .map(|version| version.stamp)
            .collect();
        let labels = local_labels(&complete, zone);
        complete
            .iter()
            .zip(labels)
            .map(|(stamp, label)| {
                let mut renderings = vec![local_text(stamp, zone)];
                if label != renderings[0] {
                    renderings.push(label);
                }
                (*stamp, renderings)
            })
            .collect()
    };
    let matching = |matches: &dyn Fn(&str) -> bool| -> Vec<Stamp> {
        texts
            .iter()
            .filter(|(_, renderings)| renderings.iter().any(|text| matches(text)))
            .map(|(stamp, _)| *stamp)
            .collect()
    };
    let mut found = matching(&|text| text == input);
    if found.is_empty() {
        found = matching(&|text| text.starts_with(input));
    }
    found.sort();
    match found.as_slice() {
        [] => Lookup::NoMatch,
        [only] => Lookup::Found(*only),
        _ => Lookup::Several(found),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn berlin() -> TimeZone {
        TimeZone::get("Europe/Berlin").expect("the time zone database knows Berlin")
    }

    fn stamp(text: &str) -> Stamp {
        text.parse().expect("a stamp")
    }

    fn complete(text: &str) -> Candidate {
        Candidate {
            stamp: stamp(text),
            incomplete: false,
        }
    }

    fn incomplete(text: &str) -> Candidate {
        Candidate {
            stamp: stamp(text),
            incomplete: true,
        }
    }

    fn find(input: &str, versions: &[Candidate], zone: &TimeZone) -> Lookup {
        find_version(input, versions, zone)
    }

    #[test]
    fn local_text_is_the_time_in_the_zone_with_seconds() {
        let at = stamp("2026-11-04T090211");
        assert_eq!(local_text(&at, &berlin()), "2026-11-04 10:02:11");
        assert_eq!(local_text(&at, &TimeZone::UTC), "2026-11-04 09:02:11");
    }

    #[test]
    fn a_whole_stamp_finds_its_version() {
        let versions = [complete("2026-11-04T090211"), complete("2026-11-04T090212")];
        assert_eq!(
            find("2026-11-04T090211", &versions, &berlin()),
            Lookup::Found(stamp("2026-11-04T090211"))
        );
    }

    #[test]
    fn a_stamp_with_a_counter_finds_only_that_version() {
        let versions = [
            complete("2026-11-04T090211"),
            complete("2026-11-04T090211-2"),
        ];
        assert_eq!(
            find("2026-11-04T090211-2", &versions, &berlin()),
            Lookup::Found(stamp("2026-11-04T090211-2"))
        );
    }

    #[test]
    fn an_exact_stamp_wins_over_the_stamps_it_is_a_prefix_of() {
        let versions = [
            complete("2026-11-04T090211"),
            complete("2026-11-04T090211-2"),
        ];
        assert_eq!(
            find("2026-11-04T090211", &versions, &berlin()),
            Lookup::Found(stamp("2026-11-04T090211"))
        );
    }

    #[test]
    fn a_stamp_prefix_with_several_matches_names_them_in_order() {
        let versions = [
            complete("2026-11-04T090300"),
            complete("2026-11-05T090211"),
            complete("2026-11-04T090211"),
        ];
        assert_eq!(
            find("2026-11-04T09", &versions, &berlin()),
            Lookup::Several(vec![stamp("2026-11-04T090211"), stamp("2026-11-04T090300")])
        );
    }

    #[test]
    fn a_local_input_is_a_prefix_of_the_local_time() {
        let versions = [complete("2026-11-04T090211")];
        let found = Lookup::Found(stamp("2026-11-04T090211"));
        for input in ["2026-11-04", "2026-11-04 10:02", "2026-11-04 10:02:11"] {
            assert_eq!(find(input, &versions, &berlin()), found, "{input}");
        }
    }

    #[test]
    fn the_zone_decides_what_a_local_input_matches() {
        let versions = [complete("2026-11-04T090211")];
        assert_eq!(
            find("2026-11-04 10:02", &versions, &TimeZone::UTC),
            Lookup::NoMatch
        );
        assert_eq!(
            find("2026-11-04 09:02", &versions, &TimeZone::UTC),
            Lookup::Found(stamp("2026-11-04T090211"))
        );
    }

    #[test]
    fn near_midnight_the_local_date_is_not_the_date_of_the_stamp() {
        let versions = [complete("2026-11-03T233000")];
        assert_eq!(
            find("2026-11-04", &versions, &berlin()),
            Lookup::Found(stamp("2026-11-03T233000"))
        );
        assert_eq!(find("2026-11-03", &versions, &berlin()), Lookup::NoMatch);
    }

    #[test]
    fn a_local_prefix_with_several_matches_names_them() {
        let versions = [complete("2026-11-04T090212"), complete("2026-11-04T090211")];
        assert_eq!(
            find("2026-11-04 10:02", &versions, &berlin()),
            Lookup::Several(vec![stamp("2026-11-04T090211"), stamp("2026-11-04T090212")])
        );
    }

    #[test]
    fn versions_of_one_second_cannot_be_told_apart_by_their_local_time() {
        let versions = [
            complete("2026-11-04T090211"),
            complete("2026-11-04T090211-2"),
        ];
        assert_eq!(
            find("2026-11-04 10:02:11", &versions, &berlin()),
            Lookup::Several(vec![
                stamp("2026-11-04T090211"),
                stamp("2026-11-04T090211-2")
            ])
        );
    }

    #[test]
    fn an_incomplete_version_is_named_by_its_stamp_only() {
        let versions = [incomplete("2026-11-04T090211")];
        assert_eq!(
            find("2026-11-04T090211", &versions, &berlin()),
            Lookup::Found(stamp("2026-11-04T090211"))
        );
        assert_eq!(
            find("2026-11-04 10:02", &versions, &berlin()),
            Lookup::NoMatch
        );
    }

    #[test]
    fn an_empty_input_matches_every_complete_version() {
        let versions = [
            complete("2026-11-04T090212"),
            incomplete("2026-11-04T090300"),
            complete("2026-11-04T090211"),
        ];
        assert_eq!(
            find("", &versions, &berlin()),
            Lookup::Several(vec![stamp("2026-11-04T090211"), stamp("2026-11-04T090212")])
        );
    }

    #[test]
    fn an_input_that_names_no_version_finds_none() {
        let versions = [complete("2026-11-04T090211")];
        for input in ["2026-11-05", "2026-11-04T0903", "yesterday"] {
            assert_eq!(
                find(input, &versions, &berlin()),
                Lookup::NoMatch,
                "{input}"
            );
        }
        assert_eq!(find("2026-11-04", &[], &berlin()), Lookup::NoMatch);
    }

    /// The repeated hour of the end of daylight saving time in Berlin:
    /// 02:30 happens at 00:30 UTC (+02:00) and again at 01:30 UTC (+01:00).
    fn repeated_hour() -> [Candidate; 2] {
        [complete("2026-10-25T003000"), complete("2026-10-25T013000")]
    }

    #[test]
    fn two_versions_that_render_alike_get_the_offset() {
        let stamps = [stamp("2026-10-25T003000"), stamp("2026-10-25T013000")];
        assert_eq!(
            local_labels(&stamps, &berlin()),
            ["2026-10-25 02:30:00 +02:00", "2026-10-25 02:30:00 +01:00"]
        );
    }

    #[test]
    fn versions_that_render_apart_get_no_offset() {
        let stamps = [stamp("2026-11-04T090211"), stamp("2026-11-04T090212")];
        assert_eq!(
            local_labels(&stamps, &berlin()),
            ["2026-11-04 10:02:11", "2026-11-04 10:02:12"]
        );
    }

    #[test]
    fn one_repeated_pair_does_not_mark_a_third_version() {
        let stamps = [
            stamp("2026-10-25T003000"),
            stamp("2026-10-25T013000"),
            stamp("2026-10-25T020000"),
        ];
        let labels = local_labels(&stamps, &berlin());
        assert_eq!(labels[2], "2026-10-25 03:00:00");
        assert!(labels[0].ends_with("+02:00") && labels[1].ends_with("+01:00"));
    }

    #[test]
    fn versions_of_one_second_are_both_marked_though_the_offset_is_the_same() {
        let stamps = [stamp("2026-11-04T090211"), stamp("2026-11-04T090211-2")];
        assert_eq!(
            local_labels(&stamps, &berlin()),
            ["2026-11-04 10:02:11 +01:00", "2026-11-04 10:02:11 +01:00"]
        );
    }

    #[test]
    fn an_input_with_the_offset_names_one_of_the_repeated_times() {
        let versions = repeated_hour();
        assert_eq!(
            find("2026-10-25 02:30:00 +02:00", &versions, &berlin()),
            Lookup::Found(stamp("2026-10-25T003000"))
        );
        assert_eq!(
            find("2026-10-25 02:30:00 +01:00", &versions, &berlin()),
            Lookup::Found(stamp("2026-10-25T013000"))
        );
        assert_eq!(
            find("2026-10-25 02:30:00 +02", &versions, &berlin()),
            Lookup::Found(stamp("2026-10-25T003000"))
        );
    }

    #[test]
    fn an_input_without_the_offset_names_both_repeated_times() {
        assert_eq!(
            find("2026-10-25 02:30:00", &repeated_hour(), &berlin()),
            Lookup::Several(vec![stamp("2026-10-25T003000"), stamp("2026-10-25T013000")])
        );
    }

    #[test]
    fn an_offset_where_the_output_shows_none_names_nothing() {
        let versions = [complete("2026-11-04T090211")];
        assert_eq!(
            find("2026-11-04 10:02:11 +01:00", &versions, &berlin()),
            Lookup::NoMatch
        );
    }

    #[test]
    fn an_incomplete_version_does_not_make_a_complete_one_show_its_offset() {
        let versions = [
            complete("2026-10-25T003000"),
            incomplete("2026-10-25T013000"),
        ];
        assert_eq!(
            find("2026-10-25 02:30:00 +02:00", &versions, &berlin()),
            Lookup::NoMatch
        );
        assert_eq!(
            find("2026-10-25 02:30:00", &versions, &berlin()),
            Lookup::Found(stamp("2026-10-25T003000"))
        );
    }

    /// Set in the environment of the child process of
    /// [`the_system_zone_follows_tz`], which runs this test alone.
    const CHILD: &str = "ANTIQ_SYSTEM_ZONE_CHILD";

    #[test]
    fn the_child_of_the_system_zone_test() {
        if std::env::var_os(CHILD).is_none() {
            return;
        }
        let at = stamp("2026-11-04T090211");
        println!("LOCAL={}", local_text(&at, &system_zone()));
    }

    /// `TZ` cannot be set in this process: the environment is shared with
    /// the other test threads. A child process gets it instead.
    #[test]
    fn the_system_zone_follows_tz() {
        let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "versions::naming::tests::the_child_of_the_system_zone_test",
                "--exact",
                "--nocapture",
            ])
            .env("TZ", "Europe/Berlin")
            .env(CHILD, "1")
            .output()
            .expect("the test binary runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("LOCAL=2026-11-04 10:02:11"),
            "stdout: {stdout}"
        );
    }

    #[test]
    fn a_lower_case_t_is_not_a_stamp_input() {
        let versions = [complete("2026-11-04T090211")];
        assert_eq!(
            find("2026-11-04t090211", &versions, &berlin()),
            Lookup::NoMatch
        );
    }
}
