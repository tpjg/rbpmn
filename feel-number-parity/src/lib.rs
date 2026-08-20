//! Bridge between the two implementations. The check itself is
//! `tests/parity.rs`.
//!
//! Both types have the same inherent API — that is the whole premise of the
//! substitution — so one trait covers both and every case runs twice through
//! identical source.

use std::fmt::{Debug, Display};

/// What an implementation answered, in a form that can be compared as text.
/// Numbers are compared through `Display` because that is what dsntk's FEEL
/// values, its JSON output and the DMN TCK's expected results all use: a
/// difference invisible to `Display` is invisible to every caller.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Answer {
    Value(String),
    /// A `Debug` rendering — the C library's canonical `+coefficientEexponent`
    /// form, which exposes the stored scale that `Display` can hide.
    Raw(String),
    Bool(bool),
    /// The operation refused (parse failure, `None`, conversion error).
    Refused,
    /// The implementation panicked. The reference does, in two places: its
    /// `Display` splits `bid128_to_string` on `E` and unwraps, which infinity
    /// and NaN have no `E` in; and `validate_scale` adds `bid128_ilogb` to the
    /// requested scale in wrapping-free `i32`, which overflows for extreme
    /// values. Recording it as an outcome keeps the run alive and turns "the
    /// reference crashes here" into a documented deviation rather than a dead
    /// test process.
    Panicked,
}

/// Run one side, converting a panic into [`Answer::Panicked`].
pub fn guard(f: impl FnOnce() -> Answer) -> Answer {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(Answer::Panicked)
}

/// Silence panic output for the duration of a differential run — the
/// reference panics by design in the cases above, and a few hundred
/// backtraces would bury the report.
pub fn quiet_panics() {
    std::panic::set_hook(Box::new(|_| {}));
}

/// The operations both implementations must agree on.
pub trait Num: Sized + Copy + Display + Debug + PartialOrd {
    fn parse(s: &str) -> Option<Self>;
    fn new(n: i64, s: i32) -> Self;
    fn from_i64(n: i64) -> Self;

    fn add(self, rhs: Self) -> Self;
    fn sub(self, rhs: Self) -> Self;
    fn mul(self, rhs: Self) -> Self;
    fn div(self, rhs: Self) -> Self;
    fn rem(self, rhs: Self) -> Self;
    fn neg(self) -> Self;

    fn abs(&self) -> Self;
    fn trunc(&self) -> Self;
    fn frac(&self) -> Self;
    fn round(&self, rhs: &Self) -> Self;

    fn ceiling(&self, scale: i32) -> Option<Self>;
    fn floor(&self, scale: i32) -> Option<Self>;
    fn round_down(&self, scale: i32) -> Option<Self>;
    fn round_up(&self, scale: i32) -> Option<Self>;
    fn round_half_up(&self, scale: i32) -> Option<Self>;
    fn round_half_down(&self, scale: i32) -> Option<Self>;

    fn exp(&self) -> Option<Self>;
    fn ln(&self) -> Option<Self>;
    fn sqrt(&self) -> Option<Self>;
    fn square(&self) -> Option<Self>;
    fn pow(&self, rhs: &Self) -> Option<Self>;

    fn even(&self) -> bool;
    fn odd(&self) -> bool;
    fn is_integer(&self) -> bool;
    fn is_zero(&self) -> bool;
    fn is_one(&self) -> bool;
    fn is_negative(&self) -> bool;
    fn is_positive(&self) -> bool;

    fn to_i64(&self) -> Option<i64>;
    fn to_u64(&self) -> Option<u64>;
    fn to_i32(&self) -> Option<i32>;
    fn to_u8(&self) -> Option<u8>;
}

/// Both implementations expose identical signatures, so one macro writes both
/// impls — and a divergence in the *API* becomes a compile error rather than
/// something the differential has to notice at runtime.
macro_rules! impl_num {
    ($t:ty) => {
        impl Num for $t {
            fn parse(s: &str) -> Option<Self> {
                s.parse::<Self>().ok()
            }
            fn new(n: i64, s: i32) -> Self {
                <$t>::new(n, s)
            }
            fn from_i64(n: i64) -> Self {
                n.into()
            }

            fn add(self, rhs: Self) -> Self {
                self + rhs
            }
            fn sub(self, rhs: Self) -> Self {
                self - rhs
            }
            fn mul(self, rhs: Self) -> Self {
                self * rhs
            }
            fn div(self, rhs: Self) -> Self {
                self / rhs
            }
            fn rem(self, rhs: Self) -> Self {
                self % rhs
            }
            fn neg(self) -> Self {
                -self
            }

            fn abs(&self) -> Self {
                <$t>::abs(self)
            }
            fn trunc(&self) -> Self {
                <$t>::trunc(self)
            }
            fn frac(&self) -> Self {
                <$t>::frac(self)
            }
            fn round(&self, rhs: &Self) -> Self {
                <$t>::round(self, rhs)
            }

            fn ceiling(&self, scale: i32) -> Option<Self> {
                <$t>::ceiling(self, scale).ok()
            }
            fn floor(&self, scale: i32) -> Option<Self> {
                <$t>::floor(self, scale).ok()
            }
            fn round_down(&self, scale: i32) -> Option<Self> {
                <$t>::round_down(self, scale).ok()
            }
            fn round_up(&self, scale: i32) -> Option<Self> {
                <$t>::round_up(self, scale).ok()
            }
            fn round_half_up(&self, scale: i32) -> Option<Self> {
                <$t>::round_half_up(self, scale).ok()
            }
            fn round_half_down(&self, scale: i32) -> Option<Self> {
                <$t>::round_half_down(self, scale).ok()
            }

            fn exp(&self) -> Option<Self> {
                <$t>::exp(self)
            }
            fn ln(&self) -> Option<Self> {
                <$t>::ln(self)
            }
            fn sqrt(&self) -> Option<Self> {
                <$t>::sqrt(self)
            }
            fn square(&self) -> Option<Self> {
                <$t>::square(self)
            }
            fn pow(&self, rhs: &Self) -> Option<Self> {
                <$t>::pow(self, rhs)
            }

            fn even(&self) -> bool {
                <$t>::even(self)
            }
            fn odd(&self) -> bool {
                <$t>::odd(self)
            }
            fn is_integer(&self) -> bool {
                <$t>::is_integer(self)
            }
            fn is_zero(&self) -> bool {
                <$t>::is_zero(self)
            }
            fn is_one(&self) -> bool {
                <$t>::is_one(self)
            }
            fn is_negative(&self) -> bool {
                <$t>::is_negative(self)
            }
            fn is_positive(&self) -> bool {
                <$t>::is_positive(self)
            }

            fn to_i64(&self) -> Option<i64> {
                i64::try_from(self).ok()
            }
            fn to_u64(&self) -> Option<u64> {
                u64::try_from(self).ok()
            }
            fn to_i32(&self) -> Option<i32> {
                i32::try_from(self).ok()
            }
            fn to_u8(&self) -> Option<u8> {
                u8::try_from(self).ok()
            }
        }
    };
}

impl_num!(dsntk_feel_number::FeelNumber);
impl_num!(dsntk_feel_number_pure::FeelNumber);

/// The reference: Intel's decimal floating-point library, through upstream
/// dsntk 0.3.0 exactly as published on crates.io.
pub type Reference = dsntk_feel_number::FeelNumber;
/// The replacement under test: the *same crate* from rbpmn's fork, built with
/// `use-fastnum`. Same package name, different source — which is why it is
/// renamed here and why both can sit in one graph.
pub type Ours = dsntk_feel_number_pure::FeelNumber;

pub fn value<N: Num>(n: N) -> Answer {
    Answer::Value(format!("{n}"))
}

pub fn raw<N: Num>(n: N) -> Answer {
    Answer::Raw(format!("{n:?}"))
}

pub fn opt<N: Num>(n: Option<N>) -> Answer {
    match n {
        Some(n) => Answer::Value(format!("{n}")),
        None => Answer::Refused,
    }
}

/// How far apart two decimal renderings are, as a relative difference, for
/// reporting a divergence's *magnitude* rather than only its existence.
///
/// Computed with the **reference** implementation, not ours: the thing under
/// test must not be the thing that judges whether it passed. Returns `None`
/// when either side is not a plain decimal (infinity, NaN, a refusal), where
/// "how far apart" has no meaning.
pub fn relative_difference(a: &str, b: &str) -> Option<f64> {
    let (a, b) = (Reference::parse(a)?, Reference::parse(b)?);
    if a == b {
        return Some(0.0);
    }
    let scale = if a.abs() > b.abs() { a.abs() } else { b.abs() };
    if scale.is_zero() {
        return Some(0.0);
    }
    // Only the *ratio* goes through f64, which is far inside its range even
    // when the operands are not.
    a.sub(b).abs().div(scale).to_string().parse().ok()
}

/// Whether two answers are the same number, whatever the rendering.
pub fn same_value(a: &str, b: &str) -> bool {
    match (Reference::parse(a), Reference::parse(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The deviation classes Gate 0 accepted, each with the reason it is
/// accepted. Anything outside them fails the run — the `just tla` discipline,
/// where expected failures are named rather than tolerated in bulk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deviation {
    /// The reference panicked and we returned a value. Two upstream bugs:
    /// `Display` unwraps a split on `E` that infinity and NaN do not have,
    /// and `validate_scale` adds `bid128_ilogb` (which is `i32::MIN` for
    /// zero) to the requested scale in checked `i32`.
    ReferencePanics,
    /// Same number, different number of rendered zero digits. Only ever a
    /// *zero* result: the reference expands a positive exponent into zeros,
    /// so `0 / 0.1` renders as `00` — which is also what its `jsonify` emits,
    /// making that JSON invalid. Declining to reproduce it is deliberate.
    ZeroRendering,
    /// A transcendental's last digits. Two independent implementations of
    /// `exp`, `ln`, `sqrt` and `pow` do not agree bit-for-bit at 34 digits,
    /// and the DMN specification does not require them to.
    TranscendentalTail,
}

/// Decide whether a divergence is one Gate 0 already accounted for.
///
/// `tolerance` is the relative difference allowed for this call site: zero
/// everywhere except the transcendentals, so a drift in exact arithmetic can
/// never be absorbed by a loose global threshold.
pub fn classify(theirs: &Answer, ours: &Answer, tolerance: f64) -> Option<Deviation> {
    match (theirs, ours) {
        (Answer::Panicked, o) if !matches!(o, Answer::Panicked) => {
            Some(Deviation::ReferencePanics)
        }
        // The canonical form of a zero carries the exponent the operation
        // produced, and the reference's exponent follows IEEE's preferred-
        // exponent rule for the operation while fastnum's does not. Same
        // value, and the only thing that reads it is `Display`.
        (Answer::Raw(t), Answer::Raw(o))
            if t.starts_with("+0E") | t.starts_with("-0E")
                && (o.starts_with("+0E") | o.starts_with("-0E"))
                && t.starts_with(&o[..2]) =>
        {
            Some(Deviation::ZeroRendering)
        }
        (Answer::Value(t), Answer::Value(o)) => {
            if same_value(t, o) {
                Some(Deviation::ZeroRendering)
            } else if tolerance > 0.0
                && relative_difference(t, o).is_some_and(|d| d <= tolerance)
            {
                Some(Deviation::TranscendentalTail)
            } else {
                None
            }
        }
        _ => None,
    }
}
