//! Hand-rolled ISO-8601 validation for timer definitions (`timer-iso8601`).
//!
//! Deliberately strict: datetimes require an explicit UTC offset ('Z' or
//! ±hh:mm) so a timer never depends on server-local time. Cycles (repeating
//! timers) are rejected at the lint layer until v2, so no cycle validator
//! exists yet.

/// `YYYY-MM-DDThh:mm:ss[.fff](Z|±hh:mm)`
pub fn validate_datetime(s: &str) -> Result<(), String> {
    let b = s.as_bytes();
    let mut i = 0;

    let digits = |i: &mut usize, n: usize, what: &str| -> Result<u32, String> {
        let end = *i + n;
        if end > b.len() || !b[*i..end].iter().all(u8::is_ascii_digit) {
            return Err(format!("expected {n} digits for {what} in '{s}'"));
        }
        let v = s[*i..end].parse::<u32>().unwrap();
        *i = end;
        Ok(v)
    };
    let expect = |i: &mut usize, c: u8| -> Result<(), String> {
        if b.get(*i) == Some(&c) {
            *i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at offset {} in '{s}'", c as char, i))
        }
    };

    let year = digits(&mut i, 4, "year")?;
    expect(&mut i, b'-')?;
    let month = digits(&mut i, 2, "month")?;
    expect(&mut i, b'-')?;
    let day = digits(&mut i, 2, "day")?;
    expect(&mut i, b'T')?;
    let hour = digits(&mut i, 2, "hour")?;
    expect(&mut i, b':')?;
    let minute = digits(&mut i, 2, "minute")?;
    expect(&mut i, b':')?;
    let second = digits(&mut i, 2, "second")?;

    if year == 0 {
        // Postgres timestamptz has no year zero: 0000-… would pass lint
        // and then abort the arming transaction at runtime.
        return Err(format!("year 0000 does not exist: '{s}'"));
    }
    if !(1..=12).contains(&month) {
        return Err(format!("month {month} out of range in '{s}'"));
    }
    if day < 1 || day > month_days(year, month) {
        return Err(format!("day {day} out of range in '{s}'"));
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(format!("time out of range in '{s}'"));
    }

    if b.get(i) == Some(&b'.') {
        i += 1;
        let frac_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == frac_start {
            return Err(format!("digits required after '.' in '{s}'"));
        }
    }

    match b.get(i) {
        Some(b'Z') => {
            i += 1;
        }
        Some(b'+') | Some(b'-') => {
            i += 1;
            let oh = digits(&mut i, 2, "offset hours")?;
            expect(&mut i, b':')?;
            let om = digits(&mut i, 2, "offset minutes")?;
            if oh > 14 || om > 59 {
                return Err(format!("UTC offset out of range in '{s}'"));
            }
        }
        _ => {
            return Err(format!(
                "timer dates require an explicit UTC offset ('Z' or ±hh:mm): '{s}'"
            ));
        }
    }

    if i != b.len() {
        return Err(format!("trailing characters after datetime in '{s}'"));
    }
    Ok(())
}

/// `PnW` or `PnYnMnDTnHnMnS` with at least one component; fraction only on
/// seconds; components in order, each at most once.
pub fn validate_duration(s: &str) -> Result<(), String> {
    let b = s.as_bytes();
    if b.first() != Some(&b'P') {
        return Err(format!("duration must start with 'P': '{s}'"));
    }
    let mut i = 1;
    if i == b.len() {
        return Err(format!("empty duration: '{s}'"));
    }

    // Scans digits with an optional fraction; whether a fraction is legal
    // depends on the unit that follows, so the caller checks that. Component
    // magnitude is capped so a syntactically-valid duration can never overflow
    // the engine's later conversion to an actual deadline — lint must catch at
    // deploy what would fail at runtime.
    let number = |i: &mut usize| -> Result<Option<bool>, String> {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start {
            return Ok(None);
        }
        if !component_value_ok(&s[start..*i]) {
            return Err(component_too_large(s));
        }
        let mut has_fraction = false;
        if *i < b.len() && b[*i] == b'.' {
            *i += 1;
            let frac_start = *i;
            while *i < b.len() && b[*i].is_ascii_digit() {
                *i += 1;
            }
            if *i == frac_start {
                return Err(format!("digits required after '.' in '{s}'"));
            }
            if *i - frac_start > MAX_COMPONENT_DIGITS {
                return Err(component_too_large(s));
            }
            has_fraction = true;
        }
        Ok(Some(has_fraction))
    };

    // Week form: PnW, nothing else.
    let mut j = 1;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j > 1 && b.get(j) == Some(&b'W') {
        if !component_value_ok(&s[1..j]) {
            return Err(component_too_large(s));
        }
        if component_value(&s[1..j]) * 7.0 > MAX_TOTAL_DAYS {
            return Err(total_too_large(s));
        }
        return if j + 1 == b.len() {
            Ok(())
        } else {
            Err(format!(
                "'W' cannot be combined with other components: '{s}'"
            ))
        };
    }

    let mut components = 0;
    let mut total_days = 0f64;
    for (unit, days_per) in [(b'Y', 365.25), (b'M', 30.4375), (b'D', 1.0)] {
        let save = i;
        let start = i;
        if let Some(has_fraction) = number(&mut i)? {
            if b.get(i) == Some(&unit) {
                if has_fraction {
                    return Err(format!(
                        "fractions are only allowed on the seconds component: '{s}'"
                    ));
                }
                total_days += component_value(&s[start..i]) * days_per;
                i += 1;
                components += 1;
            } else {
                i = save;
            }
        }
    }

    if b.get(i) == Some(&b'T') {
        i += 1;
        let mut time_components = 0;
        for (unit, days_per) in [
            (b'H', 1.0 / 24.0),
            (b'M', 1.0 / 1440.0),
            (b'S', 1.0 / 86400.0),
        ] {
            let save = i;
            let start = i;
            if let Some(has_fraction) = number(&mut i)? {
                if b.get(i) == Some(&unit) {
                    if has_fraction && unit != b'S' {
                        return Err(format!(
                            "fractions are only allowed on the seconds component: '{s}'"
                        ));
                    }
                    total_days += component_value(&s[start..i]) * days_per;
                    i += 1;
                    time_components += 1;
                } else {
                    i = save;
                }
            }
        }
        if time_components == 0 {
            return Err(format!("'T' must be followed by a time component: '{s}'"));
        }
        components += time_components;
    }

    if components == 0 {
        return Err(format!("duration needs at least one component: '{s}'"));
    }
    if i != b.len() {
        return Err(format!("unexpected characters in duration: '{s}'"));
    }
    if total_days > MAX_TOTAL_DAYS {
        return Err(total_too_large(s));
    }
    Ok(())
}

/// Per-component value bound. This must reject at lint time everything the
/// runtime's `now() + spec::interval` would reject: PostgreSQL intervals
/// hold months and days as int32, so e.g. P999999999W (7e9 days) errors at
/// timer-arm time with "interval field value out of range" — after deploy
/// accepted it. One million per unit keeps every combination far inside
/// int32 months/days and int64 microseconds while allowing multi-millennium
/// timers nobody will outlive.
const MAX_COMPONENT_VALUE: u64 = 1_000_000;
/// Fraction digits carry no magnitude; bound them for sanity only.
const MAX_COMPONENT_DIGITS: usize = 9;

/// Total-magnitude bound: 10,000 years. Per-component caps alone are not
/// enough — Postgres's timestamptz tops out at 294276 AD, so a lint-passing
/// P500000Y would fail `now() + interval` at timer-arm time. Ten millennia
/// is deliberately far below that ceiling and far beyond any process
/// anybody will outlive.
const MAX_TOTAL_DAYS: f64 = 10_000.0 * 365.25;

fn total_too_large(s: &str) -> String {
    format!("duration exceeds the 10,000-year total cap: '{s}'")
}

/// Parse a digit run already validated by `component_value_ok`.
fn component_value(digits: &str) -> f64 {
    digits.parse::<f64>().unwrap_or(f64::INFINITY)
}

fn component_value_ok(digits: &str) -> bool {
    digits
        .parse::<u64>()
        .is_ok_and(|v| v <= MAX_COMPONENT_VALUE)
}

fn component_too_large(s: &str) -> String {
    format!(
        "duration component too large (max {MAX_COMPONENT_VALUE} per unit — \
         larger values overflow the runtime's deadline arithmetic): '{s}'"
    )
}

fn month_days(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_zero_is_rejected() {
        assert!(validate_datetime("0000-02-29T00:00:00Z").is_err());
        assert!(validate_datetime("0001-01-01T00:00:00Z").is_ok());
    }

    #[test]
    fn valid_datetimes() {
        for s in [
            "2030-01-15T09:00:00Z",
            "2028-02-29T23:59:59.5Z",
            "2030-06-01T12:00:00+02:00",
            "2030-06-01T12:00:00-05:30",
        ] {
            validate_datetime(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
    }

    #[test]
    fn invalid_datetimes() {
        for s in [
            "2030-01-15",
            "2030-01-15T09:00:00",
            "2030-13-01T09:00:00Z",
            "2030-02-30T09:00:00Z",
            "2029-02-29T09:00:00Z",
            "2030-01-15T24:00:00Z",
            "2030-01-15T09:00:00+15:00",
            "2030-01-15T09:00:00Zx",
            "next tuesday",
        ] {
            assert!(validate_datetime(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn valid_durations() {
        for s in [
            "PT15M",
            "P14D",
            "P1Y2M3DT4H5M6S",
            "PT0.5S",
            "P3W",
            "PT36H",
            "P1DT12H",
            "PT0S",
        ] {
            validate_duration(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
    }

    #[test]
    fn invalid_durations() {
        for s in [
            "P", "PT", "15M", "P1.5D", "P3W2D", "PT5X", "P1D2H", "PT1H!", "soon",
        ] {
            assert!(validate_duration(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn duration_magnitude_is_bounded() {
        // The bound tracks what PostgreSQL's interval type accepts at
        // timer-arm time: P999999999W passed the old digit-count cap but
        // overflowed int32 interval days at runtime.
        assert!(validate_duration("P1000000D").is_ok());
        assert!(validate_duration("P1000001D").is_err());
        assert!(validate_duration("P999999999D").is_err());
        assert!(validate_duration("P999999999W").is_err());
        assert!(validate_duration("PT9999999999H").is_err());
        assert!(validate_duration("PT1.9999999999S").is_err());
        // The 10,000-year total cap: Postgres timestamptz ends at 294276 AD,
        // so a per-component cap alone would let P500000Y deploy and then
        // fail at timer-arm time.
        assert!(validate_duration("P9999Y").is_ok());
        assert!(validate_duration("P500000Y").is_err());
        assert!(validate_duration("P600000W").is_err());
        assert!(validate_duration("P9999Y11M").is_ok());
        assert!(validate_duration("P10000Y1D").is_err());
    }
}
