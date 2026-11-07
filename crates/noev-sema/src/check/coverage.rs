//! Whether a `match`'s arms cover every value (`spec/grammar.md` §2), and an example of each
//! kind of value they miss: the usefulness algorithm of Maranget's "Warnings for pattern
//! matching", on rows of patterns.

use super::*;

/// A value no arm matches: a constructor and its parts, or any value.
pub(super) enum Witness {
    Any,
    Ctor(Ctor, Vec<Witness>),
}

/// One of the finitely many shapes of a value: a variant, `false` or `true` (`0` or `1` for
/// a `Bit`), or the one shape of a tuple or array.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Ctor {
    Variant(u32),
    Bool(bool),
    Single,
}

/// Patterns to match one after another; `None` matches anything.
type Row = Vec<Option<PatId>>;

/// What the first pattern of a row tests.
enum Head {
    Any,
    Ctor(Ctor, Row),
    /// A literal of a type with endlessly many values (`Int`, `String`).
    Other,
}

/// Arrays longer than this are only covered by a catch-all, so coverage stays quick.
const MAX_ARRAY: u64 = 16;

impl Checker<'_> {
    /// The values of `ty` that `pats` miss: one for each missing shape, or a single
    /// `Witness::Any` for a type whose values cannot all be listed. Empty if they cover all.
    pub(super) fn uncovered(&self, ty: Ty, pats: &[PatId]) -> Vec<Witness> {
        let rows: Vec<Row> = pats.iter().map(|&p| vec![Some(p)]).collect();
        match self.ctors(ty) {
            Some(ctors) => ctors
                .into_iter()
                .filter_map(|(ctor, parts)| {
                    let rows = self.specialize(&rows, ctor, parts.len(), ty);
                    self.witness(&rows, &parts).map(|w| Witness::Ctor(ctor, w))
                })
                .collect(),
            None => self.witness(&rows, &[ty]).into_iter().flatten().collect(),
        }
    }

    /// Values of `tys`, one each, that no row matches, if there are any.
    fn witness(&self, rows: &[Row], tys: &[Ty]) -> Option<Vec<Witness>> {
        let Some((&ty, rest)) = tys.split_first() else {
            return rows.is_empty().then(Vec::new);
        };
        let heads: Vec<Head> = rows.iter().map(|r| self.head(r[0], ty)).collect();
        let named = heads.iter().any(|h| matches!(h, Head::Ctor(..)));
        if let Some(ctors) = self.ctors(ty).filter(|_| named) {
            for (ctor, parts) in ctors {
                let specialized = self.specialize(rows, ctor, parts.len(), ty);
                let arity = parts.len();
                if let Some(mut found) =
                    self.witness(&specialized, &[parts, rest.to_vec()].concat())
                {
                    let tail = found.split_off(arity);
                    let mut all = vec![Witness::Ctor(ctor, found)];
                    all.extend(tail);
                    return Some(all);
                }
            }
            return None;
        }
        // No row names a shape here, so only rows starting with a catch-all can match.
        let default: Vec<Row> = rows
            .iter()
            .zip(&heads)
            .filter(|(_, h)| matches!(h, Head::Any))
            .map(|(r, _)| r[1..].to_vec())
            .collect();
        let mut found = self.witness(&default, rest)?;
        found.insert(0, Witness::Any);
        Some(found)
    }

    /// The rows that can match a value of shape `ctor`, with the first pattern replaced by
    /// patterns for its `arity` parts.
    fn specialize(&self, rows: &[Row], ctor: Ctor, arity: usize, ty: Ty) -> Vec<Row> {
        rows.iter()
            .filter_map(|row| {
                let mut parts = match self.head(row[0], ty) {
                    Head::Any => vec![None; arity],
                    Head::Ctor(c, parts) if c == ctor => parts,
                    _ => return None,
                };
                parts.extend_from_slice(&row[1..]);
                Some(parts)
            })
            .collect()
    }

    /// The shapes of `ty` and the types of each one's parts, if it has finitely many.
    fn ctors(&self, ty: Ty) -> Option<Vec<(Ctor, Vec<Ty>)>> {
        let types = &self.t.types;
        let ty = types.deref(ty);
        if let Some(variants) = types.variants(ty) {
            return Some(
                variants
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (Ctor::Variant(i as u32), v.data.clone()))
                    .collect(),
            );
        }
        match types.kind(ty) {
            TyKind::Bit | TyKind::Bool => Some(vec![
                (Ctor::Bool(false), Vec::new()),
                (Ctor::Bool(true), Vec::new()),
            ]),
            TyKind::Tuple(elems) => Some(vec![(Ctor::Single, elems.to_vec())]),
            &TyKind::Array(elem, len) if len <= MAX_ARRAY => {
                Some(vec![(Ctor::Single, vec![elem; len as usize])])
            }
            _ => None,
        }
    }

    /// What `pat`, matching a value of `ty`, tests first. Patterns the checker already
    /// reported count as catch-alls, so one mistake gives one error.
    fn head(&self, pat: Option<PatId>, ty: Ty) -> Head {
        let ast = self.ast;
        let Some(pat) = pat else { return Head::Any };
        let arity = |ctor| {
            self.ctors(ty)
                .and_then(|c| c.into_iter().find(|(c, _)| *c == ctor))
                .map_or(0, |(_, parts)| parts.len())
        };
        match ast.pat(pat).kind {
            PatKind::Name(_) | PatKind::Wildcard => Head::Any,
            PatKind::Literal(PatLiteral::Bool(b)) => Head::Ctor(Ctor::Bool(b), Vec::new()),
            PatKind::Literal(PatLiteral::Int(v)) if self.t.types.deref(ty) == Ty::BIT => {
                Head::Ctor(Ctor::Bool(v == 1), Vec::new())
            }
            PatKind::Literal(_) => Head::Other,
            PatKind::Tuple(pats) | PatKind::Array(pats) => {
                let pats = ast.list(pats);
                if self.ctors(ty).is_none() || pats.len() != arity(Ctor::Single) {
                    return Head::Any;
                }
                Head::Ctor(Ctor::Single, pats.iter().map(|&p| Some(p)).collect())
            }
            PatKind::Variant { data, .. } => {
                let Some(index) = self.t.pat_variants[pat.index()] else {
                    return Head::Any;
                };
                let ctor = Ctor::Variant(index);
                let arity = arity(ctor);
                let parts = match data {
                    PatData::None => vec![None; arity],
                    PatData::Tuple(pats) => {
                        let pats = ast.list(pats);
                        if pats.len() != arity {
                            return Head::Any;
                        }
                        pats.iter().map(|&p| Some(p)).collect()
                    }
                    PatData::Fields { fields, .. } => {
                        let variant = &self.t.types.variants(ty).expect("an enum")[index as usize];
                        let fields = ast.list(fields);
                        variant
                            .names
                            .iter()
                            .map(|&name| fields.iter().find(|f| f.name == name).and_then(|f| f.pat))
                            .collect()
                    }
                };
                Head::Ctor(ctor, parts)
            }
        }
    }

    /// `w` as a pattern a value of `ty` would fit: `(1, _)`, `Some(0)`,
    /// `Outcome::Retry { last: 1, .. }`.
    pub(super) fn show_witness(&self, w: &Witness, ty: Ty) -> String {
        let types = &self.t.types;
        let ty = types.deref(ty);
        let Witness::Ctor(ctor, parts) = w else {
            return "_".into();
        };
        let parts_of = |tys: &[Ty]| -> Vec<String> {
            parts
                .iter()
                .zip(tys)
                .map(|(p, &t)| self.show_witness(p, t))
                .collect()
        };
        match (*ctor, types.kind(ty)) {
            (Ctor::Bool(b), TyKind::Bit) => u8::from(b).to_string(),
            (Ctor::Bool(b), _) => b.to_string(),
            (Ctor::Single, TyKind::Tuple(elems)) => {
                let shown = parts_of(elems);
                if shown.len() == 1 {
                    format!("({},)", shown[0])
                } else {
                    format!("({})", shown.join(", "))
                }
            }
            (Ctor::Single, &TyKind::Array(elem, len)) => {
                format!("[{}]", parts_of(&vec![elem; len as usize]).join(", "))
            }
            (Ctor::Variant(index), &TyKind::Struct(s)) => {
                let def = &types.structs[s.index()];
                let variant = &types.variants(ty).expect("an enum")[index as usize];
                let mut name = self.name(variant.name).to_string();
                if !def.bare {
                    name = format!("{}::{name}", types.display(ty, self.ast));
                }
                let shown = parts_of(&variant.data);
                if shown.is_empty() {
                    name
                } else if variant.named {
                    let known: Vec<String> = variant
                        .names
                        .iter()
                        .zip(&shown)
                        .filter(|(_, s)| *s != "_")
                        .map(|(&n, s)| format!("{}: {s}", self.name(n)))
                        .collect();
                    if known.is_empty() {
                        format!("{name} {{ .. }}")
                    } else {
                        format!("{name} {{ {}, .. }}", known.join(", "))
                    }
                } else {
                    format!("{name}({})", shown.join(", "))
                }
            }
            _ => "_".into(),
        }
    }
}
