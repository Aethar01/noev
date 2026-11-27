//! Shared IR analyses used by both Cranelift and LLVM code generators:
//! - enum variant-tag inference per basic block,
//! - measurement-feedback detection for `sample` blocks,
//! - thunk collection (`sample`, `Expectation`, `Overlap`, `Undo`).

use std::collections::{HashMap, HashSet, VecDeque};

use noev_ir::Program;
use noev_ir::ir::{
    BinOp, Const, FuncId, Function, Local, Operand, Place, Prep, Proj, Rvalue, Stmt, Terminator,
};
use noev_sema::{Ty, TyKind};

/// Hashable key for a [`Place`] used in enum-variant dataflow analysis.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaceKey {
    /// Local index.
    pub local: Local,
    /// Projection chain.
    pub proj: Vec<ProjKey>,
}

/// Hashable projection step.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ProjKey {
    /// Deref.
    Deref,
    /// Field index.
    Field(u32),
    /// Constant integer array index.
    IndexConst(i64),
    /// Dynamic array index.
    IndexOther,
}

impl PlaceKey {
    /// Builds a [`PlaceKey`] from an IR [`Place`].
    pub fn from_place(p: &Place) -> Self {
        Self {
            local: p.local,
            proj: p
                .proj
                .iter()
                .map(|step| match step {
                    Proj::Deref => ProjKey::Deref,
                    Proj::Field(i) => ProjKey::Field(*i),
                    Proj::Index(Operand::Const(Const::Int(k))) => ProjKey::IndexConst(*k),
                    Proj::Index(_) => ProjKey::IndexOther,
                })
                .collect(),
        }
    }
}

/// Collects sorted [`FuncId`]s that require `NoevCompiledFn` thunks.
pub fn collect_thunks(program: &Program) -> Vec<FuncId> {
    let mut set = HashSet::new();
    for func in &program.functions {
        for block in &func.blocks {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Sample { func: f, .. }
                    | Stmt::Expectation {
                        prep: Prep::Call(f, _),
                        ..
                    } => {
                        set.insert(*f);
                    }
                    Stmt::Overlap { bra, ket, .. } => {
                        if let Prep::Call(f, _) = bra {
                            set.insert(*f);
                        }
                        if let Prep::Call(f, _) = ket {
                            set.insert(*f);
                        }
                    }
                    _ => {}
                }
            }
            if let Terminator::Undo { func: f, .. } = &block.term {
                set.insert(*f);
            }
        }
    }
    let mut list: Vec<FuncId> = set.into_iter().collect();
    list.sort_unstable();
    list
}

/// Checks whether `func` (or any callee) contains measurement-dependent control flow
/// (`Branch { control: Some(_) }` or a dynamic `Bit` label in `Prepare`), requiring
/// shot-by-shot execution rather than static circuit recording.
pub fn has_feedback(program: &Program, root: FuncId) -> bool {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(root);
    visited.insert(root);
    while let Some(fid) = queue.pop_front() {
        let func = &program.functions[fid.index()];
        for block in &func.blocks {
            for stmt in &block.stmts {
                if let Stmt::Prepare { label, .. } = stmt {
                    let is_const = matches!(label, Operand::Const(_));
                    if !is_const && operand_ty(program, func, label) == Ty::BIT {
                        return true;
                    }
                }
            }
            match &block.term {
                Terminator::Branch {
                    control: Some(_), ..
                } => return true,
                Terminator::Call { func: callee, .. } if visited.insert(*callee) => {
                    queue.push_back(*callee);
                }
                _ => {}
            }
        }
    }
    false
}

/// Forward dataflow pass per function: infers which enum variant a [`PlaceKey`] holds
/// in each basic block (from `tag = Tag(place); cond = (tag == K); Branch(cond, then, else)`).
pub fn analyze_enum_variants(func: &Function) -> Vec<HashMap<PlaceKey, u32>> {
    let n = func.blocks.len();
    let mut in_facts: Vec<Option<HashMap<PlaceKey, u32>>> = vec![None; n];
    if n == 0 {
        return Vec::new();
    }
    in_facts[0] = Some(HashMap::new());
    let mut queue = VecDeque::new();
    queue.push_back(0usize);

    while let Some(bi) = queue.pop_front() {
        let mut facts = in_facts[bi].clone().unwrap_or_default();
        let mut tag_of: HashMap<Local, PlaceKey> = HashMap::new();
        let mut cond_fact: HashMap<Local, (PlaceKey, u32)> = HashMap::new();
        let block = &func.blocks[bi];

        for stmt in &block.stmts {
            if let Stmt::Assign(dest, rv) = stmt {
                facts.retain(|p, _| p.local != dest.local);
                if dest.proj.is_empty() {
                    tag_of.remove(&dest.local);
                    cond_fact.remove(&dest.local);
                    match rv {
                        Rvalue::Tag(Operand::Copy(p) | Operand::Move(p)) => {
                            tag_of.insert(dest.local, PlaceKey::from_place(p));
                        }
                        Rvalue::Binary(BinOp::Eq, a, b) => {
                            let matched = match (a, b) {
                                (
                                    Operand::Copy(p) | Operand::Move(p),
                                    Operand::Const(Const::Int(k)),
                                )
                                | (
                                    Operand::Const(Const::Int(k)),
                                    Operand::Copy(p) | Operand::Move(p),
                                ) if p.proj.is_empty() && *k >= 0 => {
                                    tag_of.get(&p.local).map(|src| (src.clone(), *k as u32))
                                }
                                _ => None,
                            };
                            if let Some(pair) = matched {
                                cond_fact.insert(dest.local, pair);
                            }
                        }
                        Rvalue::Variant(v, _) => {
                            facts.insert(PlaceKey::from_place(dest), *v);
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut push_edge = |target: usize, edge_facts: HashMap<PlaceKey, u32>| {
            let changed = match &mut in_facts[target] {
                None => {
                    in_facts[target] = Some(edge_facts);
                    true
                }
                Some(existing) => {
                    let old_len = existing.len();
                    existing.retain(|k, v| edge_facts.get(k) == Some(v));
                    existing.len() != old_len
                }
            };
            if changed {
                queue.push_back(target);
            }
        };

        match &block.term {
            Terminator::Goto(next)
            | Terminator::Call { next, .. }
            | Terminator::Undo { next, .. }
            | Terminator::Assert { next, .. } => {
                push_edge(next.index(), facts);
            }
            Terminator::Branch {
                cond,
                then,
                otherwise,
                ..
            } => {
                let mut then_facts = facts.clone();
                if let Operand::Copy(p) | Operand::Move(p) = cond
                    && p.proj.is_empty()
                    && let Some((enum_place, v)) = cond_fact.get(&p.local)
                {
                    then_facts.insert(enum_place.clone(), *v);
                }
                push_edge(then.index(), then_facts);
                push_edge(otherwise.index(), facts);
            }
            Terminator::Return | Terminator::Unreachable => {}
        }
    }

    in_facts
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect()
}

/// Computes the concrete [`Ty`] of an [`Operand`] in `func`.
pub fn operand_ty(program: &Program, func: &Function, op: &Operand) -> Ty {
    match op {
        Operand::Copy(p) | Operand::Move(p) => {
            let mut ty = func.locals[p.local.index()].ty;
            for proj in &p.proj {
                ty = match proj {
                    Proj::Deref => match program.types.kind(ty) {
                        TyKind::Ref { inner, .. } => *inner,
                        _ => ty,
                    },
                    Proj::Field(i) => match program.types.kind(ty) {
                        TyKind::Tuple(elems) => elems[*i as usize],
                        TyKind::Struct(sid) => {
                            let def = &program.types.structs[sid.index()];
                            if let Some(variants) = &def.variants {
                                variants
                                    .iter()
                                    .find_map(|v| v.data.get(*i as usize).copied())
                                    .unwrap_or(Ty::UNIT)
                            } else {
                                def.fields[*i as usize].1
                            }
                        }
                        _ => Ty::UNIT,
                    },
                    Proj::Index(_) => match program.types.kind(ty) {
                        TyKind::Array(elem, _) => *elem,
                        _ => Ty::UNIT,
                    },
                };
            }
            ty
        }
        Operand::Const(c) => const_ty(*c),
    }
}

/// Concrete [`Ty`] of a [`Const`].
pub fn const_ty(c: Const) -> Ty {
    match c {
        Const::Unit => Ty::UNIT,
        Const::Int(_) => Ty::INT,
        Const::Sized(_, w) => match (w.bits, w.signed) {
            (8, true) => Ty::I8,
            (16, true) => Ty::I16,
            (32, true) => Ty::I32,
            (64, true) => Ty::I64,
            (8, false) => Ty::U8,
            (16, false) => Ty::U16,
            (32, false) => Ty::U32,
            _ => Ty::U64,
        },
        Const::Float(_) => Ty::FLOAT,
        Const::F32(_) => Ty::F32,
        Const::Complex(..) => Ty::COMPLEX,
        Const::C32(..) => Ty::C32,
        Const::Bool(_) => Ty::BOOL,
        Const::Bit(_) => Ty::BIT,
    }
}
