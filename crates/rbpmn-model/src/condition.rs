//! Sequence-flow conditions: a **strict subset of FEEL** (DMN 1.3+) with
//! identical syntax and semantics, so every v1 condition remains a valid,
//! identically-evaluating FEEL expression when dsntk lands post-v1, and
//! models authored by FEEL-aware tooling parse as-is.
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
//! Evaluation semantics (the phase-1 evaluator must implement exactly this):
//! a name resolves as a path into the instance's JSONB variable document; a
//! missing path evaluates to null, and any comparison with null is **false**
//! (FEEL's ternary logic collapsed to the final boolean).

use serde::{Deserialize, Serialize};
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
    Num(f64),
    Str(String),
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
                let n: f64 = src[i..j].parse().map_err(|_| CondError {
                    offset: start,
                    message: format!("invalid number '{}'", &src[i..j]),
                })?;
                toks.push((start, Tok::Lit(Literal::Num(n))));
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
/// exact semantics**, so dsntk can swap in post-v1 without any behavior
/// change. Ternary (three-valued) logic is used internally and collapsed to a
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

fn compare(actual: &serde_json::Value, op: CmpOp, literal: &Literal) -> Option<bool> {
    use serde_json::Value;
    match op {
        CmpOp::Eq | CmpOp::Ne => {
            let eq = match (actual, literal) {
                (Value::Null, Literal::Null) => Some(true),
                (Value::Null, _) | (_, Literal::Null) => Some(false),
                (Value::Bool(a), Literal::Bool(b)) => Some(a == b),
                (Value::Number(a), Literal::Num(b)) => Some(a.as_f64() == Some(*b)),
                (Value::String(a), Literal::Str(b)) => Some(a == b),
                _ => None, // incomparable types: FEEL yields null
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
            let a = a.as_f64()?;
            Some(match op {
                CmpOp::Lt => a < *b,
                CmpOp::Le => a <= *b,
                CmpOp::Gt => a > *b,
                CmpOp::Ge => a >= *b,
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
                    value: Literal::Num(1.0)
                },
                Expr::Cmp {
                    path: vec!["b".into()],
                    op: CmpOp::Eq,
                    value: Literal::Num(2.0)
                },
                Expr::Cmp {
                    path: vec!["c".into()],
                    op: CmpOp::Eq,
                    value: Literal::Num(3.0)
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

        // Missing/null: `= null` is the "is missing" test; everything else
        // involving null is false.
        assert!(t("missing = null"));
        assert!(t("gone = null"));
        assert!(t("order.nope = null"));
        assert!(!t("missing = 1"));
        assert!(t("missing != 1")); // equality is null-safe: null != 1 is true
        assert!(!t("missing > 1"));
        assert!(t("amount != null"));

        // Type mismatch is null -> false, in both directions.
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
