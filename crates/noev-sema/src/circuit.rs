//! Which runs are circuits (`spec/execution.md` §4).
//!
//! A run is a circuit if, until its last quantum operation, its measurement results are only
//! stored, combined with logic, used in `if`/`while` conditions, or put into its result. This
//! module works that out for every function and `sample` run, finds the first use that
//! breaks the rule, and reports `circuit fn`s that are not circuits (N021) and repeated runs
//! that will run shot by shot.
//!
//! Two passes over each body. The first marks, for every statement, whether a quantum
//! operation can still happen at or after it (counting later loop iterations). The second
//! walks forward tracking which values come from measurements, judging each use by the first
//! pass. Both work a statement at a time: a measurement result used in the same statement as
//! a later quantum operation counts as used before it.

use noev_syntax::ast::*;
use noev_syntax::{Diagnostic, FxHashMap, Severity, Span};

use crate::check::{Callee, LocalId, Res, Typed};
use crate::prelude::Builtin;
use crate::ty::{Ty, TyKind};

/// What the rule says about every function and `sample` run.
#[derive(Clone, Debug, Default)]
pub struct Circuits {
    /// For each function, indexed by [`FnId::index`].
    pub fns: Vec<CircuitInfo>,
    /// For each `sample` expression.
    pub runs: FxHashMap<ExprId, CircuitInfo>,
}

/// What the rule says about one function or run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CircuitInfo {
    /// Whether it does quantum operations, itself or through calls.
    pub quantum: bool,
    /// Whether its parameters and result contain no qubits or references (a closed run).
    pub closed: bool,
    /// The first use of a measurement result that stops it being a circuit, and what the use
    /// was ("used in arithmetic"); `None` when it is a circuit.
    pub not_circuit: Option<(Span, String)>,
    /// Whether a condition depends on a measurement result before its last quantum operation.
    pub feedback: bool,
    /// Where it calls `release` (itself or through a call). The simulator checks that claim,
    /// which needs the state of every shot, so such runs are not sampled from one simulation.
    pub releases: Option<Span>,
}

impl CircuitInfo {
    /// Whether the run can be computed once and sampled by the current compiler: a circuit
    /// whose control flow does not depend on measurement results (classical control is not
    /// compiled yet).
    pub fn sampled(&self) -> bool {
        self.quantum && self.not_circuit.is_none() && !self.feedback && self.releases.is_none()
    }
}

/// The rule for every function and run, and its diagnostics: N021, and warnings for
/// repeated runs that will run shot by shot.
pub fn check_circuits(ast: &Ast, typed: &Typed) -> (Circuits, Vec<Diagnostic>) {
    let quantum = infer_quantum(ast, typed);
    let releases = infer_releases(ast, typed);
    // A call is part of the run that makes it, so a function is only a circuit if what it
    // calls is: repeat until nothing changes, since functions can call each other.
    let mut infos = vec![CircuitInfo::default(); ast.all_fns().len()];
    for _ in 0..=ast.all_fns().len() {
        let mut next = Vec::with_capacity(infos.len());
        for (index, decl) in ast.all_fns().iter().enumerate() {
            let sig = &typed.fns[index].sig;
            let closed = !decl.receiver.is_some_and(|r| r.kind != OwnKind::Mv)
                && sig
                    .params
                    .iter()
                    .chain([&sig.ret])
                    .all(|&t| !holds_quantum_or_ref(typed, t))
                && typed.fns[index]
                    .receiver_ty
                    .is_none_or(|t| !holds_quantum_or_ref(typed, t));
            let mut info = CircuitInfo {
                quantum: quantum[index],
                closed,
                releases: releases[index],
                ..CircuitInfo::default()
            };
            if let (true, Some(body)) = (quantum[index], decl.body) {
                Walker::run(ast, typed, &quantum, &infos, &mut info, body);
            }
            next.push(info);
        }
        if next == infos {
            break;
        }
        infos = next;
    }

    let mut runs = FxHashMap::default();
    for (index, expr) in ast.all_exprs().iter().enumerate() {
        let ExprKind::Sample { body, .. } = expr.kind else {
            continue;
        };
        let mut info = CircuitInfo {
            closed: true,
            releases: block_releases(ast, typed, &releases, body),
            ..CircuitInfo::default()
        };
        info.quantum = contains_quantum(ast, typed, &quantum, Node::Block(body));
        if info.quantum {
            Walker::run(ast, typed, &quantum, &infos, &mut info, body);
        }
        runs.insert(ExprId::from_index(index), info);
    }

    let mut diagnostics = Vec::new();
    for (index, decl) in ast.all_fns().iter().enumerate() {
        if !decl.is_circuit {
            continue;
        }
        let info = &infos[index];
        let name = ast.name(decl.name);
        if !info.closed {
            diagnostics.push(
                Diagnostic::error(
                    decl.name_span,
                    format!("`{name}` is declared `circuit fn`, but it is not a closed run"),
                )
                .with_code("N021")
                .with_help("a circuit's parameters and result contain no qubits or references"),
            );
        } else if let Some((span, what)) = &info.not_circuit {
            diagnostics.push(
                Diagnostic::error(*span, format!("`{name}` is declared `circuit fn`, but a measurement result is {what} before its last quantum operation"))
                    .with_code("N021")
                    .with_label(decl.name_span, "declared `circuit fn` here")
                    .with_help("use measurement results only in `if`/`while` conditions or the result until the last quantum operation"),
            );
        }
    }
    let circuits = Circuits { fns: infos, runs };
    repeated_run_warnings(ast, typed, &circuits, &mut diagnostics);
    (circuits, diagnostics)
}

/// Warns, at each `sample` and expectation value, when the repeated run will run shot by
/// shot, and says why (`spec/execution.md` §5).
fn repeated_run_warnings(ast: &Ast, typed: &Typed, circuits: &Circuits, out: &mut Vec<Diagnostic>) {
    for (index, expr) in ast.all_exprs().iter().enumerate() {
        let (info, span, subject) = match expr.kind {
            ExprKind::Sample { .. } => {
                let Some(info) = circuits.runs.get(&ExprId::from_index(index)) else {
                    continue;
                };
                // Point at `sample`, not the whole block.
                let span = Span::new(expr.span.start, expr.span.start + "sample".len() as u32);
                (info, span, "`sample` runs this".to_string())
            }
            ExprKind::Expectation { prep, .. } => {
                let Some(Callee::Fn(f)) = typed.callees[prep.index()] else {
                    continue;
                };
                let name = ast.name(ast.function(f).name);
                (
                    &circuits.fns[f.index()],
                    ast.expr(prep).span,
                    format!("`{name}` runs"),
                )
            }
            _ => continue,
        };
        if !info.quantum || info.sampled() {
            continue;
        }
        let warning = match &info.not_circuit {
            Some((at, what)) => Diagnostic::error(span, format!("{subject} shot by shot: a measurement result is {what} before its last quantum operation"))
                .with_label(*at, "here")
                .with_help("use measurement results only in `if`/`while` conditions or the result until the last quantum operation, and the run is sampled from one simulation"),
            None if info.feedback => Diagnostic::error(span, format!("{subject} shot by shot: conditions on measurement results are not compiled to circuits yet")),
            None => Diagnostic::error(span, format!("{subject} shot by shot so the simulator can check its `release`"))
                .with_label(info.releases.unwrap_or(span), "released here")
                .with_help("`discard` resets instead, and allows sampling from one simulation"),
        };
        out.push(Diagnostic {
            severity: Severity::Warning,
            ..warning
        });
    }
}

fn holds_quantum_or_ref(typed: &Typed, ty: Ty) -> bool {
    let types = &typed.types;
    match types.kind(ty) {
        TyKind::Qubit | TyKind::Ref { .. } => true,
        TyKind::Tuple(elems) => elems.iter().any(|&t| holds_quantum_or_ref(typed, t)),
        &TyKind::Array(elem, _) => holds_quantum_or_ref(typed, elem),
        &TyKind::Struct(s) => types.structs[s.index()]
            .fields
            .iter()
            .any(|&(_, t)| holds_quantum_or_ref(typed, t)),
        _ => false,
    }
}

/// Where each function calls `release`, itself or through the functions it calls.
fn infer_releases(ast: &Ast, typed: &Typed) -> Vec<Option<Span>> {
    let mut result: Vec<Option<Span>> = vec![None; ast.all_fns().len()];
    loop {
        let mut changed = false;
        for (index, decl) in ast.all_fns().iter().enumerate() {
            if result[index].is_some() {
                continue;
            }
            let Some(body) = decl.body else { continue };
            if let Some(span) = block_releases(ast, typed, &result, body) {
                result[index] = Some(span);
                changed = true;
            }
        }
        if !changed {
            return result;
        }
    }
}

/// Where a block calls `release`, itself or through a function in `releases`.
fn block_releases(
    ast: &Ast,
    typed: &Typed,
    releases: &[Option<Span>],
    body: BlockId,
) -> Option<Span> {
    let mut stack = vec![Node::Block(body)];
    while let Some(node) = stack.pop() {
        children(ast, node, &mut stack);
        let Node::Expr(e) = node else { continue };
        let found = match typed.callees[e.index()] {
            Some(Callee::Builtin(Builtin::Release)) => true,
            Some(Callee::Fn(f)) => releases[f.index()].is_some(),
            _ => false,
        };
        if found {
            return Some(ast.expr(e).span);
        }
    }
    None
}

/// Whether each function does quantum operations, itself or through the functions it calls.
fn infer_quantum(ast: &Ast, typed: &Typed) -> Vec<bool> {
    let mut quantum = vec![false; ast.all_fns().len()];
    loop {
        let mut changed = false;
        for (index, decl) in ast.all_fns().iter().enumerate() {
            if quantum[index] {
                continue;
            }
            let Some(body) = decl.body else { continue };
            let mut found = false;
            let mut stack = vec![Node::Block(body)];
            while let Some(node) = stack.pop() {
                children(ast, node, &mut stack);
                if let Node::Expr(e) = node {
                    found |= is_quantum_op(ast, typed, &quantum, e);
                }
            }
            if found {
                quantum[index] = true;
                changed = true;
            }
        }
        if !changed {
            return quantum;
        }
    }
}

/// Whether an expression is itself a quantum operation of the run it is in. `sample` and
/// expectation values start runs of their own, so they are not.
fn is_quantum_op(ast: &Ast, typed: &Typed, quantum: &[bool], e: ExprId) -> bool {
    match ast.expr(e).kind {
        ExprKind::Ket | ExprKind::BraTest { .. } | ExprKind::Ctrl { .. } => true,
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } => match typed.callees[e.index()] {
            Some(Callee::Builtin(
                Builtin::Gate(_)
                | Builtin::Measure
                | Builtin::Reset
                | Builtin::Discard
                | Builtin::Release,
            )) => true,
            Some(Callee::Fn(f)) => quantum[f.index()],
            _ => false,
        },
        _ => false,
    }
}

/// Whether a node contains a quantum operation of the run it is in.
fn contains_quantum(ast: &Ast, typed: &Typed, quantum: &[bool], node: Node) -> bool {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let Node::Expr(e) = node
            && is_quantum_op(ast, typed, quantum, e)
        {
            return true;
        }
        children(ast, node, &mut stack);
    }
    false
}

#[derive(Clone, Copy)]
enum Node {
    Block(BlockId),
    Stmt(StmtId),
    Expr(ExprId),
}

/// Pushes a node's children. Expectation preparations and `sample` runs are left out: they
/// are separate runs.
fn children(ast: &Ast, node: Node, out: &mut Vec<Node>) {
    match node {
        Node::Block(b) => out.extend(ast.list(ast.block(b).stmts).iter().map(|&s| Node::Stmt(s))),
        Node::Stmt(s) => match ast.stmt(s).kind {
            StmtKind::Let { init, .. } => out.push(Node::Expr(init)),
            StmtKind::Assign { place, value, .. } => {
                out.extend([Node::Expr(place), Node::Expr(value)])
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr { expr: e, .. } => {
                out.push(Node::Expr(e))
            }
            _ => {}
        },
        Node::Expr(e) => {
            let mut push = |e: ExprId| out.push(Node::Expr(e));
            match ast.expr(e).kind {
                ExprKind::Tuple(l) | ExprKind::Array(l) => {
                    ast.list(l).iter().for_each(|&x| push(x))
                }
                ExprKind::Str(parts) => ast.list(parts).iter().for_each(|p| {
                    if let StrPart::Value { expr, .. } = *p {
                        push(expr);
                    }
                }),
                ExprKind::ArrayRepeat { value, count } => {
                    push(value);
                    push(count);
                }
                ExprKind::StructLit { fields, .. } => ast
                    .list(fields)
                    .iter()
                    .filter_map(|f| f.value)
                    .for_each(&mut push),
                ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => push(base),
                ExprKind::Index { base, index } => {
                    push(base);
                    push(index);
                }
                ExprKind::Call { args, .. } => ast.list(args).iter().for_each(|&a| push(a)),
                ExprKind::Sample { shots, .. } => push(shots),
                ExprKind::MethodCall { receiver, args, .. } => {
                    push(receiver);
                    ast.list(args).iter().for_each(|&a| push(a));
                }
                ExprKind::Unary { operand, .. } => push(operand),
                ExprKind::Binary { lhs, rhs, .. } => {
                    push(lhs);
                    push(rhs);
                }
                ExprKind::Own { place, .. } => push(place),
                ExprKind::Adj(call) => push(call),
                ExprKind::BraTest { operand, .. } => push(operand),
                ExprKind::Expectation { observable, .. } => push(observable),
                ExprKind::Block(b) | ExprKind::Unsafe(b) => out.push(Node::Block(b)),
                ExprKind::If {
                    cond,
                    then,
                    otherwise,
                } => {
                    out.push(Node::Expr(cond));
                    out.push(Node::Block(then));
                    if let Some(o) = otherwise {
                        out.push(Node::Expr(o));
                    }
                }
                ExprKind::While { cond, body } => {
                    out.push(Node::Expr(cond));
                    out.push(Node::Block(body));
                }
                ExprKind::For {
                    start, end, body, ..
                } => {
                    out.extend([Node::Expr(start), Node::Expr(end), Node::Block(body)]);
                }
                ExprKind::Ctrl { controls, body } => {
                    ast.list(controls)
                        .iter()
                        .for_each(|&c| out.push(Node::Expr(c)));
                    out.push(Node::Block(body));
                }
                ExprKind::With { with, body } => out.extend([Node::Block(with), Node::Block(body)]),
                _ => {}
            }
        }
    }
}

struct Walker<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    quantum: &'a [bool],
    /// What is known so far about each function, for the calls the run makes.
    fns: &'a [CircuitInfo],
    /// For each statement, whether a quantum operation can happen at or after it.
    later: Vec<bool>,
    /// Whether each local holds a value that depends on a measurement.
    tainted: Vec<bool>,
    /// How many enclosing conditions depend on measurements: assignments under them depend
    /// on measurements too.
    control: u32,
    info: &'a mut CircuitInfo,
}

impl<'a> Walker<'a> {
    fn new(
        ast: &'a Ast,
        typed: &'a Typed,
        quantum: &'a [bool],
        fns: &'a [CircuitInfo],
        info: &'a mut CircuitInfo,
    ) -> Self {
        Walker {
            ast,
            typed,
            quantum,
            fns,
            later: vec![false; ast.all_stmts().len()],
            tainted: vec![false; typed.locals.len()],
            control: 0,
            info,
        }
    }

    /// Applies the rule to a run whose body is `body`, recording what it finds in `info`.
    fn run(
        ast: &'a Ast,
        typed: &'a Typed,
        quantum: &'a [bool],
        fns: &'a [CircuitInfo],
        info: &'a mut CircuitInfo,
        body: BlockId,
    ) {
        let mut walker = Walker::new(ast, typed, quantum, fns, info);
        walker.mark_block(body, false);
        walker.block(body);
    }

    fn contains_quantum(&self, node: Node) -> bool {
        contains_quantum(self.ast, self.typed, self.quantum, node)
    }

    /// First pass: marks `later` for every statement in `block`, given whether a quantum
    /// operation can happen after the block. Returns whether the block contains one.
    fn mark_block(&mut self, block: BlockId, after: bool) -> bool {
        let stmts = self.ast.list(self.ast.block(block).stmts).to_vec();
        let mut later = after;
        let mut any = false;
        for &stmt in stmts.iter().rev() {
            let here = self.contains_quantum(Node::Stmt(stmt));
            any |= here;
            later |= here;
            self.later[stmt.index()] = later;
            self.mark_nested(Node::Stmt(stmt), later);
        }
        any
    }

    /// Marks the blocks nested in a statement or expression. Inside a loop that does quantum
    /// work, every point comes before the next iteration's quantum operations.
    fn mark_nested(&mut self, node: Node, later: bool) {
        let mut stack = Vec::new();
        children(self.ast, node, &mut stack);
        while let Some(child) = stack.pop() {
            match child {
                Node::Block(b) => {
                    self.mark_block(b, later);
                }
                Node::Expr(e) => match self.ast.expr(e).kind {
                    ExprKind::While { cond, body }
                    | ExprKind::For {
                        body, start: cond, ..
                    } => {
                        let loops = self.contains_quantum(Node::Block(body));
                        self.mark_block(body, later || loops);
                        stack.push(Node::Expr(cond));
                    }
                    _ => children(self.ast, child, &mut stack),
                },
                Node::Stmt(_) => children(self.ast, child, &mut stack),
            }
        }
    }

    fn violate(&mut self, span: Span, what: &str) {
        if self.info.not_circuit.is_none() {
            self.info.not_circuit = Some((span, what.to_string()));
        }
    }

    /// Second pass.
    fn block(&mut self, block: BlockId) {
        for &stmt in self.ast.list(self.ast.block(block).stmts) {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, id: StmtId) {
        let before = self.later[id.index()];
        match self.ast.stmt(id).kind {
            StmtKind::Let { pat, init, .. } => {
                let tainted = self.expr(init, before) || self.control > 0;
                self.taint_pattern(pat, tainted);
            }
            StmtKind::Assign { place, value, .. } => {
                let tainted = self.expr(value, before) || self.control > 0;
                self.place_indices(place, before);
                if let Some(local) = root(self.ast, self.typed, place)
                    && tainted
                {
                    self.tainted[local.index()] = true;
                }
            }
            StmtKind::Return(Some(e)) | StmtKind::Expr { expr: e, .. } => {
                self.expr(e, before);
            }
            StmtKind::Assert(e) => {
                if self.expr(e, before) && before {
                    let span = self.ast.expr(e).span;
                    self.violate(span, "used in `assert`");
                }
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn taint_pattern(&mut self, pat: PatId, tainted: bool) {
        match self.ast.pat(pat).kind {
            PatKind::Name(_) => {
                if let Some(local) = self.typed.pat_locals[pat.index()] {
                    self.tainted[local.index()] = tainted;
                }
            }
            PatKind::Wildcard => {}
            PatKind::Tuple(pats) => {
                for &p in self.ast.list(pats) {
                    self.taint_pattern(p, tainted);
                }
            }
        }
    }

    /// Checks the indices inside an assignment target.
    fn place_indices(&mut self, place: ExprId, before: bool) {
        match self.ast.expr(place).kind {
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.place_indices(base, before)
            }
            ExprKind::Index { base, index } => {
                self.place_indices(base, before);
                if self.expr(index, before) && before {
                    let span = self.ast.expr(index).span;
                    self.violate(span, "used as an array index");
                }
            }
            _ => {}
        }
    }

    /// Walks an expression; returns whether its value depends on a measurement. `before` says
    /// whether a quantum operation can still follow.
    fn expr(&mut self, id: ExprId, before: bool) -> bool {
        let ast = self.ast;
        let expr = *ast.expr(id);
        let is_logic_type = |ty: Ty| matches!(self.typed.types.deref(ty), Ty::BIT | Ty::BOOL);
        match expr.kind {
            ExprKind::Name(_) => match self.typed.res[id.index()] {
                Res::Local(local) => self.tainted[local.index()],
                _ => false,
            },
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.expr(base, before)
            }
            ExprKind::Index { base, index } => {
                let base = self.expr(base, before);
                if self.expr(index, before) && before {
                    self.violate(ast.expr(index).span, "used as an array index");
                }
                base
            }
            ExprKind::Own { place, .. } => self.expr(place, before),
            ExprKind::Tuple(l) | ExprKind::Array(l) => {
                let mut tainted = false;
                for &e in ast.list(l) {
                    tainted |= self.expr(e, before);
                }
                tainted
            }
            ExprKind::ArrayRepeat { value, count } => {
                self.expr(count, before);
                self.expr(value, before)
            }
            ExprKind::StructLit { fields, .. } => {
                let mut tainted = false;
                for field in ast.list(fields) {
                    match field.value {
                        Some(v) => tainted |= self.expr(v, before),
                        None => {
                            let local = (0..self.typed.locals.len())
                                .rev()
                                .map(LocalId::from_index)
                                .find(|&l| self.typed.local(l).name == field.name);
                            tainted |= local.is_some_and(|l| self.tainted[l.index()]);
                        }
                    }
                }
                tainted
            }
            ExprKind::Str(parts) => {
                for part in ast.list(parts) {
                    if let StrPart::Value { expr, .. } = *part
                        && self.expr(expr, before)
                        && before
                    {
                        self.violate(ast.expr(expr).span, "formatted into a string");
                    }
                }
                false
            }
            ExprKind::BraTest { operand, .. } => {
                self.expr(operand, before);
                true
            }
            ExprKind::Unary { op, operand } => {
                let tainted = self.expr(operand, before);
                let logic = matches!(op, UnaryOp::Not | UnaryOp::BitNot)
                    && is_logic_type(self.typed.ty(operand));
                if tainted && before && !logic {
                    self.violate(expr.span, "used in arithmetic");
                }
                tainted
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let tainted = self.expr(lhs, before) | self.expr(rhs, before);
                let logic = matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::And
                        | BinaryOp::Or
                        | BinaryOp::BitAnd
                        | BinaryOp::BitOr
                        | BinaryOp::BitXor
                ) && is_logic_type(self.typed.ty(lhs));
                if tainted && before && !logic {
                    self.violate(expr.span, "used in arithmetic");
                }
                tainted
            }
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => self.call(id, before),
            ExprKind::Adj(call) => self.expr(call, before),
            // A separate run: its results are known before this run continues.
            ExprKind::Sample { shots, .. } => {
                if self.expr(shots, before) && before {
                    self.violate(ast.expr(shots).span, "used as a shot count");
                }
                false
            }
            ExprKind::Expectation { observable, .. } => {
                if self.expr(observable, before) && before {
                    self.violate(expr.span, "used in an observable");
                }
                false
            }
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.value_block(b),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                let condition = self.expr(cond, before);
                if condition && before {
                    self.info.feedback = true;
                }
                self.control += u32::from(condition);
                let mut tainted = self.value_block(then);
                if let Some(o) = otherwise {
                    tainted |= self.expr(o, before);
                }
                self.control -= u32::from(condition);
                tainted || condition
            }
            ExprKind::While { cond, body } => {
                // Twice, so values that become measurement-dependent in one iteration are
                // seen in the next.
                for _ in 0..2 {
                    let condition = self.expr(cond, before);
                    if condition && before {
                        self.info.feedback = true;
                    }
                    self.control += u32::from(condition);
                    self.block(body);
                    self.control -= u32::from(condition);
                }
                false
            }
            ExprKind::For {
                start, end, body, ..
            } => {
                if (self.expr(start, before) | self.expr(end, before)) && before {
                    self.violate(expr.span, "used as a loop bound");
                }
                for _ in 0..2 {
                    self.block(body);
                }
                false
            }
            ExprKind::Ctrl { controls, body } => {
                for &c in ast.list(controls) {
                    self.expr(c, before);
                }
                self.block(body);
                false
            }
            ExprKind::Error
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Ket
            | ExprKind::Path(_)
            | ExprKind::Unit
            | ExprKind::With { .. } => false,
        }
    }

    /// A block used as a value: tainted if its final expression is.
    fn value_block(&mut self, block: BlockId) -> bool {
        let stmts = self.ast.list(self.ast.block(block).stmts);
        let mut tainted = false;
        for (i, &stmt) in stmts.iter().enumerate() {
            match self.ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    tainted = self.expr(expr, self.later[stmt.index()]);
                }
                _ => self.stmt(stmt),
            }
        }
        tainted
    }

    fn call(&mut self, id: ExprId, before: bool) -> bool {
        let ast = self.ast;
        let (receiver, args) = match ast.expr(id).kind {
            ExprKind::Call { args, .. } => (None, ast.list(args)),
            ExprKind::MethodCall { receiver, args, .. } => (Some(receiver), ast.list(args)),
            _ => return false,
        };
        let callee = self.typed.callees[id.index()];
        let mut tainted_args = Vec::new();
        if let Some(r) = receiver {
            tainted_args.push((r, self.expr(r, before)));
        }
        for &a in args {
            tainted_args.push((a, self.expr(a, before)));
        }
        let any = tainted_args.iter().any(|&(_, t)| t);
        match callee {
            Some(Callee::Builtin(Builtin::Measure)) => true,
            // `Bool(b)` and `Bit(flag)` are logic, so allowed.
            Some(Callee::Convert(Ty::BOOL | Ty::BIT)) => any,
            Some(Callee::Convert(_)) => {
                if any && before {
                    self.violate(ast.expr(id).span, "converted to a number");
                }
                any
            }
            Some(Callee::Builtin(Builtin::Gate(_))) => {
                if let Some(&(arg, _)) = tainted_args.iter().find(|&&(_, t)| t)
                    && before
                {
                    self.violate(ast.expr(arg).span, "used in a gate's angle");
                }
                false
            }
            Some(Callee::Fn(f)) => {
                if let Some(&(arg, _)) = tainted_args.iter().find(|&&(_, t)| t)
                    && before
                {
                    let name = ast.name(ast.function(f).name);
                    let what = format!("passed to `{name}`");
                    self.violate(ast.expr(arg).span, &what);
                }
                // What happens inside the call is part of this run.
                let inner = &self.fns[f.index()];
                if let Some((span, what)) = &inner.not_circuit {
                    // Name the function where it happens, once.
                    let what = if what.contains(" in `") {
                        what.clone()
                    } else {
                        format!("{what} in `{}`", ast.name(ast.function(f).name))
                    };
                    self.violate(*span, &what);
                }
                self.info.feedback |= inner.feedback;
                // A quantum function's result comes from its own measurements.
                self.quantum[f.index()] || any
            }
            Some(Callee::Builtin(Builtin::Print { .. })) => {
                if let Some(&(arg, _)) = tainted_args.iter().find(|&&(_, t)| t)
                    && before
                {
                    self.violate(ast.expr(arg).span, "printed");
                }
                false
            }
            _ => {
                if let Some(&(arg, _)) = tainted_args.iter().find(|&&(_, t)| t)
                    && before
                {
                    self.violate(ast.expr(arg).span, "passed to a function");
                }
                any
            }
        }
    }
}

fn root(ast: &Ast, typed: &Typed, mut id: ExprId) -> Option<LocalId> {
    loop {
        match ast.expr(id).kind {
            ExprKind::Name(_) => {
                return match typed.res[id.index()] {
                    Res::Local(local) => Some(local),
                    _ => None,
                };
            }
            ExprKind::Field { base, .. }
            | ExprKind::TupleIndex { base, .. }
            | ExprKind::Index { base, .. } => id = base,
            _ => return None,
        }
    }
}
