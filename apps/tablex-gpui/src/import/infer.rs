//! What the text cells of an import file mean: the number and date spellings the user said the file
//! uses ([`Hints`]), and the type a new table gives a column after its sample ([`Inferred`]).

use std::borrow::Cow;

use chrono::{DateTime, NaiveDate, NaiveDateTime};

/// The spellings of numbers and dates in the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hints {
    /// Numbers read `1.234,56`: the comma is the decimal separator and the dots group thousands.
    pub decimal_comma: bool,
    /// How dates are written (`%d/%m/%Y`, strftime). Empty: ISO (`2024-03-31`).
    pub date_format: Option<String>,
    /// How timestamps are written (`%d/%m/%Y %H:%M`). Empty: ISO.
    pub timestamp_format: Option<String>,
    /// A timestamp that carries a zone offset goes to a column that has no zone as the UTC wall clock (a data transfer: the
    /// servers differ in what they make of an offset in a plain `datetime`, and the instant must not move).
    pub offsets_to_utc: bool,
    /// A date or timestamp that does not parse (`0000-00-00`, `infinity`, a zone offset with seconds) is passed on as it is
    /// for the server to read: the transfer is between servers of one dialect, which wrote it.
    pub passthrough_temporal: bool,
}

impl Hints {
    /// `text` as a plain number (`1234.56`): the grouping gone, the decimal separator a point. With a decimal comma
    /// a dot is only a thousands separator, between groups of exactly three digits (`1.234.567,5`); any other
    /// spelling (`1.5`, `12.34`, `1,234.5`) is not a number in such a file: `None`.
    pub fn number<'a>(&self, text: &'a str) -> Option<Cow<'a, str>> {
        let text = text.trim();
        if !self.decimal_comma {
            return Some(Cow::Borrowed(text));
        }
        let (sign, unsigned) = match text.strip_prefix(['+', '-']) {
            Some(rest) => (&text[..1], rest),
            None => ("", text),
        };
        let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
            Some(at) => (&unsigned[..at], &unsigned[at..]),
            None => (unsigned, ""),
        };
        let (whole, fraction) = match mantissa.split_once(',') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (mantissa, None),
        };
        if fraction.is_some_and(|fraction| fraction.contains(['.', ','])) {
            return None;
        }
        let mut groups = whole.split('.');
        let first = groups.next().unwrap_or("");
        let grouped = groups.clone().next().is_some();
        if grouped && (first.is_empty() || first.len() > 3 || groups.any(|group| group.len() != 3)) {
            return None;
        }
        let digits = whole.replace('.', "");
        Some(Cow::Owned(match fraction {
            Some(fraction) => format!("{sign}{digits}.{fraction}{exponent}"),
            None => format!("{sign}{digits}{exponent}"),
        }))
    }

    /// Whether a timestamp format reads a zone offset (`%z`, `%:z`, `%::z`, `%#z`).
    fn has_offset(format: &str) -> bool {
        ["%z", "%:z", "%::z", "%#z"].iter().any(|offset| format.contains(offset))
    }

    /// What is wrong with the timestamp format itself, if anything: a zone *name* (`%Z`) cannot be read, and the
    /// offset it stands for would be lost.
    pub fn timestamp_format_problem(&self) -> Option<&'static str> {
        let format = self.timestamp_format.as_deref()?;
        format.contains("%Z").then_some("The timestamp format cannot use %Z (a zone name): use %z for an offset such as +0200")
    }

    /// `text` as an ISO date, or `None` when it is not a date in the file's spelling.
    pub fn date(&self, text: &str) -> Option<String> {
        let text = text.trim();
        match self.date_format.as_deref() {
            Some(format) => NaiveDate::parse_from_str(text, format).ok().map(|date| date.format("%Y-%m-%d").to_string()),
            None => NaiveDate::parse_from_str(text, "%Y-%m-%d").ok().map(|_| text.to_owned()),
        }
    }

    /// A timestamp with a zone offset as the UTC wall clock (`2024-02-29 11:14:15.5`); `None` for one without an offset.
    pub fn utc_naive(text: &str) -> Option<String> {
        let text = text.trim();
        let moment = ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"]
            .iter()
            .find_map(|format| DateTime::parse_from_str(text, format).ok())
            .or_else(|| DateTime::parse_from_rfc3339(text).ok())?;
        Some(moment.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string())
    }

    /// `text` as a timestamp the servers read (ISO, a zone offset kept), or `None` when it is not one in the
    /// file's spelling. A bare date is a timestamp at midnight.
    pub fn timestamp(&self, text: &str) -> Option<String> {
        let text = text.trim();
        if let Some(format) = self.timestamp_format.as_deref() {
            // A format with a zone offset keeps it: read as a plain date and time the offset would be dropped.
            if Self::has_offset(format) {
                return DateTime::parse_from_str(text, format).ok().map(|moment| moment.format("%Y-%m-%d %H:%M:%S%.f%:z").to_string());
            }
            if let Ok(moment) = NaiveDateTime::parse_from_str(text, format) {
                return Some(moment.format("%Y-%m-%d %H:%M:%S%.f").to_string());
            }
            return NaiveDate::parse_from_str(text, format).ok().map(|date| date.format("%Y-%m-%d").to_string());
        }
        let local = ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"];
        let zoned = ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"];
        let valid = local.iter().any(|format| NaiveDateTime::parse_from_str(text, format).is_ok())
            || zoned.iter().any(|format| DateTime::parse_from_str(text, format).is_ok())
            || DateTime::parse_from_rfc3339(text).is_ok()
            || NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok();
        valid.then(|| text.to_owned())
    }
}

/// A column type, as far as a sample of text tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inferred {
    Integer,
    /// Exact digits with a decimal point.
    Decimal,
    /// Written with an exponent (`1.5e3`): a floating point value.
    Float,
    Boolean,
    Date,
    Timestamp,
    Text,
}

/// A whole number that keeps its spelling when read back: no leading zeros (zip codes, ids are text).
fn is_integer(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && (digits.len() == 1 || !digits.starts_with('0'))
        && text.parse::<i64>().is_ok()
}

/// Digits with a point and an optional exponent (`1.5`, `.5`, `1e3`, `-2.5E-3`).
fn is_decimal_or_float(text: &str, exponents: bool) -> bool {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    let leading_zero = whole.len() > 1 && whole.starts_with('0');
    digits(whole)
        && digits(fraction)
        && !(whole.is_empty() && fraction.is_empty())
        && !leading_zero
        && (exponents || exponent.is_none())
        && exponent.is_none_or(|exponent| {
            let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn is_boolean_word(text: &str) -> bool {
    ["true", "false", "t", "f", "yes", "no"].iter().any(|word| text.eq_ignore_ascii_case(word))
}

impl Inferred {
    /// The narrowest type every non-NULL cell fits; a column with no value is text.
    pub fn of<'a>(cells: impl Iterator<Item = Option<&'a str>>, hints: &Hints) -> Self {
        let mut integer = true;
        let mut decimal = true;
        let mut float = true;
        let mut boolean = true;
        let mut date = true;
        let mut timestamp = true;
        let mut any = false;
        for cell in cells.flatten() {
            let cell = cell.trim();
            if cell.is_empty() {
                continue;
            }
            any = true;
            let number = hints.number(cell);
            integer &= number.as_deref().is_some_and(is_integer);
            decimal &= number.as_deref().is_some_and(|number| is_decimal_or_float(number, false));
            float &= number.as_deref().is_some_and(|number| is_decimal_or_float(number, true));
            boolean &= is_boolean_word(cell);
            date &= hints.date(cell).is_some();
            timestamp &= hints.timestamp(cell).is_some();
            if !(integer || decimal || float || boolean || date || timestamp) {
                return Self::Text;
            }
        }
        if !any {
            Self::Text
        } else if integer {
            Self::Integer
        } else if decimal {
            Self::Decimal
        } else if float {
            Self::Float
        } else if boolean {
            Self::Boolean
        } else if date {
            Self::Date
        } else if timestamp {
            Self::Timestamp
        } else {
            Self::Text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer(cells: &[Option<&str>]) -> Inferred {
        Inferred::of(cells.iter().copied(), &Hints::default())
    }

    #[test]
    fn the_narrowest_type_that_fits_every_cell() {
        assert_eq!(infer(&[Some("1"), Some("-20"), None, Some("+3")]), Inferred::Integer);
        assert_eq!(infer(&[Some("1"), Some("2.5"), Some("12.50")]), Inferred::Decimal);
        assert_eq!(infer(&[Some("1"), Some("2.5"), Some("1e3")]), Inferred::Float, "an exponent is a floating point value");
        assert_eq!(infer(&[Some("2.5E-3")]), Inferred::Float);
        assert_eq!(infer(&[Some("true"), Some("No"), Some("T")]), Inferred::Boolean);
        assert_eq!(infer(&[Some("2024-02-29"), Some("1999-12-31")]), Inferred::Date);
        assert_eq!(infer(&[Some("2024-02-29 10:30:00"), Some("2024-03-01T08:00:00.25Z")]), Inferred::Timestamp);
        assert_eq!(infer(&[Some("2024-02-29"), Some("2024-03-01 10:00:00")]), Inferred::Timestamp);
        assert_eq!(infer(&[Some("1"), Some("two")]), Inferred::Text);
    }

    #[test]
    fn zero_one_columns_are_integers_and_empty_columns_are_text() {
        assert_eq!(infer(&[Some("0"), Some("1")]), Inferred::Integer);
        assert_eq!(infer(&[None, Some(""), Some("  ")]), Inferred::Text);
        assert_eq!(infer(&[]), Inferred::Text);
    }

    #[test]
    fn identifiers_with_leading_zeros_are_text_and_oversized_integers_are_decimals() {
        assert_eq!(infer(&[Some("007"), Some("12")]), Inferred::Text);
        assert_eq!(infer(&[Some("01234")]), Inferred::Text);
        assert_eq!(infer(&[Some("0"), Some("0.5")]), Inferred::Decimal);
        // Too big for 64 bits, still an exact number.
        assert_eq!(infer(&[Some("12345678901234567890")]), Inferred::Decimal);
    }

    #[test]
    fn an_impossible_date_is_text() {
        assert_eq!(infer(&[Some("2024-02-30")]), Inferred::Text);
        assert_eq!(infer(&[Some("2024-13-01")]), Inferred::Text);
    }

    #[test]
    fn the_hints_read_the_files_own_spellings() {
        let hints = Hints { decimal_comma: true, date_format: Some("%d/%m/%Y".into()), timestamp_format: Some("%d/%m/%Y %H:%M".into()), ..Hints::default() };
        assert_eq!(Inferred::of([Some("1.234,5"), Some("12")].into_iter(), &hints), Inferred::Decimal);
        assert_eq!(Inferred::of([Some("31/12/2024")].into_iter(), &hints), Inferred::Date);
        assert_eq!(hints.date("31/12/2024").as_deref(), Some("2024-12-31"));
        assert_eq!(hints.timestamp("31/12/2024 23:59").as_deref(), Some("2024-12-31 23:59:00"));
        assert_eq!(hints.number("1.234,56").as_deref(), Some("1234.56"));
        assert_eq!(hints.date("2024-12-31"), None);
    }

    #[test]
    fn a_decimal_comma_file_accepts_dots_only_as_groups_of_three_digits() {
        let hints = Hints { decimal_comma: true, ..Hints::default() };
        for (text, plain) in [("1.234,56", "1234.56"), ("-1.234.567,5", "-1234567.5"), ("12,5", "12.5"), ("1234", "1234"), ("999.999", "999999"), ("1,5e3", "1.5e3"), ("+7", "+7")] {
            assert_eq!(hints.number(text).as_deref(), Some(plain), "{text}");
        }
        for text in ["1.5", "12.34", "1.2345", "1,234.5", ".123", "1.23.456", "1,2,3", "1.234,5.6", "1234.567"] {
            assert_eq!(hints.number(text), None, "{text} is not a number here");
        }
        // Without the hint the text is as written.
        assert_eq!(Hints::default().number(" 1.5 ").as_deref(), Some("1.5"));
        assert_eq!(Inferred::of([Some("1.5")].into_iter(), &hints), Inferred::Text);
    }

    #[test]
    fn a_timestamp_format_with_an_offset_keeps_the_offset_and_a_zone_name_is_refused() {
        let hints = Hints { timestamp_format: Some("%d/%m/%Y %H:%M %z".into()), ..Hints::default() };
        assert_eq!(hints.timestamp("31/12/2024 23:59 +0200").as_deref(), Some("2024-12-31 23:59:00+02:00"));
        assert_eq!(hints.timestamp("31/12/2024 23:59"), None, "an offset is required when the format reads one");
        assert_eq!(hints.timestamp_format_problem(), None);
        let named = Hints { timestamp_format: Some("%d/%m/%Y %H:%M %Z".into()), ..Hints::default() };
        assert!(named.timestamp_format_problem().is_some_and(|problem| problem.contains("%Z")));
        assert_eq!(Hints::default().timestamp_format_problem(), None);
    }

    #[test]
    fn iso_text_goes_through_as_written_so_a_zone_offset_survives() {
        let hints = Hints::default();
        assert_eq!(hints.timestamp("2024-03-01T08:00:00+02:00").as_deref(), Some("2024-03-01T08:00:00+02:00"));
        assert_eq!(hints.timestamp("2024-03-01 08:00:00.123456").as_deref(), Some("2024-03-01 08:00:00.123456"));
        assert_eq!(hints.timestamp("yesterday"), None);
    }
}
