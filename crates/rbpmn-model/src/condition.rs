//! The deliberately tiny sequence-flow condition grammar:
//!
//! ```text
//! expr    := or
//! or      := and ("or" and)*
//! and     := atom ("and" atom)*
//! atom    := "(" expr ")" | pointer op literal
//! op      := == | != | < | <= | > | >=
//! pointer := RFC 6901 JSON pointer, e.g. /order/amount
//! literal := number | "string" | true | false | null
//! ```
//!
//! No scripting, no FEEL, no functions. Ordering operators require a number
//! literal. Decisions belong in application code — compute the decision
//! outside, store the result, let the gateway read a flag.

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
        pointer: String,
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

/// Validate an RFC 6901 JSON pointer as used by conditions and correlation keys.
/// The empty pointer (whole document) is rejected: keys and conditions must
/// address a location.
pub fn validate_pointer(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("JSON pointer must not be empty".to_string());
    }
    if !p.starts_with('/') {
        return Err(format!("JSON pointer must start with '/': '{p}'"));
    }
    let bytes = p.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'~' {
            match bytes.get(i + 1) {
                Some(b'0') | Some(b'1') => i += 2,
                _ => {
                    return Err(format!(
                        "invalid '~' escape in JSON pointer '{p}' (only ~0 and ~1 are allowed)"
                    ));
                }
            }
        } else {
            i += 1;
        }
    }
    Ok(())
}

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
    Ptr(String),
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
            return err(end, "expected '(' or a JSON pointer, found end of input");
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
            Tok::Ptr(pointer) => {
                self.pos += 1;
                validate_pointer(&pointer).map_err(|m| CondError { offset, message: m })?;
                let Some((op_offset, Tok::Op(op))) = self.toks.get(self.pos).cloned() else {
                    return err(
                        offset,
                        "expected a comparison operator (==, !=, <, <=, >, >=) after the JSON pointer",
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
                Ok(Expr::Cmp { pointer, op, value })
            }
            _ => err(
                offset,
                "expected '(' or a JSON pointer (conditions look like: /amount >= 100)",
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
                if bytes.get(i + 1) == Some(&b'=') {
                    toks.push((start, Tok::Op(CmpOp::Eq)));
                    i += 2;
                } else {
                    return err(start, "single '=' is not an operator; use '=='");
                }
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
            b'/' => {
                let mut j = i;
                while j < bytes.len() {
                    let c = bytes[j] as char;
                    if c.is_whitespace() || "()<>=!\"".contains(c) {
                        break;
                    }
                    j += 1;
                }
                toks.push((start, Tok::Ptr(src[i..j].to_string())));
                i = j;
            }
            b'-' | b'0'..=b'9' => {
                let mut j = i;
                if bytes[j] == b'-' {
                    j += 1;
                }
                let int_start = j;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == int_start {
                    return err(start, "'-' must be followed by digits");
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
                let n: f64 = src[i..j]
                    .parse()
                    .map_err(|_| CondError {
                        offset: start,
                        message: format!("invalid number '{}'", &src[i..j]),
                    })?;
                toks.push((start, Tok::Lit(Literal::Num(n))));
                i = j;
            }
            b if (b as char).is_ascii_alphabetic() => {
                let mut j = i;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let word = &src[i..j];
                let tok = match word {
                    "and" => Tok::And,
                    "or" => Tok::Or,
                    "true" => Tok::Lit(Literal::Bool(true)),
                    "false" => Tok::Lit(Literal::Bool(false)),
                    "null" => Tok::Lit(Literal::Null),
                    _ => {
                        return err(
                            start,
                            format!(
                                "unknown word '{word}' — variables are addressed with JSON pointers (/{word})"
                            ),
                        );
                    }
                };
                toks.push((start, tok));
                i = j;
            }
            _ => {
                let ch = src[i..].chars().next().unwrap();
                return err(i, format!("unexpected character '{ch}'"));
            }
        }
    }
    Ok(toks)
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
            ok("/approved == true"),
            Expr::Cmp {
                pointer: "/approved".into(),
                op: CmpOp::Eq,
                value: Literal::Bool(true)
            }
        );
        ok("/amount >= 100");
        ok("/amount < -1.5e3");
        ok("/status != \"open\"");
        ok("/parent/child~0x == null");
    }

    #[test]
    fn boolean_combinations() {
        ok("/a == 1 and /b == 2");
        ok("/a == 1 or /b == 2 and /c == 3");
        ok("(/a == 1 or /b == 2) and /c == 3");
        assert_eq!(
            ok("/a == 1 and /b == 2 and /c == 3"),
            Expr::And(vec![
                Expr::Cmp {
                    pointer: "/a".into(),
                    op: CmpOp::Eq,
                    value: Literal::Num(1.0)
                },
                Expr::Cmp {
                    pointer: "/b".into(),
                    op: CmpOp::Eq,
                    value: Literal::Num(2.0)
                },
                Expr::Cmp {
                    pointer: "/c".into(),
                    op: CmpOp::Eq,
                    value: Literal::Num(3.0)
                },
            ])
        );
    }

    #[test]
    fn rejects_scripting_and_sloppiness() {
        bad("");
        bad("amount > 100");
        bad("/a = 1");
        bad("/a && /b");
        bad("/amount > \"high\"");
        bad("/a == 1 extra");
        bad("${amount > 100}");
        bad("/a == ");
        bad("(/a == 1");
        bad("/a~2b == 1");
    }
}
