//! Values written to a column: the text the grid shows (or the JSON a client
//! sends) becomes the exact value of the column's ClickHouse type, or is refused
//! with `mutation.invalid_value` / `mutation.value_not_editable` before anything
//! is sent.
//!
//! The server is not trusted to refuse: it wraps `300` into a `UInt8` (44),
//! truncates `1.239` into a `Decimal(9, 2)` (1.23) and saturates a `DateTime`
//! past 2106, all without an error. So every kind is validated here, against its
//! exact range, **without going through `f64` or `i64`**: wide integers and
//! decimals stay digit strings, and the value reaches the server as the
//! canonical text of the same number.
//!
//! * integers (`Int8` ... `UInt256`): `[+-]digits`, any leading zeros, in range;
//! * `Float32` / `Float64`: a number, or `nan`, `inf`, `-inf` (also `NaN`,
//!   `Infinity`, `-Infinity`, `+inf`); a finite text that overflows is refused;
//! * `Decimal(P, S)`: `[+-]digits[.digits]`, at most `S` significant fraction
//!   digits and `P - S` integer digits (trailing zeros past `S` are dropped);
//! * `Date` / `Date32`: `YYYY-MM-DD`;
//! * `DateTime` / `DateTime64(p)`: an instant, see [`crate::datetime`]. It is
//!   bound as a UTC value whatever the column's zone: a `DateTime` as
//!   `{p:DateTime('UTC')}`, a `DateTime64` as an `Int64` tick count run through
//!   `fromUnixTimestamp64Milli` / `Micro` / `Nano` (a `DateTime64` *text*
//!   parameter breaks inside an `ALTER ... UPDATE`, see [`Target`]);
//! * `Enum8` / `Enum16`: the entry **name**;
//! * `Bool`: `true` / `false` (also `t`, `f`, `1`, `0`);
//! * everything else textual (`String`, `FixedString`, `UUID`, `IPv4` ...): the
//!   text as it is. `0x...` is *text* here: a `String` column may well hold
//!   `0xdeadbeef` as text, so the driver never turns it into bytes on a value
//!   write (see `mutation` for row keys);
//! * `Array`, `Map`, `Tuple`, `JSON`, `Variant`, `Dynamic`: no text the grid can
//!   show round-trips (`['a, b']` and `['a', 'b']` look alike), so updates and
//!   keys are refused with `mutation.value_not_editable`; an insert takes native
//!   JSON.

use crate::datetime::{self, DateTimeError, Instant};
use crate::types::ChType;
use engine_driver_kit::api::sql::codes;
use engine_driver_kit::BackendError;
use serde_json::Value;
use std::fmt;

/// How the text of a value is read for a column type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    Bool,
    Int {
        bits: u16,
        signed: bool,
    },
    Float {
        single: bool,
    },
    Decimal {
        precision: u32,
        scale: u32,
    },
    Date,
    Date32,
    DateTime,
    DateTime64 {
        precision: u32,
    },
    Enum(Vec<String>),
    Text,
    /// `Array`, `Map`, `Tuple`, `JSON` ...: native JSON on insert, never edited as text.
    Composite,
    /// Aggregate states, `Nothing`: cannot be written as text at all.
    Opaque,
}

impl Kind {
    /// The kind of a column of type `ty` (`Nullable`, `LowCardinality` peeled).
    pub(crate) fn of(ty: &ChType) -> Self {
        let peeled = ty.peeled();
        let literal = |index: usize| {
            peeled
                .literal(index)
                .and_then(|text| text.parse::<u32>().ok())
        };
        match peeled.name() {
            "Bool" => Self::Bool,
            "Int8" => Self::Int {
                bits: 8,
                signed: true,
            },
            "Int16" => Self::Int {
                bits: 16,
                signed: true,
            },
            "Int32" => Self::Int {
                bits: 32,
                signed: true,
            },
            "Int64" => Self::Int {
                bits: 64,
                signed: true,
            },
            "Int128" => Self::Int {
                bits: 128,
                signed: true,
            },
            "Int256" => Self::Int {
                bits: 256,
                signed: true,
            },
            "UInt8" => Self::Int {
                bits: 8,
                signed: false,
            },
            "UInt16" => Self::Int {
                bits: 16,
                signed: false,
            },
            "UInt32" => Self::Int {
                bits: 32,
                signed: false,
            },
            "UInt64" => Self::Int {
                bits: 64,
                signed: false,
            },
            "UInt128" => Self::Int {
                bits: 128,
                signed: false,
            },
            "UInt256" => Self::Int {
                bits: 256,
                signed: false,
            },
            "Float32" => Self::Float { single: true },
            "Float64" => Self::Float { single: false },
            "Decimal" => Self::Decimal {
                precision: literal(0).unwrap_or(10),
                scale: literal(1).unwrap_or(0),
            },
            "Decimal32" => Self::Decimal {
                precision: 9,
                scale: literal(0).unwrap_or(0),
            },
            "Decimal64" => Self::Decimal {
                precision: 18,
                scale: literal(0).unwrap_or(0),
            },
            "Decimal128" => Self::Decimal {
                precision: 38,
                scale: literal(0).unwrap_or(0),
            },
            "Decimal256" => Self::Decimal {
                precision: 76,
                scale: literal(0).unwrap_or(0),
            },
            "Date" => Self::Date,
            "Date32" => Self::Date32,
            "DateTime" => Self::DateTime,
            "DateTime64" => Self::DateTime64 {
                precision: literal(0).unwrap_or(3).min(9),
            },
            "Enum8" | "Enum16" => Self::Enum(
                peeled
                    .enum_entries()
                    .into_iter()
                    .map(|(_, name)| name)
                    .collect(),
            ),
            "Array" | "Map" | "Tuple" | "Nested" | "JSON" | "Object" | "Variant" | "Dynamic"
            | "Point" | "Ring" | "LineString" | "MultiLineString" | "Polygon" | "MultiPolygon" => {
                Self::Composite
            }
            "AggregateFunction" | "Nothing" => Self::Opaque,
            _ => Self::Text,
        }
    }

    /// Whether a value can be written as text at all (updates and keys).
    pub(crate) fn is_editable(&self) -> bool {
        !matches!(self, Self::Composite | Self::Opaque)
    }
}

/// Why a value was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Invalid {
    /// The JSON value has the wrong shape for the kind (an array for a number).
    Shape,
    /// Not a number / not text of this kind: the expected form.
    Form(&'static str),
    /// A well-formed number outside the type's range.
    Range(String),
    /// More fraction digits than the type keeps.
    Fraction(u32),
    /// Not one of the entries of the enum.
    NotAnEntry,
    DateTime(DateTimeError),
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape => f.write_str("is not a scalar value"),
            Self::Form(expected) => write!(f, "is not {expected}"),
            Self::Range(range) => write!(f, "does not fit the type ({range})"),
            Self::Fraction(scale) => write!(f, "has more than {scale} fraction digit(s)"),
            Self::NotAnEntry => f.write_str("is not one of the enum's entries (send the name)"),
            Self::DateTime(error) => write!(f, "{error}"),
        }
    }
}

impl From<DateTimeError> for Invalid {
    fn from(error: DateTimeError) -> Self {
        Self::DateTime(error)
    }
}

/// What a value turned into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scalar {
    Bool(bool),
    Text(String),
}

/// Where the value goes: a typed query parameter (`{name:Type}`) or a
/// `JSONEachRow` insert. Only date-times differ. An insert takes ISO text with
/// a `Z`, which the server's best-effort parser reads in UTC whatever the
/// column's zone. A `DateTime` parameter is plain text typed `DateTime('UTC')`;
/// a `DateTime64` parameter is an `Int64` count of ticks since the epoch,
/// turned into the instant by `fromUnixTimestamp64Milli` / `Micro` / `Nano`
/// (see [`Bound::expression`]): a `DateTime64` *text* parameter inside an
/// `ALTER ... UPDATE` is stored as a number and read back as a date-time text,
/// which the server cannot do for any instant before 2001-09-09.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Param,
    Json,
}

fn text_of(value: &Value) -> Result<String, Invalid> {
    match value {
        Value::String(text) => Ok(text.trim().to_owned()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(u8::from(*flag).to_string()),
        _ => Err(Invalid::Shape),
    }
}

/// Converts `value` (not `null`) to the exact value of `kind`.
pub(crate) fn coerce(kind: &Kind, value: &Value, target: Target) -> Result<Scalar, Invalid> {
    Ok(match kind {
        Kind::Bool => Scalar::Bool(boolean(value)?),
        Kind::Int { bits, signed } => Scalar::Text(integer(*bits, *signed, &text_of(value)?)?),
        Kind::Float { single } => Scalar::Text(float(*single, &text_of(value)?)?),
        Kind::Decimal { precision, scale } => {
            Scalar::Text(decimal(*precision, *scale, &text_of(value)?)?)
        }
        Kind::Date => {
            let days = datetime::parse_date(&text_of(value)?)?;
            if !(0..=65_535).contains(&days) {
                return Err(Invalid::Range("1970-01-01 to 2149-06-06".into()));
            }
            Scalar::Text(datetime::format_date(days)?)
        }
        Kind::Date32 => {
            let days = datetime::parse_date(&text_of(value)?)?;
            Scalar::Text(datetime::format_date(days)?)
        }
        Kind::DateTime => {
            let instant = Instant::parse(&text_of(value)?)?;
            instant.datetime_seconds()?;
            Scalar::Text(instant.format(0, target == Target::Json)?)
        }
        Kind::DateTime64 { precision } => {
            let instant = Instant::parse(&text_of(value)?)?;
            instant.check_datetime64(*precision)?;
            match target {
                Target::Json => Scalar::Text(instant.format(*precision, true)?),
                Target::Param => {
                    Scalar::Text(instant.epoch_ticks(epoch_scale(*precision))?.to_string())
                }
            }
        }
        Kind::Enum(names) => match value {
            Value::String(name) if names.contains(name) => Scalar::Text(name.clone()),
            Value::String(_) => return Err(Invalid::NotAnEntry),
            _ => return Err(Invalid::Shape),
        },
        Kind::Text => Scalar::Text(match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => u8::from(*flag).to_string(),
            Value::Array(_) | Value::Object(_) => value.to_string(),
            Value::Null => return Err(Invalid::Shape),
        }),
        Kind::Composite | Kind::Opaque => return Err(Invalid::Shape),
    })
}

fn boolean(value: &Value) -> Result<bool, Invalid> {
    const FORM: Invalid = Invalid::Form("true or false");
    match value {
        Value::Bool(flag) => Ok(*flag),
        Value::Number(number) => match number.as_u64() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(FORM),
        },
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Ok(true),
            "false" | "f" | "0" => Ok(false),
            _ => Err(FORM),
        },
        _ => Err(Invalid::Shape),
    }
}

// ---------------------------------------------------------------- integers

/// An unsigned integer of up to 256 bits, most significant limb first (so the
/// derived order is the numeric order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Magnitude([u64; 4]);

impl Magnitude {
    const ZERO: Self = Self([0; 4]);

    /// `self * 10 + digit`; `false` when it no longer fits 256 bits.
    fn push_digit(&mut self, digit: u8) -> bool {
        let mut carry = u128::from(digit);
        for limb in self.0.iter_mut().rev() {
            let value = u128::from(*limb) * 10 + carry;
            *limb = value as u64;
            carry = value >> 64;
        }
        carry == 0
    }

    /// `2^bit`, `bit < 256`.
    fn power_of_two(bit: u32) -> Self {
        let mut limbs = [0; 4];
        limbs[3 - (bit / 64) as usize] = 1 << (bit % 64);
        Self(limbs)
    }
}

/// `text` as the canonical digits of an integer of `bits` bits.
fn integer(bits: u16, signed: bool, text: &str) -> Result<String, Invalid> {
    const FORM: &str = "an integer";
    let (negative, digits) = match text.as_bytes() {
        [b'-', rest @ ..] => (true, rest),
        [b'+', rest @ ..] => (false, rest),
        rest => (false, rest),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(Invalid::Form(FORM));
    }
    let digits = std::str::from_utf8(digits).map_err(|_| Invalid::Form(FORM))?;
    let digits = digits.trim_start_matches('0');
    let mut magnitude = Magnitude::ZERO;
    let mut fits = true;
    for digit in digits.bytes() {
        fits &= magnitude.push_digit(digit - b'0');
    }
    let zero = magnitude == Magnitude::ZERO;
    fits &= match (signed, negative && !zero) {
        (false, true) => false,
        (false, false) => bits == 256 || magnitude < Magnitude::power_of_two(u32::from(bits)),
        (true, true) => magnitude <= Magnitude::power_of_two(u32::from(bits) - 1),
        (true, false) => magnitude < Magnitude::power_of_two(u32::from(bits) - 1),
    };
    if !fits {
        let range = if signed {
            format!("Int{bits}")
        } else {
            format!("UInt{bits}")
        };
        return Err(Invalid::Range(range));
    }
    Ok(match (zero, negative) {
        (true, _) => "0".to_owned(),
        (false, true) => format!("-{digits}"),
        (false, false) => digits.to_owned(),
    })
}

// ------------------------------------------------------------------ floats

/// `nan`, `inf` / `-inf`, or the shortest text of the float the text names.
fn float(single: bool, text: &str) -> Result<String, Invalid> {
    const FORM: Invalid = Invalid::Form("a number, nan, inf or -inf");
    let lower = text.to_ascii_lowercase();
    match lower.strip_prefix(['+', '-']).unwrap_or(&lower) {
        "nan" => return Ok("nan".to_owned()),
        "inf" | "infinity" => {
            return Ok(if lower.starts_with('-') {
                "-inf"
            } else {
                "inf"
            }
            .to_owned());
        }
        _ => {}
    }
    // Only plain decimal / exponent forms: Rust also reads `inf` and `nan`
    // spellings, which were handled above.
    if !lower
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e'))
    {
        return Err(FORM);
    }
    if single {
        let value: f32 = lower.parse().map_err(|_| FORM)?;
        if value.is_finite() {
            Ok(format!("{value:e}"))
        } else {
            Err(Invalid::Range("Float32".into()))
        }
    } else {
        let value: f64 = lower.parse().map_err(|_| FORM)?;
        if value.is_finite() {
            Ok(format!("{value:e}"))
        } else {
            Err(Invalid::Range("Float64".into()))
        }
    }
}

// ---------------------------------------------------------------- decimals

/// `text` as `[-]integer[.fraction]` with at most `scale` significant fraction
/// digits and `precision - scale` integer digits.
fn decimal(precision: u32, scale: u32, text: &str) -> Result<String, Invalid> {
    const FORM: Invalid = Invalid::Form("a decimal number");
    let (negative, unsigned) = match text.as_bytes() {
        [b'-', ..] => (true, &text[1..]),
        [b'+', ..] => (false, &text[1..]),
        _ => (false, text),
    };
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits_only = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !digits_only(whole) || !digits_only(fraction) {
        return Err(FORM);
    }
    let whole = whole.trim_start_matches('0');
    let fraction = {
        let kept = fraction.trim_end_matches('0');
        if kept.len() > scale as usize {
            return Err(Invalid::Fraction(scale));
        }
        kept
    };
    if whole.len() > precision.saturating_sub(scale) as usize {
        return Err(Invalid::Range(format!("Decimal({precision}, {scale})")));
    }
    let zero = whole.is_empty() && fraction.is_empty();
    let mut out = String::with_capacity(unsigned.len() + 2);
    if negative && !zero {
        out.push('-');
    }
    out.push_str(if whole.is_empty() { "0" } else { whole });
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    Ok(out)
}

// ------------------------------------------------------------- the outside

/// `10^-scale` seconds: the unit of the epoch function that carries a
/// `DateTime64(precision)` value without losing a digit.
fn epoch_scale(precision: u32) -> u32 {
    match precision {
        0..=3 => 3,
        4..=6 => 6,
        _ => 9,
    }
}

/// A `DateTime64` value as an epoch tick count.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Epoch {
    precision: u32,
    scale: u32,
}

/// A value as a typed query parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bound {
    /// The type to declare: `{name:<sql_type>}`.
    pub sql_type: String,
    /// The canonical text, not yet escaped for the parameter wire format.
    pub text: String,
    /// A float `NaN` (never equal to itself: a key needs `isNaN`).
    pub nan: bool,
    epoch: Option<Epoch>,
}

impl Bound {
    /// The SQL that stands for the value, given the parameter `name`.
    pub(crate) fn expression(&self, name: &str) -> String {
        match &self.epoch {
            None => format!("{{{name}:{}}}", self.sql_type),
            Some(Epoch { precision, scale }) => {
                let function = match scale {
                    3 => "fromUnixTimestamp64Milli",
                    6 => "fromUnixTimestamp64Micro",
                    _ => "fromUnixTimestamp64Nano",
                };
                let instant = format!("{function}({{{name}:Int64}}, 'UTC')");
                if precision == scale {
                    instant
                } else {
                    format!("CAST({instant} AS DateTime64({precision}, 'UTC'))")
                }
            }
        }
    }
}

/// What a bound value is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The new value of a column.
    Value,
    /// Part of a row key: compared, never stored.
    Key,
}

/// The refusal of a value of a column that cannot be written as text.
fn not_editable(column: &str, ty: &ChType) -> BackendError {
    BackendError::with_code(
        codes::MUTATION_VALUE_NOT_EDITABLE,
        format!(
            "column `{column}` is {ty}: no text can be written back reliably, so the value is \
             not editable here"
        ),
    )
}

fn shorten(text: &str) -> String {
    const LIMIT: usize = 60;
    match text.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_owned(),
    }
}

fn invalid(column: &str, ty: &ChType, value: &Value, reason: &Invalid) -> BackendError {
    let shown = match value {
        Value::String(text) => shorten(text),
        other => shorten(&other.to_string()),
    };
    BackendError::with_code(
        codes::MUTATION_INVALID_VALUE,
        format!("column `{column}` ({ty}): `{shown}` {reason}; nothing was written"),
    )
}

/// The type a parameter of column type `ty` is declared with.
fn param_type(kind: &Kind, ty: &ChType, role: Role) -> String {
    let peeled = ty.peeled();
    match kind {
        // The instant is bound in UTC; the server converts it to the column's zone.
        Kind::DateTime => "DateTime('UTC')".to_owned(),
        Kind::DateTime64 { .. } => "Int64".to_owned(),
        // A key is compared as text: a longer text than N is simply no match
        // (a `FixedString(N)` parameter would be an error), a shorter one is padded.
        Kind::Text if role == Role::Key && peeled.name() == "FixedString" => "String".to_owned(),
        _ => peeled.to_string(),
    }
}

/// `value` (not `null`) of column `column` as a typed query parameter.
pub(crate) fn bind(
    column: &str,
    ty: &ChType,
    value: &Value,
    role: Role,
) -> Result<Bound, BackendError> {
    let kind = Kind::of(ty);
    if !kind.is_editable() {
        return Err(not_editable(column, ty));
    }
    let scalar = coerce(&kind, value, Target::Param)
        .map_err(|reason| invalid(column, ty, value, &reason))?;
    let text = match scalar {
        Scalar::Bool(flag) => flag.to_string(),
        Scalar::Text(text) => text,
    };
    Ok(Bound {
        sql_type: param_type(&kind, ty, role),
        nan: matches!(kind, Kind::Float { .. }) && text == "nan",
        epoch: match kind {
            Kind::DateTime64 { precision } => Some(Epoch {
                precision,
                scale: epoch_scale(precision),
            }),
            _ => None,
        },
        text,
    })
}

/// `value` of column `column` as the JSON an insert sends. Composite columns
/// take native JSON arrays and objects; every other value is validated like an
/// update's and sent as exact text.
pub(crate) fn insert_json(column: &str, ty: &ChType, value: &Value) -> Result<Value, BackendError> {
    let kind = Kind::of(ty);
    match (&kind, value) {
        (_, Value::Null) => Ok(Value::Null),
        // The server decides about composite, opaque and unknown shapes.
        (Kind::Composite | Kind::Opaque, other) => Ok(other.clone()),
        _ => match coerce(&kind, value, Target::Json)
            .map_err(|reason| invalid(column, ty, value, &reason))?
        {
            Scalar::Bool(flag) => Ok(Value::Bool(flag)),
            Scalar::Text(text) => Ok(Value::String(text)),
        },
    }
}

/// `0x` followed by an even number of hex digits: the digits.
pub(crate) fn hex_digits(text: &str) -> Option<&str> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    (digits.len() % 2 == 0 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(ty: &str) -> Kind {
        Kind::of(&ChType::parse(ty))
    }

    fn text(ty: &str, value: Value) -> Result<String, Invalid> {
        match coerce(&kind(ty), &value, Target::Param)? {
            Scalar::Text(text) => Ok(text),
            Scalar::Bool(flag) => Ok(flag.to_string()),
        }
    }

    #[test]
    fn integers_keep_every_digit_and_stay_in_range() {
        let int = |ty: &str, v: &str| text(ty, json!(v));
        assert_eq!(
            int("UInt64", "18446744073709551615").unwrap(),
            "18446744073709551615"
        );
        assert!(int("UInt64", "18446744073709551616").is_err());
        assert_eq!(
            int("Int64", "-9223372036854775808").unwrap(),
            "-9223372036854775808"
        );
        assert!(int("Int64", "-9223372036854775809").is_err());
        assert!(int("Int64", "9223372036854775808").is_err());
        // 256 bits and back, never through a float.
        let min256 =
            "-57896044618658097711785492504343953926634992332820282019728792003956564819968";
        assert_eq!(int("Int256", min256).unwrap(), min256);
        assert!(int("Int256", &min256.replace("968", "969")).is_err());
        let max256 =
            "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        assert_eq!(int("UInt256", max256).unwrap(), max256);
        assert!(int("UInt256", &format!("{max256}0")).is_err());
        assert_eq!(
            int("Int128", "170141183460469231731687303715884105727").unwrap(),
            "170141183460469231731687303715884105727"
        );
        assert!(int("Int128", "170141183460469231731687303715884105728").is_err());
        // The spellings the grid and the editor produce.
        assert_eq!(int("Int32", "+5").unwrap(), "5");
        assert_eq!(int("Int32", "000123").unwrap(), "123");
        assert_eq!(int("Int32", "-0").unwrap(), "0");
        assert_eq!(int("UInt8", "-0").unwrap(), "0");
        assert_eq!(int("Int32", "0").unwrap(), "0");
        assert_eq!(text("Int32", json!(-7)).unwrap(), "-7");
        assert_eq!(
            text("UInt64", json!(u64::MAX)).unwrap(),
            "18446744073709551615"
        );
        // The server would wrap 300 into a UInt8 (44) and a negative into nothing.
        assert!(matches!(int("UInt8", "300"), Err(Invalid::Range(_))));
        assert!(int("UInt8", "256").is_err() && int("UInt8", "255").is_ok());
        assert!(int("UInt8", "-1").is_err());
        assert!(int("Int8", "-128").is_ok() && int("Int8", "-129").is_err());
        for bad in [
            "", "+", "-", "1.5", "1e5", "0x10", " ", "12a", "nan", "１２",
        ] {
            assert!(int("Int64", bad).is_err(), "{bad:?}");
        }
        assert!(text("Int32", json!(1.5)).is_err());
        assert!(text("Int32", json!([1])).is_err());
        assert!(text("Int32", json!(null)).is_err());
        assert_eq!(text("Int32", json!(true)).unwrap(), "1");
    }

    #[test]
    fn floats_accept_the_specials_in_every_spelling_and_refuse_overflow() {
        let float = |ty: &str, v: &str| text(ty, json!(v));
        for nan in ["nan", "NaN", "NAN", "-nan"] {
            assert_eq!(float("Float64", nan).unwrap(), "nan", "{nan}");
        }
        for (inf, expected) in [
            ("inf", "inf"),
            ("+inf", "inf"),
            ("Infinity", "inf"),
            ("-Infinity", "-inf"),
            ("-inf", "-inf"),
            ("+Infinity", "inf"),
        ] {
            assert_eq!(float("Float32", inf).unwrap(), expected, "{inf}");
        }
        assert_eq!(float("Float64", "1.5").unwrap(), "1.5e0");
        assert_eq!(float("Float64", "+5").unwrap(), "5e0");
        assert_eq!(float("Float64", "1e5").unwrap(), "1e5");
        assert_eq!(float("Float64", "-0").unwrap(), "-0e0");
        assert_eq!(float("Float64", ".5").unwrap(), "5e-1");
        // Float32 is parsed as a Float32, not rounded twice.
        assert_eq!(float("Float32", "0.1").unwrap(), "1e-1");
        assert_eq!(float("Float64", "0.1").unwrap(), "1e-1");
        assert!(matches!(float("Float32", "1e50"), Err(Invalid::Range(_))));
        assert!(matches!(float("Float64", "1e999"), Err(Invalid::Range(_))));
        for bad in ["", "abc", "1,5", "0x10", "1 2", "infinit", "n/a"] {
            assert!(float("Float64", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn decimals_are_exact_text_never_truncated() {
        let dec = |ty: &str, v: &str| text(ty, json!(v));
        assert_eq!(dec("Decimal(9, 2)", "1.50").unwrap(), "1.5");
        assert_eq!(dec("Decimal(9, 2)", "1.500").unwrap(), "1.5");
        assert_eq!(dec("Decimal(9, 2)", "-0.00").unwrap(), "0");
        assert_eq!(dec("Decimal(9, 2)", "+5").unwrap(), "5");
        assert_eq!(dec("Decimal(9, 2)", ".25").unwrap(), "0.25");
        assert_eq!(dec("Decimal(9, 2)", "0005.10").unwrap(), "5.1");
        // The server truncates 1.239 to 1.23 and accepts 10 digits into Decimal(9, 2) only to fail.
        assert!(matches!(
            dec("Decimal(9, 2)", "1.239"),
            Err(Invalid::Fraction(2))
        ));
        assert!(matches!(
            dec("Decimal(9, 2)", "12345678"),
            Err(Invalid::Range(_))
        ));
        assert!(dec("Decimal(9, 2)", "1234567.89").is_ok());
        assert!(dec("Decimal(9, 2)", "12345678.9").is_err());
        assert_eq!(
            dec("Decimal(38, 10)", "1234567890123456789012345678.1234567890").unwrap(),
            "1234567890123456789012345678.123456789"
        );
        assert_eq!(dec("Decimal32(2)", "99999.99").unwrap(), "99999.99");
        assert!(dec("Decimal32(2)", "10000000.00").is_err());
        assert!(dec("Decimal(76, 3)", &format!("{}.5", "9".repeat(73))).is_ok());
        for bad in ["", ".", "-", "1e5", "1.2.3", "abc", "1,5", "nan"] {
            assert!(dec("Decimal(9, 2)", bad).is_err(), "{bad:?}");
        }
        assert_eq!(text("Decimal(9, 2)", json!(1.25)).unwrap(), "1.25");
    }

    #[test]
    fn dates_and_instants_follow_the_grid_convention() {
        assert_eq!(text("Date", json!("2020-01-02")).unwrap(), "2020-01-02");
        assert!(text("Date", json!("2149-06-06")).is_ok());
        assert!(text("Date", json!("2149-06-07")).is_err());
        assert!(text("Date", json!("1969-12-31")).is_err());
        assert_eq!(text("Date32", json!("1900-01-01")).unwrap(), "1900-01-01");
        assert!(text("Date32", json!("2020-02-30")).is_err());
        // A DateTime of any zone (or none) reads the text as UTC.
        for ty in [
            "DateTime",
            "DateTime('Asia/Tokyo')",
            "Nullable(DateTime('America/New_York'))",
        ] {
            for shown in [
                "2020-01-02 03:04:05",
                "2020-01-02 03:04:05 UTC",
                "2020-01-02T03:04:05Z",
                "2020-01-02 12:04:05+09:00",
            ] {
                assert_eq!(
                    text(ty, json!(shown)).unwrap(),
                    "2020-01-02 03:04:05",
                    "{ty} {shown}"
                );
            }
        }
        assert!(text("DateTime", json!("2020-01-02 03:04:05.5")).is_err());
        assert!(text("DateTime", json!("2106-02-07 06:28:16")).is_err());
        // A DateTime64 parameter is epoch ticks (milli / micro / nano seconds).
        assert_eq!(
            text(
                "DateTime64(3, 'Asia/Tokyo')",
                json!("2020-01-02 03:04:05.12 UTC")
            )
            .unwrap(),
            "1577934245120"
        );
        assert_eq!(
            text(
                "DateTime64(9)",
                json!("2020-01-02 12:04:05.123456789+09:00")
            )
            .unwrap(),
            "1577934245123456789"
        );
        assert_eq!(
            text("DateTime64(0)", json!("2020-01-02 03:04:05")).unwrap(),
            "1577934245000"
        );
        assert_eq!(
            text("DateTime64(5)", json!("1969-12-31 23:59:59.99999")).unwrap(),
            "-10"
        );
        assert_eq!(
            text("DateTime64(3)", json!("1970-01-01 00:00:00.001")).unwrap(),
            "1"
        );
        assert!(text("DateTime64(3)", json!("2020-01-02 03:04:05.1234")).is_err());
        // The same instant, as the insert's ISO text.
        let json_form = coerce(
            &kind("DateTime64(3, 'America/New_York')"),
            &json!("2020-01-02 03:04:05.123"),
            Target::Json,
        );
        assert_eq!(
            json_form,
            Ok(Scalar::Text("2020-01-02T03:04:05.123Z".into()))
        );
    }

    #[test]
    fn enums_take_names_and_booleans_take_their_spellings() {
        let ty = "Nullable(Enum8('a,b' = 1, 'it''s' = 2, '1' = 3))";
        assert_eq!(text(ty, json!("a,b")).unwrap(), "a,b");
        assert_eq!(text(ty, json!("it's")).unwrap(), "it's");
        assert_eq!(text(ty, json!("1")).unwrap(), "1");
        assert_eq!(text(ty, json!("b")), Err(Invalid::NotAnEntry));
        assert_eq!(text(ty, json!(2)), Err(Invalid::Shape));
        for (value, expected) in [
            (json!(true), true),
            (json!("T"), true),
            (json!("false"), false),
            (json!(0), false),
            (json!("1"), true),
        ] {
            assert_eq!(
                coerce(&kind("Bool"), &value, Target::Param),
                Ok(Scalar::Bool(expected))
            );
        }
        assert!(coerce(&kind("Bool"), &json!("maybe"), Target::Param).is_err());
        assert!(coerce(&kind("Bool"), &json!(2), Target::Param).is_err());
    }

    #[test]
    fn text_is_text_and_hex_only_means_bytes_in_a_key() {
        assert_eq!(text("String", json!("0xdeadbeef")).unwrap(), "0xdeadbeef");
        assert_eq!(text("FixedString(4)", json!("ab")).unwrap(), "ab");
        assert_eq!(
            text("UUID", json!("c91bf7e0-92ee-4169-ac36-4ec41d2d4d54"))
                .unwrap()
                .len(),
            36
        );
        assert_eq!(
            text("String", json!({"a": [1, null]})).unwrap(),
            "{\"a\":[1,null]}"
        );
        assert_eq!(hex_digits("0xff00"), Some("ff00"));
        assert_eq!(hex_digits("0XAB"), Some("AB"));
        assert_eq!(hex_digits("0x"), Some(""));
        for not_hex in ["ff00", "0xf", "0xzz", "0x 00", ""] {
            assert_eq!(hex_digits(not_hex), None, "{not_hex:?}");
        }
    }

    #[test]
    fn bound_parameters_are_typed_for_the_value_not_the_column() {
        let bind = |ty: &str, value: Value, role| bind("c", &ChType::parse(ty), &value, role);
        let tokyo = bind(
            "Nullable(DateTime64(3, 'Asia/Tokyo'))",
            json!("2020-01-02 03:04:05.123 UTC"),
            Role::Value,
        )
        .unwrap();
        assert_eq!(
            (tokyo.sql_type.as_str(), tokyo.text.as_str()),
            ("Int64", "1577934245123")
        );
        assert_eq!(
            tokyo.expression("v0"),
            "fromUnixTimestamp64Milli({v0:Int64}, 'UTC')"
        );
        let coarse = bind("DateTime64(1)", json!("2020-01-02 03:04:05.1"), Role::Key).unwrap();
        assert_eq!(
            coarse.expression("k"),
            "CAST(fromUnixTimestamp64Milli({k:Int64}, 'UTC') AS DateTime64(1, 'UTC'))"
        );
        let fine = bind(
            "DateTime64(7)",
            json!("2020-01-02 03:04:05.1234567"),
            Role::Key,
        )
        .unwrap();
        assert!(fine
            .expression("k")
            .starts_with("CAST(fromUnixTimestamp64Nano("));
        assert_eq!(
            bind("DateTime64(6)", json!("2020-01-02"), Role::Key)
                .unwrap()
                .expression("k"),
            "fromUnixTimestamp64Micro({k:Int64}, 'UTC')"
        );
        let plain = bind("DateTime", json!("2020-01-02 03:04:05"), Role::Key).unwrap();
        assert_eq!(
            (plain.sql_type.as_str(), plain.expression("k").as_str()),
            ("DateTime('UTC')", "{k:DateTime('UTC')}")
        );
        assert_eq!(
            bind("FixedString(4)", json!("ab"), Role::Value)
                .unwrap()
                .sql_type,
            "FixedString(4)"
        );
        assert_eq!(
            bind("FixedString(4)", json!("ab"), Role::Key)
                .unwrap()
                .sql_type,
            "String"
        );
        assert_eq!(
            bind("LowCardinality(String)", json!("x"), Role::Key)
                .unwrap()
                .sql_type,
            "String"
        );
        assert_eq!(
            bind(
                "SimpleAggregateFunction(sum, UInt64)",
                json!("5"),
                Role::Value
            )
            .unwrap()
            .sql_type,
            "UInt64"
        );
        assert!(bind("Float64", json!("NaN"), Role::Key).unwrap().nan);
        assert!(!bind("Float64", json!("1"), Role::Key).unwrap().nan);
        assert_eq!(bind("Bool", json!(true), Role::Value).unwrap().text, "true");
        let enum8 = bind("Enum8('a' = 1)", json!("a"), Role::Value).unwrap();
        assert_eq!(enum8.sql_type, "Enum8('a' = 1)");
        // Refusals carry the contract's codes and name nothing but the column.
        for (ty, value, code) in [
            ("UInt8", json!("300"), codes::MUTATION_INVALID_VALUE),
            (
                "Decimal(9, 2)",
                json!("1.239"),
                codes::MUTATION_INVALID_VALUE,
            ),
            ("Enum8('a' = 1)", json!("zz"), codes::MUTATION_INVALID_VALUE),
            (
                "Array(String)",
                json!("['a']"),
                codes::MUTATION_VALUE_NOT_EDITABLE,
            ),
            (
                "Map(String, Int32)",
                json!("{}"),
                codes::MUTATION_VALUE_NOT_EDITABLE,
            ),
            (
                "Tuple(a Int32)",
                json!("(1)"),
                codes::MUTATION_VALUE_NOT_EDITABLE,
            ),
            ("JSON", json!("{}"), codes::MUTATION_VALUE_NOT_EDITABLE),
            (
                "AggregateFunction(sum, UInt64)",
                json!("0x00"),
                codes::MUTATION_VALUE_NOT_EDITABLE,
            ),
        ] {
            let error = bind(ty, value, Role::Value).unwrap_err();
            assert_eq!(error.code.as_deref(), Some(code), "{ty}: {}", error.message);
            assert!(error.message.contains("`c`"), "{}", error.message);
        }
    }

    #[test]
    fn inserts_send_exact_text_and_native_json_for_composites() {
        let insert = |ty: &str, value: Value| insert_json("c", &ChType::parse(ty), &value);
        assert_eq!(
            insert("UInt256", json!("000115792")).unwrap(),
            json!("115792")
        );
        assert_eq!(
            insert("Decimal(9, 2)", json!("1.50")).unwrap(),
            json!("1.5")
        );
        assert_eq!(
            insert("Float64", json!("-Infinity")).unwrap(),
            json!("-inf")
        );
        assert_eq!(insert("Bool", json!("t")).unwrap(), json!(true));
        assert_eq!(
            insert("DateTime('Asia/Tokyo')", json!("2020-01-02 03:04:05 UTC")).unwrap(),
            json!("2020-01-02T03:04:05Z")
        );
        assert_eq!(
            insert("Array(String)", json!(["a", null])).unwrap(),
            json!(["a", null])
        );
        assert_eq!(insert("Nullable(Int32)", Value::Null).unwrap(), Value::Null);
        assert_eq!(
            insert("String", json!([1, {"a": null}])).unwrap(),
            json!("[1,{\"a\":null}]")
        );
        assert_eq!(
            insert("UInt8", json!("256")).unwrap_err().code.as_deref(),
            Some(codes::MUTATION_INVALID_VALUE)
        );
    }
}
