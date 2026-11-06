//! The ownership checker (`spec/ownership.md`).
//!
//! Runs after type checking, one function at a time. It walks each body in program order,
//! following the structure of the code: both branches of an `if` are checked and their
//! effects compared, and loop bodies are checked knowing they may run again. It tracks:
//!
//! - which places have been moved, to catch use after move and values left unconsumed;
//! - which places are lent, and to whom (a call, a variable, a `ctrl` block), with lexical
//!   lifetimes: a lend held by a variable lasts to the end of its block;
//! - facts about integers, to prove array indices differ and are in range;
//! - whether the code is in a reversible context.

use noev_syntax::ast::*;
use noev_syntax::{Diagnostic, Span, Symbol};

use crate::check::{Adjust, Callee, LocalId, Res, Typed};
use crate::facts::{self, Fact, Linear};
use crate::prelude::Reversible;
use crate::ty::{Kind, Ty, TyKind};

/// Checks ownership in every function of `ast`.
pub fn check_ownership(ast: &Ast, typed: &Typed) -> Vec<Diagnostic> {
    let irreversible = infer_reversibility(ast, typed);
    let mut checker = Owner {
        ast,
        typed,
        irreversible,
        diagnostics: Vec::new(),
        locals: vec![LocalInfo::default(); typed.locals.len()],
        order: 0,
        scope: Vec::new(),
        moved: Vec::new(),
        lends: Vec::new(),
        facts: Vec::new(),
        loops: Vec::new(),
        reversible: Vec::new(),
        unsafe_depth: 0,
        diverged: false,
        returns_ref: false,
    };
    for (index, decl) in ast.all_fns().iter().enumerate() {
        if let Some(body) = decl.body {
            checker.function(FnId::from_index(index), body);
        }
    }
    checker.diagnostics
}

// ---------------------------------------------------------------------------------------
// Places
// ---------------------------------------------------------------------------------------

/// A local variable followed by fields, tuple indices and array indices.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    root: LocalId,
    proj: Vec<Proj>,
}

#[derive(Clone, Debug, PartialEq)]
enum Proj {
    /// A struct field or tuple element, by position.
    Field(u32),
    /// An array element. `None` if the index is not a linear form.
    Index(Option<Linear>),
}

impl Proj {
    fn constant_index(&self) -> Option<i64> {
        match self {
            Proj::Index(Some(form)) => form.as_constant(),
            _ => None,
        }
    }

    fn is_dynamic(&self) -> bool {
        matches!(self, Proj::Index(_)) && self.constant_index().is_none()
    }
}

/// How two places relate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overlap {
    /// They share no part.
    Disjoint,
    /// They overlap.
    Overlaps,
    /// They overlap unless two indices differ, which the facts cannot prove.
    Unproven,
}

#[derive(Clone, Copy, Debug, Default)]
struct LocalInfo {
    /// Declaration order within the function, for "declared outside the context".
    order: u32,
    /// How many loops enclose the declaration.
    loop_depth: u32,
    /// Its initialiser already failed to check; it is not checked for consumption.
    poisoned: bool,
}

#[derive(Clone, Debug)]
struct Moved {
    place: Place,
    span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    /// A value still being computed: a call argument or a returned reference.
    Pending,
    /// A variable holding the reference, until the end of its block.
    Local(LocalId),
    /// A `ctrl` block's control, for the length of the block.
    Control,
}

#[derive(Clone, Debug)]
struct Lend {
    place: Place,
    mutable: bool,
    holder: Holder,
    span: Span,
    alive: bool,
}

/// How the surrounding code uses a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// Taken: moved if it cannot be copied.
    Value,
    /// Only read, as an operand or condition.
    Read,
    /// Thrown away, as in a statement.
    Discard,
}

/// Lends carried by a value, released when the value is done with.
type Carried = Vec<usize>;

/// Why a function is not reversible, for N013's note.
#[derive(Clone, Debug)]
struct Irreversible {
    span: Span,
    why: String,
}

struct Owner<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    irreversible: Vec<Option<Irreversible>>,
    diagnostics: Vec<Diagnostic>,
    locals: Vec<LocalInfo>,
    order: u32,
    /// Locals in scope, in declaration order.
    scope: Vec<LocalId>,
    moved: Vec<Moved>,
    lends: Vec<Lend>,
    facts: Vec<Fact>,
    /// For each enclosing loop, the scope length when it started.
    loops: Vec<usize>,
    /// For each enclosing reversible context, the declaration order when it started.
    reversible: Vec<u32>,
    unsafe_depth: u32,
    /// Control never reaches the current point (after `return`, `break` or `continue`).
    diverged: bool,
    /// Whether the current function returns a reference.
    returns_ref: bool,
}

// ---------------------------------------------------------------------------------------
// Reversibility inference
// ---------------------------------------------------------------------------------------

/// For each function, why it is not reversible, or `None` if it is. Functions without `rev`
/// are inferred from their bodies, repeating until nothing changes, since they can call each
/// other.
fn infer_reversibility(ast: &Ast, typed: &Typed) -> Vec<Option<Irreversible>> {
    let mut result: Vec<Option<Irreversible>> = vec![None; ast.all_fns().len()];
    loop {
        let mut changed = false;
        for (index, decl) in ast.all_fns().iter().enumerate() {
            if result[index].is_some() || typed.fns[index].sig.reversible != Reversible::Infer {
                continue;
            }
            let Some(body) = decl.body else { continue };
            let info = &typed.fns[index];
            let outside: Vec<LocalId> = info
                .self_local
                .into_iter()
                .chain(info.params.iter().copied())
                .collect();
            let mut found = None;
            walk_block(ast, body, &mut |node| {
                if found.is_none() {
                    found = irreversible_op(ast, typed, &result, &outside, node);
                }
            });
            if found.is_some() {
                result[index] = found;
                changed = true;
            }
        }
        if !changed {
            return result;
        }
    }
}

#[derive(Clone, Copy)]
enum Node {
    Expr(ExprId),
    Assign(ExprId, Span),
}

fn irreversible_op(
    ast: &Ast,
    typed: &Typed,
    result: &[Option<Irreversible>],
    outside: &[LocalId],
    node: Node,
) -> Option<Irreversible> {
    let (span, why) = match node {
        Node::Assign(place, span) => {
            let root = root_local(ast, typed, place)?;
            if !outside.contains(&root) {
                return None;
            }
            let name = ast.name(typed.local(root).name);
            (
                span,
                format!("it changes `{name}`, which belongs to its caller"),
            )
        }
        Node::Expr(id) => {
            let expr = ast.expr(id);
            match expr.kind {
                ExprKind::Ket => (expr.span, "it creates qubits".into()),
                ExprKind::BraTest { .. } => (expr.span, "it measures qubits".into()),
                ExprKind::Expectation { .. } => {
                    (expr.span, "it computes an expectation value".into())
                }
                ExprKind::Own {
                    kind: OwnKind::Mv, ..
                } if typed.types.linearity(typed.ty(id)) == Kind::Linear => {
                    (expr.span, "it moves qubits".into())
                }
                ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                    match typed.callees[id.index()]? {
                        Callee::Builtin(b) if !b.reversible() => {
                            (expr.span, format!("it calls `{}`", b.name()))
                        }
                        Callee::Fn(f)
                            if typed.fns[f.index()].sig.reversible == Reversible::Infer
                                && result[f.index()].is_some() =>
                        {
                            let name = ast.name(ast.function(f).name);
                            (
                                expr.span,
                                format!("it calls `{name}`, which is not reversible"),
                            )
                        }
                        _ => return None,
                    }
                }
                _ => return None,
            }
        }
    };
    Some(Irreversible { span, why })
}

/// The local at the root of a place expression.
fn root_local(ast: &Ast, typed: &Typed, mut id: ExprId) -> Option<LocalId> {
    loop {
        match ast.expr(id).kind {
            ExprKind::Name(_) => match typed.res[id.index()] {
                Res::Local(local) => return Some(local),
                _ => return None,
            },
            ExprKind::Field { base, .. }
            | ExprKind::TupleIndex { base, .. }
            | ExprKind::Index { base, .. } => id = base,
            _ => return None,
        }
    }
}

/// Calls `visit` on every expression and assignment in a block, outside-in. Expectation
/// preparations are skipped: they run separately.
fn walk_block(ast: &Ast, block: BlockId, visit: &mut dyn FnMut(Node)) {
    for &stmt in ast.list(ast.block(block).stmts) {
        let stmt = ast.stmt(stmt);
        match stmt.kind {
            StmtKind::Let { init, .. } => walk_expr(ast, init, visit),
            StmtKind::Assign { place, value, .. } => {
                visit(Node::Assign(place, stmt.span));
                walk_expr(ast, place, visit);
                walk_expr(ast, value, visit);
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr { expr: e, .. } => {
                walk_expr(ast, e, visit)
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }
}

fn walk_expr(ast: &Ast, id: ExprId, visit: &mut dyn FnMut(Node)) {
    visit(Node::Expr(id));
    let mut children: Vec<ExprId> = Vec::new();
    let mut blocks: Vec<BlockId> = Vec::new();
    match ast.expr(id).kind {
        ExprKind::Tuple(l) | ExprKind::Array(l) => children.extend(ast.list(l)),
        ExprKind::ArrayRepeat { value, count } => children.extend([value, count]),
        ExprKind::StructLit { fields, .. } => {
            children.extend(ast.list(fields).iter().filter_map(|f| f.value));
        }
        ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => children.push(base),
        ExprKind::Index { base, index } => children.extend([base, index]),
        ExprKind::Call { args, .. } => children.extend(ast.list(args)),
        ExprKind::MethodCall { receiver, args, .. } => {
            children.push(receiver);
            children.extend(ast.list(args));
        }
        ExprKind::Unary { operand, .. } => children.push(operand),
        ExprKind::Binary { lhs, rhs, .. } => children.extend([lhs, rhs]),
        ExprKind::Own { place, .. } => children.push(place),
        ExprKind::Adj(call) => children.push(call),
        ExprKind::BraTest { operand, .. } => children.push(operand),
        ExprKind::Expectation { observable, .. } => children.push(observable),
        ExprKind::Block(b) | ExprKind::Unsafe(b) => blocks.push(b),
        ExprKind::If {
            cond,
            then,
            otherwise,
        } => {
            children.push(cond);
            blocks.push(then);
            children.extend(otherwise);
        }
        ExprKind::While { cond, body } => {
            children.push(cond);
            blocks.push(body);
        }
        ExprKind::For {
            start, end, body, ..
        } => {
            children.extend([start, end]);
            blocks.push(body);
        }
        ExprKind::Ctrl { controls, body } => {
            children.extend(ast.list(controls));
            blocks.push(body);
        }
        ExprKind::With { with, body } => blocks.extend([with, body]),
        _ => {}
    }
    for child in children {
        walk_expr(ast, child, visit);
    }
    for block in blocks {
        walk_block(ast, block, visit);
    }
}

// ---------------------------------------------------------------------------------------
// Functions, blocks and statements
// ---------------------------------------------------------------------------------------

impl Owner<'_> {
    fn error(
        &mut self,
        code: &'static str,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.diagnostics
            .push(Diagnostic::error(span, message).with_code(code));
        self.diagnostics.last_mut().expect("just pushed")
    }

    fn name(&self, local: LocalId) -> &str {
        self.ast.name(self.typed.local(local).name)
    }

    fn function(&mut self, f: FnId, body: BlockId) {
        let info = &self.typed.fns[f.index()];
        self.scope.clear();
        self.moved.clear();
        self.lends.clear();
        self.facts.clear();
        self.loops.clear();
        self.reversible.clear();
        self.unsafe_depth = 0;
        self.diverged = false;
        self.order = 0;
        self.returns_ref = matches!(self.typed.types.kind(info.sig.ret), TyKind::Ref { .. });

        let params: Vec<LocalId> = info
            .self_local
            .into_iter()
            .chain(info.params.iter().copied())
            .collect();
        for local in params {
            self.declare(local, false);
        }
        if info.sig.reversible == Reversible::Yes {
            self.reversible.push(self.order);
        }
        let use_ = if info.sig.ret == Ty::UNIT {
            Use::Discard
        } else {
            Use::Value
        };
        let carried = self.block(body, use_);
        self.check_returned_ref(&carried);
        if !self.diverged {
            let end = self.closing(body);
            self.check_consumed(0, end);
        }
    }

    fn declare(&mut self, local: LocalId, poisoned: bool) {
        self.locals[local.index()] = LocalInfo {
            order: self.order,
            loop_depth: self.loops.len() as u32,
            poisoned,
        };
        self.order += 1;
        self.scope.push(local);
    }

    /// The `}` of a block.
    fn closing(&self, block: BlockId) -> Span {
        let span = self.ast.block(block).span;
        Span::new(span.end.saturating_sub(1), span.end)
    }

    fn block(&mut self, block: BlockId, use_: Use) -> Carried {
        let ast = self.ast;
        let scope_mark = self.scope.len();
        let facts_mark = self.facts.len();
        let stmts = ast.list(ast.block(block).stmts);
        let mut carried = Vec::new();
        for (i, &stmt) in stmts.iter().enumerate() {
            if self.diverged {
                break; // The rest is unreachable.
            }
            match ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    carried = self.expr(expr, use_);
                }
                _ => self.stmt(stmt),
            }
            self.release_pending(&carried);
        }
        if !self.diverged {
            let end = self.closing(block);
            self.check_consumed(scope_mark, end);
        }
        self.leave_scope(scope_mark);
        self.facts.truncate(facts_mark);
        carried
    }

    /// Ends the scope of every local declared since `mark`.
    fn leave_scope(&mut self, mark: usize) {
        let gone: Vec<LocalId> = self.scope.drain(mark..).collect();
        for lend in &mut self.lends {
            if let Holder::Local(l) = lend.holder
                && gone.contains(&l)
            {
                lend.alive = false;
            }
        }
        self.moved.retain(|m| !gone.contains(&m.place.root));
        while self.lends.last().is_some_and(|l| !l.alive) {
            self.lends.pop();
        }
    }

    /// Releases lends made while computing a value that has now been used, except `keep`.
    fn release_pending(&mut self, keep: &[usize]) {
        for (i, lend) in self.lends.iter_mut().enumerate() {
            if lend.holder == Holder::Pending && !keep.contains(&i) {
                lend.alive = false;
            }
        }
    }

    fn release(&mut self, carried: &[usize]) {
        for &i in carried {
            self.lends[i].alive = false;
        }
    }

    /// N003 for every linear local declared since `mark` that is still owned.
    fn check_consumed(&mut self, mark: usize, at: Span) {
        let locals: Vec<LocalId> = self.scope[mark..].to_vec();
        for local in locals {
            if self.locals[local.index()].poisoned {
                continue;
            }
            let ty = self.typed.local(local).ty;
            let place = Place {
                root: local,
                proj: Vec::new(),
            };
            if self.fully_moved(&place, ty) {
                continue;
            }
            let name = self.name(local).to_string();
            let declared = self.typed.local(local).span;
            self.error(
                "N003",
                at,
                format!("`{name}` is still owned when it goes out of scope"),
            )
            .with_label_mut(declared, "declared here")
            .help = Some(format!(
                "measure it with `mv {name}.measure()`, release it with `mv {name}.discard()`, or return it"
            ));
            // Report each local once.
            self.moved.push(Moved { place, span: at });
        }
    }

    fn stmt(&mut self, id: StmtId) {
        let ast = self.ast;
        let stmt = *ast.stmt(id);
        match stmt.kind {
            StmtKind::Let { pat, init, .. } => {
                let errors = self.diagnostics.len();
                let carried = self.expr(init, Use::Value);
                let poisoned = self.diagnostics.len() > errors;
                let mut bound = Vec::new();
                self.bind(pat, poisoned, &mut bound);
                match bound.as_slice() {
                    [local] if !carried.is_empty() => {
                        for &i in &carried {
                            self.lends[i].holder = Holder::Local(*local);
                        }
                    }
                    _ => self.release(&carried),
                }
            }
            StmtKind::Assign { op, place, value } => {
                let carried = self.expr(value, Use::Value);
                self.release(&carried);
                self.assign(place, op, stmt.span);
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    let carried = self.expr(value, Use::Value);
                    self.check_returned_ref(&carried);
                }
                self.check_consumed(0, stmt.span);
                self.diverged = true;
            }
            StmtKind::Break | StmtKind::Continue => {
                let mark = self.loops.last().copied().unwrap_or(0);
                self.check_consumed(mark, stmt.span);
                self.diverged = true;
            }
            StmtKind::Assert(cond) => {
                self.expr(cond, Use::Read);
                let mut new = Vec::new();
                facts::facts_from(ast, self.typed, cond, true, &mut new);
                self.facts.extend(new);
            }
            StmtKind::Expr { expr, .. } => {
                let carried = self.expr(expr, Use::Discard);
                self.release(&carried);
            }
        }
    }

    fn bind(&mut self, pat: PatId, poisoned: bool, bound: &mut Vec<LocalId>) {
        match self.ast.pat(pat).kind {
            PatKind::Name(_) => {
                if let Some(local) = self.typed.pat_locals[pat.index()] {
                    self.declare(local, poisoned);
                    bound.push(local);
                }
            }
            PatKind::Wildcard => {}
            PatKind::Tuple(pats) => {
                for &p in self.ast.list(pats) {
                    self.bind(p, poisoned, bound);
                }
            }
        }
    }

    /// N008: a returned reference must not point into a local of this function.
    fn check_returned_ref(&mut self, carried: &[usize]) {
        if !self.returns_ref {
            return;
        }
        for &i in carried {
            let root = self.lends[i].place.root;
            if !self.is_reference(root) {
                let name = self.name(root).to_string();
                let span = self.lends[i].span;
                self.error("N008", span, format!("`{name}` is dropped when the function returns, so a reference to it cannot be returned"));
            }
        }
    }

    fn assign(&mut self, place_expr: ExprId, op: AssignOp, span: Span) {
        self.place_subexprs(place_expr);
        let Some(place) = self.place_of(place_expr) else {
            self.expr(place_expr, Use::Read);
            return;
        };
        let root = place.root;
        let name = self.name(root).to_string();
        if matches!(op, AssignOp::Compound(_)) {
            self.read(&place, span);
        }
        if self.is_reference(root) {
            if !self.is_mut_reference(root) {
                self.error(
                    "N015",
                    span,
                    format!(
                        "`{name}` is a `ref` reference, so it cannot be used to change the value"
                    ),
                )
                .help = Some("take it as `mut` to change it".into());
            }
        } else if !self.typed.local(root).mutable {
            self.error(
                "N015",
                span,
                format!("`{name}` is not declared `let mut`, so it cannot be changed"),
            )
            .help = Some(format!("declare it with `let mut {name}`"));
        }
        if let Some(&base) = self.reversible.last()
            && self.locals[root.index()].order < base
        {
            self.error("N013", span, format!("assigning to `{name}`, declared outside this reversible context, is not reversible"));
        }
        if let Some(i) = self.conflicting_lend(&place, true) {
            let lend_span = self.lends[i].span;
            self.error(
                "N005",
                span,
                format!("`{name}` cannot be changed while it is lent"),
            )
            .with_label_mut(lend_span, "lent here");
        }
        let ty = self.typed.ty(place_expr);
        if !self.is_reference(root)
            && self.typed.types.linearity(ty) == Kind::Linear
            && !self.fully_moved(&place, ty)
        {
            self.error(
                "N003",
                span,
                format!("assigning to `{name}` would drop the qubits it holds"),
            )
            .help = Some("consume the old value first".into());
        }
        // The place holds a new value.
        self.moved.retain(|m| !is_prefix(&place, &m.place));
        self.facts.retain(|f| !f.mentions(root));
    }
}

// ---------------------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------------------

impl Owner<'_> {
    fn expr(&mut self, id: ExprId, use_: Use) -> Carried {
        let ast = self.ast;
        let expr = *ast.expr(id);
        match expr.kind {
            ExprKind::Error
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit
            | ExprKind::Path(_) => Vec::new(),
            ExprKind::Ket => {
                self.not_reversible(expr.span, "creating qubits");
                self.discarded(id, use_);
                Vec::new()
            }
            ExprKind::Name(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. } => self.place_use(id, use_),
            ExprKind::Tuple(elems) | ExprKind::Array(elems) => {
                for &e in ast.list(elems) {
                    let carried = self.expr(e, Use::Value);
                    self.release(&carried);
                }
                self.discarded(id, use_);
                Vec::new()
            }
            ExprKind::ArrayRepeat { value, count } => {
                self.expr(value, Use::Value);
                self.expr(count, Use::Read);
                self.discarded(id, use_);
                Vec::new()
            }
            ExprKind::StructLit { fields, .. } => {
                for field in ast.list(fields) {
                    match field.value {
                        Some(value) => {
                            let carried = self.expr(value, Use::Value);
                            self.release(&carried);
                        }
                        None => self.shorthand_field(field.name, field.span),
                    }
                }
                self.discarded(id, use_);
                Vec::new()
            }
            ExprKind::Call { args, .. } => self.call(id, None, args, use_),
            ExprKind::MethodCall { receiver, args, .. } => {
                self.call(id, Some(receiver), args, use_)
            }
            ExprKind::Unary { operand, .. } => {
                self.expr(operand, Use::Read);
                Vec::new()
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, Use::Read);
                self.expr(rhs, Use::Read);
                Vec::new()
            }
            ExprKind::Own { kind, place } => self.own(id, kind, place, use_),
            ExprKind::Adj(call) => {
                self.check_adj(call);
                self.expr(call, use_)
            }
            ExprKind::BraTest { operand, .. } => {
                self.not_reversible(expr.span, "measuring");
                let carried = self.expr(operand, Use::Value);
                self.release(&carried);
                Vec::new()
            }
            ExprKind::Expectation { prep, observable } => {
                if self.not_reversible(expr.span, "computing an expectation value") {
                    return Vec::new();
                }
                self.preparation(prep);
                self.expr(observable, Use::Read);
                Vec::new()
            }
            ExprKind::Block(block) => self.block(block, use_),
            ExprKind::Unsafe(block) => {
                self.unsafe_depth += 1;
                let carried = self.block(block, use_);
                self.unsafe_depth -= 1;
                carried
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.if_expr(cond, then, otherwise, use_, expr.span);
                Vec::new()
            }
            ExprKind::While { cond, body } => {
                self.expr(cond, Use::Read);
                self.loop_body(body, Vec::new(), None);
                Vec::new()
            }
            ExprKind::For {
                start, end, body, ..
            } => {
                self.expr(start, Use::Read);
                self.expr(end, Use::Read);
                let mut new = Vec::new();
                let var = match self.typed.res[id.index()] {
                    Res::Local(local) => Some(local),
                    _ => None,
                };
                if let Some(var) = var {
                    let start = facts::linear(ast, self.typed, start);
                    let end = facts::linear(ast, self.typed, end);
                    facts::loop_facts(var, start, end, &mut new);
                }
                self.loop_body(body, new, var);
                Vec::new()
            }
            ExprKind::Ctrl { controls, body } => {
                self.ctrl(controls, body);
                Vec::new()
            }
            // Rejected by the type checker.
            ExprKind::With { .. } => Vec::new(),
        }
    }

    /// N003 for a fresh value containing qubits that is thrown away.
    fn discarded(&mut self, id: ExprId, use_: Use) {
        if use_ != Use::Discard {
            return;
        }
        let ty = self.typed.ty(id);
        if self.typed.types.linearity(ty) == Kind::Linear {
            let span = self.ast.expr(id).span;
            self.error(
                "N003",
                span,
                "this value contains qubits and is thrown away",
            )
            .help = Some("bind it with `let` and consume it, or measure it".into());
        }
    }

    /// N013 if the current context is reversible. Returns whether it was reported.
    fn not_reversible(&mut self, span: Span, what: &str) -> bool {
        if self.reversible.is_empty() {
            return false;
        }
        self.error("N013", span, format!("{what} is not reversible"))
            .help = Some("move it outside the `ctrl` block or reversible function".into());
        true
    }

    fn loop_body(&mut self, body: BlockId, facts: Vec<Fact>, var: Option<LocalId>) {
        let facts_mark = self.facts.len();
        let scope_mark = self.scope.len();
        self.loops.push(scope_mark);
        if let Some(var) = var {
            self.declare(var, false);
        }
        self.facts.extend(facts);
        self.block(body, Use::Discard);
        self.facts.truncate(facts_mark);
        self.leave_scope(scope_mark);
        self.loops.pop();
        // A loop may run zero times, and `break` leaves it.
        self.diverged = false;
    }

    fn if_expr(
        &mut self,
        cond: ExprId,
        then: BlockId,
        otherwise: Option<ExprId>,
        use_: Use,
        span: Span,
    ) {
        self.expr(cond, Use::Read);
        let mut then_facts = Vec::new();
        let mut else_facts = Vec::new();
        facts::facts_from(self.ast, self.typed, cond, true, &mut then_facts);
        facts::facts_from(self.ast, self.typed, cond, false, &mut else_facts);

        let before = self.moved.clone();
        let mark = self.facts.len();
        self.facts.extend(then_facts);
        let carried = self.block(then, use_);
        self.release(&carried);
        self.facts.truncate(mark);
        let then_moved = std::mem::replace(&mut self.moved, before.clone());
        let then_diverged = std::mem::replace(&mut self.diverged, false);

        self.facts.extend(else_facts);
        if let Some(otherwise) = otherwise {
            let carried = self.expr(otherwise, use_);
            self.release(&carried);
        }
        self.facts.truncate(mark);
        let else_moved = std::mem::take(&mut self.moved);
        let else_diverged = self.diverged;

        self.diverged = then_diverged && else_diverged;
        self.moved = match (then_diverged, else_diverged) {
            (true, _) => else_moved,
            (_, true) => then_moved,
            _ => self.merge_branches(then_moved, else_moved, span),
        };
    }

    /// N009 for anything with qubits moved in one branch but not the other.
    fn merge_branches(&mut self, a: Vec<Moved>, b: Vec<Moved>, span: Span) -> Vec<Moved> {
        let mut merged = a.clone();
        let mut reported: Vec<LocalId> = Vec::new();
        for (side, other, is_then) in [(&a, &b, true), (&b, &a, false)] {
            for m in side {
                if moved_in(other, &m.place) {
                    continue;
                }
                if !is_then {
                    merged.push(m.clone());
                }
                let ty = self.place_ty(&m.place);
                if self.typed.types.linearity(ty) == Kind::Copyable
                    || reported.contains(&m.place.root)
                {
                    continue;
                }
                reported.push(m.place.root);
                let name = self.name(m.place.root).to_string();
                let branch = if is_then {
                    "when the condition is true"
                } else {
                    "when the condition is false"
                };
                self.error(
                    "N009",
                    span,
                    format!("`{name}` is moved {branch} but not otherwise"),
                )
                .with_label_mut(m.span, "moved here")
                .help = Some("move it in both branches, or in neither".into());
            }
        }
        merged
    }

    fn ctrl(&mut self, controls: List<ExprId>, body: BlockId) {
        let mut placed: Vec<(Place, Span)> = Vec::new();
        let mut held = Vec::new();
        for &control in self.ast.list(controls) {
            self.place_subexprs(control);
            let span = self.ast.expr(control).span;
            let Some(place) = self.place_of(control) else {
                continue;
            };
            for (other, _) in &placed {
                match self.overlap(&place, other) {
                    Overlap::Disjoint => {}
                    Overlap::Overlaps => {
                        let text = self.expr_text(control);
                        self.error("N005", span, format!("`{text}` is used twice as a control"));
                    }
                    Overlap::Unproven => {
                        let text = self.expr_text(control);
                        self.error(
                            "N017",
                            span,
                            format!("cannot prove `{text}` is different from the other controls"),
                        );
                    }
                }
            }
            if let Some(i) = self.lend_place(&place, control, false, false, Holder::Control) {
                held.push(i);
            }
            placed.push((place, span));
        }
        self.reversible.push(self.order);
        self.block(body, Use::Discard);
        self.reversible.pop();
        self.release(&held);
    }

    /// N014: `adj` needs something reversible.
    fn check_adj(&mut self, call: ExprId) {
        let Some(callee) = self.typed.callees[call.index()] else {
            return;
        };
        let ok = match callee {
            Callee::Builtin(b) => b.reversible(),
            Callee::Convert(_) => true,
            Callee::Fn(f) => match self.typed.fns[f.index()].sig.reversible {
                Reversible::Yes => true,
                Reversible::No => false,
                Reversible::Infer => self.irreversible[f.index()].is_none(),
            },
        };
        if !ok {
            let span = self.ast.expr(call).span;
            let why = match callee {
                Callee::Fn(f) => self.irreversible[f.index()].clone(),
                _ => None,
            };
            let diagnostic = self.error(
                "N014",
                span,
                "only reversible functions can be inverted with `adj`",
            );
            if let Some(why) = why {
                diagnostic.labels.push((why.span, why.why));
            }
        }
    }

    /// N020: a preparation is run many times, so its arguments must be classical.
    fn preparation(&mut self, prep: ExprId) {
        let ExprKind::Call { args, .. } = self.ast.expr(prep).kind else {
            return;
        };
        for &arg in self.ast.list(args) {
            let ty = self.typed.ty(arg);
            let lends_mut = matches!(
                self.typed.adjust[arg.index()],
                Adjust::Lend { mutable: true }
            );
            let owns = matches!(
                self.ast.expr(arg).kind,
                ExprKind::Own {
                    kind: OwnKind::Mv | OwnKind::Mut,
                    ..
                }
            );
            if self.typed.types.linearity(ty) != Kind::Copyable || lends_mut || owns {
                let span = self.ast.expr(arg).span;
                let text = self.expr_text(arg);
                self.error("N020", span, format!("the preparation is run many times, but `{text}` is not a classical value it can be given each time"))
                    .help = Some("create the qubits inside the preparation, and pass only classical values".into());
                return;
            }
            self.expr(arg, Use::Read);
        }
    }

    fn shorthand_field(&mut self, name: Symbol, span: Span) {
        let Some(&local) = self
            .scope
            .iter()
            .rev()
            .find(|&&l| self.typed.local(l).name == name)
        else {
            return;
        };
        let ty = self.typed.local(local).ty;
        let place = Place {
            root: local,
            proj: Vec::new(),
        };
        if self.typed.types.linearity(ty) == Kind::Copyable {
            self.read(&place, span);
        } else {
            let text = self.ast.name(name).to_string();
            self.error(
                "N001",
                span,
                format!("`{text}` holds qubits, which cannot be copied"),
            )
            .help = Some(format!(
                "write `{text}: mv {text}` to move it into the struct"
            ));
            self.move_place(&place, ty, span);
        }
    }

    fn own(&mut self, id: ExprId, kind: OwnKind, place_expr: ExprId, use_: Use) -> Carried {
        self.place_subexprs(place_expr);
        let span = self.ast.expr(id).span;
        let Some(place) = self.place_of(place_expr) else {
            return self.expr(place_expr, Use::Value);
        };
        match kind {
            OwnKind::Mv => {
                if let Adjust::Lend { mutable } = self.typed.adjust[id.index()] {
                    let text = self.expr_text(place_expr);
                    self.error(
                        "N004",
                        span,
                        format!("`{text}` is only lent here, so it cannot be moved"),
                    )
                    .help = Some("remove `mv`".into());
                    return self
                        .lend_place(&place, place_expr, mutable, false, Holder::Pending)
                        .into_iter()
                        .collect();
                }
                let ty = self.typed.ty(place_expr);
                self.move_place(&place, ty, span);
                self.discarded(id, use_);
                Vec::new()
            }
            OwnKind::Ref | OwnKind::Mut => {
                let mutable = kind == OwnKind::Mut;
                self.lend_place(&place, place_expr, mutable, false, Holder::Pending)
                    .into_iter()
                    .collect()
            }
        }
    }

    /// A place used without an ownership word: read, copied, moved, or lent implicitly.
    fn place_use(&mut self, id: ExprId, use_: Use) -> Carried {
        self.place_subexprs(id);
        let span = self.ast.expr(id).span;
        let Some(place) = self.place_of(id) else {
            return Vec::new();
        };
        let ty = self.typed.ty(id);
        match self.typed.adjust[id.index()] {
            Adjust::Lend { mutable } => {
                return self
                    .lend_place(&place, id, mutable, true, Holder::Pending)
                    .into_iter()
                    .collect();
            }
            Adjust::Deref => {
                let target = self.typed.types.deref(ty);
                if use_ == Use::Value && self.typed.types.linearity(target) != Kind::Copyable {
                    let text = self.expr_text(id);
                    self.error(
                        "N007",
                        span,
                        format!("cannot move the value out of `{text}`, a reference"),
                    );
                    return Vec::new();
                }
                self.read(&place, span);
                return Vec::new();
            }
            Adjust::None => {}
        }
        let copyable = self.typed.types.linearity(ty) == Kind::Copyable
            || matches!(self.typed.types.kind(ty), TyKind::Ref { .. });
        if use_ == Use::Value && !copyable {
            let text = self.expr_text(id);
            self.error(
                "N001",
                span,
                format!("`{text}` holds qubits, which cannot be copied"),
            )
            .help = Some(format!("write `mv {text}` to move it"));
            self.move_place(&place, ty, span);
        } else {
            self.read(&place, span);
        }
        Vec::new()
    }

    fn call(
        &mut self,
        id: ExprId,
        receiver: Option<ExprId>,
        args: List<ExprId>,
        use_: Use,
    ) -> Carried {
        let span = self.ast.expr(id).span;
        let callee = self.typed.callees[id.index()];
        self.callee_rules(callee, span);

        let mut receiver_carried = Vec::new();
        if let Some(receiver) = receiver {
            receiver_carried = self.receiver(id, receiver);
        }
        let mut arg_carried = Vec::new();
        for &arg in self.ast.list(args) {
            arg_carried.extend(self.expr(arg, Use::Value));
        }

        // A returned reference keeps its lend alive: the receiver's for methods, otherwise
        // the single lent argument's (the elision rule).
        let ret = self.typed.ty(id);
        let returns_ref = matches!(self.typed.types.kind(ret), TyKind::Ref { .. });
        let carried = if !returns_ref {
            Vec::new()
        } else if receiver.is_some() {
            receiver_carried.clone()
        } else {
            arg_carried.clone()
        };
        let finished: Vec<usize> = receiver_carried
            .into_iter()
            .chain(arg_carried)
            .filter(|i| !carried.contains(i))
            .collect();
        self.release(&finished);
        if use_ == Use::Discard {
            self.discarded(id, use_);
        }
        carried
    }

    /// N013 and N019 for what is called.
    fn callee_rules(&mut self, callee: Option<Callee>, span: Span) {
        let Some(callee) = callee else { return };
        let (is_unsafe, reversible, name) = match callee {
            Callee::Builtin(b) => (b.is_unsafe(), b.reversible(), b.name().to_string()),
            Callee::Convert(_) => (false, true, String::new()),
            Callee::Fn(f) => {
                let sig = &self.typed.fns[f.index()].sig;
                let reversible = match sig.reversible {
                    Reversible::Yes => true,
                    Reversible::No => false,
                    Reversible::Infer => self.irreversible[f.index()].is_none(),
                };
                (
                    sig.is_unsafe,
                    reversible,
                    self.ast.name(self.ast.function(f).name).to_string(),
                )
            }
        };
        if is_unsafe && self.unsafe_depth == 0 {
            let message = if name == "release" {
                "`release` is unsafe: it trusts that the qubit is in |0⟩".to_string()
            } else {
                format!("`{name}` is an `unsafe fn`; calling it needs `unsafe`")
            };
            let help = if name == "release" {
                "use `discard`, which resets first, or wrap the call in `unsafe { … }`"
            } else {
                "wrap the call in `unsafe { … }`"
            };
            self.error("N019", span, message).help = Some(help.into());
        }
        if !reversible && !self.reversible.is_empty() {
            let why = match callee {
                Callee::Fn(f) => self.irreversible[f.index()].clone(),
                _ => None,
            };
            let diagnostic = self.error("N013", span, format!("`{name}` is not reversible"));
            if let Some(why) = why {
                diagnostic.labels.push((why.span, why.why));
            }
        }
    }

    /// A method's receiver, by how the method takes `self`.
    fn receiver(&mut self, call: ExprId, receiver: ExprId) -> Carried {
        let kind = self.typed.receivers[call.index()].unwrap_or(OwnKind::Ref);
        let receiver_expr = *self.ast.expr(receiver);
        let is_place = matches!(
            receiver_expr.kind,
            ExprKind::Name(_)
                | ExprKind::Field { .. }
                | ExprKind::TupleIndex { .. }
                | ExprKind::Index { .. }
        );
        let ty = self.typed.ty(receiver);
        let is_ref = matches!(self.typed.types.kind(ty), TyKind::Ref { .. });
        let method = match self.ast.expr(call).kind {
            ExprKind::MethodCall { method, .. } => self.ast.name(method).to_string(),
            _ => String::new(),
        };
        match kind {
            OwnKind::Mv => {
                if matches!(
                    receiver_expr.kind,
                    ExprKind::Own {
                        kind: OwnKind::Mv,
                        ..
                    }
                ) {
                    return self.expr(receiver, Use::Value);
                }
                if is_place && !is_ref {
                    self.place_subexprs(receiver);
                    let Some(place) = self.place_of(receiver) else {
                        return Vec::new();
                    };
                    if self.typed.types.linearity(ty) == Kind::Copyable {
                        self.read(&place, receiver_expr.span);
                    } else {
                        let text = self.expr_text(receiver);
                        self.error(
                            "N001",
                            receiver_expr.span,
                            format!("`{method}` takes ownership of `{text}`"),
                        )
                        .help = Some(format!("write `mv {text}.{method}()`"));
                        self.move_place(&place, ty, receiver_expr.span);
                    }
                    return Vec::new();
                }
                if is_ref {
                    let text = self.expr_text(receiver);
                    let span = receiver_expr.span;
                    self.error(
                        "N007",
                        span,
                        format!("`{method}` takes ownership, but `{text}` is a reference"),
                    )
                    .help = Some(if is_place {
                        "take the value as owned (`q: Qubit`) to consume it".to_string()
                    } else {
                        format!(
                            "end the chain, then consume the qubit separately: `mv q.{method}()`"
                        )
                    });
                    // Recover as if the chain had been split and its root consumed.
                    let carried = self.expr(receiver, Use::Read);
                    if let Some(&i) = carried.first() {
                        let place = self.lends[i].place.clone();
                        self.release(&carried);
                        self.moved.push(Moved { place, span });
                    }
                    return Vec::new();
                }
                self.expr(receiver, Use::Value)
            }
            OwnKind::Ref | OwnKind::Mut => {
                let mutable = kind == OwnKind::Mut;
                if is_place && !is_ref {
                    self.place_subexprs(receiver);
                    let Some(place) = self.place_of(receiver) else {
                        return Vec::new();
                    };
                    return self
                        .lend_place(&place, receiver, mutable, false, Holder::Pending)
                        .into_iter()
                        .collect();
                }
                // A reference (a variable or the result of a chain) is lent on as it is.
                self.expr(receiver, Use::Read)
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Moves, lends and reads
// ---------------------------------------------------------------------------------------

impl Owner<'_> {
    fn is_reference(&self, local: LocalId) -> bool {
        matches!(
            self.typed.types.kind(self.typed.local(local).ty),
            TyKind::Ref { .. }
        )
    }

    fn is_mut_reference(&self, local: LocalId) -> bool {
        matches!(
            self.typed.types.kind(self.typed.local(local).ty),
            TyKind::Ref { mutable: true, .. }
        )
    }

    /// The place an expression names, if it is one.
    fn place_of(&self, id: ExprId) -> Option<Place> {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Name(_) => match self.typed.res[id.index()] {
                Res::Local(root) => Some(Place {
                    root,
                    proj: Vec::new(),
                }),
                _ => None,
            },
            ExprKind::Field { base, name } => {
                let mut place = self.place_of(base)?;
                let base_ty = self.typed.types.deref(self.typed.ty(base));
                let TyKind::Struct(s) = *self.typed.types.kind(base_ty) else {
                    return None;
                };
                let index = self.typed.types.structs[s.index()]
                    .fields
                    .iter()
                    .position(|(n, _)| *n == name)?;
                place.proj.push(Proj::Field(index as u32));
                Some(place)
            }
            ExprKind::TupleIndex { base, index } => {
                let mut place = self.place_of(base)?;
                place.proj.push(Proj::Field(index));
                Some(place)
            }
            ExprKind::Index { base, index } => {
                let mut place = self.place_of(base)?;
                place
                    .proj
                    .push(Proj::Index(facts::linear(ast, self.typed, index)));
                Some(place)
            }
            _ => None,
        }
    }

    /// Reads the index expressions inside a place, and checks qubit-array bounds (N018).
    fn place_subexprs(&mut self, id: ExprId) {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.place_subexprs(base)
            }
            ExprKind::Index { base, index } => {
                self.place_subexprs(base);
                self.expr(index, Use::Read);
                self.check_bounds(base, index);
            }
            _ => {}
        }
    }

    /// N018: indices into arrays containing qubits must be proven in range.
    fn check_bounds(&mut self, base: ExprId, index: ExprId) {
        let array = self.typed.types.deref(self.typed.ty(base));
        let TyKind::Array(elem, len) = *self.typed.types.kind(array) else {
            return;
        };
        if self.typed.types.linearity(elem) != Kind::Linear {
            return;
        }
        let len = i64::try_from(len).unwrap_or(i64::MAX);
        let span = self.ast.expr(index).span;
        let text = self.expr_text(index);
        let array_text = self.expr_text(base);
        let (lo, hi) = match facts::linear(self.ast, self.typed, index) {
            Some(form) => facts::interval(&self.facts, &form),
            None => (None, None),
        };
        let message = match (lo, hi) {
            (Some(lo), Some(hi)) if lo >= 0 && hi < len => return,
            (Some(lo), _) if lo < 0 => {
                format!("`{text}` can be {lo}, outside `{array_text}` (length {len})")
            }
            (_, Some(hi)) if hi >= len => {
                format!("`{text}` can be {hi}, outside `{array_text}` (length {len})")
            }
            _ => format!("cannot prove `{text}` is within `{array_text}` (length {len})"),
        };
        self.error("N018", span, message).help = Some(format!(
            "loop over a range that fits, or `assert 0 <= {text} and {text} < {len}`"
        ));
    }

    fn overlap(&self, a: &Place, b: &Place) -> Overlap {
        if a.root != b.root {
            return Overlap::Disjoint;
        }
        let mut result = Overlap::Overlaps;
        for (x, y) in a.proj.iter().zip(&b.proj) {
            match (x, y) {
                (Proj::Field(i), Proj::Field(j)) if i != j => return Overlap::Disjoint,
                (Proj::Index(Some(i)), Proj::Index(Some(j))) => {
                    if facts::differ(&self.facts, i, j) {
                        return Overlap::Disjoint;
                    }
                    if i != j {
                        result = Overlap::Unproven;
                    }
                }
                (Proj::Index(_), Proj::Index(_)) => result = Overlap::Unproven,
                _ => {}
            }
        }
        result
    }

    /// The type of a place, following its projections from the root local.
    fn place_ty(&self, place: &Place) -> Ty {
        let types = &self.typed.types;
        let mut ty = types.deref(self.typed.local(place.root).ty);
        for proj in &place.proj {
            ty = match (types.kind(ty), proj) {
                (&TyKind::Struct(s), Proj::Field(i)) => {
                    types.structs[s.index()].fields[*i as usize].1
                }
                (TyKind::Tuple(elems), Proj::Field(i)) => elems[*i as usize],
                (&TyKind::Array(elem, _), Proj::Index(_)) => elem,
                _ => return Ty::ERROR,
            };
        }
        ty
    }

    /// Whether `place` and everything in it that needs consuming has been moved.
    fn fully_moved(&self, place: &Place, ty: Ty) -> bool {
        if moved_in(&self.moved, place) {
            return true;
        }
        let types = &self.typed.types;
        if types.linearity(ty) != Kind::Linear {
            return true;
        }
        let child = |proj: Proj| {
            let mut p = place.clone();
            p.proj.push(proj);
            p
        };
        match types.kind(ty) {
            &TyKind::Struct(s) => types.structs[s.index()]
                .fields
                .iter()
                .enumerate()
                .all(|(i, &(_, t))| self.fully_moved(&child(Proj::Field(i as u32)), t)),
            TyKind::Tuple(elems) => elems
                .iter()
                .enumerate()
                .all(|(i, &t)| self.fully_moved(&child(Proj::Field(i as u32)), t)),
            &TyKind::Array(elem, len) if len <= 4096 => (0..len).all(|i| {
                let index = facts_constant(i as i64);
                self.fully_moved(&child(Proj::Index(Some(index))), elem)
            }),
            _ => false,
        }
    }

    /// The earlier move that `place` runs into: (the move, whether only part of `place` was
    /// moved, whether the overlap is only unproven).
    fn moved_conflict(&self, place: &Place) -> Option<(Moved, bool, bool)> {
        for m in &self.moved {
            match self.overlap(place, &m.place) {
                Overlap::Disjoint => {}
                overlap => {
                    let partial = m.place.proj.len() > place.proj.len();
                    return Some((m.clone(), partial, overlap == Overlap::Unproven));
                }
            }
        }
        None
    }

    /// N002 or N011 if `place` (or part of it) was moved. Returns whether it was.
    fn report_moved(&mut self, place: &Place, span: Span) -> bool {
        let Some((m, partial, unproven)) = self.moved_conflict(place) else {
            return false;
        };
        let name = self.place_text(place);
        if partial {
            let part = self.place_text(&m.place);
            self.error(
                "N011",
                span,
                format!("`{part}` was moved, so `{name}` cannot be used as a whole"),
            )
            .with_label_mut(m.span, "moved here");
        } else {
            let message = if unproven {
                format!("`{name}` may have been moved")
            } else {
                format!("`{name}` was moved")
            };
            self.error("N002", span, message)
                .with_label_mut(m.span, "moved here");
        }
        true
    }

    fn read(&mut self, place: &Place, span: Span) {
        if self.report_moved(place, span) {
            return;
        }
        if let Some(i) = self.conflicting_lend(place, false) {
            let name = self.place_text(place);
            let lend_span = self.lends[i].span;
            self.error(
                "N005",
                span,
                format!("`{name}` cannot be used while it is lent for changing"),
            )
            .with_label_mut(lend_span, "lent here");
        }
    }

    /// An alive lend that conflicts with using `place` (for changing, if `mutable`).
    fn conflicting_lend(&self, place: &Place, mutable: bool) -> Option<usize> {
        self.lends.iter().position(|l| {
            l.alive && (mutable || l.mutable) && self.overlap(place, &l.place) != Overlap::Disjoint
        })
    }

    fn move_place(&mut self, place: &Place, ty: Ty, span: Span) {
        let name = self.place_text(place);
        if self.is_reference(place.root) {
            let root = self.name(place.root).to_string();
            self.error(
                "N007",
                span,
                format!("cannot move out of `{name}`: `{root}` is a reference"),
            )
            .help = Some(format!(
                "take ownership with `{root}: {}`",
                self.typed.types.display(
                    self.typed.types.deref(self.typed.local(place.root).ty),
                    self.ast
                )
            ));
            return;
        }
        if let Some(i) = place.proj.iter().position(Proj::is_dynamic) {
            self.error(
                "N012",
                span,
                format!("cannot move out of `{name}` with a non-constant index"),
            )
            .help = Some("move the whole array, or use constant indices".into());
            // Recover as if the whole array were moved.
            let array = Place {
                root: place.root,
                proj: place.proj[..i].to_vec(),
            };
            self.moved.push(Moved { place: array, span });
            return;
        }
        if self.report_moved(place, span) {
            return;
        }
        if let Some(i) = self
            .lends
            .iter()
            .position(|l| l.alive && self.overlap(place, &l.place) != Overlap::Disjoint)
        {
            let lend_span = self.lends[i].span;
            let holder = match self.lends[i].holder {
                Holder::Local(l) => format!(" to `{}`", self.name(l)),
                Holder::Control => " as a control".into(),
                Holder::Pending => String::new(),
            };
            self.error(
                "N006",
                span,
                format!("`{name}` is moved while it is lent{holder}"),
            )
            .with_label_mut(lend_span, "lent here");
        }
        let linear = self.typed.types.linearity(ty) != Kind::Copyable;
        if linear && self.locals[place.root.index()].loop_depth < self.loops.len() as u32 {
            self.error(
                "N010",
                span,
                format!("`{name}` would be moved on every iteration of the loop"),
            )
            .help = Some("move it after the loop, or create it inside the loop".into());
        }
        if self.typed.types.linearity(ty) == Kind::Linear && !self.reversible.is_empty() {
            self.error("N013", span, format!("moving `{name}` is not reversible"));
        }
        self.moved.push(Moved {
            place: place.clone(),
            span,
        });
    }

    /// Lends a place, checking it is usable and conflicts with no other lend. Returns the
    /// lend, if it was made.
    fn lend_place(
        &mut self,
        place: &Place,
        expr: ExprId,
        mutable: bool,
        implicit_arg: bool,
        holder: Holder,
    ) -> Option<usize> {
        let span = self.ast.expr(expr).span;
        if self.report_moved(place, span) {
            return None;
        }
        let ty = self.place_ty(place);
        let name = self.place_text(place);
        let root = place.root;
        let classical = !self.typed.types.is_pure_quantum(ty);
        if mutable && self.is_reference(root) && !self.is_mut_reference(root) {
            let root_name = self.name(root).to_string();
            self.error(
                "N015",
                span,
                format!(
                    "`{root_name}` is a `ref` reference, so it cannot be used to change `{name}`"
                ),
            );
        } else if mutable && classical {
            if implicit_arg {
                self.error(
                    "N016",
                    span,
                    format!("this call changes `{name}`, which holds classical data"),
                )
                .help = Some(format!("write `mut {name}` to show it is changed"));
            } else if !self.is_reference(root) && !self.typed.local(root).mutable {
                let root_name = self.name(root).to_string();
                let what = if name == root_name {
                    "it".to_string()
                } else {
                    format!("`{name}`")
                };
                self.error(
                    "N015",
                    span,
                    format!("`{root_name}` is not declared `let mut`, so {what} cannot be changed"),
                )
                .help = Some(format!("declare it with `let mut {root_name}`"));
            }
        }
        for i in 0..self.lends.len() {
            let other = &self.lends[i];
            if !other.alive || !(mutable || other.mutable) {
                continue;
            }
            match self.overlap(place, &other.place) {
                Overlap::Disjoint => continue,
                Overlap::Overlaps => {
                    let other_span = other.span;
                    let message = match other.holder {
                        Holder::Control => format!("`{name}` is a control of this block"),
                        Holder::Local(l) => {
                            format!("`{name}` is already lent to `{}`", self.name(l))
                        }
                        Holder::Pending if mutable && other.mutable => {
                            format!("`{name}` is lent for changing twice")
                        }
                        Holder::Pending => {
                            format!("`{name}` is lent for changing and for reading at once")
                        }
                    };
                    self.error("N005", span, message)
                        .with_label_mut(other_span, "lent here");
                }
                Overlap::Unproven => {
                    let other_span = other.span;
                    let other_name = self.place_text(&other.place);
                    self.error("N017", span, format!("cannot prove `{name}` and `{other_name}` are different"))
                        .with_label_mut(other_span, "lent here")
                        .help = Some("use indices that differ by a constant, or establish it with `if` or `assert`".into());
                }
            }
            break;
        }
        self.lends.push(Lend {
            place: place.clone(),
            mutable,
            holder,
            span,
            alive: true,
        });
        Some(self.lends.len() - 1)
    }
}

// ---------------------------------------------------------------------------------------
// Text for messages
// ---------------------------------------------------------------------------------------

impl Owner<'_> {
    fn place_text(&self, place: &Place) -> String {
        let mut text = self.name(place.root).to_string();
        let types = &self.typed.types;
        let mut ty = types.deref(self.typed.local(place.root).ty);
        for proj in &place.proj {
            match (types.kind(ty), proj) {
                (&TyKind::Struct(s), Proj::Field(i)) => {
                    let (name, field_ty) = types.structs[s.index()].fields[*i as usize];
                    text.push('.');
                    text.push_str(self.ast.name(name));
                    ty = field_ty;
                }
                (TyKind::Tuple(elems), Proj::Field(i)) => {
                    text.push_str(&format!(".{i}"));
                    ty = elems[*i as usize];
                }
                (&TyKind::Array(elem, _), Proj::Index(index)) => {
                    let index = index
                        .as_ref()
                        .map_or("…".to_string(), |f| self.linear_text(f));
                    text.push_str(&format!("[{index}]"));
                    ty = elem;
                }
                _ => break,
            }
        }
        text
    }

    /// A linear form written out, such as `3 - i` or `2 * i + 1`.
    fn linear_text(&self, form: &Linear) -> String {
        let mut text = String::new();
        for &(local, coeff) in form.terms() {
            let name = self.name(local);
            let (sign, magnitude) = if coeff < 0 {
                ("-", -coeff)
            } else {
                ("+", coeff)
            };
            if text.is_empty() {
                if sign == "-" {
                    text.push('-');
                }
            } else {
                text.push_str(&format!(" {sign} "));
            }
            if magnitude == 1 {
                text.push_str(name);
            } else {
                text.push_str(&format!("{magnitude} * {name}"));
            }
        }
        let c = form.constant_part();
        if text.is_empty() {
            return c.to_string();
        }
        // Lead with the constant when the first term is negative: `3 - i`, not `-i + 3`.
        if text.starts_with('-') && c > 0 {
            return format!("{c} {}", text.replacen('-', "- ", 1));
        }
        match c {
            0 => text,
            c if c > 0 => format!("{text} + {c}"),
            c => format!("{text} - {}", -c),
        }
    }

    /// Source-like text of an expression, rebuilt from the tree.
    fn expr_text(&self, id: ExprId) -> String {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Name(name) => ast.name(name).into(),
            ExprKind::Int(v) => v.to_string(),
            ExprKind::Field { base, name } => {
                format!("{}.{}", self.expr_text(base), ast.name(name))
            }
            ExprKind::TupleIndex { base, index } => format!("{}.{index}", self.expr_text(base)),
            ExprKind::Index { base, index } => {
                format!("{}[{}]", self.expr_text(base), self.expr_text(index))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                format!(
                    "{} {} {}",
                    self.expr_text(lhs),
                    op.as_str(),
                    self.expr_text(rhs)
                )
            }
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => format!("-{}", self.expr_text(operand)),
            ExprKind::Own { kind, place } => format!("{} {}", kind.as_str(), self.expr_text(place)),
            ExprKind::MethodCall {
                receiver, method, ..
            } => {
                format!("{}.{}(…)", self.expr_text(receiver), ast.name(method))
            }
            ExprKind::Call { callee, .. } => format!("{}(…)", self.expr_text(callee)),
            _ => "…".into(),
        }
    }
}

fn facts_constant(value: i64) -> Linear {
    facts::constant_form(value)
}

/// Whether `prefix` is `place` or contains it.
fn is_prefix(prefix: &Place, place: &Place) -> bool {
    prefix.root == place.root
        && prefix.proj.len() <= place.proj.len()
        && prefix.proj.iter().zip(&place.proj).all(|(a, b)| a == b)
}

/// Whether `place` is covered by one of `moved`.
fn moved_in(moved: &[Moved], place: &Place) -> bool {
    moved.iter().any(|m| is_prefix(&m.place, place))
}

/// In-place builders, for adding to a diagnostic after it has been pushed.
trait DiagnosticExt {
    fn with_label_mut(&mut self, span: Span, message: impl Into<String>) -> &mut Self;
}

impl DiagnosticExt for Diagnostic {
    fn with_label_mut(&mut self, span: Span, message: impl Into<String>) -> &mut Self {
        self.labels.push((span, message.into()));
        self
    }
}
