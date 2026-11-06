//! Runtime values, and observables.

use std::fmt;
use std::rc::Rc;

use crate::backend::Condition;

/// A value held by a local or computed by an expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `()`, and the contents of a local before it is assigned.
    Unit,
    /// An `Int`.
    Int(i64),
    /// A `Float`.
    Float(f64),
    /// A `Bool`.
    Bool(bool),
    /// A `Bit`.
    Bit(bool),
    /// A `String`; shared, since strings are immutable.
    Str(Rc<str>),
    /// A qubit: its index in the simulator's state.
    Qubit(usize),
    /// An `Observable`.
    Observable(Observable),
    /// A tuple, array or struct (fields in declaration order).
    Aggregate(Vec<Value>),
    /// A reference.
    Ref(Pointer),
    /// A `Distribution`; shared, since distributions are immutable.
    Distribution(Rc<Distribution>),
    /// While a circuit is being recorded: the result of measurement number `k`, not yet known.
    Clbit(usize),
    /// While a circuit is being recorded: logic on measurement results, not yet known; a
    /// `Bit` if the flag is set, else a `Bool`.
    Cond(Rc<Condition>, bool),
}

/// How often each result occurred in repeated runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Distribution {
    /// The outcomes, most frequent first, ties in key order.
    pub outcomes: Vec<Outcome>,
    /// How many runs were counted.
    pub shots: usize,
    /// Whether it came from one simulation of a circuit rather than shot by shot. Not visible
    /// to programs; for tests and reports.
    pub from_circuit: bool,
    /// Whether its circuit was only recorded, as when exporting circuits without running
    /// them: then its results are unknown.
    pub not_run: bool,
}

/// One result of a `Distribution`.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// How the result prints, which identifies it.
    pub key: String,
    /// The result.
    pub value: Value,
    /// How many runs gave it.
    pub count: usize,
}

impl Distribution {
    /// A distribution of `outcomes`, put in order.
    pub fn new(mut outcomes: Vec<Outcome>, shots: usize) -> Self {
        outcomes.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
        Distribution {
            outcomes,
            shots,
            from_circuit: false,
            not_run: false,
        }
    }

    /// How many runs gave the result that prints as `key`.
    pub fn count(&self, key: &str) -> usize {
        self.outcomes
            .iter()
            .find(|o| o.key == key)
            .map_or(0, |o| o.count)
    }
}

impl fmt::Display for Distribution {
    /// One line per outcome: result, count and percentage, aligned.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.not_run {
            return write!(f, "<{} shots, not run>", self.shots);
        }
        let width = self
            .outcomes
            .iter()
            .map(|o| o.key.chars().count())
            .max()
            .unwrap_or(0);
        let digits = self.shots.to_string().len();
        for (i, o) in self.outcomes.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            let share = 100.0 * o.count as f64 / self.shots as f64;
            write!(f, "{:<width$}  {:>digits$}  {share:>5.1}%", o.key, o.count)?;
        }
        Ok(())
    }
}

/// Where a reference points: a local in some call frame, then fields and elements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    /// The call frame, counted from the outermost.
    pub frame: usize,
    /// The local in that frame.
    pub local: usize,
    /// Field and element steps.
    pub path: Vec<usize>,
}

impl Value {
    /// The value with each qubit replaced by a bit of `bits`, in order (bit `k` for the
    /// `k`th qubit).
    pub fn with_bits(&self, bits: u64) -> Value {
        fn go(value: &Value, bits: u64, next: &mut usize) -> Value {
            match value {
                Value::Qubit(_) => {
                    let bit = (bits >> *next) & 1 == 1;
                    *next += 1;
                    Value::Bit(bit)
                }
                Value::Aggregate(items) => {
                    Value::Aggregate(items.iter().map(|v| go(v, bits, next)).collect())
                }
                other => other.clone(),
            }
        }
        go(self, bits, &mut 0)
    }

    /// Whether the value holds measurement results not yet known.
    pub fn has_clbits(&self) -> bool {
        match self {
            Value::Clbit(_) | Value::Cond(..) => true,
            Value::Aggregate(items) => items.iter().any(Value::has_clbits),
            _ => false,
        }
    }

    /// The value with each recorded measurement result taken from `bits` (result `k` at bit
    /// `k`).
    pub fn with_clbits(&self, bits: u64) -> Value {
        match self {
            Value::Clbit(k) => Value::Bit(*k < 64 && (bits >> k) & 1 == 1),
            Value::Cond(c, true) => Value::Bit(c.eval(bits)),
            Value::Cond(c, false) => Value::Bool(c.eval(bits)),
            Value::Aggregate(items) => {
                Value::Aggregate(items.iter().map(|v| v.with_clbits(bits)).collect())
            }
            other => other.clone(),
        }
    }

    /// The condition a recorded measurement result, or logic on them, stands for; `None` for a
    /// value that is already known.
    pub fn condition(&self) -> Option<Condition> {
        match self {
            Value::Clbit(k) => Some(Condition::Clbit(*k)),
            Value::Cond(c, _) => Some((**c).clone()),
            _ => None,
        }
    }

    /// A `Bit` or `Bool`, known or not, as a condition; `None` for anything else.
    fn as_condition(&self) -> Option<(Condition, bool)> {
        match self {
            Value::Bit(b) => Some((Condition::Const(*b), true)),
            Value::Bool(b) => Some((Condition::Const(*b), false)),
            Value::Clbit(k) => Some((Condition::Clbit(*k), true)),
            Value::Cond(c, bit) => Some(((**c).clone(), *bit)),
            _ => None,
        }
    }

    /// Logic on values at least one of which is a recorded measurement result: the result
    /// as a condition, or `None` if this is not logic on bits and booleans. `op` is a binary
    /// operator, or `None` for `not`/`~` on `self` alone.
    pub fn symbolic_logic(
        &self,
        op: Option<noev_syntax::ast::BinaryOp>,
        other: Option<&Value>,
    ) -> Option<Value> {
        use noev_syntax::ast::BinaryOp::*;
        if !self.has_clbits() && !other.is_some_and(Value::has_clbits) {
            return None;
        }
        let (a, bit) = self.as_condition()?;
        let Some(op) = op else {
            return Some(Value::Cond(Rc::new(Condition::negate(a)), bit));
        };
        let (b, _) = other?.as_condition()?;
        let (condition, bit) = match op {
            Eq => (Condition::negate(Condition::xor(a, b)), false),
            NotEq => (Condition::xor(a, b), false),
            And => (Condition::and(a, b), false),
            Or => (Condition::or(a, b), false),
            BitAnd => (Condition::and(a, b), bit),
            BitOr => (Condition::or(a, b), bit),
            BitXor => (Condition::xor(a, b), bit),
            _ => return None,
        };
        Some(Value::Cond(Rc::new(condition), bit))
    }

    /// Every qubit in the value, in order: elements and fields left to right.
    pub fn qubits(&self, out: &mut Vec<usize>) {
        match self {
            Value::Qubit(q) => out.push(*q),
            Value::Aggregate(items) => items.iter().for_each(|v| v.qubits(out)),
            _ => {}
        }
    }
}

/// How a value is shown in `noev run`'s results: bits as `0`/`1`, a tuple or array of bits
/// as a bit string in index order (`|01>` measures as `01`), anything else in brackets.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => write!(f, "()"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v:?}"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Bit(v) => write!(f, "{}", u8::from(*v)),
            Value::Str(s) => write!(f, "{s}"),
            Value::Qubit(q) => write!(f, "<qubit {q}>"),
            Value::Observable(_) => write!(f, "<observable>"),
            Value::Ref(_) => write!(f, "<reference>"),
            Value::Distribution(d) => write!(f, "{d}"),
            Value::Clbit(k) => write!(f, "<measurement {k}>"),
            Value::Cond(..) => write!(f, "<measurement logic>"),
            Value::Aggregate(items)
                if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Bit(_))) =>
            {
                items.iter().try_for_each(|v| write!(f, "{v}"))
            }
            Value::Aggregate(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, ")")
            }
        }
    }
}

/// A Pauli operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pauli {
    /// X.
    X,
    /// Y.
    Y,
    /// Z.
    Z,
}

/// A real-weighted sum of Pauli products. Indices refer to the qubits of a preparation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observable {
    /// `(coefficient, [(qubit index, Pauli)])`, each product sorted by index.
    pub terms: Vec<(f64, Vec<(usize, Pauli)>)>,
}

impl Observable {
    /// A single Pauli on qubit `index`.
    pub fn pauli(pauli: Pauli, index: usize) -> Self {
        Observable {
            terms: vec![(1.0, vec![(index, pauli)])],
        }
    }

    /// The identity.
    pub fn identity() -> Self {
        Observable {
            terms: vec![(1.0, Vec::new())],
        }
    }

    /// `c × self`.
    pub fn scale(&self, c: f64) -> Self {
        Observable {
            terms: self.terms.iter().map(|(k, p)| (k * c, p.clone())).collect(),
        }
    }

    /// `self + other`.
    pub fn add(&self, other: &Observable) -> Self {
        let mut terms = self.terms.clone();
        terms.extend(other.terms.iter().cloned());
        Observable { terms }
    }

    /// `self × other`, for factors acting on different qubits.
    pub fn mul(&self, other: &Observable) -> Result<Self, String> {
        let mut terms = Vec::with_capacity(self.terms.len() * other.terms.len());
        for (a, pa) in &self.terms {
            for (b, pb) in &other.terms {
                if pa.iter().any(|(i, _)| pb.iter().any(|(j, _)| i == j)) {
                    return Err("a product of observables must act on different qubits".into());
                }
                let mut product = pa.clone();
                product.extend(pb.iter().copied());
                product.sort();
                terms.push((a * b, product));
            }
        }
        Ok(Observable { terms })
    }

    /// `P(i, b)` or `PX(i, b)`: `(I ± Z) / 2` or `(I ± X) / 2`.
    pub fn projector(x_basis: bool, index: usize, bit: bool) -> Self {
        let pauli = if x_basis { Pauli::X } else { Pauli::Z };
        let sign = if bit { -0.5 } else { 0.5 };
        Observable::identity()
            .scale(0.5)
            .add(&Observable::pauli(pauli, index).scale(sign))
    }

    /// The largest qubit index used, plus one.
    pub fn width(&self) -> usize {
        self.terms
            .iter()
            .flat_map(|(_, p)| p.iter().map(|(i, _)| i + 1))
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projector_is_half_identity_plus_half_pauli() {
        let p = Observable::projector(false, 0, false);
        assert_eq!(p.terms, vec![(0.5, vec![]), (0.5, vec![(0, Pauli::Z)])]);
    }

    #[test]
    fn products_on_shared_qubits_are_rejected() {
        let z0 = Observable::pauli(Pauli::Z, 0);
        assert!(z0.mul(&z0).is_err());
        let zz = z0.mul(&Observable::pauli(Pauli::Z, 1)).unwrap();
        assert_eq!(zz.terms, vec![(1.0, vec![(0, Pauli::Z), (1, Pauli::Z)])]);
    }

    #[test]
    fn bit_strings() {
        let bits = Value::Aggregate(vec![Value::Bit(false), Value::Bit(true)]);
        assert_eq!(bits.to_string(), "01");
        let mixed = Value::Aggregate(vec![Value::Int(3), Value::Bool(true)]);
        assert_eq!(mixed.to_string(), "(3, true)");
    }
}
