//! A day on the calendar, with no time and no zone: when an episode first
//! aired. On the wire it is `"YYYY-MM-DD"`, so it reads the same in every
//! time zone; a Unix time for a date shows the day before west of UTC.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a string is not a `YYYY-MM-DD` date.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a YYYY-MM-DD date")]
pub struct DateError(String);

/// A day of the proleptic Gregorian calendar, years 1 to 9999.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CalendarDate {
    year: u16,
    month: u8,
    day: u8,
}

impl CalendarDate {
    /// The date, if there is such a day.
    pub fn new(year: u16, month: u8, day: u8) -> Option<Self> {
        let valid = (1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && (1..=days_in(year, month)).contains(&day);
        valid.then_some(Self { year, month, day })
    }

    pub fn year(self) -> u16 {
        self.year
    }

    pub fn month(self) -> u8 {
        self.month
    }

    pub fn day(self) -> u8 {
        self.day
    }

    /// The day `days` after 1970-01-01 (before it, negative), in UTC;
    /// `None` outside years 1 to 9999. Howard Hinnant's civil-from-days.
    pub fn from_unix_days(days: i64) -> Option<Self> {
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self::new(u16::try_from(year).ok()?, month as u8, day as u8)
    }

    /// The UTC day of Unix seconds `secs`.
    pub fn of_unix(secs: i64) -> Option<Self> {
        Self::from_unix_days(secs.div_euclid(86_400))
    }
}

fn days_in(year: u16, month: u8) -> u8 {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl fmt::Display for CalendarDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl FromStr for CalendarDate {
    type Err = DateError;

    fn from_str(s: &str) -> Result<Self, DateError> {
        let error = || DateError(s.to_owned());
        // Exactly four, two and two digits: `2019-5-7` is not one, so
        // there is one spelling of each day.
        let mut parts = s.split('-');
        let mut part = |width: usize| {
            parts
                .next()
                .filter(|p| p.len() == width && p.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(error)
        };
        let (year, month, day) = (part(4)?, part(2)?, part(2)?);
        if parts.next().is_some() {
            return Err(error());
        }
        // At most four digits each, so every parse fits; `new` checks the
        // ranges.
        match (year.parse(), month.parse(), day.parse()) {
            (Ok(year), Ok(month), Ok(day)) => Self::new(year, month, day).ok_or_else(error),
            _ => Err(error()),
        }
    }
}

impl Serialize for CalendarDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CalendarDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_days_are_the_utc_calendar_day() {
        let days = [0, 19_782, -1, 11_016].map(CalendarDate::from_unix_days);
        let expected = [(1970, 1, 1), (2024, 2, 29), (1969, 12, 31), (2000, 2, 29)]
            .map(|(y, m, d)| CalendarDate::new(y, m, d));
        assert_eq!(days, expected);
    }

    #[test]
    fn a_date_is_a_yyyy_mm_dd_string_on_the_wire() {
        let date = CalendarDate::new(2019, 5, 7).unwrap();
        let json = serde_json::to_value(date).unwrap();
        assert_eq!(json, "2019-05-07");
        assert_eq!(serde_json::from_value::<CalendarDate>(json).unwrap(), date);
    }

    #[test]
    fn only_days_that_exist_parse() {
        assert!("2024-02-29".parse::<CalendarDate>().is_ok());
        for bad in [
            "2023-02-29",
            "1900-02-29",
            "2019-13-01",
            "2019-04-31",
            "2019-00-10",
            "0000-01-01",
            "2019-5-7",
            "2019-05-07T00:00:00Z",
            "2019/05/07",
            "+201-05-07",
            "",
        ] {
            assert!(bad.parse::<CalendarDate>().is_err(), "{bad}");
        }
    }
}
