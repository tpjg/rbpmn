//! Decisions must be deterministic and self-contained (`docs/dmn.md`, D7).
//!
//! Two capabilities break that, refused in the two different places they live:
//!
//! * **`now()` and `today()`** — the only clock builtins among FEEL's 86, and
//!   dsntk answers them from `chrono::Local`, so the *node's timezone* would
//!   decide a business rule. Found here, in the expression text.
//! * **External Java and PMML functions** — a FEEL expression opening an HTTP
//!   connection to a JVM on localhost. That is a property of the model rather
//!   than of any text, so [`crate::expressions`] finds it from `FunctionKind`.
//!
//! # Why this reads text instead of walking a syntax tree
//!
//! It walked the AST first. That was unsound, and the fixtures caught it:
//!
//! ```text
//! parse("now()",           empty scope) -> FunctionInvocation(Name("now"))   found
//! parse("Amount + now()",  empty scope) -> Name("Amount+now")                MISSED
//! ```
//!
//! FEEL names may contain spaces, `+`, `-` and `*`, so where a name *ends*
//! depends on which names are in scope. dsntk builds that scope from the
//! model when it compiles; anything parsing an expression on its own is
//! guessing, and guessing wrong here means a clock call sails through. The
//! same scope-lessness also made `if Flag then now() else 1` fail to parse at
//! all, which would have rejected a perfectly good model.
//!
//! So the check is deliberately **lexical and conservative**. It errs toward
//! refusing: a false positive is a modeler renaming something, a false
//! negative is a decision whose answer depends on which machine ran it. The
//! message says as much.
//!
//! This is still only the loud half. The wall is removing the builtin from
//! the evaluator, which rides with the decision about where the reqwest
//! removal lives (`docs/dmn.md`, "Known warts").

/// The clock builtins. Not a general denylist — these are the only two
/// non-deterministic functions FEEL has.
pub const BANNED: [&str; 2] = ["now", "today"];

/// The banned builtins this expression appears to call, in source order,
/// deduplicated.
///
/// "Appears to call" is exact: a call is a name followed by `(`, and string
/// literals are skipped so a message reading `"run now()"` is not a finding.
pub fn banned_calls(text: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = Vec::new();
    for (at, _) in code_spans(text) {
        for name in BANNED {
            if found.contains(&name) {
                continue;
            }
            if calls_at(text, at, name) {
                found.push(name);
            }
        }
    }
    found
}

/// Is `name` called at byte offset `at`?
fn calls_at(text: &str, at: usize, name: &str) -> bool {
    if !text[at..].starts_with(name) {
        return false;
    }
    // Not part of a longer word: `snow(` and `nowhere(` are somebody's own
    // names, not the builtin.
    let before_ok = text[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric() && c != '_');
    if !before_ok {
        return false;
    }
    let rest = &text[at + name.len()..];
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return false;
    }
    rest.trim_start().starts_with('(')
}

/// Byte offsets of every character outside a string literal, so a quoted
/// `"now()"` is text rather than a call. FEEL strings are double-quoted with
/// backslash escapes.
fn code_spans(text: &str) -> Vec<(usize, char)> {
    let mut out = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            continue;
        }
        out.push((at, c));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_call_is_found() {
        assert_eq!(banned_calls("now()"), ["now"]);
        assert_eq!(banned_calls("today()"), ["today"]);
        assert_eq!(banned_calls(" now () "), ["now"]);
    }

    /// The case that sank the AST walk: an operand in front of the call.
    #[test]
    fn an_operand_in_front_does_not_hide_it() {
        for text in [
            "Amount + now()",
            "Amount - now()",
            "Amount * 2 + now()",
            "a.b + now()",
            "Amount+now()",
            "date and time(now())",
            "if Flag then now() else 1",
            "for x in Items return x + today()",
            "sum([1, now()])",
        ] {
            assert!(!banned_calls(text).is_empty(), "`{text}` should be refused");
        }
    }

    #[test]
    fn longer_names_are_not_the_builtin() {
        for text in [
            "snow(1)",
            "nowhere(1)",
            "knowledge(1)",
            "todays(1)",
            "yesterday(1)",
            "my_now(1)",
        ] {
            assert!(banned_calls(text).is_empty(), "`{text}` is not the builtin");
        }
    }

    /// A name is only a call when it is called; a variable that happens to be
    /// spelled `now` is somebody's data.
    #[test]
    fn a_name_that_is_not_called_is_not_a_call() {
        assert!(banned_calls("now").is_empty());
        assert!(banned_calls("order.now").is_empty());
        assert!(banned_calls("now + 1").is_empty());
    }

    #[test]
    fn string_literals_are_text_not_code() {
        assert!(banned_calls(r#""please run now()""#).is_empty());
        assert!(banned_calls(r#""today() is the day""#).is_empty());
        // ...but code after a string still counts.
        assert_eq!(banned_calls(r#""ok" + string(now())"#), ["now"]);
        // ...and an escaped quote does not end the string early.
        assert!(banned_calls(r#""a \" now() b""#).is_empty());
    }

    #[test]
    fn deterministic_temporal_expressions_are_untouched() {
        for text in [
            "date(\"2026-08-14\")",
            "date and time(\"2026-08-14T10:00:00\")",
            "Applied + duration(\"P30D\")",
            "years and months duration(Start, End)",
        ] {
            assert!(banned_calls(text).is_empty(), "`{text}` should be allowed");
        }
    }

    #[test]
    fn each_builtin_is_reported_once() {
        assert_eq!(banned_calls("now() + now() + today()"), ["now", "today"]);
    }
}
