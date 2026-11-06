//! Printing values and applying format specs (`spec/types.md` §7).

use noev_ir::ir::{Align, FormatKind, FormatSpec, Program};
use noev_sema::{Ty, TyKind};

use crate::value::{Observable, Pauli, Value};

/// `value`, of type `ty`, as `print` shows it.
pub fn display(program: &Program, value: &Value, ty: Ty) -> String {
    let mut out = String::new();
    write_value(program, value, ty, &mut out);
    out
}

/// `value`, of type `ty`, formatted with `spec`.
pub fn format(program: &Program, value: &Value, ty: Ty, spec: Option<&FormatSpec>) -> String {
    let Some(spec) = spec else {
        return display(program, value, ty);
    };
    // The text before padding, and whether it is a number (for alignment and zero padding).
    let (sign, digits, numeric) = match value {
        Value::Int(v) => {
            let magnitude = v.unsigned_abs();
            let digits = match spec.kind {
                Some(FormatKind::Binary) => format!("{magnitude:b}"),
                Some(FormatKind::Octal) => format!("{magnitude:o}"),
                Some(FormatKind::Hex) => format!("{magnitude:x}"),
                Some(FormatKind::HexUpper) => format!("{magnitude:X}"),
                _ => magnitude.to_string(),
            };
            (sign(*v < 0, spec.plus), digits, true)
        }
        Value::Float(v) => {
            let magnitude = v.abs();
            let digits = match (spec.kind, spec.precision) {
                (Some(FormatKind::Exponent), Some(p)) => format!("{magnitude:.*e}", p as usize),
                (Some(FormatKind::Exponent), None) => format!("{magnitude:e}"),
                (_, Some(p)) => format!("{magnitude:.*}", p as usize),
                (_, None) => format!("{magnitude:?}"),
            };
            (
                sign(v.is_sign_negative() && !v.is_nan(), spec.plus),
                digits,
                true,
            )
        }
        other => (String::new(), display(program, other, ty), false),
    };
    let len = sign.chars().count() + digits.chars().count();
    let width = spec.width.map_or(0, |w| w as usize);
    if len >= width {
        return sign + &digits;
    }
    let pad = width - len;
    if spec.zero && numeric && spec.align.is_none() {
        return sign + &"0".repeat(pad) + &digits;
    }
    let text = sign + &digits;
    let fill = spec.fill.to_string();
    // Numbers align right by default, everything else left, as in Rust.
    let align = spec
        .align
        .unwrap_or(if numeric { Align::Right } else { Align::Left });
    match align {
        Align::Left => text + &fill.repeat(pad),
        Align::Right => fill.repeat(pad) + &text,
        Align::Center => fill.repeat(pad / 2) + &text + &fill.repeat(pad - pad / 2),
    }
}

fn sign(negative: bool, plus: bool) -> String {
    match (negative, plus) {
        (true, _) => "-".into(),
        (false, true) => "+".into(),
        (false, false) => String::new(),
    }
}

fn write_value(program: &Program, value: &Value, ty: Ty, out: &mut String) {
    use std::fmt::Write;
    let types = &program.types;
    match (value, types.kind(ty)) {
        (Value::Unit, _) => out.push_str("()"),
        (Value::Int(v), _) => {
            let _ = write!(out, "{v}");
        }
        // Debug formatting always shows a decimal point: `-1.0`, not `-1`.
        (Value::Float(v), _) => {
            let _ = write!(out, "{v:?}");
        }
        (Value::Bool(v), _) => {
            let _ = write!(out, "{v}");
        }
        (Value::Bit(v), _) => out.push(if *v { '1' } else { '0' }),
        (Value::Str(s), _) => out.push_str(s),
        (Value::Observable(o), _) => write_observable(o, out),
        (Value::Aggregate(items), kind) => {
            if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Bit(_))) {
                // Bits read as a bit string, in index order.
                for item in items {
                    write_value(program, item, Ty::BIT, out);
                }
                return;
            }
            match kind {
                TyKind::Array(elem, _) => {
                    out.push('[');
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_value(program, item, *elem, out);
                    }
                    out.push(']');
                }
                TyKind::Tuple(elems) => {
                    out.push('(');
                    for (i, (item, &t)) in items.iter().zip(elems.iter()).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_value(program, item, t, out);
                    }
                    if items.len() == 1 {
                        out.push(',');
                    }
                    out.push(')');
                }
                &TyKind::Struct(s) => {
                    out.push_str(&program.struct_names[s.index()]);
                    out.push_str(" { ");
                    let fields = &types.structs[s.index()].fields;
                    for (i, (item, &(_, t))) in items.iter().zip(fields.iter()).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        let _ = write!(out, "{}: ", program.struct_fields[s.index()][i]);
                        write_value(program, item, t, out);
                    }
                    out.push_str(" }");
                }
                _ => out.push('…'),
            }
        }
        (Value::Distribution(d), _) => {
            let _ = write!(out, "{d}");
        }
        (Value::Qubit(_), _) => out.push_str("<qubit>"),
        (Value::Clbit(_) | Value::Cond(..), _) => out.push('0'),
        (Value::Ref(_), _) => out.push_str("<reference>"),
    }
}

fn write_observable(o: &Observable, out: &mut String) {
    use std::fmt::Write;
    if o.terms.is_empty() {
        out.push_str("0.0");
        return;
    }
    for (i, (coeff, product)) in o.terms.iter().enumerate() {
        let (sign, magnitude) = if *coeff < 0.0 {
            ("-", -coeff)
        } else {
            ("+", *coeff)
        };
        match (i, sign) {
            (0, "-") => out.push('-'),
            (0, _) => {}
            _ => {
                let _ = write!(out, " {sign} ");
            }
        }
        let _ = write!(out, "{magnitude:?}");
        if product.is_empty() {
            out.push_str(" * I()");
        }
        for (index, pauli) in product {
            let name = match pauli {
                Pauli::X => 'X',
                Pauli::Y => 'Y',
                Pauli::Z => 'Z',
            };
            let _ = write!(out, " * {name}({index})");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noev_syntax::Span;

    fn spec(text: &str) -> FormatSpec {
        // Parse through the real parser, so the tests use the same specs as programs.
        let source = format!("fn f(x: Int) -> String {{ \"{{x:{text}}}\" }}");
        let parsed = noev_syntax::parse(&source);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let found = parsed.ast.all_exprs().iter().find_map(|e| match e.kind {
            noev_syntax::ast::ExprKind::Str(parts) => {
                parsed.ast.list(parts).iter().find_map(|p| match p {
                    noev_syntax::ast::StrPart::Value { spec, .. } => *spec,
                    _ => None,
                })
            }
            _ => None,
        });
        found.unwrap_or(FormatSpec {
            fill: ' ',
            align: None,
            plus: false,
            zero: false,
            width: None,
            precision: None,
            kind: None,
            span: Span::default(),
        })
    }

    fn empty_program() -> Program {
        let parsed = noev_syntax::parse("");
        let (typed, _) = noev_sema::check(&parsed.ast);
        let (circuits, _) = noev_sema::check_circuits(&parsed.ast, &typed);
        noev_ir::lower(&parsed.ast, &typed, &circuits, "")
    }

    #[test]
    fn specs() {
        let p = empty_program();
        let int = |v: i64, s: &str| format(&p, &Value::Int(v), Ty::INT, Some(&spec(s)));
        let float = |v: f64, s: &str| format(&p, &Value::Float(v), Ty::FLOAT, Some(&spec(s)));
        assert_eq!(int(42, ">5"), "   42");
        assert_eq!(int(42, "<5"), "42   ");
        assert_eq!(int(42, "*^6"), "**42**");
        assert_eq!(int(5, "08b"), "00000101");
        assert_eq!(int(-255, "x"), "-ff");
        assert_eq!(int(255, "X"), "FF");
        assert_eq!(int(7, "+"), "+7");
        assert_eq!(float(-0.5, ".3"), "-0.500");
        assert_eq!(float(1500.0, ".2e"), "1.50e3");
        assert_eq!(float(2.0, "+08.2"), "+0002.00");
    }

    #[test]
    fn display_rules() {
        let p = empty_program();
        assert_eq!(display(&p, &Value::Float(-1.0), Ty::FLOAT), "-1.0");
        assert_eq!(display(&p, &Value::Bit(true), Ty::BIT), "1");
        let o = Observable::pauli(Pauli::Z, 0)
            .mul(&Observable::pauli(Pauli::Z, 1))
            .unwrap()
            .scale(-1.0)
            .add(&Observable::pauli(Pauli::X, 0).scale(0.5));
        assert_eq!(
            display(&p, &Value::Observable(o), Ty::OBSERVABLE),
            "-1.0 * Z(0) * Z(1) + 0.5 * X(0)"
        );
    }
}
