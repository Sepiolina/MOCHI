//! Timestamps (spec Annex B.2 D15; plan T28).
//!
//! * **Format:** RFC 3339, UTC, `Z` suffix, exactly nine fractional digits:
//!   `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`, always 30 bytes.
//! * **Range:** years 0000–9999; the writer refuses times outside it.
//! * **Leap seconds:** the writer never emits second 60 (a [`Timestamp`] made
//!   from Unix time cannot hold one); [`Timestamp::parse`] accepts it.
//! * **Ordering:** for values the writer produces, lexical order of the
//!   strings equals chronological order (fixed width, fixed digits, a single
//!   zone). [`Timestamp`]'s `Ord` is chronological, and a parsed leap second
//!   sorts after second 59 of its minute and before the next minute.
//!
//! The parser accepts exactly the D15 form (upper-case `T` and `Z`, nine
//! digits, no offset). Other RFC 3339 spellings of the same instant are
//! refused rather than normalized, so a stored value round-trips byte for
//! byte.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{ErrorCode, MochiError, Result};

/// Seconds from the Unix epoch to 0000-01-01T00:00:00Z.
pub const MIN_UNIX_SECS: i64 = -62_167_219_200;
/// Seconds from the Unix epoch to 9999-12-31T23:59:59Z.
pub const MAX_UNIX_SECS: i64 = 253_402_300_799;
/// Length of every formatted timestamp.
pub const FORMATTED_LEN: usize = 30;

/// A UTC instant in the D15 range, with nanoseconds.
///
/// Fields are ordered so the derived `Ord` is chronological: the second
/// (Unix time; for a leap second, that of second 59), then whether this is
/// the leap second 60, then the nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    secs: i64,
    leap: bool,
    nanos: u32,
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, msg)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day count since 1970-01-01 (`civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

impl Timestamp {
    /// From Unix time. Refuses nanoseconds ≥ 10⁹ and instants outside years
    /// 0000–9999 (`INVALID_ARGUMENT`): the writer never emits them.
    pub fn from_unix(secs: i64, nanos: u32) -> Result<Self> {
        if nanos >= 1_000_000_000 {
            return Err(invalid(format!(
                "{nanos} nanoseconds is not below one second"
            )));
        }
        if !(MIN_UNIX_SECS..=MAX_UNIX_SECS).contains(&secs) {
            return Err(invalid(format!(
                "Unix time {secs} is outside years 0000-9999 (Annex B.2 D15)"
            )));
        }
        Ok(Timestamp {
            secs,
            leap: false,
            nanos,
        })
    }

    /// The system clock, now.
    pub fn now() -> Result<Self> {
        let (secs, nanos) = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => (
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                d.subsec_nanos(),
            ),
            Err(e) => {
                let d = e.duration();
                let s = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
                match d.subsec_nanos() {
                    0 => (-s, 0),
                    n => (-s - 1, 1_000_000_000 - n),
                }
            }
        };
        Self::from_unix(secs, nanos)
    }

    /// Unix seconds (for a parsed leap second, those of second 59).
    pub fn unix_secs(&self) -> i64 {
        self.secs
    }

    pub fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// Whether this was parsed from a second-60 value.
    pub fn is_leap_second(&self) -> bool {
        self.leap
    }

    /// Parse the D15 form exactly. Accepts second 60 (a leap second).
    pub fn parse(s: &str) -> Result<Self> {
        let b = s.as_bytes();
        let bad = || invalid(format!("{s:?} is not an Annex B.2 D15 timestamp"));
        if b.len() != FORMATTED_LEN {
            return Err(bad());
        }
        let digits = |r: std::ops::Range<usize>| -> Result<u32> {
            let part = b.get(r).ok_or_else(bad)?;
            if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
                return Err(bad());
            }
            Ok(part.iter().fold(0u32, |a, d| a * 10 + u32::from(d - b'0')))
        };
        for (at, ch) in [
            (4, b'-'),
            (7, b'-'),
            (10, b'T'),
            (13, b':'),
            (16, b':'),
            (19, b'.'),
            (29, b'Z'),
        ] {
            if b[at] != ch {
                return Err(bad());
            }
        }
        let (y, mo, d) = (i64::from(digits(0..4)?), digits(5..7)?, digits(8..10)?);
        let (h, mi, sec) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
        let nanos = digits(20..29)?;
        if !(1..=12).contains(&mo) || d == 0 || d > days_in_month(y, mo) || h > 23 || mi > 59 {
            return Err(bad());
        }
        let leap = sec == 60;
        if sec > 60 {
            return Err(bad());
        }
        let secs = days_from_civil(y, mo, d) * 86_400
            + i64::from(h) * 3_600
            + i64::from(mi) * 60
            + i64::from(if leap { 59 } else { sec });
        Ok(Timestamp { secs, leap, nanos })
    }
}

impl fmt::Display for Timestamp {
    /// Always [`FORMATTED_LEN`] bytes. Values from [`Timestamp::from_unix`]
    /// never show second 60.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.secs.div_euclid(86_400);
        let rem = self.secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        let (h, mi, s) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
        let s = if self.leap { 60 } else { s };
        write!(
            f,
            "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:09}Z",
            self.nanos
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ts(secs: i64, nanos: u32) -> Timestamp {
        Timestamp::from_unix(secs, nanos).unwrap()
    }

    #[test]
    fn known_values() {
        assert_eq!(ts(0, 0).to_string(), "1970-01-01T00:00:00.000000000Z");
        assert_eq!(
            ts(-1, 999_999_999).to_string(),
            "1969-12-31T23:59:59.999999999Z"
        );
        assert_eq!(
            ts(1_700_000_000, 123_456_789).to_string(),
            "2023-11-14T22:13:20.123456789Z"
        );
        assert_eq!(
            ts(951_782_400, 0).to_string(),
            "2000-02-29T00:00:00.000000000Z"
        );
        assert_eq!(
            ts(MIN_UNIX_SECS, 0).to_string(),
            "0000-01-01T00:00:00.000000000Z"
        );
        assert_eq!(
            ts(MAX_UNIX_SECS, 999_999_999).to_string(),
            "9999-12-31T23:59:59.999999999Z"
        );
    }

    /// **T28 DoD (range).** Years outside 0000–9999 and out-of-range
    /// nanoseconds are refused.
    #[test]
    fn out_of_range_is_refused() {
        for (s, n) in [
            (MIN_UNIX_SECS - 1, 0),
            (MAX_UNIX_SECS + 1, 0),
            (i64::MIN, 0),
            (i64::MAX, 0),
            (0, 1_000_000_000),
        ] {
            let e = Timestamp::from_unix(s, n).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{s} {n}");
        }
        assert!(Timestamp::from_unix(MIN_UNIX_SECS, 0).is_ok());
        assert!(Timestamp::from_unix(MAX_UNIX_SECS, 999_999_999).is_ok());
    }

    #[test]
    fn now_is_in_range_and_formats_to_30_bytes() {
        let t = Timestamp::now().unwrap();
        assert_eq!(t.to_string().len(), FORMATTED_LEN);
        assert!(!t.is_leap_second());
    }

    /// Readers accept second 60 and order it after :59 and before the next
    /// minute; the writer cannot produce it.
    #[test]
    fn leap_second_is_accepted_and_ordered() {
        let leap = Timestamp::parse("2016-12-31T23:59:60.500000000Z").unwrap();
        assert!(leap.is_leap_second());
        assert_eq!(leap.to_string(), "2016-12-31T23:59:60.500000000Z");
        let before = Timestamp::parse("2016-12-31T23:59:59.999999999Z").unwrap();
        let after = Timestamp::parse("2017-01-01T00:00:00.000000000Z").unwrap();
        assert!(before < leap && leap < after);
        // The writer's constructor cannot make one.
        assert!(!ts(1_483_228_799, 500_000_000).is_leap_second());
        assert_eq!(
            ts(1_483_228_799, 500_000_000).to_string(),
            "2016-12-31T23:59:59.500000000Z"
        );
    }

    #[test]
    fn malformed_is_refused() {
        for s in [
            "",
            "1970-01-01T00:00:00Z",
            "1970-01-01T00:00:00.000000000+00:00",
            "1970-01-01t00:00:00.000000000Z",
            "1970-01-01T00:00:00.000000000z",
            "1970-01-01 00:00:00.000000000Z",
            "1970-13-01T00:00:00.000000000Z",
            "1970-00-01T00:00:00.000000000Z",
            "1970-02-30T00:00:00.000000000Z",
            "1900-02-29T00:00:00.000000000Z",
            "1970-01-01T24:00:00.000000000Z",
            "1970-01-01T00:60:00.000000000Z",
            "1970-01-01T00:00:61.000000000Z",
            "1970-01-01T00:00:00.00000000aZ",
            "+970-01-01T00:00:00.000000000Z",
        ] {
            assert!(Timestamp::parse(s).is_err(), "{s:?}");
        }
        assert!(Timestamp::parse("2000-02-29T00:00:00.000000000Z").is_ok());
    }

    fn any_ts() -> impl Strategy<Value = Timestamp> {
        (MIN_UNIX_SECS..=MAX_UNIX_SECS, 0u32..1_000_000_000).prop_map(|(s, n)| ts(s, n))
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 4096, ..ProptestConfig::default() })]

        /// **T28 DoD.** Lexical order of the formatted strings equals
        /// chronological order, and every value round-trips.
        #[test]
        fn lexical_order_equals_chronological_order(a in any_ts(), b in any_ts()) {
            let (sa, sb) = (a.to_string(), b.to_string());
            prop_assert_eq!(sa.len(), FORMATTED_LEN);
            prop_assert_eq!(sa.cmp(&sb), a.cmp(&b));
            prop_assert_eq!(Timestamp::parse(&sa).unwrap(), a);
        }

        /// Close instants too (same second, neighbouring seconds), where a
        /// width or carry error would show.
        #[test]
        fn neighbours_order(s in MIN_UNIX_SECS..MAX_UNIX_SECS - 2, n in 0u32..1_000_000_000, d in 0u32..2_000_000_000) {
            let a = ts(s, n);
            let total = u64::from(n) + u64::from(d);
            let b = ts(s + (total / 1_000_000_000) as i64, (total % 1_000_000_000) as u32);
            prop_assert!(a.to_string() <= b.to_string());
            prop_assert_eq!(a.to_string().cmp(&b.to_string()), a.cmp(&b));
        }
    }
}
