//! Which type changes are known to lose nothing.
//!
//! The classifier fails closed: [`widens`] is an allowlist of pure widenings
//! (an integer to a wider one of the same signedness, a character type to a
//! longer one, a numeric to one with at least the integer digits and the scale
//! of the old one, `real` to `double precision`, a time type to one with more
//! fractional digits, `date` to a timestamp). Every other type change is
//! reported as destructive.
//!
//! The same type text means different things per server, so the allowlist is
//! asked per [`Flavor`]: MySQL's bare `decimal` is `decimal(10,0)`, PostgreSQL's
//! `text` is unbounded where MySQL's holds 65 535 bytes, a MySQL `timestamp`
//! has a narrower range than `datetime`, and a bare `timestamp` has 6 fractional
//! digits on PostgreSQL and none on MySQL.

/// Which server's reading of a type name applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Postgres,
    /// MySQL and MariaDB.
    MySql,
    Sqlite,
    ClickHouse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Integer { bits: u32, unsigned: bool },
    /// `char(n)`: padded.
    Fixed(u64),
    /// `varchar(n)`, `text` and friends: `u64::MAX` = unbounded.
    Variable(u64),
    /// `numeric(p, s)`; `None`: unconstrained.
    Numeric(Option<(u32, u32)>),
    /// 1 = single precision, 2 = double.
    Float(u8),
    Date,
    /// A time-of-day / timestamp type: fractional digits when written, whether
    /// it carries a zone.
    Moment { kind: MomentKind, digits: Option<u32>, zone: bool },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MomentKind {
    Time,
    /// PostgreSQL / SQLite `timestamp`, ClickHouse `DateTime64`.
    Timestamp,
    /// MySQL / MariaDB `datetime` (a wider range than MySQL's `timestamp`).
    Datetime,
}

/// The type text, lower-cased, whitespace collapsed, without a ClickHouse `Nullable( )`.
fn normalize(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = collapsed.to_ascii_lowercase();
    match lower.strip_prefix("nullable(").and_then(|rest| rest.strip_suffix(')')) {
        Some(inner) => inner.trim().to_owned(),
        None => lower,
    }
}

/// Integer arguments of the first parenthesis of `text` and the text without it.
fn split_args(text: &str) -> (String, Vec<u32>) {
    let Some(open) = text.find('(') else {
        return (text.to_owned(), Vec::new());
    };
    let Some(close) = text[open..].find(')').map(|at| open + at) else {
        return (text.to_owned(), Vec::new());
    };
    let args = text[open + 1..close].split(',').filter_map(|part| part.trim().parse().ok()).collect();
    (format!("{} {}", &text[..open], &text[close + 1..]).split_whitespace().collect::<Vec<_>>().join(" "), args)
}

fn parse(flavor: Flavor, original: &str) -> Ty {
    let text = normalize(original);
    let (head, args) = split_args(&text);
    let words: Vec<&str> = head.split_whitespace().collect();
    let Some(&base) = words.first() else { return Ty::Other };
    let unsigned = words.contains(&"unsigned");
    let signed = |bits| Ty::Integer { bits, unsigned };
    // ClickHouse spells widths with a capital: `Int8` is one byte, PostgreSQL's `int8` eight.
    let clickhouse = original.trim().starts_with("Int") || original.trim().starts_with("UInt") || original.trim().starts_with("Nullable(Int") || original.trim().starts_with("Nullable(UInt");
    match base {
        "tinyint" | "int1" => signed(8),
        "smallint" | "int2" | "smallserial" | "int16" => signed(16),
        "mediumint" => signed(24),
        "int" | "integer" | "int4" | "serial" | "int32" => signed(32),
        "bigint" | "bigserial" | "int64" => signed(64),
        "int8" if clickhouse => signed(8),
        "int8" => signed(64),
        "uint8" => Ty::Integer { bits: 8, unsigned: true },
        "uint16" => Ty::Integer { bits: 16, unsigned: true },
        "uint32" => Ty::Integer { bits: 32, unsigned: true },
        "uint64" => Ty::Integer { bits: 64, unsigned: true },
        "numeric" | "decimal" | "dec" | "number" | "fixed" => match args.as_slice() {
            [p, s] => Ty::Numeric(Some((*p, *s))),
            [p] => Ty::Numeric(Some((*p, 0))),
            // MySQL / MariaDB: a bare `decimal` is `decimal(10,0)`.
            _ if flavor == Flavor::MySql => Ty::Numeric(Some((10, 0))),
            _ => Ty::Numeric(None),
        },
        "real" | "float4" | "float32" => Ty::Float(1),
        "double" | "float8" | "float64" => Ty::Float(2),
        "float" => match args.first() {
            Some(precision) if *precision > 24 => Ty::Float(2),
            _ => Ty::Float(1),
        },
        "char" | "character" | "bpchar" | "nchar" if words.get(1) != Some(&"varying") => match args.first() {
            Some(n) => Ty::Fixed(u64::from(*n)),
            None => Ty::Fixed(1),
        },
        "varchar" | "nvarchar" | "character" | "varying" => match args.first() {
            Some(n) => Ty::Variable(u64::from(*n)),
            None => Ty::Variable(u64::MAX),
        },
        "tinytext" => Ty::Variable(255),
        // PostgreSQL's `text` is unbounded, MySQL's holds 65 535 bytes; the other servers: the smaller bound is the safe one.
        "text" if flavor == Flavor::Postgres => Ty::Variable(u64::MAX),
        "text" => Ty::Variable(65_535),
        "mediumtext" => Ty::Variable(16_777_215),
        "longtext" | "string" | "citext" => Ty::Variable(u64::MAX),
        "date" | "date32" => Ty::Date,
        "time" | "timetz" => Ty::Moment {
            kind: MomentKind::Time,
            digits: args.first().copied(),
            zone: base == "timetz" || words.windows(2).any(|w| w == ["with", "time"]),
        },
        "timestamp" | "timestamptz" | "datetime" | "datetime64" => Ty::Moment {
            kind: if base == "datetime" && flavor == Flavor::MySql { MomentKind::Datetime } else { MomentKind::Timestamp },
            digits: args.first().copied(),
            zone: base == "timestamptz" || words.windows(2).any(|w| w == ["with", "time"]),
        },
        _ => Ty::Other,
    }
}

/// Decimal digits left of the point an integer of `bits` can hold.
fn integer_digits(bits: u32, unsigned: bool) -> u32 {
    match (bits, unsigned) {
        (8, _) => 3,
        (16, _) => 5,
        (24, false) => 7,
        (24, true) => 8,
        (32, _) => 10,
        (64, false) => 19,
        _ => 20,
    }
}

/// The fractional digits a time type has when none are written.
fn digits(flavor: Flavor, written: Option<u32>) -> Option<u32> {
    written.or(match flavor {
        Flavor::MySql => Some(0),
        Flavor::Postgres => Some(6),
        Flavor::Sqlite | Flavor::ClickHouse => None,
    })
}

/// Whether a `date` fits the time type `to` without losing a day.
fn date_fits(flavor: Flavor, kind: MomentKind, zone: bool) -> bool {
    match flavor {
        Flavor::Postgres | Flavor::Sqlite => kind == MomentKind::Timestamp && !zone,
        // `timestamp` only covers 1970 to 2038.
        Flavor::MySql => kind == MomentKind::Datetime,
        Flavor::ClickHouse => false,
    }
}

/// Whether changing a column from `from` to `to` provably loses nothing on `flavor`.
pub fn widens(flavor: Flavor, from: &str, to: &str) -> bool {
    if normalize(from) == normalize(to) {
        return true;
    }
    match (parse(flavor, from), parse(flavor, to)) {
        (Ty::Integer { bits: a, unsigned: ua }, Ty::Integer { bits: b, unsigned: ub }) => match (ua, ub) {
            (false, false) | (true, true) => b >= a,
            (true, false) => b > a,
            (false, true) => false,
        },
        (Ty::Integer { bits, unsigned }, Ty::Numeric(target)) => match target {
            None => true,
            Some((p, s)) => p.saturating_sub(s) >= integer_digits(bits, unsigned),
        },
        (Ty::Fixed(a), Ty::Fixed(b)) | (Ty::Fixed(a), Ty::Variable(b)) | (Ty::Variable(a), Ty::Variable(b)) => b >= a,
        (Ty::Numeric(_), Ty::Numeric(None)) => true,
        (Ty::Numeric(Some((p, s))), Ty::Numeric(Some((p2, s2)))) => p2 >= p && s2 >= s && p2 - s2 >= p - s,
        (Ty::Float(a), Ty::Float(b)) => b >= a,
        (Ty::Date, Ty::Moment { kind, zone, .. }) => date_fits(flavor, kind, zone),
        (
            Ty::Moment { kind: k1, digits: d1, zone: z1 },
            Ty::Moment { kind: k2, digits: d2, zone: z2 },
        ) => {
            k1 == k2
                && z1 == z2
                && match (digits(flavor, d1), digits(flavor, d2)) {
                    (Some(a), Some(b)) => b >= a,
                    // Unknown on this server: only an explicit 6 or more is safe.
                    (None, Some(b)) => b >= 6,
                    (Some(_), None) | (None, None) => false,
                }
        }
        _ => false,
    }
}
