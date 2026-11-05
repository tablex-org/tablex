//! Footer statistics for the current grid selection (SUM / AVG / MIN / MAX over
//! numeric cells plus cell and row counts). Pure so it can be tested without a
//! grid.
//!
//! Integers and decimals are added as exact scaled integers (`i128` mantissa,
//! decimal scale), so `9007199254740993 + 1` is `9007199254740994` and a
//! `numeric(30,2)` column sums digit for digit. A value in a float column, a
//! result that overflows `i128` and an average that does not divide evenly are
//! `f64` or rounded: those are written with a leading `≈`.

use std::cmp::Ordering;

/// Selections larger than this are counted but not summed, keeping the
/// once-per-selection-change computation bounded.
pub const MAX_STATS_CELLS: usize = 100_000;

/// Fractional digits of an average that does not divide evenly.
const AVERAGE_SCALE: u32 = 6;

/// How a column's cells take part in the statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatColumn {
    /// Text, identifiers, binary: never numeric, whatever the content.
    Skip,
    /// Integers and decimals: exact.
    Exact,
    /// `real` / `double`: the stored value is a binary float, so the result is
    /// approximate.
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionStats {
    pub rows: usize,
    pub cells: usize,
    /// `None` when the selection exceeded [`MAX_STATS_CELLS`].
    pub numeric: Option<NumericStats>,
}

/// An exact decimal: `mantissa * 10^-scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decimal {
    mantissa: i128,
    scale: u32,
}

impl Decimal {
    /// `[+-]digits[.digits]`; `None` for an exponent, a special value or more
    /// digits than an `i128` holds.
    fn parse(text: &str) -> Option<Self> {
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (integral, fractional) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        if (integral.is_empty() && fractional.is_empty())
            || !integral
                .bytes()
                .chain(fractional.bytes())
                .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let mut mantissa: i128 = 0;
        for digit in integral.bytes().chain(fractional.bytes()) {
            mantissa = mantissa
                .checked_mul(10)?
                .checked_add(i128::from(digit - b'0'))?;
        }
        Some(Self {
            mantissa: if negative { -mantissa } else { mantissa },
            scale: u32::try_from(fractional.len()).ok()?,
        })
    }

    /// Both mantissas at the larger scale; `None` when that overflows.
    fn aligned(self, other: Self) -> Option<(i128, i128, u32)> {
        let scale = self.scale.max(other.scale);
        let lift = |value: Self| {
            10i128
                .checked_pow(scale - value.scale)
                .and_then(|factor| value.mantissa.checked_mul(factor))
        };
        Some((lift(self)?, lift(other)?, scale))
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        let (left, right, scale) = self.aligned(other)?;
        Some(Self {
            mantissa: left.checked_add(right)?,
            scale,
        })
    }

    fn compare(self, other: Self) -> Option<Ordering> {
        let (left, right, _) = self.aligned(other)?;
        Some(left.cmp(&right))
    }

    /// `self / count` to [`AVERAGE_SCALE`] digits (or its own scale if
    /// larger), and whether that is the exact quotient.
    fn average(self, count: usize) -> Option<(Self, bool)> {
        let count = i128::try_from(count).ok().filter(|count| *count > 0)?;
        let scale = self.scale.max(AVERAGE_SCALE);
        let numerator = self
            .mantissa
            .checked_mul(10i128.checked_pow(scale - self.scale)?)?;
        let (mut quotient, remainder) = (numerator / count, numerator % count);
        let exact = remainder == 0;
        if !exact && remainder.unsigned_abs().checked_mul(2)? >= count.unsigned_abs() {
            quotient += numerator.signum();
        }
        Some((
            Self {
                mantissa: quotient,
                scale,
            },
            exact,
        ))
    }

    /// Plain digits without trailing fractional zeros.
    fn text(self) -> String {
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (integral, fractional) = padded.split_at(padded.len() - scale);
        let fractional = fractional.trim_end_matches('0');
        let sign = if self.mantissa < 0 { "-" } else { "" };
        if fractional.is_empty() {
            format!("{sign}{integral}")
        } else {
            format!("{sign}{integral}.{fractional}")
        }
    }
}

/// The running sum, extremes and count of a selection. `exact` holds while
/// every value was added as an exact decimal without overflow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NumericStats {
    pub count: usize,
    exact: bool,
    sum: Decimal,
    min: Decimal,
    max: Decimal,
    float_sum: f64,
    float_min: f64,
    float_max: f64,
}

impl NumericStats {
    fn new() -> Self {
        let zero = Decimal {
            mantissa: 0,
            scale: 0,
        };
        Self {
            count: 0,
            exact: true,
            sum: zero,
            min: zero,
            max: zero,
            float_sum: 0.0,
            float_min: f64::INFINITY,
            float_max: f64::NEG_INFINITY,
        }
    }

    fn add(&mut self, text: &str, float: f64, column: StatColumn) {
        let decimal = (column == StatColumn::Exact)
            .then(|| Decimal::parse(text))
            .flatten();
        self.float_sum += float;
        self.float_min = self.float_min.min(float);
        self.float_max = self.float_max.max(float);
        if self.exact {
            self.exact = decimal.is_some_and(|value| self.add_exact(value));
        }
        self.count += 1;
    }

    /// Folds `value` into the exact sum and extremes; `false` on overflow.
    fn add_exact(&mut self, value: Decimal) -> bool {
        let first = self.count == 0;
        let (Some(sum), Some(min), Some(max)) = (
            self.sum.checked_add(value),
            if first {
                Some(Ordering::Greater)
            } else {
                value.compare(self.min)
            },
            if first {
                Some(Ordering::Less)
            } else {
                value.compare(self.max)
            },
        ) else {
            return false;
        };
        self.sum = sum;
        if min == Ordering::Less || first {
            self.min = value;
        }
        if max == Ordering::Greater || first {
            self.max = value;
        }
        true
    }

    fn label(&self, exact: Option<Decimal>, float: f64) -> String {
        match exact.filter(|_| self.exact) {
            Some(value) => value.text(),
            None => format!("≈{}", format_number(float)),
        }
    }
}

impl SelectionStats {
    /// `cell(row, column)` returns the cell text (offsets inside the
    /// selection), or `None` for NULL / unavailable cells. `columns_kinds`
    /// says, per selected column, how its cells count.
    pub fn compute(
        rows: usize,
        columns: usize,
        column_kinds: &[StatColumn],
        mut cell: impl FnMut(usize, usize) -> Option<String>,
    ) -> Self {
        let cells = rows.saturating_mul(columns);
        if cells == 0 || cells > MAX_STATS_CELLS {
            return Self {
                rows,
                cells,
                numeric: None,
            };
        }
        let mut numeric = NumericStats::new();
        for row in 0..rows {
            for column in 0..columns {
                let kind = column_kinds
                    .get(column)
                    .copied()
                    .unwrap_or(StatColumn::Exact);
                if kind == StatColumn::Skip {
                    continue;
                }
                let Some(text) = cell(row, column) else {
                    continue;
                };
                if let Some(value) = parse_numeric(&text) {
                    numeric.add(text.trim(), value, kind);
                }
            }
        }
        Self {
            rows,
            cells,
            numeric: Some(numeric),
        }
    }

    pub fn sum_label(&self) -> String {
        match &self.numeric {
            Some(numeric) => numeric.label(Some(numeric.sum), numeric.float_sum),
            None => "—".into(),
        }
    }

    pub fn avg_label(&self) -> String {
        let Some(numeric) = self.numeric.as_ref().filter(|numeric| numeric.count > 0) else {
            return "—".into();
        };
        let average = numeric.float_sum / numeric.count as f64;
        if !numeric.exact {
            return format!("≈{}", format_number(average));
        }
        match numeric.sum.average(numeric.count) {
            Some((value, true)) => value.text(),
            Some((value, false)) => format!("≈{}", value.text()),
            None => format!("≈{}", format_number(average)),
        }
    }

    pub fn min_label(&self) -> String {
        match self.numeric.as_ref().filter(|numeric| numeric.count > 0) {
            Some(numeric) => numeric.label(Some(numeric.min), numeric.float_min),
            None => "—".into(),
        }
    }

    pub fn max_label(&self) -> String {
        match self.numeric.as_ref().filter(|numeric| numeric.count > 0) {
            Some(numeric) => numeric.label(Some(numeric.max), numeric.float_max),
            None => "—".into(),
        }
    }

    /// `SUM: 12  AVG: 4  MIN: 1  MAX: 7  3 cells 1 rows`. The numbers are
    /// omitted when the selection has no numeric cells; oversized selections
    /// (not summed at all) show dashes instead. `≈` marks a result that is
    /// not exact.
    pub fn label(&self) -> String {
        if self.numeric.is_some_and(|numeric| numeric.count == 0) {
            return format!("{} cells {} rows", self.cells, self.rows);
        }
        format!(
            "SUM: {}  AVG: {}  MIN: {}  MAX: {}  {} cells {} rows",
            self.sum_label(),
            self.avg_label(),
            self.min_label(),
            self.max_label(),
            self.cells,
            self.rows
        )
    }
}

/// Finite decimal/scientific numbers only; text such as `inf`, `NaN`, `0x10`
/// or thousands separators is not numeric.
pub fn parse_numeric(text: &str) -> Option<f64> {
    let text = text.trim();
    let first = text.as_bytes().first()?;
    if !(first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.')) {
        return None;
    }
    // Zero-padded digit runs (`007`, ObjectId-like hex ids) are identifiers.
    let digits = text.trim_start_matches(['-', '+']);
    if digits.len() > 1 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit() {
        return None;
    }
    text.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn format_number(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let text = format!("{value:.6}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_of(kinds: &[StatColumn], values: &[&[Option<&str>]]) -> SelectionStats {
        SelectionStats::compute(values.len(), values[0].len(), kinds, |row, column| {
            values[row][column].map(str::to_string)
        })
    }

    fn grid(values: &[&[Option<&str>]]) -> SelectionStats {
        stats_of(&[], values)
    }

    fn column(values: &[&str]) -> SelectionStats {
        let rows: Vec<[Option<&str>; 1]> = values.iter().map(|value| [Some(*value)]).collect();
        let refs: Vec<&[Option<&str>]> = rows.iter().map(|row| row.as_slice()).collect();
        grid(&refs)
    }

    #[test]
    fn sums_and_averages_numeric_cells_only() {
        let stats = grid(&[
            &[Some("1"), Some("abc"), Some("2.5")],
            &[None, Some("-3"), Some("100")],
        ]);

        assert_eq!((stats.rows, stats.cells), (2, 6));
        assert_eq!(stats.numeric.map(|n| n.count), Some(4));
        assert_eq!(stats.sum_label(), "100.5");
        assert_eq!(stats.avg_label(), "25.125");
        assert_eq!(stats.min_label(), "-3");
        assert_eq!(stats.max_label(), "100");
        assert_eq!(
            stats.label(),
            "SUM: 100.5  AVG: 25.125  MIN: -3  MAX: 100  6 cells 2 rows"
        );
    }

    #[test]
    fn no_numeric_cells_hides_sum_and_average() {
        let stats = grid(&[&[Some("text")]]);

        assert_eq!(stats.sum_label(), "0");
        assert_eq!(stats.avg_label(), "—");
        assert_eq!(stats.label(), "1 cells 1 rows");
        // NULL-only and empty-text selections are text-only too.
        assert_eq!(grid(&[&[None, Some("")]]).label(), "2 cells 1 rows");
    }

    #[test]
    fn non_finite_and_prefixed_text_is_not_numeric() {
        for text in [
            "inf", "NaN", "infinity", "0x10", "1,000", "", "  ", "e5", "12abc",
        ] {
            assert_eq!(parse_numeric(text), None, "{text:?}");
        }
        assert_eq!(parse_numeric("000000000000000000000002"), None);
        assert_eq!(parse_numeric("-007"), None);
        assert_eq!(parse_numeric("0.5"), Some(0.5));
        assert_eq!(parse_numeric("0"), Some(0.0));
        assert_eq!(parse_numeric(" 42 "), Some(42.0));
        assert_eq!(parse_numeric("+.5"), Some(0.5));
    }

    #[test]
    fn oversized_selections_are_counted_but_not_summed() {
        let mut visited = 0usize;
        let stats = SelectionStats::compute(1_000, 101, &[], |_, _| {
            visited += 1;
            Some("1".into())
        });

        assert_eq!(visited, 0);
        assert_eq!(stats.cells, 101_000);
        assert_eq!(stats.numeric, None);
        assert_eq!(
            stats.label(),
            "SUM: —  AVG: —  MIN: —  MAX: —  101000 cells 1000 rows"
        );
    }

    #[test]
    fn sums_stay_readable_for_fractions() {
        assert_eq!(column(&["0.1", "0.2"]).sum_label(), "0.3");
        assert_eq!(column(&["1", "2"]).avg_label(), "1.5");
    }

    #[test]
    fn integers_above_two_to_the_53rd_add_exactly() {
        let stats = column(&["9007199254740993", "1"]);
        assert_eq!(stats.sum_label(), "9007199254740994");
        assert_eq!(stats.avg_label(), "4503599627370497");
        assert_eq!(stats.min_label(), "1");
        assert_eq!(stats.max_label(), "9007199254740993");
        let wide = column(&["18446744073709551615", "18446744073709551615"]);
        assert_eq!(wide.sum_label(), "36893488147419103230");
    }

    #[test]
    fn decimal_text_adds_digit_for_digit() {
        let stats = column(&["12345678901234567890.25", "0.75", "-0.5"]);
        assert_eq!(stats.sum_label(), "12345678901234567890.5");
        assert_eq!(stats.min_label(), "-0.5");
        assert_eq!(stats.max_label(), "12345678901234567890.25");
        // 30 digits (a numeric(30,0) key) survive.
        assert_eq!(
            column(&["123456789012345678901234567890", "1"]).sum_label(),
            "123456789012345678901234567891"
        );
        // 0.1 + 0.2 is 0.3 here, not 0.30000000000000004.
        assert_eq!(column(&["0.1", "0.2"]).sum_label(), "0.3");
        assert_eq!(column(&["1.10", "2.20"]).sum_label(), "3.3");
    }

    #[test]
    fn an_average_that_does_not_divide_evenly_is_marked_approximate() {
        assert_eq!(column(&["5", "10", "20"]).avg_label(), "≈11.666667");
        assert_eq!(column(&["1", "2"]).avg_label(), "1.5");
        assert_eq!(column(&["-5", "-10", "-20"]).avg_label(), "≈-11.666667");
        assert_eq!(
            column(&["1", "1", "1", "1", "1", "1", "1", "2"]).avg_label(),
            "1.125"
        );
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        // i128 holds 38 digits: twice that sum cannot stay exact.
        let big = "99999999999999999999999999999999999999";
        let stats = column(&[big, big]);
        assert!(stats.sum_label().starts_with('≈'), "{}", stats.sum_label());
        assert!(stats.avg_label().starts_with('≈'), "{}", stats.avg_label());
        // More digits than an i128 reads at all.
        let huge = "1".repeat(50);
        let stats = column(&[huge.as_str(), "1"]);
        assert!(stats.sum_label().starts_with('≈'), "{}", stats.sum_label());
        assert!(!stats.sum_label().contains('-'), "never a wrapped negative");
    }

    #[test]
    fn floats_and_exponents_are_marked_approximate() {
        let floats = stats_of(&[StatColumn::Float], &[&[Some("0.1")], &[Some("0.2")]]);
        assert_eq!(floats.sum_label(), "≈0.3");
        assert_eq!(floats.min_label(), "≈0.1");
        assert!(floats.label().starts_with("SUM: ≈0.3  AVG: ≈0.15"));
        // A float column's whole numbers are float values too.
        assert_eq!(
            stats_of(&[StatColumn::Float], &[&[Some("2")], &[Some("3")]]).sum_label(),
            "≈5"
        );
        // Exponent text is a float literal wherever it appears.
        assert_eq!(column(&["1e2", "5"]).sum_label(), "≈105");
        // Exact columns next to float ones: the result is approximate.
        let mixed = stats_of(
            &[StatColumn::Exact, StatColumn::Float],
            &[&[Some("1"), Some("2")]],
        );
        assert_eq!(mixed.sum_label(), "≈3");
    }

    #[test]
    fn skipped_columns_never_count() {
        let stats = stats_of(
            &[StatColumn::Skip, StatColumn::Exact],
            &[&[Some("100"), Some("1")], &[Some("200"), Some("2")]],
        );
        assert_eq!(stats.numeric.map(|n| n.count), Some(2));
        assert_eq!(stats.sum_label(), "3");
    }
}
