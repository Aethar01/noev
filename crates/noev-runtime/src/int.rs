//! `Int`: an integer of any size (`spec/types.md` §1). Values that fit in 64 bits are kept
//! as `i64`, so the common case costs a checked machine operation; others are shared big
//! integers.

use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

use num_bigint::BigInt;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};

/// An `Int` value. Always in its smallest form: `Big` only for values outside `i64`, so
/// equal values have equal representations.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Int {
    /// A value that fits in 64 bits.
    Small(i64),
    /// A value that does not.
    Big(Rc<BigInt>),
}

impl From<i64> for Int {
    fn from(v: i64) -> Self {
        Int::Small(v)
    }
}

impl From<BigInt> for Int {
    fn from(v: BigInt) -> Self {
        match v.to_i64() {
            Some(small) => Int::Small(small),
            None => Int::Big(Rc::new(v)),
        }
    }
}

/// Applies `small` to two `i64`s, falling back to `big` when either is big or `small`
/// overflows.
fn either(
    a: &Int,
    b: &Int,
    small: impl FnOnce(i64, i64) -> Option<i64>,
    big: impl FnOnce(BigInt, BigInt) -> BigInt,
) -> Int {
    if let (Int::Small(x), Int::Small(y)) = (a, b)
        && let Some(v) = small(*x, *y)
    {
        return Int::Small(v);
    }
    Int::from(big(a.to_big(), b.to_big()))
}

impl Int {
    /// The value as a big integer.
    pub fn to_big(&self) -> BigInt {
        match self {
            Int::Small(v) => BigInt::from(*v),
            Int::Big(v) => (**v).clone(),
        }
    }

    /// The value, if it fits in 64 bits.
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            Int::Small(v) => Some(*v),
            Int::Big(_) => None,
        }
    }

    /// The nearest `Float`: infinite past its range.
    pub fn to_f64(&self) -> f64 {
        match self {
            Int::Small(v) => *v as f64,
            Int::Big(v) => v.to_f64().unwrap_or(if v.is_negative() {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }),
        }
    }

    /// The integer `x` holds, which must already be whole; `None` for NaN and infinities.
    pub fn from_f64(x: f64) -> Option<Int> {
        if (i64::MIN as f64..i64::MAX as f64).contains(&x) {
            return Some(Int::Small(x as i64));
        }
        BigInt::from_f64(x).map(Int::from)
    }

    /// Reads a literal: decimal, `0x…` or `0b…`, with `_` between digits.
    pub fn parse(text: &str) -> Option<Int> {
        let text = text.replace('_', "");
        let (digits, radix) = if let Some(hex) = text.strip_prefix("0x") {
            (hex, 16)
        } else if let Some(bin) = text.strip_prefix("0b") {
            (bin, 2)
        } else {
            (text.as_str(), 10)
        };
        BigInt::parse_bytes(digits.as_bytes(), radix).map(Int::from)
    }

    /// Whether it is below zero.
    pub fn is_negative(&self) -> bool {
        match self {
            Int::Small(v) => *v < 0,
            Int::Big(v) => v.is_negative(),
        }
    }

    /// The digits of its absolute value in `radix` (2, 8, 10 or 16), lowercase.
    pub fn magnitude_digits(&self, radix: u32) -> String {
        match self {
            Int::Small(v) => {
                let m = v.unsigned_abs();
                match radix {
                    2 => format!("{m:b}"),
                    8 => format!("{m:o}"),
                    16 => format!("{m:x}"),
                    _ => m.to_string(),
                }
            }
            Int::Big(v) => v.magnitude().to_str_radix(radix),
        }
    }

    /// `a + b`.
    pub fn add(&self, b: &Int) -> Int {
        either(self, b, i64::checked_add, |a, b| a + b)
    }

    /// `a - b`.
    pub fn sub(&self, b: &Int) -> Int {
        either(self, b, i64::checked_sub, |a, b| a - b)
    }

    /// `a * b`.
    pub fn mul(&self, b: &Int) -> Int {
        either(self, b, i64::checked_mul, |a, b| a * b)
    }

    /// `a / b`, truncating toward zero; `None` when `b` is zero.
    pub fn div(&self, b: &Int) -> Option<Int> {
        (!b.is_zero()).then(|| either(self, b, i64::checked_div, |a, b| a / b))
    }

    /// `a % b`, with the sign of `a`; `None` when `b` is zero.
    pub fn rem(&self, b: &Int) -> Option<Int> {
        (!b.is_zero()).then(|| either(self, b, i64::checked_rem, |a, b| a % b))
    }

    /// `a & b`, as on endless two's-complement numbers.
    pub fn and(&self, b: &Int) -> Int {
        either(self, b, |a, b| Some(a & b), |a, b| a & b)
    }

    /// `a | b`.
    pub fn or(&self, b: &Int) -> Int {
        either(self, b, |a, b| Some(a | b), |a, b| a | b)
    }

    /// `a ^ b`.
    pub fn xor(&self, b: &Int) -> Int {
        either(self, b, |a, b| Some(a ^ b), |a, b| a ^ b)
    }

    /// `a << shift`.
    pub fn shl(&self, shift: u32) -> Int {
        if let Int::Small(v) = self
            && shift < 64
            && (v << shift) >> shift == *v
        {
            return Int::Small(v << shift);
        }
        Int::from(self.to_big() << shift as usize)
    }

    /// `a >> shift`, rounding toward negative infinity.
    pub fn shr(&self, shift: u32) -> Int {
        match self {
            Int::Small(v) => Int::Small(v >> shift.min(63)),
            Int::Big(v) => Int::from(&**v >> shift as usize),
        }
    }

    /// `-a`.
    pub fn neg(&self) -> Int {
        match self {
            Int::Small(v) if *v != i64::MIN => Int::Small(-v),
            _ => Int::from(-self.to_big()),
        }
    }

    /// `~a`, which is `-a - 1`.
    pub fn not(&self) -> Int {
        match self {
            Int::Small(v) => Int::Small(!v),
            Int::Big(v) => Int::from(!&**v),
        }
    }

    /// `|a|`.
    pub fn abs(&self) -> Int {
        if self.is_negative() {
            self.neg()
        } else {
            self.clone()
        }
    }

    fn is_zero(&self) -> bool {
        match self {
            Int::Small(v) => *v == 0,
            Int::Big(v) => v.is_zero(),
        }
    }
}

impl Ord for Int {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Int::Small(a), Int::Small(b)) => a.cmp(b),
            _ => self.to_big().cmp(&other.to_big()),
        }
    }
}

impl PartialOrd for Int {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Int {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Int::Small(v) => write!(f, "{v}"),
            Int::Big(v) => write!(f, "{v}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(text: &str) -> Int {
        Int::parse(text).unwrap()
    }

    #[test]
    fn grows_and_shrinks() {
        let max = Int::from(i64::MAX);
        let past = max.add(&Int::from(1));
        assert!(matches!(past, Int::Big(_)));
        assert_eq!(past.to_string(), "9223372036854775808");
        // Back in range, it is small again, so it equals the small value.
        assert_eq!(past.sub(&Int::from(1)), max);
        assert_eq!(Int::from(i64::MIN).neg().to_string(), "9223372036854775808");
        assert_eq!(
            Int::from(1).shl(100),
            int("1267650600228229401496703205376")
        );
        assert_eq!(Int::from(1).shl(100).shr(100), Int::from(1));
    }

    #[test]
    fn rust_like_division_and_endless_twos_complement() {
        let big = int("100000000000000000000");
        assert_eq!(
            big.neg().div(&Int::from(7)).unwrap(),
            int("-14285714285714285714")
        );
        assert_eq!(big.neg().rem(&Int::from(7)).unwrap(), Int::from(-2));
        assert_eq!(big.div(&Int::from(0)), None);
        assert_eq!(Int::from(0).not(), Int::from(-1));
        // -1e20 / 2^60 is -86.7…, which rounds down.
        assert_eq!(big.neg().shr(60), Int::from(-87));
        assert_eq!(big.and(&Int::from(0xff)), Int::from(0));
        assert!(big.neg() < Int::from(i64::MIN) && Int::from(i64::MAX) < big);
        assert_eq!(int("0xff_ff"), Int::from(65535));
        assert_eq!(Int::from_f64(1e20), Some(big));
        assert_eq!(Int::from_f64(f64::NAN), None);
    }
}
