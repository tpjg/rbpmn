//! The guarantee: `rbpmn-feel-number` answers exactly what the C library
//! answers, so substituting it into dsntk changes no decision (`docs/dmn.md`,
//! Gate 0a).
//!
//! Divergences are not merely counted — each is reported with its relative
//! magnitude, because the ruling taken up front was "document by how much,
//! and if it is too much, sit down and discuss". A run that finds nothing
//! prints the case count, so a suite that silently stopped generating cases
//! cannot pass as a clean bill of health.

use rbpmn_feel_number_parity::{
    Answer, Num, Ours, Reference, classify, guard, opt, quiet_panics, raw,
    relative_difference, value,
};
use std::collections::BTreeMap;

/// The relative difference allowed for `exp`, `ln`, `sqrt` and `pow`. It is
/// applied at those call sites only — exact arithmetic, comparison, rounding,
/// parsing and conversion must match the C library digit for digit, so no
/// global threshold can quietly absorb a drift there.
///
/// The value is the worst deviation Gate 0 measured (1.3e-30, `pow` with a
/// large integer exponent), rounded up one order. Tightening it is always
/// safe; loosening it is a decision, not a fix.
const TRANSCENDENTAL_TOLERANCE: f64 = 1e-29;

/// Operands chosen to hit the places two decimal implementations drift:
/// signed zeros, trailing zeros, more than 34 significant digits, values
/// straddling a rounding tie, and the exponent extremes.
const LITERALS: &[&str] = &[
    "0",
    "-0",
    "0.0",
    "1",
    "-1",
    "2",
    "10",
    "-10",
    "0.1",
    "-0.1",
    "0.5",
    "1.5",
    "2.5",
    "-1.5",
    "-2.5",
    "1.05",
    "1.15",
    "1.25",
    "49",
    "49.0",
    "50.50",
    "100",
    "123.4567",
    "-123.4567",
    "1234567890",
    "0.000001",
    "1e10",
    "1e-10",
    "1e30",
    "1e-30",
    "3.14159265358979323846264338327950288",
    "1234567890123456789012345678901234567890",
    "0.3333333333333333333333333333333333333",
    "9999999999999999999999999999999999",
    "99999999999999999999999999999999999",
    "1e6000",
    "1e-6000",
    // Exactly representable in decimal128 (34 significant digits and a
    // trailing zero) but wider than 34 *characters*, so the last digit — the
    // only one parity depends on — survives the format but not a rounded
    // quotient. This is what `even`/`odd` go through.
    "20000000000000000000000000000000050",
    "-20000000000000000000000000000000050",
];

/// Literals whose *parse* is the question, kept out of `LITERALS` because the
/// operation loops skip anything either side refuses — which is exactly the
/// disagreement `parsing_agrees` is looking for.
const PARSE_ONLY: &[&str] = &[
    "1e6144",
    "9.999999999999999999999999999999999e6144",
    "1e6145",
    "1e99999",
    "-1e6145",
    "1e-6176",
    "1e-6177",
    "1e-99999",
    // Whitespace: `bid128_from_string` skips C `isspace`, which is wider than
    // the four obvious characters.
    " 1",
    "\t1",
    "\n1",
    "\r1",
    "\x0b1",
    "\x0c1",
    "  \t\n1",
    "1 ",
    "- 1",
    "1 2",
    "",
    "+1",
    "1.",
    ".1",
    "1e",
    "0x10",
    "Infinity",
    "NaN",
];

const SCALES: &[i32] = &[-2, -1, 0, 1, 2, 3];

struct Report {
    checked: usize,
    /// Divergences Gate 0 named and accepted, counted by class.
    expected: BTreeMap<String, usize>,
    /// Everything else. Any entry here fails the run.
    diverged: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Self {
            checked: 0,
            expected: BTreeMap::new(),
            diverged: Vec::new(),
        }
    }

    fn check(&mut self, case: &str, theirs: Answer, ours: Answer, tolerance: f64) {
        self.checked += 1;
        if theirs == ours {
            return;
        }
        if let Some(kind) = classify(&theirs, &ours, tolerance) {
            *self.expected.entry(format!("{kind:?}")).or_default() += 1;
            return;
        }
        let magnitude = match (&theirs, &ours) {
            (Answer::Value(t), Answer::Value(o)) => match relative_difference(t, o) {
                Some(d) => format!("  [relative difference {d:.3e}]"),
                None => String::new(),
            },
            _ => String::new(),
        };
        self.diverged
            .push(format!("{case:<46} reference={theirs:?} ours={ours:?}{magnitude}"));
    }

    fn finish(self, label: &str) {
        assert!(self.checked > 0, "{label}: generated no cases");
        if !self.diverged.is_empty() {
            // The report goes to stdout, not into the panic payload: panic
            // output is suppressed for this run (the reference panics by
            // design in two places), and a report nobody can read is a report
            // that does not exist.
            println!(
                "{label}: {} of {} comparisons diverge from the C library:",
                self.diverged.len(),
                self.checked
            );
            for line in &self.diverged {
                println!("  {line}");
            }
            panic!(
                "{label}: {} unaccounted divergences — see stdout",
                self.diverged.len()
            );
        }
        let accounted: usize = self.expected.values().sum();
        if accounted == 0 {
            println!("{label}: {} comparisons, no divergence", self.checked);
        } else {
            let classes: Vec<String> = self
                .expected
                .iter()
                .map(|(k, n)| format!("{k}={n}"))
                .collect();
            println!(
                "{label}: {} comparisons, {accounted} in named deviation classes ({})",
                self.checked,
                classes.join(", ")
            );
        }
    }
}

/// Run the same closure against both implementations and compare.
fn both<F, G>(report: &mut Report, case: &str, f: F, g: G)
where
    F: Fn() -> Answer,
    G: Fn() -> Answer,
{
    report.check(case, guard(f), guard(g), 0.0);
}

/// For `exp`, `ln`, `sqrt` and `pow` only — see [`TRANSCENDENTAL_TOLERANCE`].
fn both_approx<F, G>(report: &mut Report, case: &str, f: F, g: G)
where
    F: Fn() -> Answer,
    G: Fn() -> Answer,
{
    report.check(case, guard(f), guard(g), TRANSCENDENTAL_TOLERANCE);
}

#[test]
fn literals_parse_and_render_identically() {
    quiet_panics();
    let mut r = Report::new();
    for lit in LITERALS {
        both(
            &mut r,
            &format!("parse({lit})"),
            || Reference::parse(lit).map_or(Answer::Refused, value),
            || Ours::parse(lit).map_or(Answer::Refused, value),
        );
        // The canonical coefficient/exponent form: `Display` can hide a scale
        // difference that this cannot.
        both(
            &mut r,
            &format!("parse({lit}) raw"),
            || Reference::parse(lit).map_or(Answer::Refused, raw),
            || Ours::parse(lit).map_or(Answer::Refused, raw),
        );
    }
    for s in ["", " ", "abc", "1.2.3", "--1", "1e", "NaN", "Infinity", "0x10"] {
        both(
            &mut r,
            &format!("parse({s:?})"),
            || Reference::parse(s).map_or(Answer::Refused, value),
            || Ours::parse(s).map_or(Answer::Refused, value),
        );
    }
    r.finish("literals");
}

#[test]
fn constructors_agree() {
    quiet_panics();
    let mut r = Report::new();
    for n in [-1234567890i64, -100, -1, 0, 1, 49, 490, 4900, 5050, i64::MAX] {
        for s in [-2i32, 0, 1, 2, 4, 8] {
            both(
                &mut r,
                &format!("new({n}, {s})"),
                || raw(Reference::new(n, s)),
                || raw(Ours::new(n, s)),
            );
        }
        both(
            &mut r,
            &format!("from_i64({n})"),
            || raw(Reference::from_i64(n)),
            || raw(Ours::from_i64(n)),
        );
    }
    r.finish("constructors");
}

#[test]
fn arithmetic_agrees() {
    quiet_panics();
    let mut r = Report::new();
    for a in LITERALS {
        for b in LITERALS {
            let (Some(ta), Some(tb)) = (Reference::parse(a), Reference::parse(b)) else {
                continue;
            };
            let (Some(oa), Some(ob)) = (Ours::parse(a), Ours::parse(b)) else {
                continue;
            };
            for (name, tf, of) in [
                (
                    "+",
                    Num::add as fn(Reference, Reference) -> Reference,
                    Num::add as fn(Ours, Ours) -> Ours,
                ),
                ("-", Num::sub, Num::sub),
                ("*", Num::mul, Num::mul),
                ("/", Num::div, Num::div),
                ("%", Num::rem, Num::rem),
            ] {
                both(
                    &mut r,
                    &format!("{a} {name} {b}"),
                    || value(tf(ta, tb)),
                    || value(of(oa, ob)),
                );
                // Also in the canonical `Debug` form. It matters for two
                // reasons: it exposes a scale difference `Display` can hide,
                // and it is the *only* way to compare a non-finite result —
                // the reference's `Display` panics on one, so without this
                // every infinity and NaN would be waved through as "the
                // reference panics" without anyone checking we produced the
                // same one. It found `0 / 0` answering infinity here.
                both(
                    &mut r,
                    &format!("{a} {name} {b} raw"),
                    || raw(tf(ta, tb)),
                    || raw(of(oa, ob)),
                );
            }
        }
    }
    r.finish("arithmetic");
}

#[test]
fn comparisons_agree() {
    quiet_panics();
    let mut r = Report::new();
    for a in LITERALS {
        for b in LITERALS {
            let (Some(ta), Some(tb)) = (Reference::parse(a), Reference::parse(b)) else {
                continue;
            };
            let (Some(oa), Some(ob)) = (Ours::parse(a), Ours::parse(b)) else {
                continue;
            };
            for (name, tf, of) in [
                (
                    "==",
                    (|x, y| x == y) as fn(Reference, Reference) -> bool,
                    (|x, y| x == y) as fn(Ours, Ours) -> bool,
                ),
                ("<", |x, y| x < y, |x, y| x < y),
                ("<=", |x, y| x <= y, |x, y| x <= y),
                (">", |x, y| x > y, |x, y| x > y),
                (">=", |x, y| x >= y, |x, y| x >= y),
            ] {
                both(
                    &mut r,
                    &format!("{a} {name} {b}"),
                    || Answer::Bool(tf(ta, tb)),
                    || Answer::Bool(of(oa, ob)),
                );
            }
        }
    }
    r.finish("comparisons");
}

/// Parsing has to be checked on its own, because every other loop here bails
/// out when either side refuses a literal — so an implementation that
/// *accepts* what the C library rejects would be skipped silently rather than
/// caught. `1e6145` is the case that matters: fastnum's type is far wider than
/// decimal128, so the literal parses cleanly and only overflows when clamped
/// into range, and a `from_str` that checked finiteness before the clamp
/// handed back `Infinity` where the C library refuses.
#[test]
fn parsing_agrees() {
    quiet_panics();
    let mut r = Report::new();
    for lit in LITERALS.iter().chain(PARSE_ONLY) {
        // Escaped, because the corpus carries control characters and a raw
        // `\n` in a case label splits the report line in two — which is how a
        // first reading of this suite came out with the wrong set of accepted
        // whitespace.
        let lit = *lit;
        let shown = lit.escape_debug().to_string();
        both(
            &mut r,
            &format!("parse({shown})"),
            || Reference::parse(lit).map_or(Answer::Refused, |n| guard(|| value(n))),
            || Ours::parse(lit).map_or(Answer::Refused, |n| guard(|| value(n))),
        );
        // ...and the stored scale, not just the rendering: `Display` hides it,
        // and it is what every subsequent operation rounds against.
        both(
            &mut r,
            &format!("parse({shown}) raw"),
            || Reference::parse(lit).map_or(Answer::Refused, |n| guard(|| raw(n))),
            || Ours::parse(lit).map_or(Answer::Refused, |n| guard(|| raw(n))),
        );
    }
    r.finish("parsing");
}

#[test]
fn unary_operations_agree() {
    quiet_panics();
    let mut r = Report::new();
    for lit in LITERALS {
        let (Some(t), Some(o)) = (Reference::parse(lit), Ours::parse(lit)) else {
            continue;
        };
        macro_rules! unary {
            ($name:literal, $m:ident) => {
                both(
                    &mut r,
                    &format!("{}({lit})", $name),
                    || value(Num::$m(&t)),
                    || value(Num::$m(&o)),
                );
            };
        }
        unary!("abs", abs);
        unary!("trunc", trunc);
        unary!("frac", frac);
        both(&mut r, &format!("neg({lit})"), || value(t.neg()), || value(o.neg()));

        macro_rules! fallible {
            ($name:literal, $m:ident) => {
                both_approx(
                    &mut r,
                    &format!("{}({lit})", $name),
                    || opt(Num::$m(&t)),
                    || opt(Num::$m(&o)),
                );
            };
        }
        fallible!("exp", exp);
        fallible!("ln", ln);
        fallible!("sqrt", sqrt);
        fallible!("square", square);

        macro_rules! predicate {
            ($name:literal, $m:ident) => {
                both(
                    &mut r,
                    &format!("{}({lit})", $name),
                    || Answer::Bool(Num::$m(&t)),
                    || Answer::Bool(Num::$m(&o)),
                );
            };
        }
        predicate!("even", even);
        predicate!("odd", odd);
        predicate!("is_integer", is_integer);
        predicate!("is_zero", is_zero);
        predicate!("is_one", is_one);
        predicate!("is_negative", is_negative);
        predicate!("is_positive", is_positive);
    }
    r.finish("unary");
}

#[test]
fn scaled_rounding_agrees() {
    quiet_panics();
    let mut r = Report::new();
    for lit in LITERALS {
        let (Some(t), Some(o)) = (Reference::parse(lit), Ours::parse(lit)) else {
            continue;
        };
        for &scale in SCALES {
            macro_rules! scaled {
                ($name:literal, $m:ident) => {
                    both(
                        &mut r,
                        &format!("{}({lit}, {scale})", $name),
                        || opt(Num::$m(&t, scale)),
                        || opt(Num::$m(&o, scale)),
                    );
                };
            }
            scaled!("ceiling", ceiling);
            scaled!("floor", floor);
            scaled!("round_down", round_down);
            scaled!("round_up", round_up);
            scaled!("round_half_up", round_half_up);
            scaled!("round_half_down", round_half_down);

            let (ts, os) = (Reference::from_i64(scale as i64), Ours::from_i64(scale as i64));
            both(
                &mut r,
                &format!("round({lit}, {scale})"),
                || value(t.round(&ts)),
                || value(o.round(&os)),
            );
        }
    }
    r.finish("scaled rounding");
}

#[test]
fn powers_agree() {
    quiet_panics();
    let mut r = Report::new();
    let exponents = [
        "0", "1", "2", "3", "-1", "-2", "0.5", "-0.5", "2.5", "10", "100", "9999",
        // Above 2^31: an exponent that no longer fits the integer type a
        // repeated-squaring path would use, where the sign of a negative base
        // is decided by a parity that can be lost.
        "2147483647", "2147483648", "2147483649", "1000000000001", "1e30",
    ];
    for base in LITERALS {
        for e in exponents {
            let (Some(tb), Some(te)) = (Reference::parse(base), Reference::parse(e)) else {
                continue;
            };
            let (Some(ob), Some(oe)) = (Ours::parse(base), Ours::parse(e)) else {
                continue;
            };
            both_approx(
                &mut r,
                &format!("{base} ** {e}"),
                || opt(tb.pow(&te)),
                || opt(ob.pow(&oe)),
            );
        }
    }
    r.finish("powers");
}

#[test]
fn integer_conversions_agree() {
    quiet_panics();
    let mut r = Report::new();
    for lit in LITERALS {
        let (Some(t), Some(o)) = (Reference::parse(lit), Ours::parse(lit)) else {
            continue;
        };
        macro_rules! convert {
            ($name:literal, $m:ident) => {
                both(
                    &mut r,
                    &format!("{}({lit})", $name),
                    || {
                        Num::$m(&t).map_or(Answer::Refused, |v| Answer::Value(v.to_string()))
                    },
                    || {
                        Num::$m(&o).map_or(Answer::Refused, |v| Answer::Value(v.to_string()))
                    },
                );
            };
        }
        convert!("to_i64", to_i64);
        convert!("to_u64", to_u64);
        convert!("to_i32", to_i32);
        convert!("to_u8", to_u8);
    }
    r.finish("integer conversions");
}

/// Formatting flags are part of the contract: dsntk renders FEEL numbers into
/// decision output and JSON through exactly these paths.
#[test]
fn formatting_flags_agree() {
    quiet_panics();
    let mut r = Report::new();
    for lit in LITERALS {
        let (Some(t), Some(o)) = (Reference::parse(lit), Ours::parse(lit)) else {
            continue;
        };
        for (name, tf, of) in [
            (
                "{:.2}",
                (|n: &Reference| format!("{n:.2}")) as fn(&Reference) -> String,
                (|n: &Ours| format!("{n:.2}")) as fn(&Ours) -> String,
            ),
            ("{:.6}", |n| format!("{n:.6}"), |n| format!("{n:.6}")),
            ("{:>12}", |n| format!("{n:>12}"), |n| format!("{n:>12}")),
            ("{:+}", |n| format!("{n:+}"), |n| format!("{n:+}")),
        ] {
            both(
                &mut r,
                &format!("{name} of {lit}"),
                || Answer::Value(tf(&t)),
                || Answer::Value(of(&o)),
            );
        }
    }
    r.finish("formatting");
}
