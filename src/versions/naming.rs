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

/// Finds the version `input` names among `versions`.
///
/// The spelling of the input decides what it is matched against, and the
/// two are never mixed. An input with a `T` is a prefix of a version
/// stamp. Any other input is a prefix of the version's time in `zone`, and
/// an incomplete version has no such time to match. A version whose text
/// equals the input wins over the versions it is a prefix of.
pub fn find_version(input: &str, versions: &[Candidate], zone: &TimeZone) -> Lookup {
    let texts: Vec<(Stamp, String)> = if input.contains('T') {
        versions
            .iter()
            .map(|version| (version.stamp, version.stamp.to_string()))
            .collect()
    } else {
        versions
            .iter()
            .filter(|version| !version.incomplete)
            .map(|version| (version.stamp, local_text(&version.stamp, zone)))
            .collect()
    };
    let mut found: Vec<Stamp> = texts
        .iter()
        .filter(|(_, text)| text == input)
        .map(|(stamp, _)| *stamp)
        .collect();
    if found.is_empty() {
        found = texts
            .iter()
            .filter(|(_, text)| text.starts_with(input))
            .map(|(stamp, _)| *stamp)
            .collect();
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

    #[test]
    fn a_lower_case_t_is_not_a_stamp_input() {
        let versions = [complete("2026-11-04T090211")];
        assert_eq!(
            find("2026-11-04t090211", &versions, &berlin()),
            Lookup::NoMatch
        );
    }
}
