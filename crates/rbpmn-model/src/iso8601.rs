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
    // depends on the unit that follows, so the caller checks that.
    let number = |i: &mut usize| -> Result<Option<bool>, String> {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start {
            return Ok(None);
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
        return if j + 1 == b.len() {
            Ok(())
        } else {
            Err(format!(
                "'W' cannot be combined with other components: '{s}'"
            ))
        };
    }

    let mut components = 0;
    for unit in [b'Y', b'M', b'D'] {
        let save = i;
        if let Some(has_fraction) = number(&mut i)? {
            if b.get(i) == Some(&unit) {
                if has_fraction {
                    return Err(format!(
                        "fractions are only allowed on the seconds component: '{s}'"
                    ));
                }
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
        for unit in [b'H', b'M', b'S'] {
            let save = i;
            if let Some(has_fraction) = number(&mut i)? {
                if b.get(i) == Some(&unit) {
                    if has_fraction && unit != b'S' {
                        return Err(format!(
                            "fractions are only allowed on the seconds component: '{s}'"
                        ));
                    }
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
    Ok(())
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
}
