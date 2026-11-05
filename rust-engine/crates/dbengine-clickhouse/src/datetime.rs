//! Calendar arithmetic and the date/time text the driver reads from the grid.
//!
//! # One convention for read, edit and key
//!
//! A `DateTime` / `DateTime64` value is shown as the **UTC instant**
//! (`YYYY-MM-DD HH:MM:SS[.fraction]`, followed by ` UTC` when the column has a
//! named zone other than UTC; see `types`). What is written is read the same
//! way: the text is an instant in UTC unless it carries its own `UTC` / `Z` /
//! `±HH:MM` suffix, whatever time zone the *column* has. The driver turns the
//! text into an exact instant itself (no time-zone database is needed: the zone
//! is always explicit or UTC) and binds it as a UTC-typed value, so the server
//! stores that instant in a column of any zone.

use std::fmt;

/// Why a date/time text was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateTimeError {
    /// Not `YYYY-MM-DD[ HH:MM:SS[.fraction]][ UTC|Z|±HH:MM]`.
    Syntax,
    /// A real calendar date or time was expected (month 13, February 30 ...).
    Calendar,
    /// More fraction digits than the column keeps, or a fraction on a `DateTime`.
    Precision,
    /// Outside what the column type can hold.
    Range,
}

impl fmt::Display for DateTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Syntax => {
                "expected YYYY-MM-DD HH:MM:SS[.fraction] with an optional UTC, Z or +HH:MM suffix"
            }
            Self::Calendar => "not a calendar date and time",
            Self::Precision => "has more fraction digits than the column keeps",
            Self::Range => "is outside the range of the column type",
        })
    }
}

/// An instant: whole seconds since 1970-01-01T00:00:00Z and the nanoseconds
/// after them (`0..1_000_000_000`, also for instants before 1970).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instant {
    pub seconds: i64,
    pub nanos: u32,
}

/// Days since 1970-01-01 of a proleptic Gregorian date (month `1..=12`).
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = i64::from((month + 9) % 12);
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The `(year, month, day)` of a day count since 1970-01-01.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `n` ASCII digits as a number.
fn digits(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |acc, byte| {
        byte.is_ascii_digit()
            .then(|| acc * 10 + u32::from(byte - b'0'))
    })
}

/// `YYYY-MM-DD` as days since 1970-01-01.
pub(crate) fn parse_date(text: &str) -> Result<i64, DateTimeError> {
    let (days, rest) = date_prefix(text.trim().as_bytes())?;
    if rest.is_empty() {
        Ok(days)
    } else {
        Err(DateTimeError::Syntax)
    }
}

/// The date at the start of `bytes` and what follows it.
fn date_prefix(bytes: &[u8]) -> Result<(i64, &[u8]), DateTimeError> {
    let [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1, rest @ ..] = bytes else {
        return Err(DateTimeError::Syntax);
    };
    let year = digits(&[*y0, *y1, *y2, *y3]).ok_or(DateTimeError::Syntax)?;
    let month = digits(&[*m0, *m1]).ok_or(DateTimeError::Syntax)?;
    let day = digits(&[*d0, *d1]).ok_or(DateTimeError::Syntax)?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(i64::from(year), month) {
        return Err(DateTimeError::Calendar);
    }
    Ok((days_from_civil(i64::from(year), month, day), rest))
}

impl Instant {
    /// Parses `YYYY-MM-DD[ |T]HH:MM:SS[.fraction][ ]{UTC|Z|±HH:MM|±HHMM|±HH}`. A
    /// date alone is midnight; no suffix means UTC.
    pub(crate) fn parse(text: &str) -> Result<Self, DateTimeError> {
        let (days, rest) = date_prefix(text.trim().as_bytes())?;
        let mut seconds = days * 86_400;
        let mut nanos = 0_u32;
        let mut rest = rest;
        if let [b' ' | b'T', h0, h1, b':', m0, m1, b':', s0, s1, tail @ ..] = rest {
            let hour = digits(&[*h0, *h1]).ok_or(DateTimeError::Syntax)?;
            let minute = digits(&[*m0, *m1]).ok_or(DateTimeError::Syntax)?;
            let second = digits(&[*s0, *s1]).ok_or(DateTimeError::Syntax)?;
            if hour > 23 || minute > 59 || second > 59 {
                return Err(DateTimeError::Calendar);
            }
            seconds += i64::from(hour * 3600 + minute * 60 + second);
            rest = tail;
            if let [b'.', fraction @ ..] = rest {
                let length = fraction.iter().take_while(|b| b.is_ascii_digit()).count();
                if length == 0 {
                    return Err(DateTimeError::Syntax);
                }
                // Digits past the ninth must be zeros: nothing is rounded away.
                let (kept, extra) = fraction[..length].split_at(length.min(9));
                if extra.iter().any(|digit| *digit != b'0') {
                    return Err(DateTimeError::Precision);
                }
                nanos = kept
                    .iter()
                    .fold(0_u32, |acc, digit| acc * 10 + u32::from(digit - b'0'))
                    * 10_u32.pow(9 - kept.len() as u32);
                rest = &fraction[length..];
            }
        }
        seconds -= i64::from(zone_offset(rest)?);
        Ok(Self { seconds, nanos })
    }

    /// The sub-second part in ticks of `10^-precision` seconds, when no digit
    /// finer than that is lost.
    fn ticks(&self, precision: u32) -> Result<u32, DateTimeError> {
        let unit = 10_u32.pow(9 - precision.min(9));
        if self.nanos.is_multiple_of(unit) {
            Ok(self.nanos / unit)
        } else {
            Err(DateTimeError::Precision)
        }
    }

    /// Seconds for a `DateTime` column: whole seconds in `0 ..= u32::MAX` (a
    /// value outside it would be saturated silently by the server).
    pub(crate) fn datetime_seconds(&self) -> Result<u32, DateTimeError> {
        self.ticks(0)?;
        u32::try_from(self.seconds).map_err(|_| DateTimeError::Range)
    }

    /// Checks the instant fits a `DateTime64(precision)` column (an `Int64` of
    /// `10^-precision` second ticks).
    pub(crate) fn check_datetime64(&self, precision: u32) -> Result<(), DateTimeError> {
        let ticks = self.ticks(precision)?;
        10_i64
            .checked_pow(precision)
            .and_then(|per_second| self.seconds.checked_mul(per_second))
            .and_then(|whole| whole.checked_add(i64::from(ticks)))
            .map(drop)
            .ok_or(DateTimeError::Range)
    }

    /// Ticks of `10^-scale` seconds since the epoch (`scale` 3, 6 or 9: the
    /// units of `fromUnixTimestamp64Milli` / `Micro` / `Nano`), when they fit an
    /// `Int64`. Call [`Self::check_datetime64`] first: this drops finer digits.
    pub(crate) fn epoch_ticks(&self, scale: u32) -> Result<i64, DateTimeError> {
        let sub = i64::from(self.nanos / 10_u32.pow(9 - scale));
        10_i64
            .checked_pow(scale)
            .and_then(|per_second| self.seconds.checked_mul(per_second))
            .and_then(|whole| whole.checked_add(sub))
            .ok_or(DateTimeError::Range)
    }

    /// `YYYY-MM-DD HH:MM:SS[.fraction]` (UTC) with `precision` fraction digits,
    /// or `…T…Z` when `iso`.
    pub(crate) fn format(&self, precision: u32, iso: bool) -> Result<String, DateTimeError> {
        let ticks = self.ticks(precision)?;
        let (year, month, day) = civil_from_days(self.seconds.div_euclid(86_400));
        if !(0..=9999).contains(&year) {
            return Err(DateTimeError::Range);
        }
        let of_day = self.seconds.rem_euclid(86_400);
        let mut out = format!(
            "{year:04}-{month:02}-{day:02}{}{:02}:{:02}:{:02}",
            if iso { 'T' } else { ' ' },
            of_day / 3600,
            of_day % 3600 / 60,
            of_day % 60
        );
        if precision > 0 {
            out.push_str(&format!(".{ticks:0width$}", width = precision as usize));
        }
        if iso {
            out.push('Z');
        }
        Ok(out)
    }
}

/// The offset east of UTC, in seconds, of a suffix: nothing, `UTC`, `Z`, `GMT`,
/// `±HH:MM`, `±HHMM` or `±HH` (spaces before it are ignored).
fn zone_offset(suffix: &[u8]) -> Result<i32, DateTimeError> {
    let suffix = suffix.trim_ascii();
    let (sign, rest) = match suffix {
        [] => return Ok(0),
        b"UTC" | b"utc" | b"Z" | b"z" | b"GMT" => return Ok(0),
        [b'+', rest @ ..] => (1, rest),
        [b'-', rest @ ..] => (-1, rest),
        _ => return Err(DateTimeError::Syntax),
    };
    let (hours, minutes) = match rest {
        [h0, h1] => (digits(&[*h0, *h1]), Some(0)),
        [h0, h1, m0, m1] | [h0, h1, b':', m0, m1] => (digits(&[*h0, *h1]), digits(&[*m0, *m1])),
        _ => return Err(DateTimeError::Syntax),
    };
    let (Some(hours), Some(minutes)) = (hours, minutes) else {
        return Err(DateTimeError::Syntax);
    };
    if hours > 23 || minutes > 59 {
        return Err(DateTimeError::Calendar);
    }
    Ok(sign * (hours * 3600 + minutes * 60) as i32)
}

/// `YYYY-MM-DD` of a day count since 1970-01-01.
pub(crate) fn format_date(days: i64) -> Result<String, DateTimeError> {
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return Err(DateTimeError::Range);
    }
    Ok(format!("{year:04}-{month:02}-{day:02}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seconds(text: &str) -> i64 {
        Instant::parse(text).unwrap().seconds
    }

    #[test]
    fn civil_dates_round_trip_with_days_since_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2020, 1, 2), 18_263);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(1900, 1, 1), -25_567);
        for days in [-100_000, -25_567, -1, 0, 1, 59, 60, 18_263, 49_709, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "{days}");
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(parse_date("2024-02-29"), Ok(19_782));
        assert_eq!(parse_date("2023-02-29"), Err(DateTimeError::Calendar));
        assert_eq!(parse_date("2023-13-01"), Err(DateTimeError::Calendar));
        assert_eq!(parse_date("2023-1-01"), Err(DateTimeError::Syntax));
        assert_eq!(parse_date("2023-01-01 "), Ok(19_358));
        assert_eq!(format_date(19_782).as_deref(), Ok("2024-02-29"));
    }

    #[test]
    fn the_text_the_grid_shows_is_an_utc_instant_unless_it_names_an_offset() {
        let base = 1_577_934_245;
        for text in [
            "2020-01-02 03:04:05",
            "2020-01-02 03:04:05 UTC",
            "2020-01-02 03:04:05UTC",
            "2020-01-02T03:04:05Z",
            "2020-01-02 03:04:05+00:00",
            "2020-01-02 12:04:05+09:00",
            "2020-01-01 22:04:05-05:00",
            "2020-01-02 12:04:05 +0900",
            "2020-01-02 12:04:05+09",
            "  2020-01-02 03:04:05  ",
        ] {
            assert_eq!(seconds(text), base, "{text}");
        }
        assert_eq!(seconds("2020-01-02"), base - 3 * 3600 - 4 * 60 - 5);
        assert_eq!(seconds("1969-12-31 23:59:59"), -1);
    }

    #[test]
    fn fractions_keep_every_digit_and_never_round() {
        let moment = Instant::parse("2020-01-02 03:04:05.123456789").unwrap();
        assert_eq!(moment.nanos, 123_456_789);
        assert_eq!(
            moment.format(9, false).unwrap(),
            "2020-01-02 03:04:05.123456789"
        );
        assert_eq!(moment.format(6, false), Err(DateTimeError::Precision));
        let millis = Instant::parse("2020-01-02 03:04:05.120 UTC").unwrap();
        assert_eq!(millis.format(3, false).unwrap(), "2020-01-02 03:04:05.120");
        assert_eq!(millis.format(3, true).unwrap(), "2020-01-02T03:04:05.120Z");
        assert_eq!(millis.format(0, false), Err(DateTimeError::Precision));
        // Trailing zeros past the ninth digit lose nothing.
        assert!(Instant::parse("2020-01-02 03:04:05.1234567890").is_ok());
        assert_eq!(
            Instant::parse("2020-01-02 03:04:05.1234567891"),
            Err(DateTimeError::Precision)
        );
        // Before 1970 the fraction still counts forward from the whole second.
        let early = Instant::parse("1969-12-31 23:59:59.999").unwrap();
        assert_eq!((early.seconds, early.nanos), (-1, 999_000_000));
        assert_eq!(early.format(3, false).unwrap(), "1969-12-31 23:59:59.999");
    }

    #[test]
    fn bad_text_is_refused_not_guessed() {
        for (text, error) in [
            ("", DateTimeError::Syntax),
            ("yesterday", DateTimeError::Syntax),
            ("2020-01-02 3:04:05", DateTimeError::Syntax),
            ("2020-01-02 03:04", DateTimeError::Syntax),
            ("2020-01-02 03:04:05.", DateTimeError::Syntax),
            ("2020-01-02 03:04:05 CET", DateTimeError::Syntax),
            ("2020-01-02 03:04:05 +9", DateTimeError::Syntax),
            ("2020-01-02 24:00:00", DateTimeError::Calendar),
            ("2020-01-02 03:60:00", DateTimeError::Calendar),
            ("2020-01-02 03:04:60", DateTimeError::Calendar),
            ("2020-02-30 00:00:00", DateTimeError::Calendar),
            ("2020-01-02 03:04:05+24:00", DateTimeError::Calendar),
        ] {
            assert_eq!(Instant::parse(text), Err(error), "{text:?}");
        }
    }

    #[test]
    fn the_ranges_of_the_column_types_are_checked_here() {
        let at = |text: &str| Instant::parse(text).unwrap();
        // DateTime is an unsigned 32-bit second count; the server would saturate.
        assert_eq!(at("1970-01-01 00:00:00").datetime_seconds(), Ok(0));
        assert_eq!(at("2106-02-07 06:28:15").datetime_seconds(), Ok(u32::MAX));
        assert_eq!(
            at("2106-02-07 06:28:16").datetime_seconds(),
            Err(DateTimeError::Range)
        );
        assert_eq!(
            at("1969-12-31 23:59:59").datetime_seconds(),
            Err(DateTimeError::Range)
        );
        assert_eq!(
            at("2020-01-02 03:04:05.5").datetime_seconds(),
            Err(DateTimeError::Precision)
        );
        // DateTime64(9) is an Int64 of nanoseconds: 1677-09-21 .. 2262-04-11.
        assert!(at("2262-04-11 23:47:16.854775807")
            .check_datetime64(9)
            .is_ok());
        assert_eq!(
            at("2262-04-11 23:47:16.854775808").check_datetime64(9),
            Err(DateTimeError::Range)
        );
        assert!(at("1900-01-01 00:00:00").check_datetime64(9).is_ok());
        assert!(at("2299-12-31 23:59:59.999").check_datetime64(3).is_ok());
        // Epoch ticks, also before 1970 where the fraction counts forward.
        assert_eq!(
            at("2020-01-02 03:04:05.123").epoch_ticks(3),
            Ok(1_577_934_245_123)
        );
        assert_eq!(at("1969-12-31 23:59:59.999999").epoch_ticks(6), Ok(-1));
        assert_eq!(at("1969-12-31 23:59:58.5").epoch_ticks(3), Ok(-1_500));
        assert_eq!(
            at("2020-01-02 03:04:05.123456789").epoch_ticks(9),
            Ok(1_577_934_245_123_456_789)
        );
        assert_eq!(
            at("2262-04-11 23:47:16.854775808").epoch_ticks(9),
            Err(DateTimeError::Range)
        );
    }
}
