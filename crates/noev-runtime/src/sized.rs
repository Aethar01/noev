//! Sized numbers (`spec/types.md` §1): `i8`…`u64` and `f32`. Every integer value fits in an
//! `i128`, so each operation is computed there and then checked against the type's range.
//! `f64` values are `Float`s while `Float` is 64 bits.

use noev_ir::ir::BinOp;
use noev_sema::ty::Width;

use crate::int::Int;
use crate::machine::RuntimeError;
use crate::value::Value;

type Result<T> = std::result::Result<T, RuntimeError>;

/// `v`, if it is in range for `w`; otherwise an overflow, naming what was computed.
fn checked(v: Option<i128>, w: Width, what: impl FnOnce() -> String) -> Result<Value> {
    match v.filter(|&v| w.holds(v)) {
        Some(v) => Ok(Value::Sized(v, w)),
        None => Err(RuntimeError::new(format!(
            "`{}` overflows `{}`",
            what(),
            w.name()
        ))),
    }
}

/// The low `w.bits` bits of `bits`, read as `w` reads them: what a shift keeps.
fn wrap(bits: u128, w: Width) -> i128 {
    let low = bits & ((1u128 << w.bits) - 1);
    if w.signed && (low >> (w.bits - 1)) & 1 == 1 {
        low as i128 - (1i128 << w.bits)
    } else {
        low as i128
    }
}

/// `-v`.
pub fn neg(v: i128, w: Width) -> Result<Value> {
    checked(v.checked_neg(), w, || format!("-({v})"))
}

/// `~v`: every bit of the type flipped.
pub fn not(v: i128, w: Width) -> Value {
    Value::Sized(if w.signed { !v } else { v ^ w.max() }, w)
}

/// `a op b`, where `a` is of width `w`. `b` is of the same type, or for a shift an `Int`.
pub fn binary(op: BinOp, a: i128, w: Width, b: &Value) -> Result<Value> {
    use BinOp::*;
    if matches!(op, Shl | Shr) {
        let shift = match b {
            Value::Sized(s, _) => Some(*s),
            Value::Int(s) => s.to_i128(),
            _ => None,
        }
        .filter(|s| (0..i128::from(w.bits)).contains(s))
        .ok_or_else(|| RuntimeError::new(format!("cannot shift a `{}` by {b}", w.name())))?;
        let shifted = if op == Shl {
            wrap((a as u128) << shift, w)
        } else {
            a >> shift
        };
        return Ok(Value::Sized(shifted, w));
    }
    let Value::Sized(b, _) = *b else {
        return Err(RuntimeError::new(format!(
            "cannot apply `{}` to a `{}` and {b:?}",
            op.as_str(),
            w.name()
        )));
    };
    let what = || format!("{a} {} {b}", op.as_str());
    if matches!(op, Div | Rem) && b == 0 {
        let what = if op == Div { "division" } else { "remainder" };
        return Err(RuntimeError::new(format!("{what} by zero")));
    }
    Ok(match op {
        Add => checked(a.checked_add(b), w, what)?,
        Sub => checked(a.checked_sub(b), w, what)?,
        Mul => checked(a.checked_mul(b), w, what)?,
        // `i8`'s -128 / -1 is the one division that overflows.
        Div => checked(a.checked_div(b), w, what)?,
        Rem => checked(a.checked_rem(b), w, what)?,
        BitAnd => Value::Sized(a & b, w),
        BitOr => Value::Sized(a | b, w),
        BitXor => Value::Sized(a ^ b, w),
        Eq => Value::Bool(a == b),
        NotEq => Value::Bool(a != b),
        Lt => Value::Bool(a < b),
        Le => Value::Bool(a <= b),
        Gt => Value::Bool(a > b),
        Ge => Value::Bool(a >= b),
        Shl | Shr | And | Or => {
            return Err(RuntimeError::new(format!(
                "`{}` on `{}`",
                op.as_str(),
                w.name()
            )));
        }
    })
}

/// `T::try_from(x)` for an integer `x`: `Ok` (variant 0) with it as a `T`, or `Err`
/// (variant 1) saying why not.
pub fn try_from(value: &Value, w: Width) -> Result<Value> {
    let v = match value {
        Value::Int(v) => v.to_i128(),
        Value::Sized(v, _) => Some(*v),
        other => {
            return Err(RuntimeError::new(format!(
                "expected an integer, found {other:?}"
            )));
        }
    };
    Ok(match v.filter(|&v| w.holds(v)) {
        Some(v) => Value::Variant(0, vec![Value::Sized(v, w)]),
        None => {
            let why = format!(
                "{value} does not fit in `{}`, which holds {} to {}",
                w.name(),
                w.min(),
                w.max()
            );
            Value::Variant(1, vec![Value::Str(why.into())])
        }
    })
}

/// The bits of an unsigned `v`, most significant first, as `x.bits()` gives them.
pub fn bits(v: i128, w: Width) -> Value {
    Value::Aggregate(
        (0..w.bits)
            .map(|i| Value::Bit((v >> (w.bits - 1 - i)) & 1 == 1))
            .collect(),
    )
}

/// The number a bit array spells, most significant bit first: `u8(bits)`.
pub fn from_bits(items: &[Value], w: Width) -> Result<Value> {
    let mut v = 0i128;
    for item in items {
        let Value::Bit(bit) = item else {
            return Err(RuntimeError::new(format!("expected a Bit, found {item:?}")));
        };
        v = (v << 1) | i128::from(*bit);
    }
    Ok(Value::Sized(v, w))
}

/// An integer of any kind as an `Int`.
pub fn to_int(v: i128) -> Int {
    Int::from_i128(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    const U8: Width = Width {
        bits: 8,
        signed: false,
    };
    const I8: Width = Width {
        bits: 8,
        signed: true,
    };

    #[test]
    fn checked_arithmetic_and_shifts() {
        let one = Value::Sized(1, U8);
        assert!(binary(BinOp::Add, 255, U8, &one).is_err());
        assert_eq!(
            binary(BinOp::Add, 254, U8, &one).unwrap(),
            Value::Sized(255, U8)
        );
        assert!(binary(BinOp::Div, -128, I8, &Value::Sized(-1, I8)).is_err());
        assert!(neg(-128, I8).is_err());
        // Shifts drop the bits shifted out, and read the rest in the type.
        assert_eq!(
            binary(BinOp::Shl, 0b1100_0001, U8, &Value::Int(1.into())).unwrap(),
            Value::Sized(0b1000_0010, U8)
        );
        assert_eq!(
            binary(BinOp::Shl, 0b0100_0000, I8, &Value::Int(1.into())).unwrap(),
            Value::Sized(-128, I8)
        );
        assert!(binary(BinOp::Shr, 1, U8, &Value::Int(8.into())).is_err());
        assert_eq!(not(0, U8), Value::Sized(255, U8));
        assert_eq!(not(0, I8), Value::Sized(-1, I8));
    }

    #[test]
    fn bits_and_conversions() {
        let five = bits(5, U8);
        assert_eq!(five.to_string(), "00000101");
        let Value::Aggregate(items) = five else {
            unreachable!()
        };
        assert_eq!(from_bits(&items, U8).unwrap(), Value::Sized(5, U8));
        assert!(matches!(
            try_from(&Value::Int(300.into()), U8).unwrap(),
            Value::Variant(1, _)
        ));
        assert_eq!(
            try_from(&Value::Sized(-1, I8), I8).unwrap(),
            Value::Variant(0, vec![Value::Sized(-1, I8)])
        );
    }
}
