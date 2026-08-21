//! Hand-rolled ISO-8601 validation for timer definitions (`timer-iso8601`).
//!
//! Deliberately strict: datetimes require an explicit UTC offset ('Z' or
//! ±hh:mm) so a timer never depends on server-local time. Cycles (repeating
//! timers, `timeCycle`) are a deliberate subset of ISO 8601's recurring
//! intervals — `R[n]/P…` and `R[n]/<datetime>/P…` with a **fixed-length**
//! period — and only a non-interrupting boundary may carry one
//! (docs/design/boundary-messages.md §2.5).

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

/// The components of one duration, as [`validate_duration`] accepted them.
/// One tokenizer for the two questions ever asked of a duration — "is it
/// valid?" and "how many seconds is it?" — so the linter and the projection's
/// cycle arithmetic can never read the same text differently.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct DurationParts {
    weeks: f64,
    years: f64,
    months: f64,
    days: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
}

/// `PnW` or `PnYnMnDTnHnMnS` with at least one component; fraction only on
/// seconds; components in order, each at most once.
pub fn validate_duration(s: &str) -> Result<(), String> {
    duration_parts(s).map(|_| ())
}

fn duration_parts(s: &str) -> Result<DurationParts, String> {
    let mut parts = DurationParts::default();
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
        parts.weeks = component_value(&s[1..j]);
        return if j + 1 == b.len() {
            Ok(parts)
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
                let value = component_value(&s[start..i]);
                total_days += value * days_per;
                match unit {
                    b'Y' => parts.years = value,
                    b'M' => parts.months = value,
                    _ => parts.days = value,
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
                    let value = component_value(&s[start..i]);
                    total_days += value * days_per;
                    match unit {
                        b'H' => parts.hours = value,
                        b'M' => parts.minutes = value,
                        _ => parts.seconds = value,
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
    if total_days > MAX_TOTAL_DAYS {
        return Err(total_too_large(s));
    }
    Ok(parts)
}

/// The pieces of a validated cycle, for the two places that need them: the
/// core reads `repeats` when it arms, the projection reads `anchor` and the
/// period when it computes an instant.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleParts {
    /// `None` for `R/…`: unbounded, ended by the host.
    pub repeats: Option<u32>,
    /// The phase-fixing datetime of `R[n]/<datetime>/P…`, if any.
    pub anchor: Option<String>,
    /// The ISO-8601 period, already known to be fixed-length.
    pub period: String,
    /// ...and its length, the one number the projection steps by.
    pub period_seconds: f64,
}

/// `R[n]/P…` or `R[n]/<datetime>/P…`: `n` fires (absent = unbounded, zero
/// refused), an optional anchor that fixes the *phase*, and a period that is
/// fixed-length — weeks, days, hours, minutes, seconds — and at least a
/// minute long ([`MIN_CYCLE_SECONDS`]). Months and years are refused: the
/// projection steps a cycle with epoch arithmetic, the previous due plus the
/// period, and a month is not a number of seconds. The `R/<start>/<end>` and
/// `R/P…/<end>` forms are refused too; the subset is stated rather than
/// silently narrowed.
pub fn validate_cycle(s: &str) -> Result<(), String> {
    split_cycle(s).map(|_| ())
}

/// [`validate_cycle`], handing back the parts. Errors are the same text lint
/// shows, so an arm-time failure on a variable-sourced cycle reads the same.
pub fn split_cycle(s: &str) -> Result<CycleParts, String> {
    let Some(rest) = s.strip_prefix('R') else {
        return Err(format!("a repeating timer starts with 'R': '{s}'"));
    };
    let digits_end = rest.bytes().take_while(u8::is_ascii_digit).count();
    let repeats = if digits_end == 0 {
        None
    } else {
        // The same cap as a duration component (one million): the count is
        // stored in an `int` column and counted down by the core, and a
        // value that fits u32 but not i32 would wrap to "fire once" on its
        // way through the row. Nobody will outlive a million occurrences.
        let n = rest[..digits_end]
            .parse::<u64>()
            .ok()
            .filter(|n| *n <= MAX_COMPONENT_VALUE)
            .ok_or_else(|| format!("repeat count too large (max {MAX_COMPONENT_VALUE}) in '{s}'"))?
            as u32;
        if n == 0 {
            return Err(format!(
                "'R0' repeats zero times and would never fire: '{s}'"
            ));
        }
        Some(n)
    };
    let Some(rest) = rest[digits_end..].strip_prefix('/') else {
        return Err(format!("expected '/' after the repeat count in '{s}'"));
    };
    let parts: Vec<&str> = rest.split('/').collect();
    // Before the shapes below get to guess: an empty component matches the
    // *arity* of a form it is not. `R/P7D/` has two parts and would be read
    // as the 'R/duration/end' form, `R//P7D` as an anchor that is not a
    // datetime — both true of the arity and both misleading about the text.
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("empty component in '{s}'"));
    }
    let (anchor, period) = match parts.as_slice() {
        [period] => (None, *period),
        [anchor, period] => {
            if validate_datetime(anchor).is_err() {
                if validate_duration(anchor).is_ok() {
                    return Err(format!(
                        "the 'R/duration/end' form is not supported — rbpmn accepts \
                         'Rn/P…' and 'Rn/<datetime>/P…': '{s}'"
                    ));
                }
                return Err(format!(
                    "the anchor of a repeating timer must be a datetime with an \
                     explicit UTC offset: '{s}'"
                ));
            }
            (Some(anchor.to_string()), *period)
        }
        _ => {
            return Err(format!(
                "a repeating timer has at most an anchor and a period — rbpmn accepts \
                 'Rn/P…' and 'Rn/<datetime>/P…': '{s}'"
            ));
        }
    };
    if validate_datetime(period).is_ok() {
        return Err(format!(
            "the 'R/start/end' form is not supported — rbpmn accepts 'Rn/P…' and \
             'Rn/<datetime>/P…': '{s}'"
        ));
    }
    let period_seconds = fixed_length_seconds(period)?;
    // The floor belongs *here* and not in `fixed_length_seconds`, which is a
    // generic "how long is this duration" and has no business refusing a
    // short one: only a *repeating* period turns the scheduler into a hot
    // loop, and only a repeating period spawns a token per fire.
    if period_seconds < MIN_CYCLE_SECONDS {
        return Err(format!(
            "a repeating period must be at least one minute (PT1M): anything \
             shorter turns the scheduler into a hot loop, spawning a token per \
             fire — '{s}'"
        ));
    }
    Ok(CycleParts {
        repeats,
        anchor,
        period: period.to_string(),
        period_seconds,
    })
}

/// A duration as a number of seconds, for durations that *have* one: weeks,
/// days, hours, minutes, seconds. `P1M` and `P1Y` are refused — their length
/// depends on where in the calendar they land, which is exactly what a cycle
/// stepped by epoch arithmetic cannot honour. Validates on the way (the same
/// tokenizer as [`validate_duration`]), so it cannot be handed text the
/// linter never saw.
pub fn fixed_length_seconds(period: &str) -> Result<f64, String> {
    let p = duration_parts(period)?;
    if p.years > 0.0 || p.months > 0.0 {
        return Err(format!(
            "a repeating period must have a fixed length — months and years \
             do not (use weeks or days): '{period}'"
        ));
    }
    let secs =
        p.weeks * 604_800.0 + p.days * 86_400.0 + p.hours * 3_600.0 + p.minutes * 60.0 + p.seconds;
    if secs <= 0.0 {
        return Err(format!("a repeating period must be positive: '{period}'"));
    }
    Ok(secs)
}

/// Per-component value bound. This must reject at lint time everything the
/// runtime's `now() + spec::interval` would reject: PostgreSQL intervals
/// hold months and days as int32, so e.g. P999999999W (7e9 days) errors at
/// timer-arm time with "interval field value out of range" — after deploy
/// accepted it. One million per unit keeps every combination far inside
/// int32 months/days and int64 microseconds while allowing multi-millennium
/// timers nobody will outlive.
const MAX_COMPONENT_VALUE: u64 = 1_000_000;

/// The other end of the same argument, for cycles only: a repeating period
/// must be at least one minute. `R/PT0.001S` is a valid ISO-8601 recurrence
/// and a hot loop — the scheduler would fire it as fast as it can claim it,
/// spawning a token per fire, and every one of those tokens is a row. A
/// minute is the shortest period a boundary event is ever a sensible way to
/// express; anything below it wanted a worker loop, not a model.
const MIN_CYCLE_SECONDS: f64 = 60.0;
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
    fn valid_cycles() {
        for (s, repeats, anchored, secs) in [
            ("R/P7D", None, false, 604_800.0),
            ("R3/P7D", Some(3), false, 604_800.0),
            ("R/2026-08-31T00:00:00+02:00/P7D", None, true, 604_800.0),
            ("R2/2026-08-31T00:00:00Z/P1W", Some(2), true, 604_800.0),
            ("R/PT90M", None, false, 5_400.0),
            ("R/P1DT12H", None, false, 129_600.0),
            ("R/PT1M", None, false, 60.0), // exactly the floor
            ("R1000000/P7D", Some(1_000_000), false, 604_800.0),
        ] {
            let parts = split_cycle(s).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert_eq!(parts.repeats, repeats, "{s}");
            assert_eq!(parts.anchor.is_some(), anchored, "{s}");
            assert_eq!(parts.period_seconds, secs, "{s}");
        }
    }

    #[test]
    fn invalid_cycles() {
        for s in [
            "P7D",                                         // no R
            "R0/P7D",                                      // never fires
            "R/P1M",                                       // not fixed-length
            "R/P1Y",                                       // not fixed-length
            "R/P7D/2026-12-31T00:00:00Z",                  // duration/end form
            "R/2026-08-31T00:00:00Z/2026-12-31T00:00:00Z", // start/end form
            "R/2026-08-31T00:00:00/P7D",                   // anchor without offset
            "R/",                                          // nothing to repeat
            "R3P7D",                                       // missing slash
            "R/P7D/P1D/P1D",                               // too many parts
            "R/PT0S",                                      // zero period
            "R99999999999/P7D",                            // repeat count overflow
            "R1000001/P7D",                                // over the component cap
            "R4294967295/P7D",                             // fits u32, not the int column
            "R/PT0.5S",                                    // under the one-minute floor
            "R/PT59S",                                     // just under it
            "R/PT0.001S",                                  // the hot loop itself
            "R/P7D/",                                      // empty period
            "R//P7D",                                      // empty anchor
        ] {
            assert!(validate_cycle(s).is_err(), "{s} should be rejected");
        }
    }

    /// A repeating period below the floor is a hot loop: the scheduler fires
    /// it as fast as it can claim it and each fire spawns a token. The
    /// complaint has to name the floor, because the text is valid ISO-8601
    /// and the author has no other way to learn why it was refused.
    #[test]
    fn a_cycle_period_has_a_one_minute_floor() {
        let why = validate_cycle("R/PT0.001S").unwrap_err();
        assert!(why.contains("at least one minute (PT1M)"), "{why}");
        // The floor is a *cycle* rule, not a duration one: PT1S is a
        // perfectly good single-shot timer and stays one.
        assert!(validate_duration("PT1S").is_ok());
        assert!(fixed_length_seconds("PT1S").is_ok());
    }

    /// An empty component matches the arity of a form it is not: `R/P7D/`
    /// has two parts and used to be reported as the 'R/duration/end' form,
    /// `R//P7D` as an anchor that is not a datetime. Both were true of the
    /// shape and misleading about the text.
    #[test]
    fn an_empty_cycle_component_says_so() {
        for s in ["R/P7D/", "R//P7D", "R3//"] {
            let why = validate_cycle(s).unwrap_err();
            assert!(why.contains("empty component"), "{s}: {why}");
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
