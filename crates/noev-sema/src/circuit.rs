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

use crate::check::{Adjust, Callee, LocalId, Res, Typed};
use crate::prelude::Builtin;
use crate::ty::{Kind, Ty, TyKind};

/// What the rule says about every function and `sample` run.
#[derive(Clone, Debug, Default)]
pub struct Circuits {
    /// For each function, indexed by [`FnId::index`].
    pub fns: Vec<CircuitInfo>,
    /// For each `sample` expression.
    pub runs: FxHashMap<ExprId, CircuitInfo>,
    /// Every `if` and `while` that becomes classical control in a circuit, with the
    /// variables it changes, sorted.
    controls: Vec<(ExprId, Vec<LocalId>)>,
}

impl Circuits {
    /// For an `if` or `while` on a measurement result that becomes an `if` or `while` in the
    /// circuit: the `Bit` and `Bool` variables from outside it that it changes, which the
    /// circuit keeps in classical bits. `None` for any other expression.
    pub fn control(&self, expr: ExprId) -> Option<&[LocalId]> {
        let i = self
            .controls
            .binary_search_by_key(&expr, |(e, _)| *e)
            .ok()?;
        Some(&self.controls[i].1)
    }
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
    /// Why some of that feedback cannot be compiled into the circuit yet, and where.
    pub feedback_unsupported: Option<(Span, String)>,
    /// The `if`s and `while`s on measurement results that become classical control in the
    /// circuit, with the variables from outside them that they change.
    pub controls: Vec<(ExprId, Vec<LocalId>)>,
    /// Where it calls `release` (itself or through a call). The simulator checks that claim,
    /// which needs the state of every shot, so such runs are not sampled from one simulation.
    pub releases: Option<Span>,
}

impl CircuitInfo {
    /// Whether the run can be computed once and sampled by the current compiler: a circuit
    /// whose classical control can all be recorded, and that does not `release`.
    pub fn sampled(&self) -> bool {
        self.quantum
            && self.not_circuit.is_none()
            && self.feedback_unsupported.is_none()
            && self.releases.is_none()
    }
}

/// The rule for every function and run, and its diagnostics: N021, and warnings for
/// repeated runs that will run shot by shot.
pub fn check_circuits(ast: &Ast, typed: &Typed) -> (Circuits, Vec<Diagnostic>) {
    let order = CallOrder::new(ast, typed);
    let quantum = infer_quantum(ast, typed, &order);
    let recordable = infer_recordable(ast, typed, &order);
    let releases = infer_releases(ast, typed, &order);
    // A call is part of the run that makes it, so a function is only a circuit if what it
    // calls is: repeat until nothing changes, since functions can call each other.
    let mut cx = Context {
        ast,
        typed,
        quantum,
        recordable,
        fns: vec![CircuitInfo::default(); ast.all_fns().len()],
        given: vec![None; ast.all_fns().len()],
    };
    let mut buffers = Buffers::new(ast, typed);
    // Callees come first, so one pass is enough unless functions call each other in a cycle;
    // the bound stops a cycle whose first violation keeps moving.
    for _ in 0..=ast.all_fns().len() {
        let mut changed = false;
        for &index in &order.fns {
            let decl = &ast.all_fns()[index];
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
            let base = CircuitInfo {
                quantum: cx.quantum[index],
                closed,
                releases: releases[index],
                ..CircuitInfo::default()
            };
            let takes_results = typed.fns[index]
                .params
                .iter()
                .any(|&p| holds_logic(typed, typed.local(p).ty));
            let mut info = base.clone();
            let mut given = takes_results.then(|| base.clone());
            if let Some(body) = decl.body {
                if cx.quantum[index] {
                    Walker::run(&cx, &mut info, body, &mut buffers, None);
                }
                // Even a classical function matters when given results: what it returns
                // depends on them, and it may branch on them.
                if let Some(given) = &mut given {
                    Walker::run(&cx, given, body, &mut buffers, Some(index));
                }
            }
            if info != cx.fns[index] || given != cx.given[index] {
                cx.fns[index] = info;
                cx.given[index] = given;
                changed = true;
            }
        }
        if !changed || !order.cyclic {
            break;
        }
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
        info.quantum = contains_quantum(ast, typed, &cx.quantum, Node::Block(body));
        if info.quantum {
            // Values from outside a run are copied in, so they start untainted.
            let mut fresh = Buffers::new(ast, typed);
            Walker::run(&cx, &mut info, body, &mut fresh, None);
        }
        runs.insert(ExprId::from_index(index), info);
    }

    let mut diagnostics = Vec::new();
    for (index, decl) in ast.all_fns().iter().enumerate() {
        if !decl.is_circuit {
            continue;
        }
        let info = &cx.fns[index];
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
    let mut controls: Vec<(ExprId, Vec<LocalId>)> = cx
        .fns
        .iter()
        .chain(cx.given.iter().flatten())
        .chain(runs.values())
        .flat_map(|info| info.controls.iter().cloned())
        .collect();
    controls.sort();
    controls.dedup();
    let circuits = Circuits {
        fns: cx.fns,
        runs,
        controls,
    };
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
            None if info.feedback_unsupported.is_some() => {
                let (at, why) = info.feedback_unsupported.clone().expect("checked");
                Diagnostic::error(span, format!("{subject} shot by shot: {why}, which is not compiled to circuits yet"))
                    .with_label(at, "here")
            }
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

/// The program's functions with each one's callees before it, as far as possible.
struct CallOrder {
    fns: Vec<usize>,
    /// Whether some functions call each other in a cycle, so no order puts every callee
    /// first.
    cyclic: bool,
}

impl CallOrder {
    fn new(ast: &Ast, typed: &Typed) -> Self {
        let n = ast.all_fns().len();
        let callees: Vec<Vec<usize>> = ast
            .all_fns()
            .iter()
            .map(|decl| {
                let mut out = Vec::new();
                let Some(body) = decl.body else { return out };
                let mut stack = vec![Node::Block(body)];
                while let Some(node) = stack.pop() {
                    children(ast, node, &mut stack);
                    if let Node::Expr(e) = node
                        && let Some(Callee::Fn(f)) = typed.callees[e.index()]
                    {
                        out.push(f.index());
                    }
                }
                out
            })
            .collect();
        // Depth-first, emitting each function after its callees. 0: unseen, 1: in progress,
        // 2: done.
        let mut state = vec![0u8; n];
        let mut fns = Vec::with_capacity(n);
        let mut cyclic = false;
        for root in 0..n {
            if state[root] != 0 {
                continue;
            }
            let mut stack = vec![(root, 0usize)];
            state[root] = 1;
            while let Some(&mut (f, ref mut next)) = stack.last_mut() {
                if let Some(&callee) = callees[f].get(*next) {
                    *next += 1;
                    match state[callee] {
                        0 => {
                            state[callee] = 1;
                            stack.push((callee, 0));
                        }
                        1 => cyclic = true,
                        _ => {}
                    }
                } else {
                    state[f] = 2;
                    fns.push(f);
                    stack.pop();
                }
            }
        }
        CallOrder { fns, cyclic }
    }
}

/// Whether a value of type `ty` can hold a measurement result: a `Bit` or `Bool`, or a
/// tuple, array or struct containing one.
fn holds_logic(typed: &Typed, ty: Ty) -> bool {
    let types = &typed.types;
    match types.kind(ty) {
        _ if matches!(types.deref(ty), Ty::BIT | Ty::BOOL) => true,
        TyKind::Tuple(elems) => elems.iter().any(|&t| holds_logic(typed, t)),
        &TyKind::Array(elem, _) => holds_logic(typed, elem),
        &TyKind::Struct(s) => types.structs[s.index()]
            .fields
            .iter()
            .any(|&(_, t)| holds_logic(typed, t)),
        _ => false,
    }
}

/// Where each function calls `release`, itself or through the functions it calls.
fn infer_releases(ast: &Ast, typed: &Typed, order: &CallOrder) -> Vec<Option<Span>> {
    let mut result: Vec<Option<Span>> = vec![None; ast.all_fns().len()];
    loop {
        let mut changed = false;
        for &index in &order.fns {
            let decl = &ast.all_fns()[index];
            if result[index].is_some() {
                continue;
            }
            let Some(body) = decl.body else { continue };
            if let Some(span) = block_releases(ast, typed, &result, body) {
                result[index] = Some(span);
                changed = true;
            }
        }
        if !changed || !order.cyclic {
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

/// Whether each function can be recorded inside classical control of a circuit, itself and
/// in the functions it calls: it does not print or start repeated runs.
fn infer_recordable(ast: &Ast, typed: &Typed, order: &CallOrder) -> Vec<bool> {
    let mut result = vec![true; ast.all_fns().len()];
    loop {
        let mut changed = false;
        for &index in &order.fns {
            let decl = &ast.all_fns()[index];
            if !result[index] {
                continue;
            }
            let Some(body) = decl.body else { continue };
            let mut stack = vec![Node::Block(body)];
            let mut only = true;
            while let Some(node) = stack.pop() {
                children(ast, node, &mut stack);
                let Node::Expr(e) = node else { continue };
                only &= match ast.expr(e).kind {
                    ExprKind::Sample { .. }
                    | ExprKind::Expectation { .. }
                    | ExprKind::Overlap { .. } => false,
                    _ => match typed.callees[e.index()] {
                        Some(Callee::Builtin(Builtin::Print { .. })) => false,
                        Some(Callee::Fn(f)) => result[f.index()],
                        _ => true,
                    },
                };
            }
            if !only {
                result[index] = false;
                changed = true;
            }
        }
        if !changed || !order.cyclic {
            return result;
        }
    }
}

/// Whether every `and` and `or` in a condition has a right side that is safe to evaluate
/// whether or not it is needed, so the condition can be computed without branching.
pub fn short_circuit_free(ast: &Ast, cond: ExprId) -> bool {
    match ast.expr(cond).kind {
        ExprKind::Binary {
            op: BinaryOp::And | BinaryOp::Or,
            lhs,
            rhs,
        } => short_circuit_free(ast, lhs) && is_pure(ast, rhs),
        ExprKind::Binary { lhs, rhs, .. } => {
            short_circuit_free(ast, lhs) && short_circuit_free(ast, rhs)
        }
        ExprKind::Unary { operand, .. } => short_circuit_free(ast, operand),
        _ => true,
    }
}

/// Whether evaluating an expression can have no effect and cannot fail: names, literals,
/// fields, and logic and comparisons of them.
pub fn is_pure(ast: &Ast, id: ExprId) -> bool {
    match ast.expr(id).kind {
        ExprKind::Name(_)
        | ExprKind::Int(_)
        | ExprKind::BigInt
        | ExprKind::Float(_)
        | ExprKind::Imag(_)
        | ExprKind::Bool(_) => true,
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => is_pure(ast, base),
        ExprKind::Unary {
            op: UnaryOp::Not | UnaryOp::BitNot,
            operand,
        } => is_pure(ast, operand),
        // Arithmetic can overflow or divide by zero, so only logic and comparisons.
        ExprKind::Binary { op, lhs, rhs } => {
            !matches!(
                op,
                BinaryOp::Shl
                    | BinaryOp::Shr
                    | BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Rem
            ) && is_pure(ast, lhs)
                && is_pure(ast, rhs)
        }
        _ => false,
    }
}

/// Whether each function does quantum operations, itself or through the functions it calls.
fn infer_quantum(ast: &Ast, typed: &Typed, order: &CallOrder) -> Vec<bool> {
    let mut quantum = vec![false; ast.all_fns().len()];
    loop {
        let mut changed = false;
        for &index in &order.fns {
            let decl = &ast.all_fns()[index];
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
        if !changed || !order.cyclic {
            return quantum;
        }
    }
}

/// Whether an expression is itself a quantum operation of the run it is in. `sample` and
/// expectation values start runs of their own, so they are not.
fn is_quantum_op(ast: &Ast, typed: &Typed, quantum: &[bool], e: ExprId) -> bool {
    match ast.expr(e).kind {
        ExprKind::Ket { .. }
        | ExprKind::BraTest { .. }
        | ExprKind::PrepTest { .. }
        | ExprKind::Ctrl { .. } => true,
        ExprKind::Call { .. } | ExprKind::MethodCall { .. } => match typed.callees[e.index()] {
            Some(Callee::Builtin(
                Builtin::Gate(_)
                | Builtin::Measure
                | Builtin::MeasureIn
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
                ExprKind::BraTest { operand, parts, .. } => {
                    crate::ownership::label_values(ast, parts).for_each(&mut push);
                    push(operand);
                }
                ExprKind::Ket { parts, .. } => {
                    crate::ownership::label_values(ast, parts).for_each(&mut push)
                }
                ExprKind::PrepTest { prep, operand } => {
                    if let ExprKind::Call { args, .. } = ast.expr(prep).kind {
                        ast.list(args).iter().for_each(|&a| push(a));
                    }
                    push(operand);
                }
                ExprKind::Expectation {
                    observable,
                    precision,
                    ..
                } => {
                    push(observable);
                    precision.into_iter().for_each(&mut push);
                }
                ExprKind::Overlap {
                    observable,
                    precision,
                    ..
                } => {
                    observable.into_iter().for_each(&mut push);
                    precision.into_iter().for_each(&mut push);
                }
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
                ExprKind::Match { scrutinee, arms } => {
                    out.push(Node::Expr(scrutinee));
                    out.extend(ast.list(arms).iter().map(|a| Node::Expr(a.body)));
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

/// The walker's tables, indexed by statement and by local, kept between functions: each
/// function only reads entries it has written (its own statements and locals), so they need
/// no clearing.
struct Buffers {
    later: Vec<bool>,
    tainted: Vec<bool>,
}

impl Buffers {
    fn new(ast: &Ast, typed: &Typed) -> Self {
        Buffers {
            later: vec![false; ast.all_stmts().len()],
            tainted: vec![false; typed.locals.len()],
        }
    }
}

/// What the walker knows about the whole program.
struct Context<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    /// Whether each function does quantum operations.
    quantum: Vec<bool>,
    /// Whether each function only applies gates and computes classical values.
    recordable: Vec<bool>,
    /// What is known so far about each function, for the calls a run makes.
    fns: Vec<CircuitInfo>,
    /// The same, for a call that passes measurement results to it: its `Bit` and `Bool`
    /// parameters are then measurement results. `None` for a function with no such
    /// parameters.
    given: Vec<Option<CircuitInfo>>,
}

struct Walker<'a> {
    cx: &'a Context<'a>,
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
    fn new(cx: &'a Context<'a>, info: &'a mut CircuitInfo, buffers: &mut Buffers) -> Self {
        Walker {
            cx,
            later: std::mem::take(&mut buffers.later),
            tainted: std::mem::take(&mut buffers.tainted),
            control: 0,
            info,
        }
    }

    /// Applies the rule to a run whose body is `body`, recording what it finds in `info`.
    /// With `given`, the body is that function's, called with measurement results.
    fn run(
        cx: &'a Context<'a>,
        info: &'a mut CircuitInfo,
        body: BlockId,
        buffers: &mut Buffers,
        given: Option<usize>,
    ) {
        let mut walker = Walker::new(cx, info, buffers);
        if let Some(f) = given {
            for &param in &cx.typed.fns[f].params {
                walker.tainted[param.index()] = holds_logic(cx.typed, cx.typed.local(param).ty);
            }
        }
        walker.mark_block(body, false);
        walker.block(body);
        buffers.later = walker.later;
        buffers.tainted = walker.tainted;
        // Parameters are only written by `run`, so clear them for the next caller.
        if let Some(f) = given {
            for &param in &cx.typed.fns[f].params {
                buffers.tainted[param.index()] = false;
            }
        }
    }

    fn contains_quantum(&self, node: Node) -> bool {
        contains_quantum(self.cx.ast, self.cx.typed, &self.cx.quantum, node)
    }

    /// First pass: marks `later` for every statement in `block`, given whether a quantum
    /// operation can happen after the block. Returns whether the block contains one.
    fn mark_block(&mut self, block: BlockId, after: bool) -> bool {
        let stmts = self.cx.ast.list(self.cx.ast.block(block).stmts).to_vec();
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
        children(self.cx.ast, node, &mut stack);
        while let Some(child) = stack.pop() {
            match child {
                Node::Block(b) => {
                    self.mark_block(b, later);
                }
                Node::Expr(e) => match self.cx.ast.expr(e).kind {
                    ExprKind::While { cond, body }
                    | ExprKind::For {
                        body, start: cond, ..
                    } => {
                        let loops = self.contains_quantum(Node::Block(body));
                        self.mark_block(body, later || loops);
                        stack.push(Node::Expr(cond));
                    }
                    _ => children(self.cx.ast, child, &mut stack),
                },
                Node::Stmt(_) => children(self.cx.ast, child, &mut stack),
            }
        }
    }

    fn unsupported(&mut self, span: Span, why: String) {
        if self.info.feedback_unsupported.is_none() {
            self.info.feedback_unsupported = Some((span, why));
        }
    }

    /// Whether an `if`, `match` or `while` on a measurement result can become classical
    /// control in the circuit: the variables from outside it that it changes, or why not. Inside, it may
    /// do anything a run does except print, start repeated runs or leave early; the only
    /// outside variables it may change are `Bit`s and `Bool`s, which the circuit keeps in
    /// classical bits.
    fn recordable_control(
        &self,
        id: ExprId,
        cond: ExprId,
        blocks: &[Node],
        repeats: bool,
    ) -> Result<Vec<LocalId>, (Span, String)> {
        let ast = self.cx.ast;
        let typed = self.cx.typed;
        let is_match = matches!(ast.expr(id).kind, ExprKind::Match { .. });
        let what = if repeats {
            "a loop on a measurement result"
        } else if is_match {
            "a `match` on a measurement result"
        } else {
            "a branch on a measurement result"
        };
        let says = |why: &str| format!("{what} {why}");
        if !repeats && !matches!(typed.types.deref(typed.ty(id)), Ty::UNIT | Ty::NEVER) {
            let why = if is_match {
                "a `match` on a measurement result gives a value"
            } else {
                "an `if` on a measurement result gives a value"
            };
            return Err((ast.expr(id).span, why.into()));
        }
        // A loop's condition is evaluated again on every pass, so it must be plain logic.
        let plain = if repeats {
            is_pure(ast, cond)
        } else {
            short_circuit_free(ast, cond)
        };
        if !plain {
            let why = if repeats {
                "a loop's measurement-dependent condition calls a function"
            } else {
                "a measurement-dependent condition calls a function after `and` or `or`"
            };
            return Err((ast.expr(cond).span, why.into()));
        }
        let span = ast.expr(id).span;
        let first_local = typed
            .pat_locals
            .iter()
            .flatten()
            .filter(|l| {
                let at = typed.local(**l).span;
                at.start >= span.start && at.end <= span.end
            })
            .min()
            .copied();
        let inside = |l: LocalId| first_local.is_some_and(|f| l >= f);
        let mut carried = Vec::new();
        let mut stack = blocks.to_vec();
        while let Some(node) = stack.pop() {
            children(ast, node, &mut stack);
            match node {
                Node::Stmt(s) => match ast.stmt(s).kind {
                    StmtKind::Return(_) | StmtKind::Break | StmtKind::Continue => {
                        return Err((ast.stmt(s).span, says("leaves it early")));
                    }
                    StmtKind::Assign { place, .. } => {
                        let Some(l) = root(ast, typed, place) else {
                            continue;
                        };
                        if inside(l) {
                            continue;
                        }
                        let logic = matches!(typed.local(l).ty, Ty::BIT | Ty::BOOL);
                        if !logic || !matches!(ast.expr(place).kind, ExprKind::Name(_)) {
                            let why = says(
                                "changes a variable from outside it that is not a `Bit` or `Bool`",
                            );
                            return Err((ast.stmt(s).span, why));
                        }
                        if !carried.contains(&l) {
                            carried.push(l);
                        }
                    }
                    _ => {}
                },
                Node::Expr(e) => {
                    let span = ast.expr(e).span;
                    let why = match ast.expr(e).kind {
                        ExprKind::Sample { .. }
                        | ExprKind::Expectation { .. }
                        | ExprKind::Overlap { .. } => Some("starts a repeated run"),
                        ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                            match typed.callees[e.index()] {
                                Some(Callee::Builtin(Builtin::Print { .. })) => Some("prints"),
                                Some(Callee::Fn(f)) if !self.cx.recordable[f.index()] => {
                                    Some("calls a function that prints or starts repeated runs")
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(why) = why {
                        return Err((span, says(why)));
                    }
                    // A classical variable from outside, lent `mut`.
                    let lent = match ast.expr(e).kind {
                        ExprKind::Own {
                            kind: OwnKind::Mut,
                            place,
                        } => Some(place),
                        ExprKind::MethodCall { receiver, .. }
                            if typed.receivers[e.index()] == Some(OwnKind::Mut) =>
                        {
                            Some(receiver)
                        }
                        _ if matches!(typed.adjust[e.index()], Adjust::Lend { mutable: true }) => {
                            Some(e)
                        }
                        _ => None,
                    };
                    if let Some(place) = lent
                        && let Some(l) = root(ast, typed, place)
                        && typed.types.linearity(typed.local(l).ty) == Kind::Copyable
                        && !inside(l)
                    {
                        return Err((span, says("lends a variable from outside it `mut`")));
                    }
                }
                Node::Block(_) => {}
            }
        }
        carried.sort();
        Ok(carried)
    }

    fn control(&mut self, id: ExprId, result: Result<Vec<LocalId>, (Span, String)>) {
        match result {
            Ok(carried) => {
                if !self.info.controls.iter().any(|(e, _)| *e == id) {
                    self.info.controls.push((id, carried));
                }
            }
            Err((span, why)) => self.unsupported(span, why),
        }
    }

    fn violate(&mut self, span: Span, what: &str) {
        if self.info.not_circuit.is_none() {
            self.info.not_circuit = Some((span, what.to_string()));
        }
    }

    /// Second pass.
    fn block(&mut self, block: BlockId) {
        for &stmt in self.cx.ast.list(self.cx.ast.block(block).stmts) {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, id: StmtId) {
        let before = self.later[id.index()];
        match self.cx.ast.stmt(id).kind {
            StmtKind::Let { pat, init, .. } => {
                let tainted = self.expr(init, before) || self.control > 0;
                self.taint_pattern(pat, tainted);
            }
            StmtKind::Assign { place, value, .. } => {
                let tainted = self.expr(value, before) || self.control > 0;
                self.place_indices(place, before);
                if let Some(local) = root(self.cx.ast, self.cx.typed, place)
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
                    let span = self.cx.ast.expr(e).span;
                    self.violate(span, "used in `assert`");
                }
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn taint_pattern(&mut self, pat: PatId, tainted: bool) {
        match self.cx.ast.pat(pat).kind {
            PatKind::Name(_) => {
                if let Some(local) = self.cx.typed.pat_locals[pat.index()] {
                    self.tainted[local.index()] = tainted;
                }
            }
            PatKind::Wildcard => {}
            PatKind::Tuple(pats) | PatKind::Array(pats) => {
                for &p in self.cx.ast.list(pats) {
                    self.taint_pattern(p, tainted);
                }
            }
            PatKind::Literal(_) => {}
            PatKind::Variant { data, .. } => {
                let ast = self.cx.ast;
                let pats: Vec<PatId> = match data {
                    PatData::None => Vec::new(),
                    PatData::Tuple(pats) => ast.list(pats).to_vec(),
                    PatData::Fields { fields, .. } => {
                        ast.list(fields).iter().filter_map(|f| f.pat).collect()
                    }
                };
                for p in pats {
                    self.taint_pattern(p, tainted);
                }
            }
        }
    }

    /// Checks the indices inside an assignment target.
    fn place_indices(&mut self, place: ExprId, before: bool) {
        match self.cx.ast.expr(place).kind {
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.place_indices(base, before)
            }
            ExprKind::Index { base, index } => {
                self.place_indices(base, before);
                if self.expr(index, before) && before {
                    let span = self.cx.ast.expr(index).span;
                    self.violate(span, "used as an array index");
                }
            }
            _ => {}
        }
    }

    /// Walks an expression; returns whether its value depends on a measurement. `before` says
    /// whether a quantum operation can still follow.
    fn expr(&mut self, id: ExprId, before: bool) -> bool {
        let ast = self.cx.ast;
        let expr = *ast.expr(id);
        let is_logic_type = |ty: Ty| matches!(self.cx.typed.types.deref(ty), Ty::BIT | Ty::BOOL);
        match expr.kind {
            ExprKind::Name(_) => match self.cx.typed.res[id.index()] {
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
                            let local = (0..self.cx.typed.locals.len())
                                .rev()
                                .map(LocalId::from_index)
                                .find(|&l| self.cx.typed.local(l).name == field.name);
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
            ExprKind::BraTest { operand, parts, .. } => {
                for value in crate::ownership::label_values(ast, parts) {
                    self.expr(value, before);
                }
                self.expr(operand, before);
                true
            }
            // A label computed from a measurement result prepares its qubit conditionally:
            // classical control, which a circuit records.
            ExprKind::Ket { parts, .. } => {
                for value in crate::ownership::label_values(ast, parts) {
                    if self.expr(value, before) && before {
                        self.info.feedback = true;
                    }
                }
                false
            }
            // The preparation's arguments choose gates, as a gate's angle does.
            ExprKind::PrepTest { prep, operand } => {
                if let ExprKind::Call { args, .. } = ast.expr(prep).kind {
                    for &arg in ast.list(args) {
                        if self.expr(arg, before) && before {
                            self.violate(ast.expr(arg).span, "used to choose a bra's preparation");
                        }
                    }
                }
                self.expr(operand, before);
                true
            }
            ExprKind::Unary { op, operand } => {
                let tainted = self.expr(operand, before);
                let logic = matches!(op, UnaryOp::Not | UnaryOp::BitNot)
                    && is_logic_type(self.cx.typed.ty(operand));
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
                ) && is_logic_type(self.cx.typed.ty(lhs));
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
            ExprKind::Overlap {
                observable,
                precision,
                ..
            } => {
                if let Some(o) = observable
                    && self.expr(o, before)
                    && before
                {
                    self.violate(expr.span, "used in an observable");
                }
                if let Some(p) = precision
                    && self.expr(p, before)
                    && before
                {
                    self.violate(ast.expr(p).span, "used as a precision");
                }
                false
            }
            ExprKind::Expectation {
                observable,
                precision,
                ..
            } => {
                if self.expr(observable, before) && before {
                    self.violate(expr.span, "used in an observable");
                }
                if let Some(p) = precision
                    && self.expr(p, before)
                    && before
                {
                    self.violate(ast.expr(p).span, "used as a precision");
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
                    let blocks: Vec<Node> = [Node::Block(then)]
                        .into_iter()
                        .chain(otherwise.map(Node::Expr))
                        .collect();
                    let result = self.recordable_control(id, cond, &blocks, false);
                    self.control(id, result);
                }
                self.control += u32::from(condition);
                let mut tainted = self.value_block(then);
                if let Some(o) = otherwise {
                    tainted |= self.expr(o, before);
                }
                self.control -= u32::from(condition);
                tainted || condition
            }
            ExprKind::Match { scrutinee, arms } => {
                let condition = self.expr(scrutinee, before);
                if condition && before {
                    self.info.feedback = true;
                    let bodies: Vec<Node> =
                        ast.list(arms).iter().map(|a| Node::Expr(a.body)).collect();
                    let result = self.recordable_control(id, scrutinee, &bodies, false);
                    self.control(id, result);
                }
                self.control += u32::from(condition);
                let mut tainted = condition;
                for arm in ast.list(arms) {
                    self.taint_pattern(arm.pat, condition);
                    tainted |= self.expr(arm.body, before);
                }
                self.control -= u32::from(condition);
                tainted
            }
            ExprKind::While { cond, body } => {
                // Twice, so values that become measurement-dependent in one iteration are
                // seen in the next.
                for _ in 0..2 {
                    let condition = self.expr(cond, before);
                    if condition && before {
                        self.info.feedback = true;
                        let result = self.recordable_control(id, cond, &[Node::Block(body)], true);
                        self.control(id, result);
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
            | ExprKind::BigInt
            | ExprKind::Float(_)
            | ExprKind::Imag(_)
            | ExprKind::Bool(_)
            | ExprKind::Path(_)
            | ExprKind::Unit
            | ExprKind::With { .. } => false,
        }
    }

    /// A block used as a value: tainted if its final expression is.
    fn value_block(&mut self, block: BlockId) -> bool {
        let stmts = self.cx.ast.list(self.cx.ast.block(block).stmts);
        let mut tainted = false;
        for (i, &stmt) in stmts.iter().enumerate() {
            match self.cx.ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    tainted = self.expr(expr, self.later[stmt.index()]);
                }
                _ => self.stmt(stmt),
            }
        }
        tainted
    }

    fn call(&mut self, id: ExprId, before: bool) -> bool {
        let ast = self.cx.ast;
        let (receiver, args) = match ast.expr(id).kind {
            ExprKind::Call { args, .. } => (None, ast.list(args)),
            ExprKind::MethodCall { receiver, args, .. } => (Some(receiver), ast.list(args)),
            _ => return false,
        };
        let callee = self.cx.typed.callees[id.index()];
        // Qubits are never measurement results, even ones created under a condition on one.
        let typed = self.cx.typed;
        let classical = |e: ExprId| typed.types.linearity(typed.ty(e)) != Kind::Linear;
        let mut tainted_args = Vec::new();
        if let Some(r) = receiver {
            let tainted = self.expr(r, before) && classical(r);
            tainted_args.push((r, tainted));
        }
        for &a in args {
            let tainted = self.expr(a, before) && classical(a);
            tainted_args.push((a, tainted));
        }
        let any = tainted_args.iter().any(|&(_, t)| t);
        match callee {
            Some(Callee::Builtin(Builtin::Measure)) => true,
            // The basis decides the gates, as an angle does.
            Some(Callee::Builtin(Builtin::MeasureIn)) => {
                if let Some(&(arg, _)) = tainted_args.iter().find(|&&(_, t)| t)
                    && before
                {
                    self.violate(ast.expr(arg).span, "used as a measurement basis");
                }
                true
            }
            // `Bool(b)` and `Bit(flag)` are logic, so allowed.
            Some(Callee::Convert(Ty::BOOL | Ty::BIT)) => any,
            // Building a variant stores its values, which the rule allows.
            Some(Callee::Variant(..)) => any,
            Some(Callee::Convert(_) | Callee::TryConvert(_)) => {
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
                // Given measurement results, the function is checked as if they were its own.
                let given = self.cx.given[f.index()].as_ref().filter(|_| any);
                if let Some(&(arg, _)) = tainted_args.iter().find(|&&(_, t)| t)
                    && before
                    && given.is_none()
                {
                    let name = ast.name(ast.function(f).name);
                    let what = format!("passed to `{name}`");
                    self.violate(ast.expr(arg).span, &what);
                }
                // What happens inside the call is part of this run.
                let inner = given.unwrap_or(&self.cx.fns[f.index()]);
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
                if let Some((span, why)) = &inner.feedback_unsupported {
                    let why = format!("{why} in `{}`", ast.name(ast.function(f).name));
                    self.unsupported(*span, why);
                }
                // A quantum function's result comes from its own measurements.
                self.cx.quantum[f.index()] || any
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
