//! Linear integer facts, and proofs that array indices differ or are in range
//! (`spec/ownership.md` §10).
//!
//! Indices are linear forms: integer variables times constants, plus a constant. Facts come
//! from loop bounds, `if` conditions and `assert`. Proofs use difference constraints
//! (`v - w ≤ c`) and shortest paths between them, the standard technique for removing bounds
//! checks.

use noev_syntax::ast::{Ast, BinaryOp, ExprId, ExprKind, UnaryOp};

use crate::check::{LocalId, Res, Typed};
use crate::ty::Ty;

/// `Σ coefficient × variable + constant`, with terms sorted by variable and no zero
/// coefficients, so equal forms compare equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Linear {
    terms: Vec<(LocalId, i64)>,
    constant: i64,
}

impl Linear {
    fn constant(value: i64) -> Self {
        Linear {
            terms: Vec::new(),
            constant: value,
        }
    }

    fn var(local: LocalId) -> Self {
        Linear {
            terms: vec![(local, 1)],
            constant: 0,
        }
    }

    /// The constant value, if the form has no variables.
    pub fn as_constant(&self) -> Option<i64> {
        self.terms.is_empty().then_some(self.constant)
    }

    /// The variables and their coefficients, sorted by variable.
    pub fn terms(&self) -> &[(LocalId, i64)] {
        &self.terms
    }

    /// The constant part.
    pub fn constant_part(&self) -> i64 {
        self.constant
    }

    /// Whether `local` appears in the form.
    pub fn mentions(&self, local: LocalId) -> bool {
        self.terms.iter().any(|&(l, _)| l == local)
    }

    fn scale(&self, k: i64) -> Option<Self> {
        if k == 0 {
            return Some(Linear::constant(0));
        }
        let terms = self
            .terms
            .iter()
            .map(|&(l, c)| c.checked_mul(k).map(|c| (l, c)))
            .collect::<Option<_>>()?;
        Some(Linear {
            terms,
            constant: self.constant.checked_mul(k)?,
        })
    }

    fn add(&self, other: &Linear) -> Option<Self> {
        let mut terms = self.terms.clone();
        for &(local, coeff) in &other.terms {
            match terms.binary_search_by_key(&local, |&(l, _)| l) {
                Ok(i) => {
                    terms[i].1 = terms[i].1.checked_add(coeff)?;
                    if terms[i].1 == 0 {
                        terms.remove(i);
                    }
                }
                Err(i) => terms.insert(i, (local, coeff)),
            }
        }
        Some(Linear {
            terms,
            constant: self.constant.checked_add(other.constant)?,
        })
    }

    fn sub(&self, other: &Linear) -> Option<Self> {
        self.add(&other.scale(-1)?)
    }

    fn neg(&self) -> Option<Self> {
        self.scale(-1)
    }
}

/// The form of a constant.
pub fn constant_form(value: i64) -> Linear {
    Linear::constant(value)
}

/// The linear form of an integer expression, if it has one.
pub fn linear(ast: &Ast, typed: &Typed, id: ExprId) -> Option<Linear> {
    match ast.expr(id).kind {
        ExprKind::Int(value) => Some(Linear::constant(i64::try_from(value).ok()?)),
        ExprKind::Name(_) => match typed.res[id.index()] {
            Res::Local(local) if typed.local(local).ty == Ty::INT => Some(Linear::var(local)),
            _ => None,
        },
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => linear(ast, typed, operand)?.neg(),
        ExprKind::Binary { op, lhs, rhs } => {
            let (a, b) = (linear(ast, typed, lhs)?, linear(ast, typed, rhs)?);
            match op {
                BinaryOp::Add => a.add(&b),
                BinaryOp::Sub => a.sub(&b),
                BinaryOp::Mul => match (a.as_constant(), b.as_constant()) {
                    (Some(k), _) => b.scale(k),
                    (_, Some(k)) => a.scale(k),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// Something known to hold at a point in the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    /// `form ≤ 0`.
    NonPositive(Linear),
    /// `form ≠ 0`.
    NonZero(Linear),
}

impl Fact {
    /// Whether `local` appears in the fact.
    pub fn mentions(&self, local: LocalId) -> bool {
        match self {
            Fact::NonPositive(f) | Fact::NonZero(f) => f.mentions(local),
        }
    }
}

/// The facts a condition establishes: when it is true (`positive`) or false.
pub fn facts_from(ast: &Ast, typed: &Typed, cond: ExprId, positive: bool, out: &mut Vec<Fact>) {
    match ast.expr(cond).kind {
        ExprKind::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
        } if positive => {
            facts_from(ast, typed, lhs, true, out);
            facts_from(ast, typed, rhs, true, out);
        }
        ExprKind::Binary {
            op: BinaryOp::Or,
            lhs,
            rhs,
        } if !positive => {
            facts_from(ast, typed, lhs, false, out);
            facts_from(ast, typed, rhs, false, out);
        }
        ExprKind::Unary {
            op: UnaryOp::Not,
            operand,
        } => facts_from(ast, typed, operand, !positive, out),
        ExprKind::Binary { op, lhs, rhs } => {
            let (Some(l), Some(r)) = (linear(ast, typed, lhs), linear(ast, typed, rhs)) else {
                return;
            };
            let op = if positive {
                op
            } else {
                match op {
                    BinaryOp::Lt => BinaryOp::Ge,
                    BinaryOp::Le => BinaryOp::Gt,
                    BinaryOp::Gt => BinaryOp::Le,
                    BinaryOp::Ge => BinaryOp::Lt,
                    BinaryOp::Eq => BinaryOp::NotEq,
                    BinaryOp::NotEq => BinaryOp::Eq,
                    _ => return,
                }
            };
            let one = Linear::constant(1);
            let facts = match op {
                BinaryOp::Lt => vec![l.sub(&r).and_then(|d| d.add(&one)).map(Fact::NonPositive)],
                BinaryOp::Le => vec![l.sub(&r).map(Fact::NonPositive)],
                BinaryOp::Gt => vec![r.sub(&l).and_then(|d| d.add(&one)).map(Fact::NonPositive)],
                BinaryOp::Ge => vec![r.sub(&l).map(Fact::NonPositive)],
                BinaryOp::Eq => vec![
                    l.sub(&r).map(Fact::NonPositive),
                    r.sub(&l).map(Fact::NonPositive),
                ],
                BinaryOp::NotEq => vec![l.sub(&r).map(Fact::NonZero)],
                _ => return,
            };
            out.extend(facts.into_iter().flatten());
        }
        _ => {}
    }
}

/// The facts a `for v in start..end` loop establishes inside its body.
pub fn loop_facts(var: LocalId, start: Option<Linear>, end: Option<Linear>, out: &mut Vec<Fact>) {
    let v = Linear::var(var);
    if let Some(f) = start.and_then(|s| s.sub(&v)) {
        out.push(Fact::NonPositive(f));
    }
    if let Some(f) = end.and_then(|e| v.sub(&e)?.add(&Linear::constant(1))) {
        out.push(Fact::NonPositive(f));
    }
}

/// Shortest paths over the difference constraints of a set of facts: `bound(u, v)` is the
/// tightest known upper bound on `v - u`. Node 0 stands for the constant zero.
struct Bounds {
    nodes: Vec<LocalId>,
    dist: Vec<Option<i64>>,
    /// The facts contradict each other, so the code is unreachable and anything holds.
    contradictory: bool,
}

impl Bounds {
    fn new(facts: &[Fact], extra: &[&Linear]) -> Self {
        let mut nodes: Vec<LocalId> = Vec::new();
        let add_node = |l: LocalId, nodes: &mut Vec<LocalId>| {
            if !nodes.contains(&l) {
                nodes.push(l);
            }
        };
        for fact in facts {
            if let Fact::NonPositive(f) = fact {
                for &(l, _) in &f.terms {
                    add_node(l, &mut nodes);
                }
            }
        }
        for f in extra {
            for &(l, _) in &f.terms {
                add_node(l, &mut nodes);
            }
        }
        let n = nodes.len() + 1;
        let mut bounds = Bounds {
            nodes,
            dist: vec![None; n * n],
            contradictory: false,
        };
        for i in 0..n {
            bounds.dist[i * n + i] = Some(0);
        }
        for fact in facts {
            let Fact::NonPositive(f) = fact else { continue };
            // a·v + b·w + c ≤ 0 with a, b ∈ {+1, −1} gives a difference constraint.
            let (from, to) = match f.terms.as_slice() {
                [(v, 1)] => (0, bounds.node(*v)),
                [(v, -1)] => (bounds.node(*v), 0),
                [(v, 1), (w, -1)] => (bounds.node(*w), bounds.node(*v)),
                [(w, -1), (v, 1)] => (bounds.node(*w), bounds.node(*v)),
                _ => continue,
            };
            // `to - from ≤ -c`.
            let Some(weight) = f.constant.checked_neg() else {
                continue;
            };
            let slot = &mut bounds.dist[from * n + to];
            *slot = Some(slot.map_or(weight, |w| w.min(weight)));
        }
        for k in 0..n {
            for i in 0..n {
                let Some(ik) = bounds.dist[i * n + k] else {
                    continue;
                };
                for j in 0..n {
                    let Some(kj) = bounds.dist[k * n + j] else {
                        continue;
                    };
                    let through = ik.saturating_add(kj);
                    let slot = &mut bounds.dist[i * n + j];
                    if slot.is_none_or(|d| through < d) {
                        *slot = Some(through);
                    }
                }
            }
        }
        bounds.contradictory = (0..n).any(|i| bounds.dist[i * n + i].is_some_and(|d| d < 0));
        bounds
    }

    fn node(&self, local: LocalId) -> usize {
        self.nodes
            .iter()
            .position(|&l| l == local)
            .map_or(0, |i| i + 1)
    }

    /// The tightest known upper bound on `to - from` (`None` for zero).
    fn bound(&self, from: Option<LocalId>, to: Option<LocalId>) -> Option<i64> {
        let n = self.nodes.len() + 1;
        let index = |l: Option<LocalId>| l.map_or(0, |l| self.node(l));
        self.dist[index(from) * n + index(to)]
    }

    /// The range of values `form` can take.
    fn interval(&self, form: &Linear) -> (Option<i64>, Option<i64>) {
        let mut lo = Some(form.constant);
        let mut hi = Some(form.constant);
        for &(var, coeff) in &form.terms {
            let var_hi = self.bound(None, Some(var));
            let var_lo = self.bound(Some(var), None).and_then(i64::checked_neg);
            let (min, max) = if coeff > 0 {
                (var_lo, var_hi)
            } else {
                (var_hi, var_lo)
            };
            lo = lo
                .zip(min)
                .and_then(|(lo, m)| lo.checked_add(m.checked_mul(coeff)?));
            hi = hi
                .zip(max)
                .and_then(|(hi, m)| hi.checked_add(m.checked_mul(coeff)?));
        }
        (lo, hi)
    }
}

/// Whether the facts prove `x ≠ y`.
pub fn differ(facts: &[Fact], x: &Linear, y: &Linear) -> bool {
    let Some(d) = x.sub(y) else { return false };
    if let Some(c) = d.as_constant() {
        return c != 0;
    }
    let negated = d.neg();
    if facts.iter().any(|f| match f {
        Fact::NonZero(g) => *g == d || Some(g) == negated.as_ref(),
        Fact::NonPositive(_) => false,
    }) {
        return true;
    }
    // d = v − w + c, where either variable may be absent.
    let (v, w) = match d.terms.as_slice() {
        [(v, 1)] => (Some(*v), None),
        [(w, -1)] => (None, Some(*w)),
        [(a, 1), (b, -1)] => (Some(*a), Some(*b)),
        [(b, -1), (a, 1)] => (Some(*a), Some(*b)),
        _ => return false,
    };
    let bounds = Bounds::new(facts, &[&d]);
    if bounds.contradictory {
        return true;
    }
    let c = d.constant;
    // d ≥ 1  ⟺  w − v ≤ c − 1;   d ≤ −1  ⟺  v − w ≤ −1 − c.
    bounds.bound(v, w).is_some_and(|b| b < c) || bounds.bound(w, v).is_some_and(|b| b <= -1 - c)
}

/// Where `form` is known to lie, for bounds checks. `None` means unbounded on that side.
pub fn interval(facts: &[Fact], form: &Linear) -> (Option<i64>, Option<i64>) {
    let bounds = Bounds::new(facts, &[form]);
    if bounds.contradictory {
        return (Some(0), Some(0));
    }
    bounds.interval(form)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(i: u32) -> Linear {
        Linear::var(LocalId::from_index(i as usize))
    }

    fn c(v: i64) -> Linear {
        Linear::constant(v)
    }

    #[test]
    fn constant_differences() {
        let i = var(0);
        assert!(differ(&[], &i, &i.add(&c(1)).unwrap()));
        assert!(!differ(&[], &i, &i));
        let two_i = i.scale(2).unwrap();
        assert!(differ(&[], &two_i, &two_i.add(&c(1)).unwrap()));
    }

    #[test]
    fn loop_bounds_separate_indices() {
        let (i, j) = (var(0), var(1));
        let mut facts = Vec::new();
        // for j in i + 1..4
        loop_facts(LocalId::from_index(1), i.add(&c(1)), Some(c(4)), &mut facts);
        assert!(differ(&facts, &j, &i));
        assert!(!differ(&[], &j, &i));
    }

    #[test]
    fn reversal_is_not_provable() {
        let i = var(0);
        let mut facts = Vec::new();
        loop_facts(LocalId::from_index(0), Some(c(0)), Some(c(4)), &mut facts);
        let mirror = c(3).sub(&i).unwrap();
        assert!(!differ(&facts, &i, &mirror));
        facts.push(Fact::NonZero(i.sub(&mirror).unwrap()));
        assert!(differ(&facts, &i, &mirror));
        assert!(differ(&facts, &mirror, &i));
    }

    #[test]
    fn intervals() {
        let i = var(0);
        let mut facts = Vec::new();
        loop_facts(LocalId::from_index(0), Some(c(0)), Some(c(3)), &mut facts);
        let form = i.scale(2).unwrap().add(&c(1)).unwrap();
        assert_eq!(interval(&facts, &form), (Some(1), Some(5)));
        assert_eq!(interval(&[], &i), (None, None));
    }
}
