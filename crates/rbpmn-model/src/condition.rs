//! Sequence-flow conditions: a **strict subset of FEEL** (DMN 1.3+) with
//! identical syntax and semantics, so every v1 condition remains a valid,
//! identically-evaluating FEEL expression now that dsntk *has* landed, and
//! models authored by FEEL-aware tooling parse as-is. This evaluator stays
//! rbpmn's own regardless: conditions are validated at deploy, deploy
//! validation runs in the browser, and this crate must therefore reach wasm32
//! without dsntk. `just feel-parity` is what keeps the two honest.
//!
//! ```text
//! expr    := or
//! or      := and ("or" and)*
//! and     := atom ("and" atom)*
//! atom    := "(" expr ")" | name op literal
//! op      := = | != | < | <= | > | >=     ("==" accepted, normalized to "=")
//! name    := ident ("." ident)*           (FEEL qualified name)
//! literal := number | "string" | true | false | null
//! ```
//!
//! Nothing else: no functions, no arithmetic, no `in`/ranges, no date
//! literals — those are rejected with targeted messages. Two deliberate
//! strictness points beyond FEEL (a strict subset may reject more, it must
//! never evaluate differently):
//!   * ordering ops (`<` `<=` `>` `>=`) require a number literal
//!   * qualified-name segments are `[A-Za-z_][A-Za-z0-9_]*` (no spaces)
//!
//! Evaluation semantics — FEEL's ternary logic, collapsed to a boolean only
//! at the root (null -> false). A name resolves as a path into the instance's
//! JSONB variable document; a missing path is null. Two independent null
//! rules, verified against dsntk by `feel-parity`:
//!   * against the `null` literal: the null-check idiom, always a boolean
//!     (`x = null` is true iff x is missing/null; `x != null` its inverse)
//!   * against any other literal: a type mismatch, so null — **including
//!     under `!=`**. `x != 1` is null (-> false), not true, when x is
//!     missing: a missing variable must never satisfy a negative condition.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    fn is_ordering(self) -> bool {
        matches!(self, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Null,
    Bool(bool),
    Num(Num),
    Str(String),
}

/// A number literal, kept **as written**.
///
/// Not an `f64`. The variable document holds arbitrary-precision numbers (the
/// `arbitrary_precision` spike in `docs/dmn.md`), and once DMN decisions
/// started producing 34-digit decimals a comparator that read through `f64`
/// would answer `a = 1.0000000000000000002` **true** for a stored `…001`.
/// Storage became exact; this is comparison catching up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Num(String);

impl Num {
    /// The literal exactly as the model wrote it.
    pub fn text(&self) -> &str {
        &self.0
    }

    /// Its value as an `f64`, for callers that only need a magnitude — the
    /// state-space explorer picking neighbouring values, say. Never used to
    /// decide a comparison.
    pub fn as_f64(&self) -> f64 {
        self.0.parse().unwrap_or(f64::NAN)
    }
}

impl From<String> for Num {
    fn from(text: String) -> Self {
        Num(text)
    }
}

impl fmt::Display for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Cmp {
        /// FEEL qualified name as path segments: `order.priority` -> ["order", "priority"].
        path: Vec<String>,
        op: CmpOp,
        value: Literal,
    },
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CondError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for CondError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at offset {})", self.message, self.offset)
    }
}

impl std::error::Error for CondError {}

fn err<T>(offset: usize, message: impl Into<String>) -> Result<T, CondError> {
    Err(CondError {
        offset,
        message: message.into(),
    })
}

const SUBSET_HINT: &str = "not in the rbpmn FEEL subset — compute the value in \
                           application code and store the result as a variable";

pub fn parse(src: &str) -> Result<Expr, CondError> {
    let toks = lex(src)?;
    if toks.is_empty() {
        return err(0, "condition is empty");
    }
    let mut p = P { toks, pos: 0 };
    let expr = p.or_expr()?;
    if p.pos != p.toks.len() {
        let (offset, _) = p.toks[p.pos];
        return err(offset, "unexpected trailing input");
    }
    Ok(expr)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Op(CmpOp),
    Name(Vec<String>),
    Lit(Literal),
}

struct P {
    toks: Vec<(usize, Tok)>,
    pos: usize,
}

impl P {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(_, t)| t)
    }

    fn or_expr(&mut self) -> Result<Expr, CondError> {
        let mut parts = vec![self.and_expr()?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            parts.push(self.and_expr()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Expr::Or(parts)
        })
    }

    fn and_expr(&mut self) -> Result<Expr, CondError> {
        let mut parts = vec![self.atom()?];
        while self.peek() == Some(&Tok::And) {
            self.pos += 1;
            parts.push(self.atom()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().unwrap()
        } else {
            Expr::And(parts)
        })
    }

    fn atom(&mut self) -> Result<Expr, CondError> {
        let Some(&(offset, ref tok)) = self.toks.get(self.pos) else {
            let end = self.toks.last().map(|(o, _)| *o).unwrap_or(0);
            return err(end, "expected '(' or a variable name, found end of input");
        };
        match tok.clone() {
            Tok::LParen => {
                self.pos += 1;
                let inner = self.or_expr()?;
                if self.peek() != Some(&Tok::RParen) {
                    return err(offset, "unclosed '('");
                }
                self.pos += 1;
                Ok(inner)
            }
            Tok::Name(path) => {
                self.pos += 1;
                if let Some(&(paren_offset, Tok::LParen)) = self.toks.get(self.pos) {
                    return err(paren_offset, format!("function calls are {SUBSET_HINT}"));
                }
                let Some((op_offset, Tok::Op(op))) = self.toks.get(self.pos).cloned() else {
                    return err(
                        offset,
                        format!(
                            "expected a comparison operator (=, !=, <, <=, >, >=) after '{}'",
                            path.join(".")
                        ),
                    );
                };
                self.pos += 1;
                let Some((lit_offset, Tok::Lit(value))) = self.toks.get(self.pos).cloned() else {
                    return err(
                        op_offset,
                        "expected a literal (number, \"string\", true, false, null) after the operator",
                    );
                };
                self.pos += 1;
                if op.is_ordering() && !matches!(value, Literal::Num(_)) {
                    return err(
                        lit_offset,
                        "ordering comparisons (<, <=, >, >=) require a number literal",
                    );
                }
                Ok(Expr::Cmp { path, op, value })
            }
            _ => err(
                offset,
                "expected '(' or a variable name (conditions look like: \
                 amount >= 100 or order.tier = \"gold\")",
            ),
        }
    }
}

fn lex(src: &str) -> Result<Vec<(usize, Tok)>, CondError> {
    let bytes = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b if (b as char).is_whitespace() => i += 1,
            b'(' => {
                toks.push((start, Tok::LParen));
                i += 1;
            }
            b')' => {
                toks.push((start, Tok::RParen));
                i += 1;
            }
            b'=' => {
                // FEEL equality is '='; '==' is accepted and normalized.
                i += if bytes.get(i + 1) == Some(&b'=') {
                    2
                } else {
                    1
                };
                toks.push((start, Tok::Op(CmpOp::Eq)));
            }
            b'!' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push((start, Tok::Op(CmpOp::Ne)));
                    i += 2;
                } else {
                    return err(start, "'!' is not an operator; use '!='");
                }
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push((start, Tok::Op(CmpOp::Le)));
                    i += 2;
                } else {
                    toks.push((start, Tok::Op(CmpOp::Lt)));
                    i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push((start, Tok::Op(CmpOp::Ge)));
                    i += 2;
                } else {
                    toks.push((start, Tok::Op(CmpOp::Gt)));
                    i += 1;
                }
            }
            b'+' | b'*' | b'/' | b'%' => {
                return err(start, format!("arithmetic is {SUBSET_HINT}"));
            }
            b'[' | b']' => {
                return err(start, format!("range tests (in [1..10]) are {SUBSET_HINT}"));
            }
            b'"' => {
                let mut s = String::new();
                i += 1;
                loop {
                    match bytes.get(i) {
                        None => return err(start, "unterminated string literal"),
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(b'\\') => {
                            match bytes.get(i + 1) {
                                Some(b'"') => s.push('"'),
                                Some(b'\\') => s.push('\\'),
                                Some(b'/') => s.push('/'),
                                Some(b'n') => s.push('\n'),
                                Some(b't') => s.push('\t'),
                                Some(b'r') => s.push('\r'),
                                _ => return err(i, "unsupported string escape"),
                            }
                            i += 2;
                        }
                        Some(_) => {
                            // Multi-byte UTF-8: copy the whole char.
                            let ch = src[i..].chars().next().unwrap();
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                toks.push((start, Tok::Lit(Literal::Str(s))));
            }
            b'-' | b'0'..=b'9' => {
                if bytes[i] == b'-' && !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    return err(start, format!("arithmetic is {SUBSET_HINT}"));
                }
                let mut j = i;
                if bytes[j] == b'-' {
                    j += 1;
                }
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if bytes.get(j) == Some(&b'.') {
                    j += 1;
                    let frac_start = j;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j == frac_start {
                        return err(start, "digits required after decimal point");
                    }
                }
                if matches!(bytes.get(j), Some(b'e') | Some(b'E')) {
                    j += 1;
                    if matches!(bytes.get(j), Some(b'+') | Some(b'-')) {
                        j += 1;
                    }
                    let exp_start = j;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j == exp_start {
                        return err(start, "digits required in exponent");
                    }
                }
                // Parsed only to reject nonsense; the *text* is what is kept,
                // because f64 cannot hold every number a model may compare
                // against.
                src[i..j].parse::<f64>().map_err(|_| CondError {
                    offset: start,
                    message: format!("invalid number '{}'", &src[i..j]),
                })?;
                toks.push((start, Tok::Lit(Literal::Num(Num(src[i..j].to_string())))));
                i = j;
            }
            b if (b as char).is_ascii_alphabetic() || b == b'_' => {
                let mut segments = vec![read_segment(src, &mut i)];
                while bytes.get(i) == Some(&b'.') {
                    let next = bytes.get(i + 1).copied();
                    if !next.is_some_and(|c| (c as char).is_ascii_alphabetic() || c == b'_') {
                        return err(i, "expected a name after '.'");
                    }
                    i += 1;
                    segments.push(read_segment(src, &mut i));
                }
                let tok = if segments.len() == 1 {
                    match segments[0].as_str() {
                        "and" => Tok::And,
                        "or" => Tok::Or,
                        "true" => Tok::Lit(Literal::Bool(true)),
                        "false" => Tok::Lit(Literal::Bool(false)),
                        "null" => Tok::Lit(Literal::Null),
                        "in" | "between" | "not" | "instance" | "some" | "every" | "if" => {
                            return err(start, format!("'{}' is {SUBSET_HINT}", segments[0]));
                        }
                        _ => Tok::Name(segments),
                    }
                } else {
                    Tok::Name(segments)
                };
                toks.push((start, tok));
            }
            _ => {
                let ch = src[i..].chars().next().unwrap();
                return err(i, format!("unexpected character '{ch}'"));
            }
        }
    }
    Ok(toks)
}

fn read_segment(src: &str, i: &mut usize) -> String {
    let bytes = src.as_bytes();
    let start = *i;
    while *i < bytes.len() && (bytes[*i].is_ascii_alphanumeric() || bytes[*i] == b'_') {
        *i += 1;
    }
    src[start..*i].to_string()
}

/// Evaluate a condition against the instance variable document with **FEEL's
/// exact semantics** — verified against dsntk by `just feel-parity`, not
/// asserted, which is how `x != 1` was caught answering true for a missing
/// `x`. Ternary (three-valued) logic is used internally and collapsed to a
/// boolean only at the root: `null` becomes `false`.
///
/// The precise rules (all FEEL, DMN 1.3):
///   * a missing path evaluates to null
///   * equality treats null as an ordinary value (null-safe): `x = null` is
///     TRUE when x is null/missing (the idiomatic "is missing" test), false
///     otherwise — and so `x != 1` is TRUE when x is missing
///   * equality across different types (e.g. `5 = "5"`) is null -> false
///   * ordering comparisons need a number on both sides; anything else
///     (null, missing, non-number) is null -> false
///   * `and`/`or` are Kleene: `false and null` = false, `true and null` =
///     null; `true or null` = true, `false or null` = null
pub fn eval(expr: &Expr, variables: &serde_json::Value) -> bool {
    truth(expr, variables).unwrap_or(false)
}

/// Parse a bare FEEL qualified name (`order.id`) into its path segments —
/// the shared syntax between conditions and correlation-key bindings
/// (`Bindings::correlation` registers these; the XML never carries them). Same
/// strictness as the condition grammar: segments are
/// `[A-Za-z_][A-Za-z0-9_]*`, joined by single dots, no spaces.
pub fn parse_qname(src: &str) -> Result<Vec<String>, CondError> {
    let mut path = Vec::new();
    let mut offset = 0;
    for segment in src.split('.') {
        let valid = !segment.is_empty()
            && segment
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return err(
                offset,
                format!(
                    "'{src}' is not a FEEL qualified name \
                     (segments are [A-Za-z_][A-Za-z0-9_]*, joined by dots)"
                ),
            );
        }
        path.push(segment.to_string());
        offset += segment.len() + 1;
    }
    Ok(path)
}

/// Resolve a qualified-name path against the variable document. Missing
/// segments resolve to null — FEEL semantics, identical to how conditions
/// read variables.
pub fn resolve_path<'a>(
    variables: &'a serde_json::Value,
    path: &[String],
) -> &'a serde_json::Value {
    resolve(variables, path)
}

fn truth(expr: &Expr, variables: &serde_json::Value) -> Option<bool> {
    match expr {
        Expr::And(parts) => {
            let mut result = Some(true);
            for part in parts {
                result = match (result, truth(part, variables)) {
                    (Some(false), _) | (_, Some(false)) => return Some(false),
                    (Some(true), x) => x,
                    (None, _) => None,
                };
            }
            result
        }
        Expr::Or(parts) => {
            let mut result = Some(false);
            for part in parts {
                result = match (result, truth(part, variables)) {
                    (Some(true), _) | (_, Some(true)) => return Some(true),
                    (Some(false), x) => x,
                    (None, _) => None,
                };
            }
            result
        }
        Expr::Cmp { path, op, value } => compare(resolve(variables, path), *op, value),
    }
}

fn resolve<'a>(variables: &'a serde_json::Value, path: &[String]) -> &'a serde_json::Value {
    let mut current = variables;
    for segment in path {
        match current.get(segment) {
            Some(next) => current = next,
            None => return &serde_json::Value::Null,
        }
    }
    current
}

/// Compare two decimal numbers written as text, exactly.
///
/// Both sides can be wider than an `f64`: the stored value because the
/// variable document is arbitrary-precision, the literal because a model may
/// write any number it likes. Going through `f64` would collapse values that
/// differ past the 17th digit, which is a wrong branch rather than a rounding
/// error.
///
/// Returns `None` only when a side is not a decimal number at all (`NaN`,
/// `Infinity` — which JSON cannot hold anyway).
fn compare_decimals(a: &str, b: &str) -> Option<Ordering> {
    let (a_neg, a_digits, a_exp) = decimal_parts(a)?;
    let (b_neg, b_digits, b_exp) = decimal_parts(b)?;
    // Zero has no sign here: `-0` and `0` are the same number.
    let (a_zero, b_zero) = (a_digits.is_empty(), b_digits.is_empty());
    if a_zero || b_zero {
        return Some(match (a_zero, b_zero) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if b_neg {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, true) => {
                if a_neg {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, false) => unreachable!(),
        });
    }
    if a_neg != b_neg {
        return Some(if a_neg {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let magnitude = compare_magnitudes(&a_digits, a_exp, &b_digits, b_exp);
    Some(if a_neg {
        magnitude.reverse()
    } else {
        magnitude
    })
}

/// Compare two non-zero magnitudes given as significant digits and the
/// decimal exponent of the *last* digit.
///
/// The arithmetic is `i128` because the exponent is `i64` and the digit count
/// is added to it: `1e9223372036854775807` is a number `arbitrary_precision`
/// now *accepts* from an ordinary API caller, and in `i64` that sum panicked
/// in debug and wrapped — into a wrong branch — in release.
fn compare_magnitudes(a: &str, a_exp: i128, b: &str, b_exp: i128) -> Ordering {
    // Where the most significant digit sits. Different positions decide it
    // outright, which is what makes this cheap for the common case.
    let a_top = a.len() as i128 + a_exp;
    let b_top = b.len() as i128 + b_exp;
    if a_top != b_top {
        return a_top.cmp(&b_top);
    }
    // Same position: compare digit by digit, treating a shorter number as
    // padded with zeros.
    let width = a.len().max(b.len());
    let pad = |s: &str| {
        let mut out = s.to_string();
        out.push_str(&"0".repeat(width - s.len()));
        out
    };
    pad(a).cmp(&pad(b))
}

/// `"-1.50e2"` -> `(true, "15", 1)`: sign, significant digits with leading and
/// trailing zeros stripped, and the decimal exponent of the last kept digit.
/// Empty digits mean zero.
fn decimal_parts(text: &str) -> Option<(bool, String, i128)> {
    let text = text.trim();
    let (negative, rest) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
        // `i64` on the way in — a literal outside that range is not a number
        // this can compare — then `i128` for the arithmetic below.
        Some((m, e)) => (m, i128::from(e.parse::<i64>().ok()?)),
        None => (rest, 0),
    };
    let (integer, fraction) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if integer.is_empty() && fraction.is_empty() {
        return None;
    }
    if !integer
        .bytes()
        .chain(fraction.bytes())
        .all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let mut digits = String::with_capacity(integer.len() + fraction.len());
    digits.push_str(integer);
    digits.push_str(fraction);
    let mut exp = exponent - fraction.len() as i128;
    // Strip trailing zeros (raising the exponent) then leading zeros, so
    // `1.50` and `1.5` become the same (digits, exponent) pair.
    while digits.ends_with('0') {
        digits.pop();
        exp += 1;
    }
    let digits = digits.trim_start_matches('0').to_string();
    if digits.is_empty() {
        exp = 0;
    }
    Some((negative, digits, exp))
}

fn compare(actual: &serde_json::Value, op: CmpOp, literal: &Literal) -> Option<bool> {
    use serde_json::Value;
    match op {
        CmpOp::Eq | CmpOp::Ne => {
            // Two independent FEEL rules, deliberately separate arms — folding
            // them into one is what made `x != 1` true for a missing x.
            let eq = match (actual, literal) {
                // (A) Comparison against the `null` literal is the null-check
                // idiom: always a boolean, never null.
                (Value::Null, Literal::Null) => Some(true),
                (_, Literal::Null) => Some(false),
                // (B) Otherwise a type mismatch yields null — and null is a
                // type, so a missing value against any non-null literal is
                // null too (hence `x != 1` is null, not true, when x is
                // missing).
                (Value::Null, _) => None,
                (Value::Bool(a), Literal::Bool(b)) => Some(a == b),
                (Value::Number(a), Literal::Num(b)) => {
                    Some(compare_decimals(&a.to_string(), b.text()) == Some(Ordering::Equal))
                }
                (Value::String(a), Literal::Str(b)) => Some(a == b),
                _ => None,
            };
            match op {
                CmpOp::Eq => eq,
                _ => eq.map(|b| !b),
            }
        }
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            let (Value::Number(a), Literal::Num(b)) = (actual, literal) else {
                return None;
            };
            let ordering = compare_decimals(&a.to_string(), b.text())?;
            Some(match op {
                CmpOp::Lt => ordering == Ordering::Less,
                CmpOp::Le => ordering != Ordering::Greater,
                CmpOp::Gt => ordering == Ordering::Greater,
                CmpOp::Ge => ordering != Ordering::Less,
                _ => unreachable!(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Expr {
        parse(src).unwrap_or_else(|e| panic!("'{src}' should parse: {e}"))
    }

    fn bad(src: &str) -> CondError {
        match parse(src) {
            Ok(e) => panic!("'{src}' should be rejected, got {e:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn simple_comparisons() {
        assert_eq!(
            ok("approved = true"),
            Expr::Cmp {
                path: vec!["approved".into()],
                op: CmpOp::Eq,
                value: Literal::Bool(true)
            }
        );
        // '==' is accepted on input and normalized to FEEL's '='.
        assert_eq!(ok("approved == true"), ok("approved = true"));
        ok("amount >= 100");
        ok("amount < -1.5e3");
        ok("status != \"open\"");
        ok("status = null");
    }

    #[test]
    fn qualified_names() {
        assert_eq!(
            ok("order.priority = \"high\""),
            Expr::Cmp {
                path: vec!["order".into(), "priority".into()],
                op: CmpOp::Eq,
                value: Literal::Str("high".into())
            }
        );
        ok("a.b.c_2 > 0");
        bad("a. = 1");
        bad("a.1 = 1");
    }

    #[test]
    fn boolean_combinations() {
        ok("a = 1 and b = 2");
        ok("a = 1 or b = 2 and c = 3");
        ok("(a = 1 or b = 2) and c = 3");
        assert_eq!(
            ok("a = 1 and b = 2 and c = 3"),
            Expr::And(vec![
                Expr::Cmp {
                    path: vec!["a".into()],
                    op: CmpOp::Eq,
                    value: Literal::Num(Num("1".into()))
                },
                Expr::Cmp {
                    path: vec!["b".into()],
                    op: CmpOp::Eq,
                    value: Literal::Num(Num("2".into()))
                },
                Expr::Cmp {
                    path: vec!["c".into()],
                    op: CmpOp::Eq,
                    value: Literal::Num(Num("3".into()))
                },
            ])
        );
    }

    #[test]
    fn full_feel_constructs_are_rejected() {
        assert!(bad("sum(items) > 3").message.contains("function calls"));
        assert!(bad("amount + fee > 100").message.contains("arithmetic"));
        assert!(bad("amount in [1..100]").message.contains("'in'"));
        assert!(bad("x between 1 and 3").message.contains("'between'"));
        assert!(bad("not approved").message.contains("'not'"));
        assert!(bad("if a then b else c").message.contains("'if'"));
        assert!(bad("amount - fee > 0").message.contains("arithmetic"));
    }

    #[test]
    fn rejects_sloppiness() {
        bad("");
        bad("/approved == true");
        bad("amount > \"high\"");
        bad("a = 1 extra");
        bad("${amount > 100}");
        bad("a = ");
        bad("(a = 1");
        bad("a && b");
    }

    /// The reason `Literal::Num` keeps its text. Once decisions started
    /// producing 34-digit decimals and the variable document started holding
    /// them exactly, an `f64` comparator would answer these wrong — and a
    /// wrong comparison is a token down the wrong branch, not a rounding
    /// error.
    #[test]
    fn comparison_is_exact_beyond_f64() {
        let vars: serde_json::Value = serde_json::from_str(
            r#"{ "a": 1.0000000000000000001,
                  "big": 123456789012345678901234567890,
                  "price": 1.50 }"#,
        )
        .unwrap();
        let t = |src: &str| eval(&ok(src), &vars);

        // Differing past the 17th digit: f64 called these equal.
        assert!(!t("a = 1.0000000000000000002"));
        assert!(t("a = 1.0000000000000000001"));
        assert!(t("a < 1.0000000000000000002"));
        assert!(!t("a > 1.0000000000000000002"));

        // A 30-digit integer, likewise.
        assert!(t("big = 123456789012345678901234567890"));
        assert!(!t("big = 123456789012345678901234567891"));
        assert!(t("big > 123456789012345678901234567889"));

        // Trailing zeros are not significance: 1.50 is 1.5.
        assert!(t("price = 1.5"));
        assert!(t("price = 1.50"));
        assert!(t("price >= 1.5"));
        assert!(!t("price > 1.5"));
    }

    #[test]
    fn decimal_comparison_handles_the_awkward_shapes() {
        let cmp = |a: &str, b: &str| compare_decimals(a, b);
        assert_eq!(cmp("0", "-0"), Some(Ordering::Equal));
        assert_eq!(cmp("0.0", "0"), Some(Ordering::Equal));
        assert_eq!(cmp("-1", "1"), Some(Ordering::Less));
        assert_eq!(cmp("-2", "-1"), Some(Ordering::Less));
        assert_eq!(cmp("10", "9"), Some(Ordering::Greater));
        assert_eq!(cmp("1e2", "100"), Some(Ordering::Equal));
        assert_eq!(cmp("1E2", "100.00"), Some(Ordering::Equal));
        assert_eq!(cmp("1.5e-1", "0.15"), Some(Ordering::Equal));
        assert_eq!(cmp("-0.0", "0"), Some(Ordering::Equal));
        assert_eq!(cmp("0.1", "0.09999"), Some(Ordering::Greater));
        assert_eq!(cmp("100", "1e3"), Some(Ordering::Less));
        // Not numbers at all.
        assert_eq!(cmp("abc", "1"), None);
        assert_eq!(cmp("1", ""), None);

        // Exponents at the edge of the type they are parsed into. The digit
        // count is *added* to the exponent to find the leading digit's
        // position, and in `i64` that sum panicked in debug and — worse —
        // wrapped into the opposite branch in release. `serde_json`'s
        // `arbitrary_precision` hands these straight through from an API
        // caller's payload, so it is reachable input, not a curiosity.
        let max = i64::MAX.to_string();
        let min = i64::MIN.to_string();
        assert_eq!(cmp(&format!("1e{max}"), "1"), Some(Ordering::Greater));
        assert_eq!(cmp("1", &format!("1e{max}")), Some(Ordering::Less));
        assert_eq!(cmp(&format!("1e{min}"), "1"), Some(Ordering::Less));
        assert_eq!(cmp(&format!("-1e{max}"), "1"), Some(Ordering::Less));
        assert_eq!(
            cmp(&format!("1e{max}"), &format!("1e{max}")),
            Some(Ordering::Equal)
        );
        assert_eq!(
            cmp(&format!("1.2345e{max}"), &format!("1.2344e{max}")),
            Some(Ordering::Greater)
        );
        // Past `i64` the literal is not a number this can compare, and saying
        // so beats guessing.
        assert_eq!(cmp("1e9223372036854775808", "1"), None);
    }

    /// The same overflow, through the door it actually arrives by.
    #[test]
    fn an_extreme_exponent_from_a_payload_does_not_overflow() {
        let vars: serde_json::Value =
            serde_json::from_str(r#"{"a": 1e9223372036854775807, "b": -1e9223372036854775807}"#)
                .unwrap();
        assert!(eval(&ok("a > 1"), &vars));
        assert!(!eval(&ok("a < 1"), &vars));
        assert!(eval(&ok("b < 1"), &vars));
        assert!(eval(&ok("a != 1"), &vars));
        // ...and against a literal at the same extreme, from the other side.
        assert!(eval(&ok("a = 1e9223372036854775807"), &vars));
        assert!(!eval(&ok("a > 1e9223372036854775807"), &vars));
    }

    #[test]
    fn evaluation_follows_feel() {
        use serde_json::json;
        let vars = json!({
            "amount": 250,
            "vip": false,
            "order": { "tier": "gold" },
            "note": "hi",
            "gone": null
        });
        let t = |src: &str| eval(&ok(src), &vars);

        assert!(t("amount >= 100"));
        assert!(!t("amount < 100"));
        assert!(t("order.tier = \"gold\""));
        assert!(t("vip = false"));
        assert!(t("amount >= 100 and (order.tier = \"gold\" or vip = true)"));

        // Rule A — comparison against the `null` literal is the null-check
        // idiom: a boolean, both directions, whether missing or explicitly null.
        assert!(t("missing = null"));
        assert!(t("gone = null"));
        assert!(t("order.nope = null"));
        assert!(!t("missing != null"));
        assert!(t("amount != null"));
        assert!(!t("amount = null"));

        // Rule B — anything else against a null/missing value is a type
        // mismatch, so it is null (-> false at the root). Crucially `!=` too:
        // negating null is null, NOT true. A missing variable must never
        // affirmatively satisfy a negative condition.
        assert!(!t("missing = 1"));
        assert!(!t("missing != 1"));
        assert!(!t("gone = 1"));
        assert!(!t("gone != 1"));
        assert!(!t("missing > 1"));

        // Rule B applies identically to non-null type mismatches — null is
        // just another type, which is the whole point of keeping them one rule.
        assert!(!t("note = 5"));
        assert!(!t("note != 5"));
        assert!(!t("note > 1"));

        // Kleene and/or.
        assert!(t("vip = false or missing = 1")); // true or null -> true... false-or: first true wins
        assert!(!t("missing = 1 or missing = 2"));
        assert!(!t("amount > 100 and missing > 1")); // true and null -> null -> false
        assert!(!t("amount < 100 and missing > 1")); // false and null -> false
        assert!(t("amount < 100 or amount > 100"));
    }
}
