use std::time::Duration;

pub const MAX_INTERVAL_SECONDS: u64 = 86_400;
pub const PRESET_INTERVALS: &[u64] = &[1, 5, 10, 15, 30, 60];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntervalError {
    Empty,
    NotAnInteger,
    Zero,
    TooLarge,
}

impl std::fmt::Display for IntervalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Empty => "Enter an interval in seconds",
            Self::NotAnInteger => "Interval must be a whole number of seconds",
            Self::Zero => "Interval must be greater than zero",
            Self::TooLarge => "Interval cannot exceed 86400 seconds",
        };
        f.write_str(message)
    }
}

pub fn parse_custom_seconds(value: &str) -> Result<u64, IntervalError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(IntervalError::Empty);
    }
    let seconds = value
        .parse::<u64>()
        .map_err(|_| IntervalError::NotAnInteger)?;
    if seconds == 0 {
        return Err(IntervalError::Zero);
    }
    if seconds > MAX_INTERVAL_SECONDS {
        return Err(IntervalError::TooLarge);
    }
    Ok(seconds)
}

pub fn interval_duration(seconds: u64) -> Duration {
    Duration::from_secs(seconds.clamp(1, MAX_INTERVAL_SECONDS))
}

#[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
pub fn is_eligible(
    configured: Option<u64>,
    active: bool,
    connected: bool,
    query_running: bool,
    pending_edits: bool,
    pending_mutations: bool,
    draft_active: bool,
    read_only_query: bool,
) -> bool {
    configured.is_some()
        && active
        && connected
        && !query_running
        && !pending_edits
        && !pending_mutations
        && !draft_active
        && read_only_query
}

#[cfg(test)]
mod tests {
    use super::{is_eligible, parse_custom_seconds, IntervalError, MAX_INTERVAL_SECONDS};

    #[test]
    fn custom_interval_accepts_positive_seconds_within_bound() {
        assert_eq!(parse_custom_seconds(" 120 "), Ok(120));
        assert_eq!(parse_custom_seconds("86400"), Ok(MAX_INTERVAL_SECONDS));
    }

    #[test]
    fn custom_interval_rejects_invalid_values() {
        assert_eq!(parse_custom_seconds(""), Err(IntervalError::Empty));
        assert_eq!(
            parse_custom_seconds("abc"),
            Err(IntervalError::NotAnInteger)
        );
        assert_eq!(parse_custom_seconds("0"), Err(IntervalError::Zero));
        assert_eq!(parse_custom_seconds("86401"), Err(IntervalError::TooLarge));
    }

    #[test]
    fn policy_pauses_hidden_busy_or_disconnected_tabs() {
        assert!(is_eligible(
            Some(5),
            true,
            true,
            false,
            false,
            false,
            false,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            false,
            true,
            false,
            false,
            false,
            false,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            true,
            false,
            false,
            false,
            false,
            false,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            true,
            true,
            true,
            false,
            false,
            false,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            true,
            true,
            false,
            true,
            false,
            false,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            true,
            true,
            false,
            false,
            true,
            false,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            true,
            true,
            false,
            false,
            false,
            true,
            true
        ));
        assert!(!is_eligible(
            Some(5),
            true,
            true,
            false,
            false,
            false,
            false,
            false
        ));
        assert!(!is_eligible(
            None, true, true, false, false, false, false, true
        ));
    }
}
