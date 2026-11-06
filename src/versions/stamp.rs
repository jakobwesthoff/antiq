// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Version stamps: `YYYY-MM-DDTHHMMSS`, with `-N` for a counter of 2 or more.
//!
//! A stamp and the piece path together name a version exactly. The stamp is
//! a UTC second; versions of one piece made within the same second are told
//! apart by the counter, and no counter counts as 1, so `-1` is no valid
//! spelling. A stamp names a UTC time from `0001-01-01T000000` to
//! `9999-12-30T215959`; a text outside that range is no stamp.

use super::VersionsError;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

/// The highest counter a new stamp gets. The cut of a store file name
/// reserves room for exactly this many digits.
pub const MAX_COUNTER: u32 = 9999;

/// The first time a stamp may name.
const FIRST: DateTime = DateTime::constant(1, 1, 1, 0, 0, 0, 0);

/// The last time a stamp may name: the last whole second before the end of
/// the range `jiff::Timestamp` holds (`9999-12-30T22:00:00Z`).
const LAST: DateTime = DateTime::constant(9999, 12, 30, 21, 59, 59, 0);

/// The time of a version, to the second in UTC, and its counter.
///
/// Stamps order by time, then by counter as a number; the derived order
/// does that because a missing counter sorts before every counter of 2 or
/// more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp {
    second: Timestamp,
    /// Never 1: that is the stamp without a counter.
    counter: Option<NonZeroU32>,
}

impl Stamp {
    /// The stamp of `time` cut to the whole second, without a counter. A
    /// time outside the range of stamps (`0001-01-01T000000` to
    /// `9999-12-30T215959`) has none.
    pub fn at(time: Timestamp) -> Result<Stamp, VersionsError> {
        let second = Timestamp::from_second(time.as_second())
            .expect("a whole second of a valid timestamp is valid");
        if !(FIRST..=LAST).contains(&second.to_zoned(TimeZone::UTC).datetime()) {
            return Err(VersionsError::InvalidStamp(time.to_string()));
        }
        Ok(Stamp {
            second,
            counter: None,
        })
    }

    /// The stamp of the same second with counter `counter`; 0 and 1 give
    /// the stamp without a counter, which is the same version name.
    pub fn with_counter(self, counter: u32) -> Stamp {
        Stamp {
            counter: NonZeroU32::new(counter).filter(|counter| counter.get() >= 2),
            ..self
        }
    }

    /// The UTC second of the stamp.
    pub fn time(&self) -> Timestamp {
        self.second
    }

    /// The counter as a number; 1 for a stamp without one.
    pub fn counter(&self) -> u32 {
        self.counter.map_or(1, NonZeroU32::get)
    }

    /// Reads a stamp written the way [`fmt::Display`] writes it, and
    /// nothing else.
    pub fn parse(text: &str) -> Result<Stamp, VersionsError> {
        read(text).ok_or_else(|| VersionsError::InvalidStamp(text.to_string()))
    }
}

/// `None` when `text` is not exactly a stamp. Hand-rolled because the
/// parsers of `jiff` accept forms (signs, short fields, spaces) that no
/// file name carries.
fn read(text: &str) -> Option<Stamp> {
    let bytes = text.as_bytes();
    if bytes.len() < 17 {
        return None;
    }
    let (head, rest) = bytes.split_at(17);
    let shape_ok = head.iter().enumerate().all(|(index, byte)| match index {
        4 | 7 => *byte == b'-',
        10 => *byte == b'T',
        _ => byte.is_ascii_digit(),
    });
    if !shape_ok {
        return None;
    }
    let number = |from: usize, to: usize| -> i64 {
        head[from..to]
            .iter()
            .fold(0, |sum, digit| sum * 10 + i64::from(digit - b'0'))
    };
    let civil = DateTime::new(
        i16::try_from(number(0, 4)).ok()?,
        i8::try_from(number(5, 7)).ok()?,
        i8::try_from(number(8, 10)).ok()?,
        i8::try_from(number(11, 13)).ok()?,
        i8::try_from(number(13, 15)).ok()?,
        i8::try_from(number(15, 17)).ok()?,
        0,
    )
    .ok()?;
    if !(FIRST..=LAST).contains(&civil) {
        return None;
    }
    let second = civil.to_zoned(TimeZone::UTC).ok()?.timestamp();

    let counter = match rest {
        [] => None,
        [b'-', digits @ ..] => {
            let valid =
                !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) && digits[0] != b'0';
            if !valid {
                return None;
            }
            let number: u32 = std::str::from_utf8(digits).ok()?.parse().ok()?;
            // A counter of 1 is the stamp without one.
            Some(NonZeroU32::new(number).filter(|number| number.get() >= 2)?)
        }
        _ => return None,
    };
    Some(Stamp { second, counter })
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let civil = self.second.to_zoned(TimeZone::UTC).datetime();
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}{:02}{:02}",
            civil.year(),
            civil.month(),
            civil.day(),
            civil.hour(),
            civil.minute(),
            civil.second()
        )?;
        match self.counter {
            Some(counter) => write!(f, "-{counter}"),
            None => Ok(()),
        }
    }
}

impl FromStr for Stamp {
    type Err = VersionsError;

    fn from_str(text: &str) -> Result<Stamp, VersionsError> {
        Stamp::parse(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(text: &str) -> Stamp {
        Stamp::parse(text).expect("a valid stamp")
    }

    #[test]
    fn stamps_round_trip_with_and_without_a_counter() {
        for text in [
            "2026-11-04T090211",
            "2026-11-04T090211-2",
            "2026-11-04T090211-10",
            "2026-11-04T090211-9999",
            "0001-01-01T000000",
            "9999-12-30T215959",
        ] {
            assert_eq!(stamp(text).to_string(), text);
        }
    }

    #[test]
    fn the_parts_of_a_stamp_are_read() {
        let parsed = stamp("2026-11-04T090211-7");
        assert_eq!(parsed.time().to_string(), "2026-11-04T09:02:11Z");
        assert_eq!(parsed.counter(), 7);
        assert_eq!(stamp("2026-11-04T090211").counter(), 1);
    }

    #[test]
    fn the_counter_orders_as_a_number() {
        let mut stamps = [
            stamp("2026-11-04T090211-10"),
            stamp("2026-11-04T090211-2"),
            stamp("2026-11-04T090211"),
            stamp("2026-11-04T090211-9"),
            stamp("2026-11-04T090210-50"),
        ];
        stamps.sort();
        let order: Vec<String> = stamps.iter().map(Stamp::to_string).collect();
        assert_eq!(
            order,
            [
                "2026-11-04T090210-50",
                "2026-11-04T090211",
                "2026-11-04T090211-2",
                "2026-11-04T090211-9",
                "2026-11-04T090211-10",
            ]
        );
    }

    #[test]
    fn at_cuts_to_the_second_and_has_no_counter() {
        let time: Timestamp = "2026-11-04T09:02:11.987654321Z".parse().expect("a time");
        let at = Stamp::at(time).expect("in range");
        assert_eq!(at.to_string(), "2026-11-04T090211");
        assert_eq!(at, stamp("2026-11-04T090211"));
    }

    fn at_text(text: &str) -> Result<Stamp, VersionsError> {
        Stamp::at(text.parse().expect("a timestamp"))
    }

    #[test]
    fn at_refuses_times_outside_the_range_of_stamps() {
        assert_eq!(
            at_text("0001-01-01T00:00:00Z").expect("first").to_string(),
            "0001-01-01T000000"
        );
        assert_eq!(
            at_text("9999-12-30T21:59:59.9Z").expect("last").to_string(),
            "9999-12-30T215959"
        );
        let before = at_text("0000-12-31T23:59:59Z").expect_err("before the first");
        assert!(
            matches!(before, VersionsError::InvalidStamp(_)),
            "{before:?}"
        );
        let after = at_text("9999-12-30T22:00:00Z").expect_err("after the last");
        assert!(matches!(after, VersionsError::InvalidStamp(_)), "{after:?}");
    }

    #[test]
    fn a_counter_of_zero_or_one_is_the_stamp_without_one() {
        let plain = stamp("2026-11-04T090211");
        assert_eq!(plain.with_counter(0), plain);
        assert_eq!(plain.with_counter(1), plain);
        assert_eq!(plain.with_counter(1).to_string(), "2026-11-04T090211");
        assert_eq!(plain.with_counter(3), stamp("2026-11-04T090211-3"));
        assert_eq!(plain.with_counter(3).with_counter(1), plain);
    }

    #[test]
    fn from_str_reads_like_parse() {
        let parsed: Stamp = "2026-11-04T090211-2".parse().expect("a stamp");
        assert_eq!(parsed, stamp("2026-11-04T090211-2"));
    }

    fn refused(text: &str) {
        let error = Stamp::parse(text).expect_err(text);
        assert!(
            matches!(&error, VersionsError::InvalidStamp(shown) if shown == text),
            "{error:?}"
        );
    }

    #[test]
    fn a_counter_of_one_is_refused() {
        refused("2026-11-04T090211-1");
    }

    #[test]
    fn a_counter_of_zero_is_refused() {
        refused("2026-11-04T090211-0");
    }

    #[test]
    fn a_counter_with_a_leading_zero_is_refused() {
        refused("2026-11-04T090211-02");
    }

    #[test]
    fn an_empty_counter_is_refused() {
        refused("2026-11-04T090211-");
    }

    #[test]
    fn a_counter_with_other_characters_is_refused() {
        refused("2026-11-04T090211-2x");
        refused("2026-11-04T090211-+2");
        refused("2026-11-04T090211-\u{663}");
    }

    #[test]
    fn a_counter_that_overflows_is_refused() {
        refused("2026-11-04T090211-4294967296");
    }

    #[test]
    fn a_stamp_without_the_t_is_refused() {
        refused("2026-11-04 090211");
    }

    #[test]
    fn a_stamp_of_the_wrong_length_is_refused() {
        refused("2026-11-04T09021");
        refused("2026-11-04T0902111");
        refused("");
    }

    #[test]
    fn fields_out_of_range_are_refused() {
        refused("2026-13-04T090211");
        refused("2026-11-32T090211");
        refused("2026-02-30T090211");
        refused("2026-11-04T240000");
        refused("2026-11-04T096011");
        refused("2026-11-04T090260");
    }

    #[test]
    fn a_trailing_space_or_a_sign_is_refused() {
        refused("2026-11-04T090211 ");
        refused("+026-11-04T090211");
        refused("2026-11-04T09:02:11");
    }

    #[test]
    fn the_range_of_stamps_is_pinned_at_both_ends() {
        assert_eq!(stamp("0001-01-01T000000").to_string(), "0001-01-01T000000");
        assert_eq!(stamp("9999-12-30T215959").to_string(), "9999-12-30T215959");
        refused("0000-12-31T235959");
        refused("9999-12-30T220000");
        refused("9999-12-31T235959");
    }
}
