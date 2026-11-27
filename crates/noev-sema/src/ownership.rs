//! The ownership checker (`spec/ownership.md`).
//!
//! Runs after type checking, one function at a time. It walks each body in program order,
//! following the structure of the code: both branches of an `if` are checked and their
//! effects compared, and loop bodies are checked knowing they may run again. It tracks:
//!
//! - which places have been moved, to catch use after move and values left unconsumed;
//! - which places are lent, and to whom (a call, a variable, a `ctrl` block), with
//!   non-lexical, path-sensitive lifetimes: a variable's lend lives only along paths that
//!   still use it;
//! - facts about integers, to prove array indices differ and are in range;
//! - whether the code is in a reversible context.

use noev_syntax::ast::*;
use noev_syntax::{Diagnostic, FxHashMap, Severity, Span, Symbol};

use crate::check::{Adjust, BorrowRoot, BorrowSource, Callee, LocalId, Res, Typed};
use crate::facts::{self, Fact, Linear, Var};
use crate::prelude::{Builtin, Reversible};
use crate::ty::{Kind, Ty, TyKind};

/// Checks ownership in every function of `ast`.
pub fn check_ownership(ast: &Ast, typed: &Typed) -> Vec<Diagnostic> {
    let irreversible = infer_reversibility(ast, typed);
    let prints = infer_printing(ast, typed);
    let method_fields = infer_method_fields(ast, typed);
    let return_borrows = infer_return_borrows(ast, typed);
    let mut checker = Owner {
        ast,
        module: ModuleId::from_index(0),
        typed,
        irreversible,
        preparable: None,
        prints,
        method_fields,
        return_borrows,
        diagnostics: Vec::new(),
        locals: vec![LocalInfo::default(); typed.locals.len()],
        order: 0,
        scope: Vec::new(),
        moved: Vec::new(),
        lends: Vec::new(),
        facts: Vec::new(),
        loops: Vec::new(),
        reversible: Vec::new(),
        in_with: Vec::new(),
        with_touched: Vec::new(),
        unsafe_depth: 0,
        diverged: false,
        returns_ref: false,
        current_fn: None,
        lent_match: false,
        assume_copy: Vec::new(),
        liveness: None,
        current_live: LiveSet::default(),
    };
    let mut copy_needs = FxHashMap::default();
    for (index, decl) in ast.all_fns().iter().enumerate() {
        if let Some(body) = decl.body {
            checker.function_inferring(FnId::from_index(index), body, &mut copy_needs);
        }
    }
    checker.check_copy_needs(copy_needs);
    // A library's public function that acts on qubits and is reversible only by inference
    // cannot be used as reversible by the packages that depend on it (`spec/modules.md` §5).
    // Only the library being compiled is warned about: a program has no dependents, and a
    // dependency's warnings are its own authors'.
    let library = typed.main.is_none();
    for (index, decl) in ast.all_fns().iter().enumerate() {
        let module = typed.fn_modules[index];
        if !library || ast.module(module).package.index() != 0 {
            continue;
        }
        let info = &typed.fns[index];
        let quantum = info
            .sig
            .params
            .iter()
            .chain(&info.receiver_ty)
            .any(|&ty| typed.types.linearity(typed.types.deref(ty)) == Kind::Linear);
        if decl.vis == Vis::Pub
            && decl.body.is_some()
            && quantum
            && info.sig.reversible == Reversible::Infer
            && checker.irreversible[index].is_none()
        {
            let name = ast.name(decl.name);
            let message = format!(
                "`{name}` is reversible, but other packages can use it as reversible only if it \
                 is declared `rev fn`"
            );
            checker.diagnostics.push(Diagnostic {
                severity: Severity::Warning,
                ..Diagnostic::error(decl.name_span, message)
                    .with_help(format!("write `rev fn {name}`"))
            });
        }
    }
    checker
        .diagnostics
        .extend(crate::target::check_backend_impls(ast, typed));
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
    /// Every remaining qubit owned by this local is proven to be in \|0⟩ (`spec/ownership.md` §9).
    zero: bool,
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
    /// A place allocated or used in `with`, `ref`-lent for the length of `do`.
    With,
}

#[derive(Clone, Debug)]
struct Lend {
    place: Place,
    mutable: bool,
    holder: Holder,
    span: Span,
    alive: bool,
    /// True when this lend only locks an intermediate local reference variable while a
    /// derived reference is live; the underlying storage is tracked by cloned origin lends.
    reborrow: bool,
}

/// Which fields of `self` a same-package struct method (`ref self` or `mut self`) uses.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MethodFields {
    /// Uses `self` as a whole (or is not a struct method taking `ref`/`mut self`).
    Whole,
    /// Uses only these `(field_index, mutable)` accesses on `self`.
    Fields(Vec<(u32, bool)>),
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

/// Why a function in another package, not declared `rev fn`, is not reversible where it is
/// called (`spec/modules.md` §5).
const ELSEWHERE: &str =
    "which is in another package: there, only a function declared `rev fn` is reversible";

/// Whether the functions in modules `a` and `b` are in different packages.
fn other_package(ast: &Ast, a: ModuleId, b: ModuleId) -> bool {
    ast.module(a).package != ast.module(b).package
}

struct Owner<'a> {
    ast: &'a Ast,
    /// The module of the function being checked, whose package decides which functions are
    /// reversible by inference.
    module: ModuleId,
    typed: &'a Typed,
    irreversible: Vec<Option<Irreversible>>,
    /// For each function, why it cannot be undone as a bra's preparation, if it cannot.
    /// Worked out when the first bra of a preparation needs it.
    preparable: Option<Vec<Option<Irreversible>>>,
    /// For each function, where it prints (directly or through a call), if it does.
    prints: Vec<Option<Span>>,
    /// For each struct method, which fields of `self` it touches.
    method_fields: Vec<MethodFields>,
    /// For each function returning references, which parameters (and fields) it may borrow from.
    return_borrows: Vec<Vec<BorrowSource>>,
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
    /// For each enclosing `with` block (while checking its `with` part), its declaration order.
    in_with: Vec<u32>,
    /// For each enclosing `with` block, its declaration order and the outer places holding
    /// qubits that it lent (`ref` or `mut`), which stay `ref`-lent during `do`.
    with_touched: Vec<(u32, Vec<(Place, Span)>)>,
    unsafe_depth: u32,
    /// Control never reaches the current point (after `return`, `break` or `continue`).
    diverged: bool,
    /// Whether the current function returns a reference (directly or in a struct/enum/tuple).
    returns_ref: bool,
    /// The function currently being checked.
    current_fn: Option<FnId>,
    /// While binding the pattern of a `match` on a lent value, where nothing is taken.
    lent_match: bool,
    /// The inferable type parameters taken to be `Copy` while checking this body
    /// (`spec/types.md` §6).
    assume_copy: Vec<Ty>,
    /// Backward liveness of reference-holding locals in the current function; `None` when the
    /// function declares none.
    liveness: Option<Liveness>,
    /// Reference-holding locals live immediately after the current operation.
    current_live: LiveSet,
}

// ---------------------------------------------------------------------------------------
// Reversibility inference
// ---------------------------------------------------------------------------------------

/// For each function, where and why it is not reversible, or `None` if it is (declared `rev`,
/// or inferred to be). For tools such as the language server.
pub fn reversibility(ast: &Ast, typed: &Typed) -> Vec<Option<(Span, String)>> {
    infer_reversibility(ast, typed)
        .into_iter()
        .map(|r| r.map(|i| (i.span, i.why)))
        .collect()
}

/// What makes a property inferred over the whole program hold for a function: something in
/// its own body, or a call of a function it holds for.
#[derive(Clone)]
enum Cause<T> {
    Own(T),
    Call(FnId, Span),
}

/// Every function's causes, in body order, in one list: function `i`'s are
/// `list[ranges[i]]`, and `ranges[i]` is `None` for a function the property is not inferred
/// for.
struct Causes<T> {
    list: Vec<Cause<T>>,
    ranges: Vec<Option<(u32, u32)>>,
}

/// Each function's causes, from `cause` on each node of its body, for the functions
/// `include` accepts; collecting stops at the first cause of its own, since none after it can
/// be first.
fn collect_causes<T>(
    ast: &Ast,
    include: impl Fn(usize) -> bool,
    mut cause: impl FnMut(usize, Node<'_>, &mut Vec<Cause<T>>),
) -> Causes<T> {
    let mut list = Vec::new();
    let ranges = ast
        .all_fns()
        .iter()
        .enumerate()
        .map(|(index, decl)| {
            let body = decl.body.filter(|_| include(index))?;
            let start = list.len();
            let mut with_locals = Vec::new();
            walk_block(ast, body, false, &mut with_locals, &mut |node| {
                if list.len() == start || !matches!(list.last(), Some(Cause::Own(_))) {
                    cause(index, node, &mut list);
                }
            });
            Some((start as u32, list.len() as u32))
        })
        .collect();
    Causes { list, ranges }
}

/// Infers a property that holds for a function when its body has a [`Cause::Own`] cause, or
/// calls a function it holds for: each function's first cause that holds. The functions are
/// looked at in order once, then each one left undecided again only when a function it calls
/// is found to hold, so this is linear in the size of the program however its calls are
/// ordered.
fn infer_from_calls<T: Clone>(causes: &Causes<T>) -> Vec<Option<Cause<T>>> {
    let n = causes.ranges.len();
    let of = |i: usize| match causes.ranges[i] {
        Some((start, end)) => &causes.list[start as usize..end as usize],
        None => &[],
    };
    let first = |result: &[Option<Cause<T>>], list: &[Cause<T>]| {
        list.iter()
            .find(|cause| match cause {
                Cause::Own(_) => true,
                Cause::Call(f, _) => result[f.index()].is_some(),
            })
            .cloned()
    };
    let mut result: Vec<Option<Cause<T>>> = vec![None; n];
    let mut work = Vec::new();
    for i in 0..n {
        if causes.ranges[i].is_some() {
            result[i] = first(&result, of(i));
            if result[i].is_some() {
                work.push(i);
            }
        }
    }
    if work.len() == n {
        return result;
    }
    // Who calls each function, among those still undecided, as one list in callee order.
    let mut start = vec![0u32; n + 1];
    let undecided =
        |result: &[Option<Cause<T>>], i: usize| result[i].is_none() && causes.ranges[i].is_some();
    for i in (0..n).filter(|&i| undecided(&result, i)) {
        for cause in of(i) {
            if let Cause::Call(f, _) = cause {
                start[f.index() + 1] += 1;
            }
        }
    }
    for i in 0..n {
        start[i + 1] += start[i];
    }
    let mut callers = vec![0u32; start[n] as usize];
    let mut fill = start.clone();
    for i in (0..n).filter(|&i| undecided(&result, i)) {
        for cause in of(i) {
            if let Cause::Call(f, _) = cause {
                callers[fill[f.index()] as usize] = i as u32;
                fill[f.index()] += 1;
            }
        }
    }
    while let Some(g) = work.pop() {
        for &c in &callers[start[g] as usize..start[g + 1] as usize] {
            let c = c as usize;
            if result[c].is_none() {
                result[c] = first(&result, of(c));
                if result[c].is_some() {
                    work.push(c);
                }
            }
        }
    }
    result
}

/// For each function, why it is not reversible, or `None` if it is. Functions without `rev`
/// are inferred from their bodies, and the functions they call.
fn infer_reversibility(ast: &Ast, typed: &Typed) -> Vec<Option<Irreversible>> {
    let causes = collect_causes(
        ast,
        |index| typed.fns[index].sig.reversible == Reversible::Infer,
        |index, node, list| irreversible_causes(ast, typed, index, node, list),
    );
    irreversible(ast, infer_from_calls(&causes))
}

/// For each function, why a bra cannot undo it as a preparation (`spec/ownership.md` §12): the
/// first thing it does that is not reversible, other than creating qubits and moving them out.
fn infer_preparable(ast: &Ast, typed: &Typed) -> Vec<Option<Irreversible>> {
    let causes = collect_causes(
        ast,
        |index| typed.fns[index].sig.reversible == Reversible::Infer,
        |index, node, list| {
            let creates = matches!(node, Node::Expr(id, _) if matches!(
                ast.expr(id).kind,
                ExprKind::Ket { .. } | ExprKind::Own { kind: OwnKind::Mv, .. }
            ));
            if !creates {
                irreversible_causes(ast, typed, index, node, list);
            }
        },
    );
    irreversible(ast, infer_from_calls(&causes))
}

/// Each function's reason for not being reversible, from the cause found.
fn irreversible(ast: &Ast, found: Vec<Option<Cause<Irreversible>>>) -> Vec<Option<Irreversible>> {
    found
        .into_iter()
        .map(|cause| {
            Some(match cause? {
                Cause::Own(why) => why,
                Cause::Call(f, span) => Irreversible {
                    span,
                    why: format!(
                        "it calls `{}`, which is not reversible",
                        ast.name(ast.function(f).name)
                    ),
                },
            })
        })
        .collect()
}

/// For each function, the first place it prints, directly or through a call it makes.
fn infer_printing(ast: &Ast, typed: &Typed) -> Vec<Option<Span>> {
    let causes = collect_causes(
        ast,
        |_| true,
        |_, node, list| {
            let Node::Expr(id, _) = node else { return };
            match typed.callees[id.index()] {
                Some(Callee::Builtin(Builtin::Print { .. })) => {
                    list.push(Cause::Own(ast.expr(id).span))
                }
                Some(Callee::Fn(f)) => list.push(Cause::Call(f, ast.expr(id).span)),
                _ => {}
            }
        },
    );
    infer_from_calls(&causes)
        .into_iter()
        .map(|cause| match cause? {
            Cause::Own(span) | Cause::Call(_, span) => Some(span),
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Node<'w> {
    /// An expression, with the patterns declared inside the enclosing `with` block if any.
    Expr(ExprId, Option<&'w [PatId]>),
    Assign(ExprId, Span),
}

/// Whether `local` is bound by any pattern in `pats`.
fn bound_in(ast: &Ast, typed: &Typed, pats: &[PatId], local: LocalId) -> bool {
    pats.iter().any(|&p| pat_binds(ast, typed, p, local))
}

fn pat_binds(ast: &Ast, typed: &Typed, pat: PatId, local: LocalId) -> bool {
    match ast.pat(pat).kind {
        PatKind::Name(_) => typed.pat_locals[pat.index()] == Some(local),
        PatKind::Tuple(l) | PatKind::Array(l) => {
            ast.list(l).iter().any(|&p| pat_binds(ast, typed, p, local))
        }
        PatKind::Variant { data, .. } => match data {
            PatData::None => false,
            PatData::Tuple(l) => ast.list(l).iter().any(|&p| pat_binds(ast, typed, p, local)),
            PatData::Fields { fields, .. } => ast
                .list(fields)
                .iter()
                .filter_map(|f| f.pat)
                .any(|p| pat_binds(ast, typed, p, local)),
        },
        PatKind::Wildcard | PatKind::Literal(_) => false,
    }
}

/// What in `node`, in the function `index`, makes it not reversible, if anything: pushed to
/// `list`, as a cause of its own or a call of a function that would.
fn irreversible_causes(
    ast: &Ast,
    typed: &Typed,
    index: usize,
    node: Node<'_>,
    list: &mut Vec<Cause<Irreversible>>,
) {
    let info = &typed.fns[index];
    let module = typed.fn_modules[index];
    let own = |span, why: String| Cause::Own(Irreversible { span, why });
    match node {
        // `a += b` calling `AddAssign` changes `a` through the call, which the place's own
        // node stands for.
        Node::Assign(place, _) if typed.callees[place.index()].is_some() => {}
        Node::Assign(place, span) => {
            let Some(root) = root_local(ast, typed, place) else {
                return;
            };
            if info.self_local != Some(root) && !info.params.contains(&root) {
                return;
            }
            let name = ast.name(typed.local(root).name);
            list.push(own(
                span,
                format!("it changes `{name}`, which belongs to its caller"),
            ));
        }
        Node::Expr(id, in_with) => {
            let expr = ast.expr(id);
            let why: String = match expr.kind {
                // `with` blocks may allocate ancillas and move locals they declared.
                ExprKind::Ket { .. } if in_with.is_some() => return,
                ExprKind::Ket { .. } => "it creates qubits".into(),
                ExprKind::BraTest { .. } | ExprKind::PrepTest { .. } => "it measures qubits".into(),
                ExprKind::Overlap { .. } => "it computes an amplitude".into(),
                ExprKind::Expectation { .. } => "it computes an expectation value".into(),
                ExprKind::Sample { .. } => "it samples a run".into(),
                ExprKind::Own {
                    kind: OwnKind::Mv,
                    place,
                } if typed.types.linearity(typed.ty(id)) == Kind::Linear => {
                    if let Some(pats) = in_with
                        && root_local(ast, typed, place)
                            .is_some_and(|l| bound_in(ast, typed, pats, l))
                    {
                        return;
                    }
                    "it moves qubits".into()
                }
                _ if typed.callees[id.index()].is_some() => {
                    match typed.callees[id.index()] {
                        Some(Callee::Builtin(b)) if !b.reversible() => {
                            format!("it calls `{}`", b.name())
                        }
                        Some(Callee::Fn(f)) => {
                            let name = ast.name(ast.function(f).name);
                            match typed.fns[f.index()].sig.reversible {
                                Reversible::Infer => {
                                    list.push(Cause::Call(f, expr.span));
                                    if !other_package(ast, typed.fn_modules[f.index()], module) {
                                        return;
                                    }
                                    format!("it calls `{name}`, {ELSEWHERE}")
                                }
                                // A trait's function not declared `rev`: its `impl`s are not
                                // known here.
                                Reversible::No => format!(
                                    "it calls `{name}`, which its trait does not declare `rev fn`"
                                ),
                                Reversible::Yes => return,
                            }
                        }
                        _ => return,
                    }
                }
                _ => return,
            };
            list.push(own(expr.span, why));
        }
    }
}

/// The expressions of a ket's or bra's computed labels.
pub(crate) fn label_values(ast: &Ast, parts: List<KetPart>) -> impl Iterator<Item = ExprId> + '_ {
    ast.list(parts).iter().filter_map(|p| match *p {
        KetPart::Value(value) => Some(value),
        KetPart::Label(_) => None,
    })
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
/// preparations and `sample` runs are skipped: they run separately.
fn walk_block(
    ast: &Ast,
    block: BlockId,
    in_with: bool,
    with_pats: &mut Vec<PatId>,
    visit: &mut dyn FnMut(Node<'_>),
) {
    for &stmt in ast.list(ast.block(block).stmts) {
        let stmt = ast.stmt(stmt);
        match stmt.kind {
            StmtKind::Let { pat, init, .. } => {
                walk_expr(ast, init, in_with, with_pats, visit);
                if in_with {
                    with_pats.push(pat);
                }
            }
            StmtKind::Assign { place, value, .. } => {
                visit(Node::Assign(place, stmt.span));
                walk_expr(ast, place, in_with, with_pats, visit);
                walk_expr(ast, value, in_with, with_pats, visit);
            }
            StmtKind::Assert { cond, msg } => {
                walk_expr(ast, cond, in_with, with_pats, visit);
                if let Some(m) = msg {
                    walk_expr(ast, m, in_with, with_pats, visit);
                }
            }
            StmtKind::Return(Some(e)) | StmtKind::Expr { expr: e, .. } => {
                walk_expr(ast, e, in_with, with_pats, visit)
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }
}

fn walk_expr(
    ast: &Ast,
    id: ExprId,
    in_with: bool,
    with_pats: &mut Vec<PatId>,
    visit: &mut dyn FnMut(Node<'_>),
) {
    visit(Node::Expr(id, in_with.then_some(with_pats.as_slice())));
    if let ExprKind::With { with, body } = ast.expr(id).kind {
        let mark = with_pats.len();
        walk_block(ast, with, true, with_pats, visit);
        with_pats.truncate(mark);
        walk_block(ast, body, in_with, with_pats, visit);
        return;
    }
    let mut children: Vec<ExprId> = Vec::new();
    let mut blocks: Vec<BlockId> = Vec::new();
    match ast.expr(id).kind {
        ExprKind::Tuple(l) | ExprKind::Array(l) => children.extend(ast.list(l)),
        ExprKind::Str(parts) => children.extend(ast.list(parts).iter().filter_map(|p| match *p {
            StrPart::Value { expr, .. } => Some(expr),
            StrPart::Text(_) => None,
        })),
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
        ExprKind::BraTest { operand, parts, .. } => {
            children.extend(label_values(ast, parts));
            children.push(operand);
        }
        ExprKind::Ket { parts, .. } => children.extend(label_values(ast, parts)),
        // The preparation is undone, not run, so only its arguments are evaluated here.
        ExprKind::PrepTest { prep, operand } => {
            if let ExprKind::Call { args, .. } = ast.expr(prep).kind {
                children.extend(ast.list(args));
            }
            children.push(operand);
        }
        ExprKind::Expectation {
            observable,
            precision,
            ..
        } => children.extend([observable].into_iter().chain(precision)),
        ExprKind::Overlap {
            observable,
            precision,
            ..
        } => children.extend(observable.into_iter().chain(precision)),
        // The body is a run of its own.
        ExprKind::Sample { shots, .. } => children.push(shots),
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
        ExprKind::Match { scrutinee, arms } => {
            children.push(scrutinee);
            children.extend(ast.list(arms).iter().map(|a| a.body));
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
        _ => {}
    }
    for child in children {
        walk_expr(ast, child, in_with, with_pats, visit);
    }
    for block in blocks {
        walk_block(ast, block, in_with, with_pats, visit);
    }
}

// ---------------------------------------------------------------------------------------
// Field-level method lends (`spec/ownership.md` §5)
// ---------------------------------------------------------------------------------------

/// For each function, which fields of `self` it touches when it is a same-package struct
/// method taking `ref self` or `mut self`.
fn infer_method_fields(ast: &Ast, typed: &Typed) -> Vec<MethodFields> {
    let n = ast.all_fns().len();
    let mut direct: Vec<MethodFields> = vec![MethodFields::Whole; n];
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (index, decl) in ast.all_fns().iter().enumerate() {
        let Some(body) = decl.body else { continue };
        let info = &typed.fns[index];
        let Some(owner) = info.owner else { continue };
        if typed.types.structs[owner.index()].variants.is_some() {
            continue;
        }
        let Some(self_local) = info.self_local else {
            continue;
        };
        if !matches!(info.sig.receiver, Some(OwnKind::Ref | OwnKind::Mut)) {
            continue;
        }
        let mut collector = SelfFieldCollector {
            ast,
            typed,
            owner,
            self_local,
            module: typed.fn_modules[index],
            whole: false,
            fields: Vec::new(),
            calls: Vec::new(),
        };
        collector.block(body);
        if !collector.whole {
            direct[index] = MethodFields::Fields(collector.fields);
            edges[index] = collector.calls;
        }
    }

    // Propagate accessed fields across same-struct `self.other_method()` calls until fixpoint.
    let mut result = direct;
    loop {
        let mut changed = false;
        for f in 0..n {
            if matches!(result[f], MethodFields::Whole) || edges[f].is_empty() {
                continue;
            }
            let mut next = match &result[f] {
                MethodFields::Fields(list) => list.clone(),
                MethodFields::Whole => continue,
            };
            let mut promote_whole = false;
            for &g in &edges[f] {
                match &result[g] {
                    MethodFields::Whole => {
                        promote_whole = true;
                        break;
                    }
                    MethodFields::Fields(g_fields) => {
                        for &(idx, m) in g_fields {
                            record_field_access(&mut next, idx, m);
                        }
                    }
                }
            }
            let updated = if promote_whole {
                MethodFields::Whole
            } else {
                MethodFields::Fields(next)
            };
            if updated != result[f] {
                result[f] = updated;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    result
}

fn record_field_access(list: &mut Vec<(u32, bool)>, idx: u32, mutable: bool) {
    if let Some(entry) = list.iter_mut().find(|(i, _)| *i == idx) {
        entry.1 |= mutable;
    } else {
        list.push((idx, mutable));
    }
}

struct SelfFieldCollector<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    owner: crate::ty::StructId,
    self_local: LocalId,
    module: ModuleId,
    whole: bool,
    fields: Vec<(u32, bool)>,
    calls: Vec<usize>,
}

impl SelfFieldCollector<'_> {
    fn is_self(&self, id: ExprId) -> bool {
        matches!(self.ast.expr(id).kind, ExprKind::Name(_))
            && self.typed.res[id.index()] == Res::Local(self.self_local)
    }

    fn field_index(&self, name: Symbol) -> Option<u32> {
        self.typed.types.structs[self.owner.index()]
            .fields
            .iter()
            .position(|(n, _)| *n == name)
            .map(|i| i as u32)
    }

    fn block(&mut self, block: BlockId) {
        for &stmt in self.ast.list(self.ast.block(block).stmts) {
            if self.whole {
                return;
            }
            match self.ast.stmt(stmt).kind {
                StmtKind::Let { init, .. } => self.expr(init, false),
                StmtKind::Assign { place, value, .. } => {
                    self.expr(value, false);
                    self.expr(place, true);
                }
                StmtKind::Assert { cond, msg } => {
                    self.expr(cond, false);
                    if let Some(m) = msg {
                        self.expr(m, false);
                    }
                }
                StmtKind::Return(Some(e)) | StmtKind::Expr { expr: e, .. } => self.expr(e, false),
                StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
            }
        }
    }

    fn expr(&mut self, id: ExprId, mut mutable: bool) {
        if self.whole {
            return;
        }
        if matches!(
            self.typed.adjust[id.index()],
            Adjust::Lend { mutable: true }
        ) {
            mutable = true;
        }
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Name(_) => {
                if self.typed.res[id.index()] == Res::Local(self.self_local) {
                    self.whole = true;
                }
            }
            ExprKind::Field { base, name } => {
                if self.is_self(base) {
                    if let Some(idx) = self.field_index(name) {
                        record_field_access(&mut self.fields, idx, mutable);
                    } else {
                        self.whole = true;
                    }
                } else {
                    self.expr(base, mutable);
                }
            }
            ExprKind::TupleIndex { base, .. } => self.expr(base, mutable),
            ExprKind::Index { base, index } => {
                self.expr(base, mutable);
                self.expr(index, false);
            }
            ExprKind::Own { kind, place } => {
                self.expr(place, mutable || kind == OwnKind::Mut);
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                for &arg in ast.list(args) {
                    self.expr(arg, false);
                }
                if self.is_self(receiver) {
                    let same_struct_method = match self.typed.callees[id.index()] {
                        Some(Callee::Fn(g)) => {
                            let g_info = &self.typed.fns[g.index()];
                            (g_info.owner == Some(self.owner)
                                && matches!(g_info.sig.receiver, Some(OwnKind::Ref | OwnKind::Mut))
                                && !other_package(
                                    ast,
                                    self.typed.fn_modules[g.index()],
                                    self.module,
                                ))
                            .then_some(g.index())
                        }
                        _ => None,
                    };
                    if let Some(g) = same_struct_method {
                        if !self.calls.contains(&g) {
                            self.calls.push(g);
                        }
                    } else {
                        self.whole = true;
                    }
                } else {
                    let recv_mut = self.typed.receivers[id.index()] == Some(OwnKind::Mut);
                    self.expr(receiver, recv_mut);
                }
            }
            ExprKind::Binary { lhs, rhs, .. } if self.typed.is_operator_call(ast, id) => {
                let recv_mut = self.typed.receivers[id.index()] == Some(OwnKind::Mut);
                self.expr(lhs, recv_mut);
                self.expr(rhs, false);
            }
            ExprKind::Unary { operand, .. } if self.typed.is_operator_call(ast, id) => {
                let recv_mut = self.typed.receivers[id.index()] == Some(OwnKind::Mut);
                self.expr(operand, recv_mut);
            }
            ExprKind::Call { args, .. } => {
                for &arg in ast.list(args) {
                    self.expr(arg, false);
                }
            }
            ExprKind::Tuple(l) | ExprKind::Array(l) => {
                for &e in ast.list(l) {
                    self.expr(e, false);
                }
            }
            ExprKind::ArrayRepeat { value, count } => {
                self.expr(value, false);
                self.expr(count, false);
            }
            ExprKind::StructLit { fields, .. } => {
                for f in ast.list(fields) {
                    if let Some(v) = f.value {
                        self.expr(v, false);
                    }
                }
            }
            ExprKind::Str(parts) => {
                for p in ast.list(parts) {
                    if let StrPart::Value { expr, .. } = *p {
                        self.expr(expr, false);
                    }
                }
            }
            ExprKind::Unary { operand, .. } | ExprKind::Adj(operand) => self.expr(operand, false),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, false);
                self.expr(rhs, false);
            }
            ExprKind::BraTest { operand, parts, .. } => {
                for e in label_values(ast, parts) {
                    self.expr(e, false);
                }
                self.expr(operand, false);
            }
            ExprKind::Ket { parts, .. } => {
                for e in label_values(ast, parts) {
                    self.expr(e, false);
                }
            }
            ExprKind::PrepTest { prep, operand } => {
                if let ExprKind::Call { args, .. } = ast.expr(prep).kind {
                    for &a in ast.list(args) {
                        self.expr(a, false);
                    }
                }
                self.expr(operand, false);
            }
            ExprKind::Expectation {
                observable,
                precision,
                ..
            } => {
                self.expr(observable, false);
                if let Some(p) = precision {
                    self.expr(p, false);
                }
            }
            ExprKind::Overlap {
                observable,
                precision,
                ..
            } => {
                if let Some(o) = observable {
                    self.expr(o, false);
                }
                if let Some(p) = precision {
                    self.expr(p, false);
                }
            }
            ExprKind::Sample { shots, .. } => self.expr(shots, false),
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block(b),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond, false);
                self.block(then);
                if let Some(o) = otherwise {
                    self.expr(o, false);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee, false);
                for arm in ast.list(arms) {
                    self.expr(arm.body, false);
                }
            }
            ExprKind::While { cond, body } => {
                self.expr(cond, false);
                self.block(body);
            }
            ExprKind::For {
                start, end, body, ..
            } => {
                self.expr(start, false);
                self.expr(end, false);
                self.block(body);
            }
            ExprKind::Ctrl { controls, body } => {
                for &c in ast.list(controls) {
                    self.expr(c, false);
                }
                self.block(body);
            }
            ExprKind::With { with, body } => {
                self.block(with);
                self.block(body);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------
// Package-wide return-borrow inference (`spec/ownership.md` §5)
// ---------------------------------------------------------------------------------------

/// For every function, which parameters (and field paths) its returned reference(s) may
/// borrow from: taken verbatim from [`FnInfo::return_borrows`] when explicit or elided, and
/// inferred from the body otherwise.
fn infer_return_borrows(ast: &Ast, typed: &Typed) -> Vec<Vec<BorrowSource>> {
    let n = ast.all_fns().len();
    let mut result: Vec<Vec<BorrowSource>> = Vec::with_capacity(n);
    let mut infer: Vec<usize> = Vec::new();
    for i in 0..n {
        if let Some(fixed) = &typed.fns[i].return_borrows {
            result.push(fixed.clone());
        } else {
            result.push(Vec::new());
            if ast.all_fns()[i].body.is_some() && typed.types.has_ref(typed.fns[i].sig.ret) {
                infer.push(i);
            }
        }
    }
    if infer.is_empty() {
        return result;
    }

    loop {
        let mut changed = false;
        for &index in &infer {
            let body = ast.all_fns()[index].body.expect("filtered");
            let found = infer_fn_return_borrows(ast, typed, index, body, &result);
            if found != result[index] {
                result[index] = found;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    result
}

fn infer_fn_return_borrows(
    ast: &Ast,
    typed: &Typed,
    fn_index: usize,
    body: BlockId,
    known: &[Vec<BorrowSource>],
) -> Vec<BorrowSource> {
    let info = &typed.fns[fn_index];
    let mut env: FxHashMap<LocalId, Vec<BorrowSource>> = FxHashMap::default();
    if let Some(self_local) = info.self_local {
        env.insert(
            self_local,
            vec![BorrowSource {
                root: BorrowRoot::Receiver,
                fields: Vec::new(),
            }],
        );
    }
    for (i, &param) in info.params.iter().enumerate() {
        if typed.types.has_ref(typed.local(param).ty) {
            env.insert(
                param,
                vec![BorrowSource {
                    root: BorrowRoot::Param(i),
                    fields: Vec::new(),
                }],
            );
        }
    }
    let mut returned: Vec<BorrowSource> = Vec::new();
    let mut walker = BorrowInfer {
        ast,
        typed,
        fn_index,
        known,
        env,
        returned: Vec::new(),
    };
    // Run at most twice so loop-carried reassignments converge.
    for _ in 0..2 {
        let before = walker.env.len() + walker.returned.len();
        let tail = walker.block(body);
        merge_sources(&mut walker.returned, &tail);
        if walker.env.len() + walker.returned.len() == before {
            break;
        }
    }
    returned.extend(walker.returned);
    returned
}

fn merge_sources(dst: &mut Vec<BorrowSource>, src: &[BorrowSource]) {
    for s in src {
        if !dst.contains(s) {
            dst.push(s.clone());
        }
    }
}

struct BorrowInfer<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    fn_index: usize,
    known: &'a [Vec<BorrowSource>],
    env: FxHashMap<LocalId, Vec<BorrowSource>>,
    returned: Vec<BorrowSource>,
}

impl BorrowInfer<'_> {
    fn param_root(&self, local: LocalId) -> Option<BorrowRoot> {
        let info = &self.typed.fns[self.fn_index];
        if info.self_local == Some(local) {
            return Some(BorrowRoot::Receiver);
        }
        info.params
            .iter()
            .position(|&p| p == local)
            .filter(|&i| self.typed.types.has_ref(info.sig.params[i]))
            .map(BorrowRoot::Param)
    }

    /// If `id` is a place path rooted directly at a parameter (e.g. `self.a.b` or `p.a`),
    /// returns that parameter and its leading struct field indices.
    fn direct_param_place(&self, id: ExprId) -> Option<BorrowSource> {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Own {
                kind: OwnKind::Ref | OwnKind::Mut,
                place,
            } => self.direct_param_place(place),
            ExprKind::Name(_) => match self.typed.res[id.index()] {
                Res::Local(l) => Some(BorrowSource {
                    root: self.param_root(l)?,
                    fields: Vec::new(),
                }),
                _ => None,
            },
            ExprKind::Field { base, name } => {
                let mut src = self.direct_param_place(base)?;
                let base_ty = self.typed.types.deref(self.typed.ty(base));
                if let TyKind::Struct(s) = *self.typed.types.kind(base_ty)
                    && let Some(idx) = self.typed.types.structs[s.index()]
                        .fields
                        .iter()
                        .position(|(n, _)| *n == name)
                {
                    src.fields.push(idx as u32);
                }
                Some(src)
            }
            ExprKind::TupleIndex { base, .. } | ExprKind::Index { base, .. } => {
                self.direct_param_place(base)
            }
            _ => None,
        }
    }

    fn bind_pat(&mut self, pat: PatId, sources: &[BorrowSource]) {
        if sources.is_empty() {
            return;
        }
        match self.ast.pat(pat).kind {
            PatKind::Name(_) => {
                if let Some(local) = self.typed.pat_locals[pat.index()]
                    && self.typed.types.has_ref(self.typed.local(local).ty)
                {
                    merge_sources(self.env.entry(local).or_default(), sources);
                }
            }
            PatKind::Tuple(pats) | PatKind::Array(pats) => {
                for &p in self.ast.list(pats) {
                    self.bind_pat(p, sources);
                }
            }
            PatKind::Variant { data, .. } => match data {
                PatData::None => {}
                PatData::Tuple(pats) => {
                    for &p in self.ast.list(pats) {
                        self.bind_pat(p, sources);
                    }
                }
                PatData::Fields { fields, .. } => {
                    for f in self.ast.list(fields) {
                        if let Some(p) = f.pat {
                            self.bind_pat(p, sources);
                        }
                    }
                }
            },
            PatKind::Wildcard | PatKind::Literal(_) => {}
        }
    }

    fn block(&mut self, block: BlockId) -> Vec<BorrowSource> {
        let stmts = self.ast.list(self.ast.block(block).stmts);
        let mut tail = Vec::new();
        for (i, &stmt) in stmts.iter().enumerate() {
            match self.ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    tail = self.expr(expr);
                }
                StmtKind::Let { pat, init, .. } => {
                    let s = self.expr(init);
                    self.bind_pat(pat, &s);
                }
                StmtKind::Assign { place, value, .. } => {
                    let s = self.expr(value);
                    self.expr(place);
                    if let Some(root) = root_local(self.ast, self.typed, place)
                        && self.typed.types.has_ref(self.typed.local(root).ty)
                    {
                        merge_sources(self.env.entry(root).or_default(), &s);
                    }
                }
                StmtKind::Return(Some(e)) => {
                    let s = self.expr(e);
                    merge_sources(&mut self.returned, &s);
                }
                StmtKind::Assert { cond, msg } => {
                    self.expr(cond);
                    if let Some(m) = msg {
                        self.expr(m);
                    }
                }
                StmtKind::Expr { expr: e, .. } => {
                    self.expr(e);
                }
                StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
            }
        }
        tail
    }

    fn expr(&mut self, id: ExprId) -> Vec<BorrowSource> {
        let ast = self.ast;
        let ty = self.typed.ty(id);
        match ast.expr(id).kind {
            ExprKind::Name(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. }
            | ExprKind::Own { .. } => {
                if let ExprKind::Index { base, index } = ast.expr(id).kind {
                    self.expr(index);
                    let _ = base;
                }
                if let Some(src) = self.direct_param_place(id) {
                    return vec![src];
                }
                let place_expr = match ast.expr(id).kind {
                    ExprKind::Own { place, .. } => place,
                    _ => id,
                };
                if let Some(root) = root_local(ast, self.typed, place_expr)
                    && let Some(sources) = self.env.get(&root)
                {
                    return sources.clone();
                }
                Vec::new()
            }
            ExprKind::Tuple(l) | ExprKind::Array(l) => {
                let mut out = Vec::new();
                for &e in ast.list(l) {
                    let s = self.expr(e);
                    if self.typed.types.has_ref(ty) {
                        merge_sources(&mut out, &s);
                    }
                }
                out
            }
            ExprKind::StructLit { fields, .. } => {
                let mut out = Vec::new();
                for f in ast.list(fields) {
                    let s = match f.value {
                        Some(v) => self.expr(v),
                        None => self
                            .env
                            .iter()
                            .filter(|&(&l, _)| self.typed.local(l).name == f.name)
                            .flat_map(|(_, s)| s.iter().cloned())
                            .collect(),
                    };
                    if self.typed.types.has_ref(ty) {
                        merge_sources(&mut out, &s);
                    }
                }
                out
            }
            ExprKind::Call { args, .. } => self.call(id, None, ast.list(args)),
            ExprKind::MethodCall { receiver, args, .. } => {
                self.call(id, Some(receiver), ast.list(args))
            }
            ExprKind::Binary { lhs, rhs, .. } if self.typed.is_operator_call(ast, id) => {
                self.call(id, Some(lhs), &[rhs])
            }
            ExprKind::Unary { operand, .. } if self.typed.is_operator_call(ast, id) => {
                self.call(id, Some(operand), &[])
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond);
                let mut out = self.block(then);
                if let Some(o) = otherwise {
                    let s = self.expr(o);
                    merge_sources(&mut out, &s);
                }
                if self.typed.types.has_ref(ty) {
                    out
                } else {
                    Vec::new()
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let scr = self.expr(scrutinee);
                let mut out = Vec::new();
                for arm in ast.list(arms) {
                    self.bind_pat(arm.pat, &scr);
                    let s = self.expr(arm.body);
                    if self.typed.types.has_ref(ty) {
                        merge_sources(&mut out, &s);
                    }
                }
                out
            }
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block(b),
            ExprKind::With { with, body } => {
                self.block(with);
                self.block(body)
            }
            ExprKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
                Vec::new()
            }
            ExprKind::For {
                start, end, body, ..
            } => {
                self.expr(start);
                self.expr(end);
                self.block(body);
                Vec::new()
            }
            ExprKind::Ctrl { controls, body } => {
                for &c in ast.list(controls) {
                    self.expr(c);
                }
                self.block(body);
                Vec::new()
            }
            ExprKind::Unary { operand, .. } | ExprKind::Adj(operand) => self.expr(operand),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn call(&mut self, id: ExprId, receiver: Option<ExprId>, args: &[ExprId]) -> Vec<BorrowSource> {
        let recv_src = receiver.map(|r| (r, self.expr(r)));
        let arg_srcs: Vec<(ExprId, Vec<BorrowSource>)> =
            args.iter().map(|&a| (a, self.expr(a))).collect();
        let ret = self.typed.ty(id);
        if !self.typed.types.has_ref(ret) {
            return Vec::new();
        }
        let Some(callee) = self.typed.callees[id.index()] else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match callee {
            Callee::Variant(..) => {
                for (_, s) in &arg_srcs {
                    merge_sources(&mut out, s);
                }
            }
            Callee::Builtin(_) | Callee::Convert(_) | Callee::TryConvert(_) | Callee::FnPtr => {
                if let Some((_, s)) = &recv_src {
                    merge_sources(&mut out, s);
                } else {
                    for (_, s) in &arg_srcs {
                        merge_sources(&mut out, s);
                    }
                }
            }
            Callee::Fn(g) => {
                let caller_mod = self.typed.fn_modules[self.fn_index];
                let callee_mod = self.typed.fn_modules[g.index()];
                let g_sources = if other_package(self.ast, callee_mod, caller_mod) {
                    self.typed.fns[g.index()]
                        .return_borrows
                        .as_deref()
                        .unwrap_or(&[])
                } else {
                    &self.known[g.index()]
                };
                for src in g_sources {
                    let target = match src.root {
                        BorrowRoot::Receiver => recv_src.as_ref(),
                        BorrowRoot::Param(k) => arg_srcs.get(k),
                    };
                    let Some((expr, sources)) = target else {
                        continue;
                    };
                    let direct = self.direct_param_place(*expr);
                    for s in sources {
                        let mut mapped = s.clone();
                        if !src.fields.is_empty() && direct.as_ref() == Some(s) {
                            mapped.fields.extend_from_slice(&src.fields);
                        }
                        if !out.contains(&mapped) {
                            out.push(mapped);
                        }
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// Polonius-style backward liveness for non-lexical lifetimes (`spec/ownership.md` §5)
// ---------------------------------------------------------------------------------------

/// Compact bitset over the reference-holding locals in one function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LiveSet {
    low: u64,
    high: Vec<u64>,
}

impl LiveSet {
    fn clear(&mut self) {
        self.low = 0;
        self.high.clear();
    }

    fn insert(&mut self, bit: usize) {
        if bit < 64 {
            self.low |= 1u64 << bit;
        } else {
            let word = (bit - 64) / 64;
            if self.high.len() <= word {
                self.high.resize(word + 1, 0);
            }
            self.high[word] |= 1u64 << ((bit - 64) % 64);
        }
    }

    fn remove(&mut self, bit: usize) {
        if bit < 64 {
            self.low &= !(1u64 << bit);
        } else {
            let word = (bit - 64) / 64;
            if let Some(slot) = self.high.get_mut(word) {
                *slot &= !(1u64 << ((bit - 64) % 64));
            }
        }
    }

    fn contains(&self, bit: usize) -> bool {
        if bit < 64 {
            (self.low & (1u64 << bit)) != 0
        } else {
            let word = (bit - 64) / 64;
            self.high
                .get(word)
                .is_some_and(|w| (*w & (1u64 << ((bit - 64) % 64))) != 0)
        }
    }

    fn union_with(&mut self, other: &LiveSet) {
        self.low |= other.low;
        if other.high.len() > self.high.len() {
            self.high.resize(other.high.len(), 0);
        }
        for (a, &b) in self.high.iter_mut().zip(&other.high) {
            *a |= b;
        }
    }
}

/// Backward liveness of reference-holding local variables in one function.
struct Liveness {
    /// Maps each reference-holding local in the function to its bit index.
    bits: FxHashMap<LocalId, usize>,
    /// Reference-holding locals live strictly after each expression.
    after_expr: FxHashMap<ExprId, LiveSet>,
    /// Reference-holding locals live strictly after each statement.
    after_stmt: FxHashMap<StmtId, LiveSet>,
    /// Reference-holding locals live strictly after a shorthand struct field `(struct_lit, idx)`.
    after_shorthand: FxHashMap<(ExprId, usize), LiveSet>,
}

impl Liveness {
    fn contains(&self, set: &LiveSet, local: LocalId) -> bool {
        self.bits.get(&local).is_none_or(|&bit| set.contains(bit))
    }
}

/// Computes backward liveness for `body`, or returns `None` when `body` declares no
/// reference-holding locals (fast path: zero overhead for functions without local borrows).
fn compute_liveness(ast: &Ast, typed: &Typed, f: FnId, body: BlockId) -> Option<Liveness> {
    let mut bits: FxHashMap<LocalId, usize> = FxHashMap::default();
    let mut by_name: FxHashMap<Symbol, Vec<usize>> = FxHashMap::default();
    let collect_pat = |pat: PatId,
                       bits: &mut FxHashMap<LocalId, usize>,
                       by_name: &mut FxHashMap<Symbol, Vec<usize>>| {
        let mut stack = vec![pat];
        while let Some(p) = stack.pop() {
            match ast.pat(p).kind {
                PatKind::Name(_) => {
                    if let Some(l) = typed.pat_locals[p.index()]
                        && typed.types.has_ref(typed.local(l).ty)
                    {
                        let idx = bits.len();
                        if bits.entry(l).or_insert(idx) == &idx {
                            by_name.entry(typed.local(l).name).or_default().push(idx);
                        }
                    }
                }
                PatKind::Tuple(ps) | PatKind::Array(ps) => stack.extend(ast.list(ps)),
                PatKind::Variant { data, .. } => match data {
                    PatData::None => {}
                    PatData::Tuple(ps) => stack.extend(ast.list(ps)),
                    PatData::Fields { fields, .. } => {
                        stack.extend(ast.list(fields).iter().filter_map(|f| f.pat));
                    }
                },
                PatKind::Wildcard | PatKind::Literal(_) => {}
            }
        }
    };

    // Scan `body` for any bound reference-holding locals (`let` or `match` patterns).
    let mut blocks = vec![body];
    let mut exprs = Vec::new();
    while !blocks.is_empty() || !exprs.is_empty() {
        while let Some(b) = blocks.pop() {
            for &s in ast.list(ast.block(b).stmts) {
                match ast.stmt(s).kind {
                    StmtKind::Let { pat, init, .. } => {
                        collect_pat(pat, &mut bits, &mut by_name);
                        exprs.push(init);
                    }
                    StmtKind::Assign { place, value, .. } => {
                        exprs.push(place);
                        exprs.push(value);
                    }
                    StmtKind::Assert { cond, msg } => {
                        exprs.push(cond);
                        if let Some(m) = msg {
                            exprs.push(m);
                        }
                    }
                    StmtKind::Return(Some(e)) | StmtKind::Expr { expr: e, .. } => exprs.push(e),
                    StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
                }
            }
        }
        while let Some(e) = exprs.pop() {
            match ast.expr(e).kind {
                ExprKind::Match { scrutinee, arms } => {
                    exprs.push(scrutinee);
                    for arm in ast.list(arms) {
                        collect_pat(arm.pat, &mut bits, &mut by_name);
                        exprs.push(arm.body);
                    }
                }
                ExprKind::Block(b) | ExprKind::Unsafe(b) => blocks.push(b),
                ExprKind::If {
                    cond,
                    then,
                    otherwise,
                } => {
                    exprs.push(cond);
                    blocks.push(then);
                    exprs.extend(otherwise);
                }
                ExprKind::While { cond, body } => {
                    exprs.push(cond);
                    blocks.push(body);
                }
                ExprKind::For {
                    start, end, body, ..
                } => {
                    exprs.push(start);
                    exprs.push(end);
                    blocks.push(body);
                }
                ExprKind::Ctrl { controls, body } => {
                    exprs.extend(ast.list(controls));
                    blocks.push(body);
                }
                ExprKind::With { with, body } => {
                    blocks.push(with);
                    blocks.push(body);
                }
                ExprKind::Tuple(l) | ExprKind::Array(l) => exprs.extend(ast.list(l)),
                ExprKind::ArrayRepeat { value, count } => {
                    exprs.push(value);
                    exprs.push(count);
                }
                ExprKind::StructLit { fields, .. } => {
                    exprs.extend(ast.list(fields).iter().filter_map(|f| f.value));
                }
                ExprKind::Field { base, .. }
                | ExprKind::TupleIndex { base, .. }
                | ExprKind::Unary { operand: base, .. }
                | ExprKind::Own { place: base, .. }
                | ExprKind::Adj(base) => exprs.push(base),
                ExprKind::Index { base, index } => {
                    exprs.push(base);
                    exprs.push(index);
                }
                ExprKind::Call { args, .. } => exprs.extend(ast.list(args)),
                ExprKind::MethodCall { receiver, args, .. } => {
                    exprs.push(receiver);
                    exprs.extend(ast.list(args));
                }
                ExprKind::Binary { lhs, rhs, .. } => {
                    exprs.push(lhs);
                    exprs.push(rhs);
                }
                ExprKind::Str(parts) => {
                    for p in ast.list(parts) {
                        if let StrPart::Value { expr, .. } = *p {
                            exprs.push(expr);
                        }
                    }
                }
                ExprKind::BraTest { operand, parts, .. } => {
                    exprs.push(operand);
                    exprs.extend(label_values(ast, parts));
                }
                ExprKind::Ket { parts, .. } => exprs.extend(label_values(ast, parts)),
                ExprKind::PrepTest { prep, operand } => {
                    if let ExprKind::Call { args, .. } = ast.expr(prep).kind {
                        exprs.extend(ast.list(args));
                    }
                    exprs.push(operand);
                }
                ExprKind::Sample { shots, .. } => exprs.push(shots),
                _ => {}
            }
        }
    }

    if bits.is_empty() {
        return None;
    }
    let _ = f;
    let mut pass = LivenessPass {
        ast,
        typed,
        bits,
        by_name,
        after_expr: FxHashMap::default(),
        after_stmt: FxHashMap::default(),
        after_shorthand: FxHashMap::default(),
        loops: Vec::new(),
    };
    let mut live = LiveSet::default();
    pass.block(body, &mut live);
    Some(Liveness {
        bits: pass.bits,
        after_expr: pass.after_expr,
        after_stmt: pass.after_stmt,
        after_shorthand: pass.after_shorthand,
    })
}

struct LivenessPass<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    bits: FxHashMap<LocalId, usize>,
    by_name: FxHashMap<Symbol, Vec<usize>>,
    after_expr: FxHashMap<ExprId, LiveSet>,
    after_stmt: FxHashMap<StmtId, LiveSet>,
    after_shorthand: FxHashMap<(ExprId, usize), LiveSet>,
    /// Stack of `(loop_head_live, loop_exit_live)`.
    loops: Vec<(LiveSet, LiveSet)>,
}

impl LivenessPass<'_> {
    fn kill_pat(&self, pat: PatId, live: &mut LiveSet) {
        match self.ast.pat(pat).kind {
            PatKind::Name(_) => {
                if let Some(l) = self.typed.pat_locals[pat.index()]
                    && let Some(&bit) = self.bits.get(&l)
                {
                    live.remove(bit);
                }
            }
            PatKind::Tuple(ps) | PatKind::Array(ps) => {
                for &p in self.ast.list(ps) {
                    self.kill_pat(p, live);
                }
            }
            PatKind::Variant { data, .. } => match data {
                PatData::None => {}
                PatData::Tuple(ps) => {
                    for &p in self.ast.list(ps) {
                        self.kill_pat(p, live);
                    }
                }
                PatData::Fields { fields, .. } => {
                    for f in self.ast.list(fields) {
                        if let Some(p) = f.pat {
                            self.kill_pat(p, live);
                        }
                    }
                }
            },
            PatKind::Wildcard | PatKind::Literal(_) => {}
        }
    }

    fn place_subexprs_rev(&mut self, id: ExprId, live: &mut LiveSet) {
        match self.ast.expr(id).kind {
            ExprKind::Field { base, .. } | ExprKind::TupleIndex { base, .. } => {
                self.place_subexprs_rev(base, live)
            }
            ExprKind::Index { base, index } => {
                self.expr(index, live);
                self.place_subexprs_rev(base, live);
            }
            _ => {}
        }
    }

    fn block(&mut self, block: BlockId, live: &mut LiveSet) {
        let stmts = self.ast.list(self.ast.block(block).stmts);
        for &stmt in stmts.iter().rev() {
            self.after_stmt.insert(stmt, live.clone());
            match self.ast.stmt(stmt).kind {
                StmtKind::Return(opt) => {
                    live.clear();
                    if let Some(e) = opt {
                        self.expr(e, live);
                    }
                }
                StmtKind::Break => {
                    if let Some((_, exit)) = self.loops.last() {
                        *live = exit.clone();
                    } else {
                        live.clear();
                    }
                }
                StmtKind::Continue => {
                    if let Some((head, _)) = self.loops.last() {
                        *live = head.clone();
                    } else {
                        live.clear();
                    }
                }
                StmtKind::Let { pat, init, .. } => {
                    self.kill_pat(pat, live);
                    self.expr(init, live);
                }
                StmtKind::Assign { op, place, value } => {
                    if self.typed.callees[place.index()].is_some() {
                        // Operator `a += b`: `value` runs first, then `place` is lent.
                        if let Some(root) = root_local(self.ast, self.typed, place)
                            && let Some(&bit) = self.bits.get(&root)
                        {
                            live.insert(bit);
                        }
                        self.place_subexprs_rev(place, live);
                        self.expr(value, live);
                    } else {
                        if op == AssignOp::Assign
                            && matches!(self.ast.expr(place).kind, ExprKind::Name(_))
                            && let Res::Local(l) = self.typed.res[place.index()]
                        {
                            if let Some(&bit) = self.bits.get(&l) {
                                live.remove(bit);
                            }
                        } else {
                            self.expr(place, live);
                        }
                        self.expr(value, live);
                    }
                }
                StmtKind::Assert { cond, msg } => {
                    if let Some(m) = msg {
                        self.expr(m, live);
                    }
                    self.expr(cond, live);
                }
                StmtKind::Expr { expr: e, .. } => {
                    self.expr(e, live);
                }
            }
        }
    }

    fn expr(&mut self, id: ExprId, live: &mut LiveSet) {
        self.after_expr.insert(id, live.clone());
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Name(_) => {
                if let Res::Local(l) = self.typed.res[id.index()]
                    && let Some(&bit) = self.bits.get(&l)
                {
                    live.insert(bit);
                }
            }
            ExprKind::Field { base, .. }
            | ExprKind::TupleIndex { base, .. }
            | ExprKind::Unary { operand: base, .. }
            | ExprKind::Own { place: base, .. }
            | ExprKind::Adj(base) => self.expr(base, live),
            ExprKind::Index { base, index } => {
                self.expr(index, live);
                self.expr(base, live);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(rhs, live);
                self.expr(lhs, live);
            }
            ExprKind::Tuple(l) | ExprKind::Array(l) => {
                for &e in ast.list(l).iter().rev() {
                    self.expr(e, live);
                }
            }
            ExprKind::ArrayRepeat { value, count } => {
                self.expr(count, live);
                self.expr(value, live);
            }
            ExprKind::Str(parts) => {
                for p in ast.list(parts).iter().rev() {
                    if let StrPart::Value { expr, .. } = *p {
                        self.expr(expr, live);
                    }
                }
            }
            ExprKind::Ket { parts, .. } => {
                for p in ast.list(parts).iter().rev() {
                    if let KetPart::Value(e) = *p {
                        self.expr(e, live);
                    }
                }
            }
            ExprKind::BraTest { operand, parts, .. } => {
                self.expr(operand, live);
                for p in ast.list(parts).iter().rev() {
                    if let KetPart::Value(e) = *p {
                        self.expr(e, live);
                    }
                }
            }
            ExprKind::PrepTest { prep, operand } => {
                self.expr(operand, live);
                if let ExprKind::Call { args, .. } = ast.expr(prep).kind {
                    for &a in ast.list(args).iter().rev() {
                        self.expr(a, live);
                    }
                }
            }
            ExprKind::StructLit { fields, .. } => {
                for (idx, f) in ast.list(fields).iter().enumerate().rev() {
                    match f.value {
                        Some(v) => self.expr(v, live),
                        None => {
                            self.after_shorthand.insert((id, idx), live.clone());
                            if let Some(bs) = self.by_name.get(&f.name) {
                                for &bit in bs {
                                    live.insert(bit);
                                }
                            }
                        }
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                for &a in ast.list(args).iter().rev() {
                    self.expr(a, live);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                let two_phase = is_place_expr(ast, receiver)
                    && matches!(
                        self.typed.receivers[id.index()],
                        Some(OwnKind::Ref | OwnKind::Mut)
                    );
                if two_phase {
                    // Forward order: place_subexprs(receiver), args, lend(receiver).
                    if let Some(root) = root_local(ast, self.typed, receiver)
                        && let Some(&bit) = self.bits.get(&root)
                    {
                        live.insert(bit);
                    }
                    for &a in ast.list(args).iter().rev() {
                        self.expr(a, live);
                    }
                    self.place_subexprs_rev(receiver, live);
                } else {
                    for &a in ast.list(args).iter().rev() {
                        self.expr(a, live);
                    }
                    self.expr(receiver, live);
                }
            }
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block(b, live),
            ExprKind::With { with, body } => {
                self.block(body, live);
                self.block(with, live);
            }
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                let after = live.clone();
                let mut then_live = after.clone();
                self.block(then, &mut then_live);
                let mut else_live = after;
                if let Some(o) = otherwise {
                    self.expr(o, &mut else_live);
                }
                then_live.union_with(&else_live);
                *live = then_live;
                self.expr(cond, live);
            }
            ExprKind::Match { scrutinee, arms } => {
                let after = live.clone();
                let mut joined = LiveSet::default();
                for arm in ast.list(arms) {
                    let mut arm_live = after.clone();
                    self.expr(arm.body, &mut arm_live);
                    self.kill_pat(arm.pat, &mut arm_live);
                    joined.union_with(&arm_live);
                }
                *live = joined;
                self.expr(scrutinee, live);
            }
            ExprKind::While { cond, body } => {
                let exit = live.clone();
                let mut head = exit.clone();
                let limit = self.bits.len() + 2;
                for _ in 0..limit {
                    self.loops.push((head.clone(), exit.clone()));
                    let mut body_in = head.clone();
                    self.block(body, &mut body_in);
                    self.loops.pop();
                    let mut new_head = exit.clone();
                    new_head.union_with(&body_in);
                    self.expr(cond, &mut new_head);
                    if new_head == head {
                        break;
                    }
                    head = new_head;
                }
                *live = head;
            }
            ExprKind::For {
                start, end, body, ..
            } => {
                let exit = live.clone();
                let mut head = exit.clone();
                let limit = self.bits.len() + 2;
                for _ in 0..limit {
                    self.loops.push((head.clone(), exit.clone()));
                    let mut body_in = head.clone();
                    self.block(body, &mut body_in);
                    self.loops.pop();
                    let mut new_head = exit.clone();
                    new_head.union_with(&body_in);
                    if new_head == head {
                        break;
                    }
                    head = new_head;
                }
                *live = head;
                self.expr(end, live);
                self.expr(start, live);
            }
            ExprKind::Ctrl { controls, body } => {
                self.block(body, live);
                for &c in ast.list(controls).iter().rev() {
                    self.expr(c, live);
                }
            }
            ExprKind::Sample { shots, .. } => self.expr(shots, live),
            ExprKind::Expectation {
                observable,
                precision,
                ..
            } => {
                if let Some(p) = precision {
                    self.expr(p, live);
                }
                self.expr(observable, live);
            }
            ExprKind::Overlap {
                observable,
                precision,
                ..
            } => {
                if let Some(p) = precision {
                    self.expr(p, live);
                }
                if let Some(o) = observable {
                    self.expr(o, live);
                }
            }
            _ => {}
        }
    }
}

fn is_place_expr(ast: &Ast, id: ExprId) -> bool {
    matches!(
        ast.expr(id).kind,
        ExprKind::Name(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. }
    )
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

    /// Checks `f`, inferring `Copy` for its inferable type parameters where copying or
    /// dropping their values is all that stands between the body and no errors: each such
    /// parameter goes into `needs`, with where the body first needs it.
    fn function_inferring(&mut self, f: FnId, body: BlockId, needs: &mut FxHashMap<Ty, Span>) {
        let typed = self.typed;
        let own: Vec<Ty> = typed.fns[f.index()]
            .generics
            .iter()
            .copied()
            .filter(|t| typed.inferable.get(t) == Some(&f))
            .collect();
        let start = self.diagnostics.len();
        self.assume_copy.clear();
        self.function(f, body);
        let errors = |d: &[Diagnostic]| d.iter().filter(|d| d.severity == Severity::Error).count();
        if own.is_empty() || errors(&self.diagnostics[start..]) == 0 {
            return;
        }
        let linear = self.diagnostics.split_off(start);
        self.assume_copy = own.clone();
        self.function(f, body);
        let copy = self.diagnostics.split_off(start);
        if errors(&copy) >= errors(&linear) {
            self.assume_copy.clear();
            self.diagnostics.extend(linear);
            return;
        }
        // Only the parameters some copy or drop needs: without one, the errors it removed
        // come back, and the first of them is where it is needed.
        let mut needed = own.clone();
        for &p in &own {
            self.assume_copy = needed.iter().copied().filter(|&q| q != p).collect();
            self.function(f, body);
            let without = self.diagnostics.split_off(start);
            if errors(&without) == errors(&copy) {
                needed.retain(|&q| q != p);
                continue;
            }
            let at = without
                .iter()
                .find(|d| {
                    d.severity == Severity::Error
                        && !copy.iter().any(|c| c.span == d.span && c.code == d.code)
                })
                .map_or(self.ast.function(f).name_span, |d| d.span);
            needs.insert(p, at);
        }
        if needed == own {
            self.diagnostics.extend(copy);
        } else {
            self.assume_copy = needed;
            self.function(f, body);
        }
        self.assume_copy.clear();
    }

    /// `ty`'s linearity, with the inferable parameters being checked as `Copy` taken to be.
    fn lin(&self, ty: Ty) -> Kind {
        self.typed.types.linearity_assuming(ty, &self.assume_copy)
    }

    /// After every body: the `Copy` bounds inferred for each parameter, followed through
    /// the calls that pass it on, and checked at each call that gives a type
    /// (`spec/types.md` §6).
    fn check_copy_needs(&mut self, mut needs: FxHashMap<Ty, Span>) {
        let (ast, typed) = (self.ast, self.typed);
        let types = &typed.types;
        // The inferable parameters in `ty`, and those of them it cannot do without being
        // `Copy` to be copyable.
        let inferable_in = |ty: Ty| -> Vec<Ty> {
            types
                .params_in(ty)
                .into_iter()
                .filter(|p| typed.inferable.contains_key(p))
                .collect()
        };
        let required = |ty: Ty, inferable: &[Ty]| -> Vec<Ty> {
            inferable
                .iter()
                .copied()
                .filter(|&q| {
                    let others: Vec<Ty> = inferable.iter().copied().filter(|&o| o != q).collect();
                    types.linearity_assuming(ty, &others) != Kind::Copyable
                })
                .collect()
        };
        for need in &typed.copy_needs {
            for q in required(need.ty, &inferable_in(need.ty)) {
                needs.entry(q).or_insert(need.span);
            }
        }
        if needs.is_empty() {
            return;
        }
        let mut calls: Vec<(ExprId, FnId, &Vec<Ty>)> = typed
            .type_args
            .iter()
            .filter_map(|(&e, args)| match typed.callees[e.index()] {
                Some(Callee::Fn(g)) => Some((e, g, args)),
                _ => None,
            })
            .collect();
        calls.sort_by_key(|c| c.0.index());
        // A parameter passed on to one that needs `Copy` needs it too, through any number of
        // calls: each call is an edge from the callee's parameter to the caller's it needs.
        let mut edges: FxHashMap<Ty, Vec<(Ty, Span)>> = FxHashMap::default();
        for &(e, g, args) in &calls {
            for (&p, &arg) in typed.fns[g.index()].generics.iter().zip(args) {
                if arg == p {
                    continue;
                }
                for q in required(arg, &inferable_in(arg)) {
                    edges.entry(p).or_default().push((q, ast.expr(e).span));
                }
            }
        }
        let mut work: Vec<Ty> = needs.keys().copied().collect();
        while let Some(p) = work.pop() {
            for &(q, span) in edges.get(&p).into_iter().flatten() {
                if let std::collections::hash_map::Entry::Vacant(slot) = needs.entry(q) {
                    slot.insert(span);
                    work.push(q);
                }
            }
        }
        for &(e, g, args) in &calls {
            for (p, &arg) in typed.fns[g.index()].generics.iter().zip(args) {
                let Some(&at) = needs.get(p) else {
                    continue;
                };
                if types.is_wildcard(arg)
                    || types.linearity_assuming(arg, &inferable_in(arg)) == Kind::Copyable
                {
                    continue;
                }
                let param = types.display(*p, ast).to_string();
                let message = format!(
                    "`{}` needs `{param}: Copy`, but here `{param}` is `{}`",
                    ast.name(ast.function(g).name),
                    types.display(arg, ast)
                );
                self.diagnostics.push(
                    Diagnostic::error(ast.expr(e).span, message)
                        .with_label(at, format!("`{param}: Copy` is needed here")),
                );
            }
        }
    }

    fn function(&mut self, f: FnId, body: BlockId) {
        self.module = self.typed.fn_modules[f.index()];
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
        self.current_fn = Some(f);
        self.returns_ref = self.typed.types.has_ref(info.sig.ret);
        self.liveness = compute_liveness(self.ast, self.typed, f, body);
        self.current_live.clear();

        let params: Vec<LocalId> = info
            .self_local
            .into_iter()
            .chain(info.params.iter().copied())
            .collect();
        for local in params {
            self.declare(local, false);
        }
        // The sizes its signature subtracts in are at least 0: each call proves it (N024).
        for size in self.size_requirements(f) {
            if let Some(fact) = facts::size_form(&self.typed.types, size)
                .as_ref()
                .and_then(facts::assume_at_least_zero)
            {
                self.facts.push(fact);
            }
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
            zero: false,
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
            if self.fully_moved(&place, ty) || self.locals[local.index()].zero {
                continue;
            }
            let name = self.name(local).to_string();
            let declared = self.typed.local(local).span;
            let help = match self.typed.types.quantum_param(ty) {
                // A value of a generic type cannot be measured or released: it is passed on.
                Some(p) => {
                    let param = self.ast.name(self.typed.types.params[p.index()].name);
                    format!(
                        "it may hold qubits, since `{param}` can be a qubit: move it on with \
                         `mv {name}`, or require `{param}: Copy`"
                    )
                }
                None => format!(
                    "measure it with `mv {name}.measure()`, release it with `mv {name}.discard()`, or return it"
                ),
            };
            self.error(
                "N003",
                at,
                format!("`{name}` is still owned when it goes out of scope"),
            )
            .with_label_mut(declared, "declared here")
            .help = Some(help);
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
                let zero = !poisoned && self.expr_is_zero(init);
                let mut bound = Vec::new();
                let ty = self.typed.ty(init);
                self.bind(pat, ty, poisoned, zero, &mut Vec::new(), &mut bound);
                let locals: Vec<LocalId> = bound.iter().map(|(l, _)| *l).collect();
                self.attach_carried(&locals, &carried);
            }
            // `a += b` on a type of the program's own calls `AddAssign::add_assign`, lending
            // `a` for changing once `b` is evaluated.
            StmtKind::Assign { place, value, .. }
                if self.typed.callees[place.index()].is_some() =>
            {
                self.callee_rules(self.typed.callees[place.index()], stmt.span);
                self.check_size_requirements(place, stmt.span);
                let mut carried = self.expr(value, Use::Value);
                let prev_live = self.set_stmt_live(id);
                carried.extend(self.receiver(place, place));
                self.restore_live(prev_live);
                self.release(&carried);
            }
            StmtKind::Assign { op, place, value } => {
                let carried = self.expr(value, Use::Value);
                let value_zero = self.expr_is_zero(value);
                let value_is_ref = matches!(
                    self.typed.types.kind(self.typed.ty(value)),
                    TyKind::Ref { .. }
                );
                let prev_live = self.set_stmt_live(id);
                self.assign(place, op, value_zero, value_is_ref, stmt.span);
                self.restore_live(prev_live);
                if op == AssignOp::Assign
                    && let Some(p) = self.place_of(place)
                    && self.typed.types.has_ref(self.typed.local(p.root).ty)
                {
                    if p.proj.is_empty() {
                        for lend in &mut self.lends {
                            if lend.holder == Holder::Local(p.root) {
                                lend.alive = false;
                            }
                        }
                    }
                    self.attach_carried(&[p.root], &carried);
                } else {
                    self.release(&carried);
                }
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
            StmtKind::Assert { cond, msg } => {
                self.expr(cond, Use::Read);
                if let Some(m) = msg {
                    self.expr(m, Use::Read);
                }
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

    /// Declares the locals a pattern binds from a value of type `ty`, recording each local's
    /// projection path within the matched value. N003 for a `_` that would throw qubits away
    /// (unless proven to be in \|0⟩).
    fn bind(
        &mut self,
        pat: PatId,
        ty: Ty,
        poisoned: bool,
        zero: bool,
        path: &mut Vec<Proj>,
        bound: &mut Vec<(LocalId, Vec<Proj>)>,
    ) {
        let types = &self.typed.types;
        match self.ast.pat(pat).kind {
            PatKind::Name(_) => {
                if let Some(local) = self.typed.pat_locals[pat.index()] {
                    self.declare(local, poisoned);
                    self.locals[local.index()].zero = zero;
                    bound.push((local, path.clone()));
                }
            }
            PatKind::Wildcard => {
                // In a lent `match`, `_` takes nothing; qubits proven |0⟩ release automatically.
                if !poisoned && !self.lent_match && !zero && self.lin(ty) == Kind::Linear {
                    let span = self.ast.pat(pat).span;
                    self.error("N003", span, "`_` would throw away qubits").help =
                        Some("bind them to a name and consume them, or measure them".into());
                }
            }
            PatKind::Tuple(pats) => {
                let elems = match types.kind(ty) {
                    TyKind::Tuple(elems) => elems.to_vec(),
                    _ => Vec::new(),
                };
                for (i, &p) in self.ast.list(pats).iter().enumerate() {
                    let ty = elems.get(i).copied().unwrap_or(Ty::ERROR);
                    path.push(Proj::Field(i as u32));
                    self.bind(p, ty, poisoned, zero, path, bound);
                    path.pop();
                }
            }
            PatKind::Array(pats) => {
                let elem = types.elem_of(ty).unwrap_or(Ty::ERROR);
                for (i, &p) in self.ast.list(pats).iter().enumerate() {
                    path.push(Proj::Index(Some(facts_constant(i as i64))));
                    self.bind(p, elem, poisoned, zero, path, bound);
                    path.pop();
                }
            }
            PatKind::Literal(_) => {}
            PatKind::Variant { data, .. } => {
                let pats: Vec<(u32, PatId)> = match data {
                    PatData::None => Vec::new(),
                    PatData::Tuple(pats) => self
                        .ast
                        .list(pats)
                        .iter()
                        .enumerate()
                        .map(|(i, &p)| (i as u32, p))
                        .collect(),
                    PatData::Fields { fields, .. } => self
                        .ast
                        .list(fields)
                        .iter()
                        .enumerate()
                        .filter_map(|(i, f)| f.pat.map(|p| (i as u32, p)))
                        .collect(),
                };
                for (i, p) in pats {
                    let ty = self.typed.pat_types[p.index()];
                    path.push(Proj::Field(i));
                    self.bind(p, ty, poisoned, zero, path, bound);
                    path.pop();
                }
            }
        }
    }

    /// Attaches `carried` lends to every local in `bound` whose type holds a reference, or
    /// releases them if none do.
    fn attach_carried(&mut self, bound: &[LocalId], carried: &[usize]) {
        if carried.is_empty() {
            return;
        }
        let holders: Vec<LocalId> = bound
            .iter()
            .copied()
            .filter(|&l| self.typed.types.has_ref(self.typed.local(l).ty))
            .collect();
        if holders.is_empty() {
            self.release(carried);
            return;
        }
        let first = holders[0];
        for &i in carried {
            self.lends[i].holder = Holder::Local(first);
            self.lends[i].alive = true;
        }
        for &other in &holders[1..] {
            for &i in carried {
                let mut cloned = self.lends[i].clone();
                cloned.holder = Holder::Local(other);
                cloned.alive = true;
                self.lends.push(cloned);
            }
        }
    }

    fn set_stmt_live(&mut self, id: StmtId) -> Option<LiveSet> {
        let next = self.liveness.as_ref()?.after_stmt.get(&id)?.clone();
        Some(std::mem::replace(&mut self.current_live, next))
    }

    fn set_expr_live(&mut self, id: ExprId) -> Option<LiveSet> {
        let next = self.liveness.as_ref()?.after_expr.get(&id)?.clone();
        Some(std::mem::replace(&mut self.current_live, next))
    }

    fn set_shorthand_live(&mut self, id: ExprId, idx: usize) -> Option<LiveSet> {
        let next = self
            .liveness
            .as_ref()?
            .after_shorthand
            .get(&(id, idx))?
            .clone();
        Some(std::mem::replace(&mut self.current_live, next))
    }

    fn restore_live(&mut self, prev: Option<LiveSet>) {
        if let Some(prev) = prev {
            self.current_live = prev;
        }
    }

    /// Which parameter `local` is in the current function (if it is one that may be borrowed
    /// from in the return value).
    fn borrowable_param(&self, local: LocalId) -> Option<BorrowRoot> {
        let f = self.current_fn?;
        let info = &self.typed.fns[f.index()];
        if info.self_local == Some(local) {
            let can = matches!(info.sig.receiver, Some(OwnKind::Ref | OwnKind::Mut))
                || self.typed.types.has_ref(self.typed.local(local).ty);
            return can.then_some(BorrowRoot::Receiver);
        }
        info.params
            .iter()
            .position(|&p| p == local)
            .filter(|&i| self.typed.types.has_ref(info.sig.params[i]))
            .map(BorrowRoot::Param)
    }

    /// N008: a returned reference must point into a parameter allowed by the signature, never
    /// into a local of this function.
    fn check_returned_ref(&mut self, carried: &[usize]) {
        if !self.returns_ref {
            return;
        }
        let allowed = self
            .current_fn
            .and_then(|f| self.typed.fns[f.index()].return_borrows.clone());
        for &i in carried {
            if self.lends[i].reborrow {
                continue;
            }
            let place = self.lends[i].place.clone();
            let root = place.root;
            let span = self.lends[i].span;
            let Some(borrow_root) = self.borrowable_param(root) else {
                let name = self.name(root).to_string();
                self.error(
                    "N008",
                    span,
                    format!(
                        "`{name}` is dropped when the function returns, so a reference to it cannot be returned"
                    ),
                );
                continue;
            };
            if let Some(allowed) = &allowed {
                let matches = allowed.iter().any(|src| {
                    src.root == borrow_root
                        && src
                            .fields
                            .iter()
                            .enumerate()
                            .all(|(k, &idx)| place.proj.get(k) == Some(&Proj::Field(idx)))
                });
                if !matches {
                    let text = self.place_text(&place);
                    self.error(
                        "N008",
                        span,
                        format!(
                            "returned reference borrows from `{text}`, which is not listed in the return type"
                        ),
                    )
                    .help = Some("list it in the return type, e.g. `ref[…] T`".into());
                }
            }
        }
    }

    fn assign(
        &mut self,
        place_expr: ExprId,
        op: AssignOp,
        value_zero: bool,
        value_is_ref: bool,
        span: Span,
    ) {
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
        // Rebinding a reference variable or field (`r = ref b`, `w.q = ref b`) writes the
        // reference slot itself rather than through the reference it currently holds.
        let deref_final = !(op == AssignOp::Assign && value_is_ref);
        match self.place_through_ref(&place, deref_final) {
            Some(false) => {
                let text = self.place_text(&place);
                self.error(
                    "N015",
                    span,
                    format!(
                        "`{text}` is a `ref` reference, so it cannot be used to change the value"
                    ),
                )
                .help = Some("take it as `mut` to change it".into());
            }
            Some(true) => {}
            None if !self.typed.local(root).mutable => {
                self.error(
                    "N015",
                    span,
                    format!("`{name}` is not declared `let mut`, so it cannot be changed"),
                )
                .help = Some(format!("declare it with `let mut {name}`"));
            }
            None => {}
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
            && self.lin(ty) == Kind::Linear
            && !self.fully_moved(&place, ty)
            && !self.locals[root.index()].zero
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
        if place.proj.is_empty() {
            self.locals[root.index()].zero = value_zero;
        } else if !value_zero {
            self.locals[root.index()].zero = false;
        }
    }
}

// ---------------------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------------------

impl Owner<'_> {
    fn expr(&mut self, id: ExprId, use_: Use) -> Carried {
        let prev_live = self.set_expr_live(id);
        let carried = self.expr_inner(id, use_);
        self.restore_live(prev_live);
        carried
    }

    fn expr_inner(&mut self, id: ExprId, use_: Use) -> Carried {
        let ast = self.ast;
        let expr = *ast.expr(id);
        match expr.kind {
            ExprKind::Error
            | ExprKind::Int(_)
            | ExprKind::BigInt
            | ExprKind::Float(_)
            | ExprKind::Imag(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit
            | ExprKind::Path(_) => Vec::new(),
            ExprKind::Str(parts) => {
                for part in ast.list(parts) {
                    if let StrPart::Value { expr, .. } = *part {
                        self.expr(expr, Use::Read);
                    }
                }
                Vec::new()
            }
            ExprKind::Ket { parts, .. } => {
                if self.in_with.is_empty() {
                    self.not_reversible(expr.span, "creating qubits");
                }
                self.computed_labels(parts);
                self.discarded(id, use_);
                Vec::new()
            }
            ExprKind::Name(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. } => self.place_use(id, use_),
            ExprKind::Tuple(elems) | ExprKind::Array(elems) => {
                let keep = use_ != Use::Discard && self.typed.types.has_ref(self.typed.ty(id));
                let mut out = Vec::new();
                for &e in ast.list(elems) {
                    let carried = self.expr(e, Use::Value);
                    if keep {
                        out.extend(carried);
                    } else {
                        self.release(&carried);
                    }
                }
                self.discarded(id, use_);
                out
            }
            ExprKind::ArrayRepeat { value, count } => {
                let keep = use_ != Use::Discard && self.typed.types.has_ref(self.typed.ty(id));
                let carried = self.expr(value, Use::Value);
                self.expr(count, Use::Read);
                // `[|0>; N - 1]` needs `N - 1 ≥ 0`.
                if let TyKind::SizedArray(_, len) = *self.typed.types.kind(self.typed.ty(id))
                    && let Some(form) = facts::size_form(&self.typed.types, len)
                    && form.as_constant().is_none()
                    && !facts::at_least_zero(&self.facts, &form)
                    && matches!(self.typed.types.kind(len), TyKind::SizeSum { constant, .. } if *constant < 0)
                {
                    let shown = self.typed.types.display(len, self.ast);
                    self.error(
                        "N024",
                        self.ast.expr(count).span,
                        format!("cannot prove `{shown}` is at least 0"),
                    )
                    .help = Some(format!(
                        "`assert 0 <= {shown}` first, or take a larger size as the parameter"
                    ));
                }
                self.discarded(id, use_);
                if keep {
                    carried
                } else {
                    self.release(&carried);
                    Vec::new()
                }
            }
            ExprKind::StructLit { fields, .. } => {
                let keep = use_ != Use::Discard && self.typed.types.has_ref(self.typed.ty(id));
                let mut out = Vec::new();
                for (idx, field) in ast.list(fields).iter().enumerate() {
                    let carried = match field.value {
                        Some(value) => self.expr(value, Use::Value),
                        None => {
                            let prev = self.set_shorthand_live(id, idx);
                            let c = self.shorthand_field(field.name, field.span);
                            self.restore_live(prev);
                            c
                        }
                    };
                    if keep {
                        out.extend(carried);
                    } else {
                        self.release(&carried);
                    }
                }
                self.discarded(id, use_);
                out
            }
            ExprKind::Call { args, .. } => self.call(id, None, self.ast.list(args), use_),
            ExprKind::MethodCall { receiver, args, .. } => {
                self.call(id, Some(receiver), self.ast.list(args), use_)
            }
            // `a + b` on a type of the program's own calls `Add::add`, `a` its receiver.
            ExprKind::Binary { lhs, rhs, .. } if self.typed.is_operator_call(self.ast, id) => {
                self.call(id, Some(lhs), &[rhs], use_)
            }
            ExprKind::Unary { operand, .. } if self.typed.is_operator_call(self.ast, id) => {
                self.call(id, Some(operand), &[], use_)
            }
            ExprKind::Unary { operand, .. } => {
                self.expr(operand, Use::Read);
                Vec::new()
            }
            ExprKind::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                lhs,
                rhs,
            } => {
                self.expr(lhs, Use::Read);
                // The right side only sometimes runs, so moving qubits there is a branch.
                let before = self.moved.len();
                self.expr(rhs, Use::Read);
                let moved: Vec<Moved> = self.moved[before..].to_vec();
                for m in moved {
                    let ty = self.place_ty(&m.place);
                    if self.lin(ty) == Kind::Copyable {
                        continue;
                    }
                    let name = self.place_text(&m.place);
                    self.error(
                        "N009",
                        m.span,
                        format!(
                            "`{name}` is only moved when the right side of `{}` runs",
                            op.as_str()
                        ),
                    )
                    .help = Some("measure it first, in its own `let`".into());
                }
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
            ExprKind::BraTest { operand, parts, .. } => {
                self.not_reversible(expr.span, "measuring");
                self.computed_labels(parts);
                let carried = self.expr(operand, Use::Value);
                self.release(&carried);
                Vec::new()
            }
            ExprKind::PrepTest { prep, operand } => {
                self.not_reversible(expr.span, "measuring");
                self.preparation(prep);
                self.undoable(prep);
                let carried = self.expr(operand, Use::Value);
                self.release(&carried);
                Vec::new()
            }
            ExprKind::Overlap {
                bra,
                ket,
                observable,
                precision,
            } => {
                if self.not_reversible(expr.span, "computing an amplitude") {
                    return Vec::new();
                }
                // Both sides are undone or run controlled, many times (`spec/types.md` §3).
                for prep in [bra, ket] {
                    self.preparation(prep);
                    self.undoable(prep);
                    self.repeated_run_prints(prep, "an amplitude");
                }
                if let Some(observable) = observable {
                    self.expr(observable, Use::Read);
                }
                if let Some(precision) = precision {
                    self.expr(precision, Use::Read);
                }
                Vec::new()
            }
            ExprKind::Expectation {
                prep,
                observable,
                precision,
            } => {
                if self.not_reversible(expr.span, "computing an expectation value") {
                    return Vec::new();
                }
                self.preparation(prep);
                self.repeated_run_prints(prep, "an expectation value");
                self.expr(observable, Use::Read);
                if let Some(precision) = precision {
                    self.expr(precision, Use::Read);
                }
                Vec::new()
            }
            ExprKind::Sample { shots, body } => {
                if self.not_reversible(expr.span, "sampling") {
                    return Vec::new();
                }
                self.expr(shots, Use::Read);
                self.run_captures(id, body);
                self.run_prints(body);
                // The run's result is measured and counted: taken, like a returned value.
                let carried = self.block(body, Use::Value);
                self.release(&carried);
                self.diverged = false;
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
            } => self.if_expr(id, cond, then, otherwise, use_, expr.span),
            ExprKind::Match { scrutinee, arms } => {
                self.match_expr(id, scrutinee, arms, use_, expr.span)
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
            ExprKind::With { with, body } => self.with_do(with, body, use_),
        }
    }

    /// Whether every qubit in `id`'s value is proven to be in \|0⟩ (`spec/ownership.md` §9).
    fn expr_is_zero(&self, id: ExprId) -> bool {
        let ty = self.typed.ty(id);
        if self.lin(ty) == Kind::Copyable {
            return true;
        }
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Ket { zero, .. } => zero,
            ExprKind::ArrayRepeat { value, .. } => self.expr_is_zero(value),
            ExprKind::Array(elems) | ExprKind::Tuple(elems) => {
                ast.list(elems).iter().all(|&e| self.expr_is_zero(e))
            }
            ExprKind::StructLit { fields, .. } => ast
                .list(fields)
                .iter()
                .all(|f| f.value.is_none_or(|v| self.expr_is_zero(v))),
            ExprKind::Call { args, .. }
                if matches!(self.typed.callees[id.index()], Some(Callee::Variant(..))) =>
            {
                ast.list(args).iter().all(|&a| self.expr_is_zero(a))
            }
            ExprKind::Own {
                kind: OwnKind::Mv,
                place,
            } => self
                .place_of(place)
                .is_some_and(|p| self.locals[p.root.index()].zero),
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block_is_zero(b),
            ExprKind::If {
                then,
                otherwise: Some(else_),
                ..
            } => self.block_is_zero(then) && self.expr_is_zero(else_),
            ExprKind::With { body, .. } => self.block_is_zero(body),
            _ => false,
        }
    }

    fn block_is_zero(&self, block: BlockId) -> bool {
        match self.ast.list(self.ast.block(block).stmts).last() {
            Some(&stmt) => match self.ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } => self.expr_is_zero(expr),
                _ => false,
            },
            None => true,
        }
    }

    /// N003 for a fresh value containing qubits that is thrown away (unless proven \|0⟩).
    fn discarded(&mut self, id: ExprId, use_: Use) {
        if use_ != Use::Discard || self.expr_is_zero(id) {
            return;
        }
        let ty = self.typed.ty(id);
        if self.lin(ty) == Kind::Linear {
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
        let keep_facts = !self.in_with.is_empty();
        let facts_mark = self.facts.len();
        let scope_mark = self.scope.len();
        let before_zero: Vec<bool> = self.locals.iter().map(|l| l.zero).collect();
        let before_lends: Vec<bool> = self.lends.iter().map(|l| l.alive).collect();
        self.loops.push(scope_mark);
        if let Some(var) = var {
            self.declare(var, false);
        }
        self.facts.extend(facts);
        let body_facts_mark = if keep_facts {
            self.facts.len()
        } else {
            facts_mark
        };
        self.block(body, Use::Discard);
        self.facts.truncate(body_facts_mark);
        self.leave_scope(scope_mark);
        self.loops.pop();
        // A loop may run zero times, so outer-local lends alive before it can still be alive.
        for (lend, &was_alive) in self.lends.iter_mut().zip(&before_lends) {
            lend.alive |= was_alive;
        }
        // A loop may run zero times, so a local stays proven |0⟩ only if it was before and
        // still is after.
        for (l, &was) in self.locals.iter_mut().zip(&before_zero) {
            l.zero &= was;
        }
        // A loop may run zero times, and `break` leaves it.
        self.diverged = false;
    }

    fn if_expr(
        &mut self,
        id: ExprId,
        cond: ExprId,
        then: BlockId,
        otherwise: Option<ExprId>,
        use_: Use,
        span: Span,
    ) -> Carried {
        self.expr(cond, Use::Read);
        let mut then_facts = Vec::new();
        let mut else_facts = Vec::new();
        facts::facts_from(self.ast, self.typed, cond, true, &mut then_facts);
        facts::facts_from(self.ast, self.typed, cond, false, &mut else_facts);

        let keep = use_ != Use::Discard && self.typed.types.has_ref(self.typed.ty(id));
        let before = self.moved.clone();
        let before_zero: Vec<bool> = self.locals.iter().map(|l| l.zero).collect();
        let before_lends: Vec<bool> = self.lends.iter().map(|l| l.alive).collect();
        let mark = self.facts.len();
        self.facts.extend(then_facts);
        let then_carried = self.block(then, use_);
        if !keep {
            self.release(&then_carried);
        }
        self.facts.truncate(mark);
        let then_moved = std::mem::replace(&mut self.moved, before.clone());
        let then_zero: Vec<bool> = self.locals.iter().map(|l| l.zero).collect();
        let then_lends: Vec<bool> = self.lends.iter().map(|l| l.alive).collect();
        for (l, &was) in self.locals.iter_mut().zip(&before_zero) {
            l.zero = was;
        }
        for (i, lend) in self.lends.iter_mut().enumerate() {
            lend.alive = before_lends.get(i).copied().unwrap_or(false);
        }
        let then_diverged = std::mem::replace(&mut self.diverged, false);

        self.facts.extend(else_facts);
        let else_carried = if let Some(otherwise) = otherwise {
            let c = self.expr(otherwise, use_);
            if !keep {
                self.release(&c);
                Vec::new()
            } else {
                c
            }
        } else {
            Vec::new()
        };
        self.facts.truncate(mark);
        let else_moved = std::mem::take(&mut self.moved);
        let else_zero: Vec<bool> = self.locals.iter().map(|l| l.zero).collect();
        let else_lends: Vec<bool> = self.lends.iter().map(|l| l.alive).collect();
        let else_diverged = self.diverged;

        self.diverged = then_diverged && else_diverged;
        self.moved = match (then_diverged, else_diverged) {
            (true, _) => else_moved,
            (_, true) => then_moved,
            _ => self.merge_branches(then_moved, else_moved, span, false),
        };
        for (i, lend) in self.lends.iter_mut().enumerate() {
            let ta = then_lends.get(i).copied().unwrap_or(false);
            let ea = else_lends.get(i).copied().unwrap_or(false);
            lend.alive = match (then_diverged, else_diverged) {
                (true, _) => ea,
                (_, true) => ta,
                _ => ta || ea,
            };
        }
        for ((l, &tz), &ez) in self.locals.iter_mut().zip(&then_zero).zip(&else_zero) {
            l.zero = match (then_diverged, else_diverged) {
                (true, _) => ez,
                (_, true) => tz,
                _ => tz && ez,
            };
        }
        let mut out = Vec::new();
        if keep {
            if !then_diverged {
                for &i in &then_carried {
                    self.lends[i].alive = true;
                    out.push(i);
                }
            }
            if !else_diverged {
                for &i in &else_carried {
                    self.lends[i].alive = true;
                    out.push(i);
                }
            }
        }
        out
    }

    /// The arms of a `match` are branches: each starts from what was moved before it, and
    /// anything with qubits must be moved in all of them or none (N009).
    fn match_expr(
        &mut self,
        id: ExprId,
        scrutinee: ExprId,
        arms: List<Arm>,
        use_: Use,
        span: Span,
    ) -> Carried {
        // `match mv x` and temporaries give the arms the parts; a place or a reference is lent
        // to the names in the patterns, as the checker typed them (`spec/ownership.md` §13).
        let scrutinee_ty = self.typed.ty(scrutinee);
        let moved = matches!(
            self.ast.expr(scrutinee).kind,
            ExprKind::Own {
                kind: OwnKind::Mv,
                ..
            }
        );
        let lend_mut = match *self.typed.types.kind(scrutinee_ty) {
            TyKind::Ref { mutable, .. } => Some(mutable),
            _ if !moved && self.place_of(scrutinee).is_some() => Some(false),
            _ => None,
        };
        let lent = if moved { None } else { lend_mut };
        let lent_place = match self.ast.expr(scrutinee).kind {
            ExprKind::Own { place, .. } => self.place_of(place),
            _ => self.place_of(scrutinee),
        };
        let scrutinee_carried = if moved {
            self.expr(scrutinee, Use::Value)
        } else {
            self.expr(scrutinee, Use::Read)
        };
        let scrutinee_zero = lent.is_none() && self.expr_is_zero(scrutinee);
        let keep = use_ != Use::Discard && self.typed.types.has_ref(self.typed.ty(id));
        let before = self.moved.clone();
        let before_zero: Vec<bool> = self.locals.iter().map(|l| l.zero).collect();
        let before_lends: Vec<bool> = self.lends.iter().map(|l| l.alive).collect();
        let mut merged: Option<Vec<Moved>> = None;
        let mut merged_zero: Option<Vec<bool>> = None;
        let mut merged_lends: Option<Vec<bool>> = None;
        let mut all_carried: Vec<usize> = Vec::new();
        let mut all_diverged = true;
        for arm in self.ast.list(arms).to_vec() {
            self.moved = before.clone();
            for (l, &was) in self.locals.iter_mut().zip(&before_zero) {
                l.zero = was;
            }
            for (i, lend) in self.lends.iter_mut().enumerate() {
                lend.alive = before_lends.get(i).copied().unwrap_or(false);
            }
            self.diverged = false;
            let scope_mark = self.scope.len();
            let mut bound = Vec::new();
            let ty = self.typed.pat_types[arm.pat.index()];
            self.lent_match = lent.is_some();
            self.bind(
                arm.pat,
                ty,
                false,
                scrutinee_zero,
                &mut Vec::new(),
                &mut bound,
            );
            self.lent_match = false;
            // Attach both the scrutinee's place lend (projected per pattern binding so disjoint
            // variant/tuple bindings don't conflict) and any origin lends carried by the scrutinee.
            let ref_holders: Vec<(LocalId, Vec<Proj>)> = bound
                .into_iter()
                .filter(|&(l, _)| self.typed.types.has_ref(self.typed.local(l).ty))
                .collect();
            if !ref_holders.is_empty() {
                if let (Some(mutable), Some(place)) = (lent, &lent_place) {
                    let span = self.ast.expr(scrutinee).span;
                    if let Some(base_lend) =
                        self.lend_place(place, scrutinee, mutable, false, Holder::Pending)
                    {
                        self.lends[base_lend].alive = false;
                        for (holder, sub_proj) in &ref_holders {
                            let mut field_place = place.clone();
                            field_place.proj.extend(sub_proj.iter().cloned());
                            self.lends.push(Lend {
                                place: field_place,
                                mutable,
                                holder: Holder::Local(*holder),
                                span,
                                alive: true,
                                reborrow: false,
                            });
                        }
                    }
                    let origins = self.clone_origin_lends(place.root);
                    let holders: Vec<LocalId> = ref_holders.iter().map(|(l, _)| *l).collect();
                    self.attach_carried(&holders, &origins);
                }
                if !scrutinee_carried.is_empty() {
                    let holders: Vec<LocalId> = ref_holders.iter().map(|(l, _)| *l).collect();
                    let mut cloned = Vec::with_capacity(scrutinee_carried.len());
                    for &i in &scrutinee_carried {
                        let mut c = self.lends[i].clone();
                        c.holder = Holder::Pending;
                        c.alive = true;
                        self.lends.push(c);
                        cloned.push(self.lends.len() - 1);
                    }
                    self.attach_carried(&holders, &cloned);
                }
            }
            let carried = self.expr(arm.body, use_);
            if !keep {
                self.release(&carried);
            }
            if !self.diverged {
                let end = Span::new(arm.span.end, arm.span.end);
                self.check_consumed(scope_mark, end);
            }
            self.leave_scope(scope_mark);
            let moved = std::mem::take(&mut self.moved);
            let arm_zero: Vec<bool> = self.locals.iter().map(|l| l.zero).collect();
            let arm_lends: Vec<bool> = self.lends.iter().map(|l| l.alive).collect();
            if !self.diverged {
                merged = Some(match merged {
                    None => moved,
                    Some(earlier) => self.merge_branches(earlier, moved, span, true),
                });
                merged_zero = Some(match merged_zero {
                    None => arm_zero,
                    Some(earlier) => earlier
                        .into_iter()
                        .zip(arm_zero)
                        .map(|(a, b)| a && b)
                        .collect(),
                });
                merged_lends = Some(match merged_lends {
                    None => arm_lends,
                    Some(mut earlier) => {
                        if arm_lends.len() > earlier.len() {
                            earlier.resize(arm_lends.len(), false);
                        }
                        for (e, a) in earlier.iter_mut().zip(arm_lends) {
                            *e |= a;
                        }
                        earlier
                    }
                });
                if keep {
                    all_carried.extend(carried);
                }
            }
            all_diverged &= self.diverged;
        }
        self.release(&scrutinee_carried);
        self.diverged = all_diverged;
        self.moved = merged.unwrap_or(before);
        if let Some(lends_alive) = merged_lends {
            for (i, lend) in self.lends.iter_mut().enumerate() {
                lend.alive = lends_alive.get(i).copied().unwrap_or(false);
            }
        }
        if let Some(zeros) = merged_zero {
            for (l, z) in self.locals.iter_mut().zip(zeros) {
                l.zero = z;
            }
        }
        if keep {
            for &i in &all_carried {
                self.lends[i].alive = true;
            }
            all_carried
        } else {
            Vec::new()
        }
    }

    /// N009 for anything with qubits moved in one branch but not the other.
    fn merge_branches(
        &mut self,
        a: Vec<Moved>,
        b: Vec<Moved>,
        span: Span,
        arms: bool,
    ) -> Vec<Moved> {
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
                if self.lin(ty) == Kind::Copyable || reported.contains(&m.place.root) {
                    continue;
                }
                reported.push(m.place.root);
                let name = self.name(m.place.root).to_string();
                let message = if arms {
                    format!("`{name}` is moved in some arms of this `match` but not others")
                } else if is_then {
                    format!("`{name}` is moved when the condition is true but not otherwise")
                } else {
                    format!("`{name}` is moved when the condition is false but not otherwise")
                };
                let help = if arms {
                    "move it in every arm, or in none"
                } else {
                    "move it in both branches, or in neither"
                };
                self.error("N009", span, message)
                    .with_label_mut(m.span, "moved here")
                    .help = Some(help.into());
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

    /// `with { W } do { D }` (`spec/ownership.md` §8): `W` is reversible and may allocate
    /// ancillas; every linear place `W` allocated, acted on or read stays `ref`-lent during
    /// `D`, and `W`'s ancillas are uncomputed and released automatically at the end.
    fn with_do(&mut self, with: BlockId, body: BlockId, use_: Use) -> Carried {
        let ast = self.ast;
        let scope_mark = self.scope.len();
        let facts_mark = self.facts.len();
        let with_order = self.order;
        self.reversible.push(with_order);
        self.in_with.push(with_order);
        self.with_touched.push((with_order, Vec::new()));
        let stmts = ast.list(ast.block(with).stmts);
        for (i, &stmt) in stmts.iter().enumerate() {
            if self.diverged {
                break;
            }
            match ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    let carried = self.expr(expr, Use::Discard);
                    self.release(&carried);
                }
                _ => self.stmt(stmt),
            }
            self.release_pending(&[]);
        }
        let (_, touched) = self.with_touched.pop().expect("pushed above");
        self.in_with.pop();
        self.reversible.pop();

        // Hold `ref` lends across `do` on every linear place `with` touched, and on every
        // linear local `with` declared that was not moved inside `with`.
        let mut held = Vec::new();
        for (place, span) in touched {
            if !held.iter().any(|&i: &usize| self.lends[i].place == place) {
                self.lends.push(Lend {
                    place,
                    mutable: false,
                    holder: Holder::With,
                    span,
                    alive: true,
                    reborrow: false,
                });
                held.push(self.lends.len() - 1);
            }
        }
        let with_locals: Vec<LocalId> = self.scope[scope_mark..].to_vec();
        for local in with_locals {
            let ty = self.typed.local(local).ty;
            let place = Place {
                root: local,
                proj: Vec::new(),
            };
            if self.lin(ty) == Kind::Linear && !self.fully_moved(&place, ty) {
                self.lends.push(Lend {
                    place,
                    mutable: false,
                    holder: Holder::With,
                    span: self.typed.local(local).span,
                    alive: true,
                    reborrow: false,
                });
                held.push(self.lends.len() - 1);
            }
        }

        let carried = self.block(body, use_);
        if use_ != Use::Discard {
            for &i in &carried {
                if self.lends[i].reborrow {
                    continue;
                }
                let root = self.lends[i].place.root;
                if !self.is_reference(root) && self.locals[root.index()].order >= with_order {
                    let name = self.name(root).to_string();
                    let span = self.lends[i].span;
                    self.error(
                        "N008",
                        span,
                        format!("`{name}` is dropped at the end of `with`, so a reference to it cannot leave `do`"),
                    );
                }
            }
        }
        self.release(&held);
        self.leave_scope(scope_mark);
        self.facts.truncate(facts_mark);
        carried
    }

    /// Whether calling `f` from the current function counts as reversible: as declared, or as
    /// inferred from its body when it is in the same package (`spec/modules.md` §5).
    fn reversible_fn(&self, f: FnId) -> bool {
        match self.typed.fns[f.index()].sig.reversible {
            Reversible::Yes => true,
            Reversible::No => false,
            Reversible::Infer => {
                self.irreversible[f.index()].is_none()
                    && !other_package(self.ast, self.typed.fn_modules[f.index()], self.module)
            }
        }
    }

    /// Why calling `f` from the current function does not count as reversible, if known.
    fn why_not_reversible(&self, f: FnId) -> Option<Irreversible> {
        if let Some(why) = &self.irreversible[f.index()] {
            return Some(why.clone());
        }
        let decl = self.ast.function(f);
        if self.typed.fn_traits.contains_key(&f) && !decl.is_rev {
            return Some(Irreversible {
                span: decl.name_span,
                why: format!(
                    "`{}` is a trait's function not declared `rev fn`: generic code cannot see its `impl`s",
                    self.ast.name(decl.name)
                ),
            });
        }
        let elsewhere = self.typed.fns[f.index()].sig.reversible == Reversible::Infer
            && other_package(self.ast, self.typed.fn_modules[f.index()], self.module);
        elsewhere.then(|| Irreversible {
            span: decl.name_span,
            why: format!(
                "`{}` is declared here, {ELSEWHERE}",
                self.ast.name(decl.name)
            ),
        })
    }

    /// Why the value `text` of type `ty` cannot be copied, and how else to fix it than by
    /// moving it: requiring a type parameter to be classical.
    fn uncopyable(&self, text: &str, ty: Ty) -> (String, String) {
        if self.lin(ty) == Kind::Affine {
            let msg = if self.typed.types.has_ref(ty) {
                format!("`{text}` holds a `mut` reference, which cannot be copied")
            } else {
                format!("`{text}` cannot be copied")
            };
            return (msg, String::new());
        }
        match self.typed.types.quantum_param(ty) {
            Some(p) => {
                let param = self.ast.name(self.typed.types.params[p.index()].name);
                (
                    format!(
                        "`{text}` may hold qubits, since `{param}` can be a qubit, so it cannot be copied"
                    ),
                    format!(", or require `{param}: Copy`"),
                )
            }
            None => (
                format!("`{text}` holds qubits, which cannot be copied"),
                String::new(),
            ),
        }
    }

    /// N014: `adj` needs something reversible.
    fn check_adj(&mut self, call: ExprId) {
        let Some(callee) = self.typed.callees[call.index()] else {
            return;
        };
        let ok = match callee {
            Callee::Builtin(b) => b.reversible(),
            Callee::Convert(_) | Callee::TryConvert(_) => true,
            Callee::Variant(..) | Callee::FnPtr => false,
            Callee::Fn(f) => self.reversible_fn(f),
        };
        if !ok {
            let span = self.ast.expr(call).span;
            let why = match callee {
                Callee::Fn(f) => self.why_not_reversible(f),
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

    /// N022: a run that is repeated must not print.
    fn repeated_run_prints(&mut self, run: ExprId, by: &str) {
        let Some(Callee::Fn(f)) = self.typed.callees[run.index()] else {
            return;
        };
        let Some(print) = self.prints[f.index()] else {
            return;
        };
        let name = self.ast.name(self.ast.function(f).name).to_string();
        let span = self.ast.expr(run).span;
        self.error(
            "N022",
            span,
            format!("`{name}` prints, but it is run many times by {by}"),
        )
        .with_label_mut(print, "it prints here")
        .help = Some("compute values in the run and print them from the caller".into());
    }

    /// N022 for printing in a `sample` run, directly or through a call.
    fn run_prints(&mut self, body: BlockId) {
        let mut found = Vec::new();
        let mut with_pats = Vec::new();
        walk_block(self.ast, body, false, &mut with_pats, &mut |node| {
            let Node::Expr(id, _) = node else { return };
            match self.typed.callees[id.index()] {
                Some(Callee::Builtin(Builtin::Print { .. })) => found.push((id, None)),
                Some(Callee::Fn(f)) => {
                    if let Some(print) = self.prints[f.index()] {
                        found.push((id, Some((f, print))));
                    }
                }
                _ => {}
            }
        });
        for (id, through) in found {
            let span = self.ast.expr(id).span;
            let help = "compute values in the run and print them after `sample`";
            match through {
                None => {
                    self.error("N022", span, "this prints, but `sample` runs it many times")
                        .help = Some(help.into());
                }
                Some((f, print)) => {
                    let name = self.ast.name(self.ast.function(f).name).to_string();
                    self.error(
                        "N022",
                        span,
                        format!("`{name}` prints, but `sample` runs it many times"),
                    )
                    .with_label_mut(print, "it prints here")
                    .help = Some(help.into());
                }
            }
        }
    }

    /// N020 for a `sample` run that uses something from outside it other than by reading a
    /// classical value: each run is given a copy of the values it reads.
    fn run_captures(&mut self, id: ExprId, body: BlockId) {
        let Some(run) = self.typed.runs.get(&id) else {
            return;
        };
        let first = run.first_local as usize;
        let ast = self.ast;
        let typed = self.typed;
        let assume = self.assume_copy.clone();
        let outside = |e: ExprId| match ast.expr(e).kind {
            ExprKind::Name(_) => match typed.res[e.index()] {
                Res::Local(l) if l.index() < first => Some(l),
                _ => None,
            },
            _ => None,
        };
        let mut problems: Vec<(LocalId, Span, &str)> = Vec::new();
        let mut with_pats = Vec::new();
        walk_block(ast, body, false, &mut with_pats, &mut |node| match node {
            Node::Assign(place, span) => {
                if let Some(l) = root_local(ast, typed, place).filter(|l| l.index() < first) {
                    problems.push((l, span, "changes"));
                }
            }
            Node::Expr(e, _) => {
                if let Some(l) = outside(e) {
                    let ty = typed.local(l).ty;
                    if typed.types.linearity_assuming(ty, &assume) != Kind::Copyable {
                        problems.push((l, ast.expr(e).span, "uses"));
                    } else if typed.types.has_ref(ty) {
                        problems.push((l, ast.expr(e).span, "uses the reference"));
                    } else if matches!(typed.adjust[e.index()], Adjust::Lend { mutable: true }) {
                        problems.push((l, ast.expr(e).span, "lends"));
                    }
                }
                let owned = match ast.expr(e).kind {
                    ExprKind::Own {
                        kind: OwnKind::Mv | OwnKind::Mut,
                        place,
                    } => Some(place),
                    ExprKind::MethodCall { receiver, .. }
                        if typed.receivers[e.index()] == Some(OwnKind::Mut) =>
                    {
                        Some(receiver)
                    }
                    _ => None,
                };
                if let Some(l) = owned
                    .and_then(|p| root_local(ast, typed, p))
                    .filter(|l| l.index() < first)
                {
                    problems.push((l, ast.expr(e).span, "changes"));
                }
            }
        });
        let mut reported = Vec::new();
        for (local, span, what) in problems {
            if reported.contains(&local) {
                continue;
            }
            reported.push(local);
            // Qubits cannot be shared between runs at all, however they are used.
            let ty = self.typed.local(local).ty;
            let what = if self.lin(ty) == Kind::Copyable {
                what
            } else {
                "uses"
            };
            let name = self.name(local).to_string();
            self.error(
                "N020",
                span,
                format!("`sample` runs this many times, but it {what} `{name}`, from outside the run"),
            )
            .help = Some(
                "a run can only read classical values from outside it; create qubits inside the run"
                    .into(),
            );
        }
    }

    /// A ket's or bra's computed labels, which are read.
    fn computed_labels(&mut self, parts: List<KetPart>) {
        for part in self.ast.list(parts) {
            if let KetPart::Value(value) = *part {
                self.expr(value, Use::Read);
            }
        }
    }

    /// N023: a bra's preparation is undone, so it must be reversible apart from creating its
    /// qubits.
    fn undoable(&mut self, prep: ExprId) {
        let Some(Callee::Fn(f)) = self.typed.callees[prep.index()] else {
            return;
        };
        let preparable = self
            .preparable
            .get_or_insert_with(|| infer_preparable(self.ast, self.typed));
        let Some(why) = preparable[f.index()].clone() else {
            return;
        };
        let name = self.ast.name(self.ast.function(f).name).to_string();
        let span = self.ast.expr(prep).span;
        self.error("N023", span, format!("`{name}` cannot be undone: {}", why.why))
            .with_label_mut(why.span, "not reversible")
            .help = Some(
            "a bra's preparation may create qubits, but otherwise only do what a reversible function may".into(),
        );
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
            if self.lin(ty) != Kind::Copyable || lends_mut || owns {
                let span = self.ast.expr(arg).span;
                let text = self.expr_text(arg);
                self.error("N020", span, format!("this call is repeated many times, but `{text}` is not a classical value it can be given each time"))
                    .help = Some("create the qubits inside the function, and pass only classical values".into());
                return;
            }
            self.expr(arg, Use::Read);
        }
    }

    fn shorthand_field(&mut self, name: Symbol, span: Span) -> Carried {
        let Some(&local) = self
            .scope
            .iter()
            .rev()
            .find(|&&l| self.typed.local(l).name == name)
        else {
            return Vec::new();
        };
        let ty = self.typed.local(local).ty;
        let place = Place {
            root: local,
            proj: Vec::new(),
        };
        let copyable = self.lin(ty) == Kind::Copyable
            || matches!(self.typed.types.kind(ty), TyKind::Ref { .. });
        if !copyable {
            let text = self.ast.name(name).to_string();
            let (message, or) = self.uncopyable(&text, ty);
            self.error("N001", span, message).help = Some(format!(
                "write `{text}: mv {text}` to move it into the struct{or}"
            ));
            let origins = if self.typed.types.has_ref(ty) {
                self.clone_origin_lends(local)
            } else {
                Vec::new()
            };
            self.move_place(&place, ty, span);
            return origins;
        }
        self.carry_value_place(&place, ty, span)
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
                    return self.lend_and_origins(&place, place_expr, mutable, false);
                }
                let ty = self.typed.ty(place_expr);
                let origins = if use_ != Use::Discard && self.typed.types.has_ref(ty) {
                    self.clone_origin_lends(place.root)
                } else {
                    Vec::new()
                };
                self.move_place(&place, ty, span);
                self.discarded(id, use_);
                origins
            }
            OwnKind::Ref | OwnKind::Mut => {
                let mutable = kind == OwnKind::Mut;
                self.lend_and_origins(&place, place_expr, mutable, false)
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
                return self.lend_and_origins(&place, id, mutable, true);
            }
            Adjust::Deref => {
                let target = self.typed.types.deref(ty);
                if use_ == Use::Value && self.lin(target) != Kind::Copyable {
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
        let copyable = self.lin(ty) == Kind::Copyable
            || matches!(self.typed.types.kind(ty), TyKind::Ref { .. });
        if use_ == Use::Value && !copyable {
            let text = self.expr_text(id);
            let (message, or) = self.uncopyable(&text, ty);
            self.error("N001", span, message).help =
                Some(format!("write `mv {text}` to move it{or}"));
            let origins = if self.typed.types.has_ref(ty) {
                self.clone_origin_lends(place.root)
            } else {
                Vec::new()
            };
            self.move_place(&place, ty, span);
            return origins;
        }
        if use_ == Use::Value && self.typed.types.has_ref(ty) {
            return self.carry_value_place(&place, ty, span);
        }
        self.read(&place, span);
        Vec::new()
    }

    /// Reads/reborrows a copyable or `mut`-ref place whose value holds references (`Use::Value`),
    /// returning the `Pending` lends carried by the resulting value.
    fn carry_value_place(&mut self, place: &Place, ty: Ty, span: Span) -> Carried {
        let is_mut_ref = matches!(self.typed.types.kind(ty), TyKind::Ref { mutable: true, .. });
        let mut out = Vec::new();
        if is_mut_ref {
            if !self.report_moved(place, span) {
                self.check_lend_conflicts(place, true, span, None);
                let origins = self.clone_origin_lends(place.root);
                self.lends.push(Lend {
                    place: place.clone(),
                    mutable: true,
                    holder: Holder::Pending,
                    span,
                    alive: true,
                    // If `place.root` is a local with origin lends, this is a reborrow guard on
                    // that intermediate local; if it's a parameter, `place` itself is the origin.
                    reborrow: !origins.is_empty(),
                });
                out.push(self.lends.len() - 1);
                out.extend(origins);
            }
        } else {
            self.read(place, span);
            let origins = self.clone_origin_lends(place.root);
            if !origins.is_empty() {
                out.extend(origins);
            } else if self.borrowable_param(place.root).is_some() {
                self.lends.push(Lend {
                    place: place.clone(),
                    mutable: false,
                    holder: Holder::Pending,
                    span,
                    alive: true,
                    reborrow: false,
                });
                out.push(self.lends.len() - 1);
            }
        }
        out
    }

    /// Lends `place` (`Holder::Pending`), and if `place.root` is a local holding references,
    /// also carries cloned copies of its origin lends (marking the lend on a local ref variable
    /// as `reborrow = true` so returning through it checks its origin, not the local pointer).
    fn lend_and_origins(
        &mut self,
        place: &Place,
        expr: ExprId,
        mutable: bool,
        implicit_arg: bool,
    ) -> Carried {
        let mut out = Vec::new();
        let lent = self.lend_place(place, expr, mutable, implicit_arg, Holder::Pending);
        let origins = if self.typed.types.has_ref(self.typed.local(place.root).ty) {
            self.clone_origin_lends(place.root)
        } else {
            Vec::new()
        };
        if let Some(i) = lent {
            if self.is_reference(place.root) && !origins.is_empty() {
                self.lends[i].reborrow = true;
            }
            out.push(i);
        }
        out.extend(origins);
        out
    }

    /// Clones every alive, non-reborrow lend held by `Holder::Local(local)` as a fresh
    /// `Holder::Pending` lend (deduplicating identical `(place, mutable)` entries).
    fn clone_origin_lends(&mut self, local: LocalId) -> Carried {
        let mut copies: Vec<Lend> = Vec::new();
        for lend in &self.lends {
            if lend.alive
                && !lend.reborrow
                && lend.holder == Holder::Local(local)
                && !copies
                    .iter()
                    .any(|c| c.place == lend.place && c.mutable == lend.mutable)
            {
                let mut cloned = lend.clone();
                cloned.holder = Holder::Pending;
                cloned.alive = true;
                copies.push(cloned);
            }
        }
        let mut out = Vec::with_capacity(copies.len());
        for lend in copies {
            self.lends.push(lend);
            out.push(self.lends.len() - 1);
        }
        out
    }

    fn call(
        &mut self,
        id: ExprId,
        receiver: Option<ExprId>,
        args: &[ExprId],
        use_: Use,
    ) -> Carried {
        let span = self.ast.expr(id).span;
        let callee = self.typed.callees[id.index()];
        self.callee_rules(callee, span);
        self.check_size_requirements(id, span);

        if callee == Some(Callee::Builtin(Builtin::Qasm)) {
            if let Some(&arg) = args.first()
                && let ExprKind::Str(parts) = self.ast.expr(arg).kind
            {
                for part in self.ast.list(parts) {
                    let StrPart::Value { expr, .. } = *part else {
                        continue;
                    };
                    let ty = self.typed.ty(expr);
                    if self.typed.types.deref(ty) == Ty::QUBIT {
                        self.place_subexprs(expr);
                        if let Some(place) = self.place_of(expr)
                            && let Some(i) =
                                self.lend_place(&place, expr, true, false, Holder::Pending)
                        {
                            self.release(&[i]);
                        }
                    } else {
                        self.expr(expr, Use::Read);
                    }
                }
            } else {
                for &arg in args {
                    self.expr(arg, Use::Value);
                }
            }
            return Vec::new();
        }
        if matches!(callee, Some(Callee::Builtin(Builtin::Print { .. }))) {
            for &arg in args {
                self.expr(arg, Use::Read);
            }
            return Vec::new();
        }

        // Two-phase receiver lends (`spec/ownership.md` §5): for a place receiver taken by
        // `ref` or `mut`, evaluate its index subexpressions and all call arguments BEFORE
        // starting the receiver's lend, so `c.add(c.get())` and `e.rotate_by(e.angle)` work.
        let recv_kind = self.typed.receivers[id.index()].unwrap_or(OwnKind::Ref);
        let two_phase = receiver.is_some_and(|r| {
            is_place_expr(self.ast, r) && matches!(recv_kind, OwnKind::Ref | OwnKind::Mut)
        });
        let (receiver_carried, arg_carried_list) = if two_phase {
            let r = receiver.expect("checked");
            self.place_subexprs(r);
            let mut per_arg = Vec::with_capacity(args.len());
            for &arg in args {
                per_arg.push(self.expr(arg, Use::Value));
            }
            let recv_c = self.lend_receiver_place(id, r, callee, recv_kind == OwnKind::Mut);
            (recv_c, per_arg)
        } else {
            let recv_c = match receiver {
                Some(r) => self.receiver(id, r),
                None => Vec::new(),
            };
            let mut per_arg = Vec::with_capacity(args.len());
            for &arg in args {
                per_arg.push(self.expr(arg, Use::Value));
            }
            (recv_c, per_arg)
        };

        // Keep only the lends that the returned value (if it holds references) may borrow from.
        let ret = self.typed.ty(id);
        let returns_ref = self.typed.types.has_ref(ret);
        let carried = if !returns_ref {
            Vec::new()
        } else {
            match callee {
                Some(Callee::Variant(..)) => arg_carried_list
                    .iter()
                    .flat_map(|v| v.iter().copied())
                    .collect(),
                Some(Callee::Fn(f)) => {
                    let sources = self.return_borrows[f.index()].clone();
                    let recv_place = receiver.and_then(|r| self.place_of(r));
                    let mut kept = Vec::new();
                    for src in &sources {
                        match src.root {
                            BorrowRoot::Receiver => {
                                self.keep_matching_lends(
                                    &receiver_carried,
                                    recv_place.as_ref(),
                                    &src.fields,
                                    &mut kept,
                                );
                            }
                            BorrowRoot::Param(k) => {
                                if let Some(list) = arg_carried_list.get(k) {
                                    let arg_place = args.get(k).and_then(|&a| {
                                        let inner = match self.ast.expr(a).kind {
                                            ExprKind::Own { place, .. } => place,
                                            _ => a,
                                        };
                                        self.place_of(inner)
                                    });
                                    self.keep_matching_lends(
                                        list,
                                        arg_place.as_ref(),
                                        &src.fields,
                                        &mut kept,
                                    );
                                }
                            }
                        }
                    }
                    kept
                }
                _ => {
                    if receiver.is_some() {
                        receiver_carried.clone()
                    } else {
                        arg_carried_list
                            .iter()
                            .flat_map(|v| v.iter().copied())
                            .collect()
                    }
                }
            }
        };
        let finished: Vec<usize> = receiver_carried
            .into_iter()
            .chain(arg_carried_list.into_iter().flatten())
            .filter(|i| !carried.contains(i))
            .collect();
        self.release(&finished);
        // `q.reset()` puts a whole owned `Qubit` back in |0⟩ (`spec/ownership.md` §9).
        if callee == Some(Callee::Builtin(Builtin::Reset))
            && let Some(target) = receiver.or_else(|| args.first().copied())
            && let Some(place) = self.place_of(target)
            && place.proj.is_empty()
            && !self.is_reference(place.root)
            && self.typed.local(place.root).ty == Ty::QUBIT
        {
            self.locals[place.root.index()].zero = true;
        }
        if use_ == Use::Discard {
            self.discarded(id, use_);
        }
        carried
    }

    /// Selects lends from `candidates` matching `fields` relative to `base_place` (narrowing
    /// a whole-place lend to `base_place + fields` when a cross-package signature specifies a
    /// field borrow like `mut[self.state] Qubit`).
    fn keep_matching_lends(
        &mut self,
        candidates: &[usize],
        base_place: Option<&Place>,
        fields: &[u32],
        kept: &mut Vec<usize>,
    ) {
        if fields.is_empty() {
            for &i in candidates {
                if !kept.contains(&i) {
                    kept.push(i);
                }
            }
            return;
        }
        let target = base_place.map(|bp| {
            let mut p = bp.clone();
            for &idx in fields {
                p.proj.push(Proj::Field(idx));
            }
            p
        });
        for &i in candidates {
            if let (Some(bp), Some(tp)) = (base_place, &target) {
                if self.lends[i].place == *bp {
                    // Narrow a whole-parameter lend to the declared/inferred field path.
                    let mut narrowed = self.lends[i].clone();
                    narrowed.place = tp.clone();
                    self.lends.push(narrowed);
                    kept.push(self.lends.len() - 1);
                    continue;
                }
                if self.lends[i].place.root == bp.root {
                    if self.overlap(&self.lends[i].place, tp) != Overlap::Disjoint
                        && !kept.contains(&i)
                    {
                        kept.push(i);
                    }
                    continue;
                }
            }
            // Transitive origin lend carried inside a reference-holding parameter (e.g. `Window`).
            if !kept.contains(&i) {
                kept.push(i);
            }
        }
    }

    /// Lends a place receiver for a `ref self` or `mut self` call, lending only the fields of
    /// `self` the method uses when it is a same-package struct method (`spec/ownership.md` §5).
    fn lend_receiver_place(
        &mut self,
        _call: ExprId,
        receiver: ExprId,
        callee: Option<Callee>,
        mutable: bool,
    ) -> Carried {
        let Some(place) = self.place_of(receiver) else {
            return Vec::new();
        };
        if let Some(Callee::Fn(f)) = callee
            && !other_package(self.ast, self.typed.fn_modules[f.index()], self.module)
            && let MethodFields::Fields(fields) = self.method_fields[f.index()].clone()
        {
            // Check mutability on the receiver when the method takes `mut self` on a classical type.
            let span = self.ast.expr(receiver).span;
            let ty = self.place_ty(&place);
            let root = place.root;
            if mutable {
                let through_ref = self.place_through_ref(&place, true);
                if through_ref == Some(false) {
                    let root_name = self.name(root).to_string();
                    let name = self.place_text(&place);
                    self.error(
                        "N015",
                        span,
                        format!(
                            "`{root_name}` is a `ref` reference, so it cannot be used to change `{name}`"
                        ),
                    );
                } else if !self.typed.types.is_pure_quantum(self.typed.types.deref(ty))
                    && through_ref.is_none()
                    && !self.typed.local(root).mutable
                {
                    let root_name = self.name(root).to_string();
                    let name = self.place_text(&place);
                    let what = if name == root_name {
                        "it".to_string()
                    } else {
                        format!("`{name}`")
                    };
                    self.error(
                        "N015",
                        span,
                        format!(
                            "`{root_name}` is not declared `let mut`, so {what} cannot be changed"
                        ),
                    )
                    .help = Some(format!("declare it with `let mut {root_name}`"));
                }
            }
            let mut out = Vec::new();
            for (field_idx, field_mut) in fields {
                let mut fp = place.clone();
                fp.proj.push(Proj::Field(field_idx));
                if let Some(i) = self.lend_place(&fp, receiver, field_mut, false, Holder::Pending) {
                    out.push(i);
                }
            }
            let origins = if self.typed.types.has_ref(self.typed.local(root).ty) {
                self.clone_origin_lends(root)
            } else {
                Vec::new()
            };
            if self.is_reference(root) && !origins.is_empty() {
                for &i in &out {
                    self.lends[i].reborrow = true;
                }
            }
            out.extend(origins);
            return out;
        }
        self.lend_and_origins(&place, receiver, mutable, false)
    }

    /// The sizes `f`'s signature subtracts in (`N - 1`), each of which must be at least 0.
    fn size_requirements(&self, f: FnId) -> Vec<Ty> {
        let info = &self.typed.fns[f.index()];
        let mut found = Vec::new();
        // Only a size parameter can be subtracted from.
        if info.generics.is_empty() {
            return found;
        }
        let sig = &info.sig;
        for &ty in sig.params.iter().chain([&sig.ret]) {
            self.typed.types.size_requirements(ty, &mut found);
        }
        found
    }

    /// N024: the sizes a call's callee subtracts in must be proven at least 0, from the
    /// sizes given and the facts here.
    fn check_size_requirements(&mut self, call: ExprId, span: Span) {
        let Some(Callee::Fn(f)) = self.typed.callees[call.index()] else {
            return;
        };
        let required = self.size_requirements(f);
        if required.is_empty() {
            return;
        }
        let types = &self.typed.types;
        let generics = &self.typed.fns[f.index()].generics;
        let given = self
            .typed
            .type_args
            .get(&call)
            .map_or(&[][..], Vec::as_slice);
        for size in required {
            let Some(form) = facts::size_form_in(types, size, generics, given) else {
                continue;
            };
            if facts::at_least_zero(&self.facts, &form) {
                continue;
            }
            let name = self.ast.name(self.ast.function(f).name).to_string();
            let shown = types.display(size, self.ast);
            let (message, help) = match form.as_constant() {
                Some(value) => (
                    format!("`{name}` needs `{shown}` to be at least 0, but here it is {value}"),
                    "give it larger sizes",
                ),
                None => (
                    format!("`{name}` needs `{shown}` to be at least 0, which is not proven here"),
                    "prove it first with `assert`, as in `assert 1 <= N`, or say it in this function's signature",
                ),
            };
            self.error("N024", span, message).help = Some(help.into());
        }
    }

    /// N013 and N019 for what is called.
    fn callee_rules(&mut self, callee: Option<Callee>, span: Span) {
        let Some(callee) = callee else { return };
        let (is_unsafe, reversible, name) = match callee {
            Callee::Builtin(b) => (b.is_unsafe(), b.reversible(), b.name().to_string()),
            Callee::Convert(_) | Callee::TryConvert(_) | Callee::Variant(..) => {
                (false, true, String::new())
            }
            Callee::FnPtr => (true, false, "C function pointer".to_string()),
            Callee::Fn(f) => (
                self.typed.fns[f.index()].sig.is_unsafe,
                self.reversible_fn(f),
                self.ast.name(self.ast.function(f).name).to_string(),
            ),
        };
        if is_unsafe && self.unsafe_depth == 0 {
            let message = if name == "release" {
                "`release` is unsafe: it trusts that the qubit is in |0⟩".to_string()
            } else if name == "qasm" {
                "`qasm` is unsafe: inline OpenQASM bypasses gate verification".to_string()
            } else if matches!(callee, Callee::FnPtr) {
                "calling an `extern \"C\" fn` pointer needs `unsafe`".to_string()
            } else if matches!(callee, Callee::Fn(f) if self.ast.function(f).extern_kind == ExternKind::CImport)
            {
                format!("`{name}` is an `extern \"C\"` function; calling it needs `unsafe`")
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
                Callee::Fn(f) => self.why_not_reversible(f),
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
        let (method, operator) = match self.ast.expr(call).kind {
            ExprKind::MethodCall { method, .. } => (self.ast.name(method).to_string(), None),
            ExprKind::Binary { op, .. } => (op.as_str().to_string(), Some(op.as_str())),
            ExprKind::Unary {
                op: UnaryOp::BitNot,
                ..
            } => ("~".to_string(), Some("~")),
            ExprKind::Unary { .. } => ("-".to_string(), Some("-")),
            _ => (String::new(), None),
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
                    if self.lin(ty) == Kind::Copyable {
                        self.read(&place, receiver_expr.span);
                    } else {
                        let text = self.expr_text(receiver);
                        self.error(
                            "N001",
                            receiver_expr.span,
                            format!("`{method}` takes ownership of `{text}`"),
                        )
                        .help = Some(match operator {
                            Some(op @ ("-" | "~"))
                                if matches!(self.ast.expr(call).kind, ExprKind::Unary { .. }) =>
                            {
                                format!("write `{op}(mv {text})`")
                            }
                            Some(op) => format!("write `mv {text} {op} …`"),
                            None => format!("write `mv {text}.{method}()`"),
                        });
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
                    .help = Some(if is_place || operator.is_some() {
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
                if is_place {
                    self.place_subexprs(receiver);
                    let callee = self.typed.callees[call.index()];
                    return self.lend_receiver_place(call, receiver, callee, mutable);
                }
                // The result of a chain or expression is lent on as it is.
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
        let types = &self.typed.types;
        let array = types.deref(self.typed.ty(base));
        // The length: a number, or a size parameter, whose value only each copy knows.
        let (elem, len, size) = match *types.kind(array) {
            TyKind::Array(elem, len) => (elem, i64::try_from(len).unwrap_or(i64::MAX), None),
            TyKind::SizedArray(elem, size) => (elem, i64::MAX, Some(size)),
            _ => return,
        };
        if self.lin(elem) != Kind::Linear {
            return;
        }
        let form = facts::linear(self.ast, self.typed, index);
        let (lo, hi) = match &form {
            Some(form) => facts::interval(&self.facts, form),
            None => (None, None),
        };
        let below = match (size, &form) {
            (None, _) => hi.is_some_and(|hi| hi < len),
            (Some(size), Some(form)) => facts::size_form(&self.typed.types, size)
                .is_some_and(|size| facts::below(&self.facts, form, &size)),
            (Some(_), None) => false,
        };
        // At least zero: by its interval, or, against a size, by its difference (`N - 1 - i`).
        let at_least_zero = lo.is_some_and(|lo| lo >= 0)
            || below
                && form
                    .as_ref()
                    .is_some_and(|f| facts::below(&self.facts, &facts::constant_form(-1), f));
        if below && at_least_zero {
            return;
        }
        let lo = if at_least_zero { lo.max(Some(0)) } else { lo };
        let span = self.ast.expr(index).span;
        let text = self.expr_text(index);
        let array_text = self.expr_text(base);
        let len_text = size.map_or(len.to_string(), |size| types.display(size, self.ast));
        let len = len_text.as_str();
        let message = match (lo, hi) {
            (Some(lo), _) if lo < 0 => {
                format!("`{text}` can be {lo}, outside `{array_text}` (length {len})")
            }
            (_, Some(hi)) if size.is_none() && !below => {
                format!("`{text}` can be {hi}, outside `{array_text}` (length {len})")
            }
            _ => format!("cannot prove `{text}` is within `{array_text}` (length {len})"),
        };
        let condition = if lo.is_some_and(|lo| lo >= 0) {
            format!("{text} < {len}")
        } else {
            format!("0 <= {text} and {text} < {len}")
        };
        self.error("N018", span, message).help = Some(format!(
            "loop over a range that fits, or `assert {condition}`"
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

    /// Whether accessing the value at `place` goes through a reference (`Some(true)` if every
    /// reference along the path is `mut`, `Some(false)` if any is `ref`, `None` if direct).
    /// When `deref_final` is false, a reference type at the leaf itself is not dereferenced
    /// (for rebinding `r = ref b` or `w.q = ref b`).
    fn place_through_ref(&self, place: &Place, deref_final: bool) -> Option<bool> {
        let types = &self.typed.types;
        let mut ty = self.typed.local(place.root).ty;
        let mut through: Option<bool> = None;
        let mut step_ref = |ty: &mut Ty| {
            while let TyKind::Ref { mutable, inner } = *types.kind(*ty) {
                through = Some(through.unwrap_or(true) && mutable);
                *ty = inner;
            }
        };
        if !place.proj.is_empty() || deref_final {
            step_ref(&mut ty);
        }
        for (i, proj) in place.proj.iter().enumerate() {
            ty = match (types.kind(ty), proj) {
                (&TyKind::Struct(s), Proj::Field(idx)) => types.structs[s.index()]
                    .fields
                    .get(*idx as usize)
                    .map_or(Ty::ERROR, |&(_, t)| t),
                (TyKind::Tuple(elems), Proj::Field(idx)) => {
                    elems.get(*idx as usize).copied().unwrap_or(Ty::ERROR)
                }
                (
                    &TyKind::Array(elem, _) | &TyKind::SizedArray(elem, _) | &TyKind::Vec(elem),
                    Proj::Index(_),
                ) => elem,
                _ => return through,
            };
            if i + 1 < place.proj.len() || deref_final {
                step_ref(&mut ty);
            }
        }
        through
    }

    /// The type of a place, following its projections from the root local.
    fn place_ty(&self, place: &Place) -> Ty {
        let types = &self.typed.types;
        let mut ty = types.deref(self.typed.local(place.root).ty);
        for proj in &place.proj {
            ty = match (types.kind(ty), proj) {
                (&TyKind::Struct(s), Proj::Field(i)) => types.structs[s.index()]
                    .fields
                    .get(*i as usize)
                    .map_or(Ty::ERROR, |&(_, t)| t),
                (TyKind::Tuple(elems), Proj::Field(i)) => {
                    elems.get(*i as usize).copied().unwrap_or(Ty::ERROR)
                }
                (
                    &TyKind::Array(elem, _) | &TyKind::SizedArray(elem, _) | &TyKind::Vec(elem),
                    Proj::Index(_),
                ) => elem,
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
        if self.lin(ty) != Kind::Linear {
            return true;
        }
        let child = |proj: Proj| {
            let mut p = place.clone();
            p.proj.push(proj);
            p
        };
        match types.kind(ty) {
            // An enum is moved whole: its parts are only taken by `match mv`.
            &TyKind::Struct(s) if types.structs[s.index()].variants.is_some() => false,
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

    /// Whether `lend` is active at the current program point under Polonius-style NLL:
    /// a variable's lend is active only while that variable is still live on a future path.
    fn lend_active(&self, lend: &Lend) -> bool {
        if !lend.alive {
            return false;
        }
        match lend.holder {
            Holder::Local(l) => match &self.liveness {
                Some(live) => live.contains(&self.current_live, l),
                None => true,
            },
            Holder::Pending | Holder::Control | Holder::With => true,
        }
    }

    /// An active lend that conflicts with using `place` (for changing, if `mutable`).
    fn conflicting_lend(&self, place: &Place, mutable: bool) -> Option<usize> {
        if let Some(i) = self.lends.iter().position(|l| {
            self.lend_active(l)
                && (mutable || l.mutable)
                && self.overlap(place, &l.place) != Overlap::Disjoint
        }) {
            return Some(i);
        }
        // If `place.root` is a local reference variable, check conflicts against its origin
        // storage (excluding its own lend) so aliasing through a dead intermediate is caught.
        if self.is_reference(place.root) {
            let root = place.root;
            for origin in &self.lends {
                if origin.alive
                    && !origin.reborrow
                    && origin.holder == Holder::Local(root)
                    && let Some(i) = self.lends.iter().position(|l| {
                        self.lend_active(l)
                            && l.holder != Holder::Local(root)
                            && (mutable || l.mutable)
                            && self.overlap(&origin.place, &l.place) != Overlap::Disjoint
                    })
                {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Checks `place` (and its origin storage, if `place.root` is a local reference) for
    /// conflicting active lends, reporting N005 or N017.
    fn check_lend_conflicts(
        &mut self,
        place: &Place,
        mutable: bool,
        span: Span,
        skip_holder: Option<Holder>,
    ) {
        let name = self.place_text(place);
        for i in 0..self.lends.len() {
            let other = &self.lends[i];
            if !self.lend_active(other)
                || Some(other.holder) == skip_holder
                || !(mutable || other.mutable)
            {
                continue;
            }
            match self.overlap(place, &other.place) {
                Overlap::Disjoint => continue,
                Overlap::Overlaps => {
                    let other_span = other.span;
                    let message = match other.holder {
                        Holder::Control => format!("`{name}` is a control of this block"),
                        Holder::With => {
                            format!("`{name}` was used in `with` and can only be read in `do`")
                        }
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
            return;
        }
        if skip_holder.is_none() && self.is_reference(place.root) {
            let root = place.root;
            let origins: Vec<Place> = self
                .lends
                .iter()
                .filter(|l| l.alive && !l.reborrow && l.holder == Holder::Local(root))
                .map(|l| l.place.clone())
                .collect();
            for origin in origins {
                self.check_lend_conflicts(&origin, mutable, span, Some(Holder::Local(root)));
            }
        }
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
            .position(|l| self.lend_active(l) && self.overlap(place, &l.place) != Overlap::Disjoint)
        {
            let lend_span = self.lends[i].span;
            let holder = match self.lends[i].holder {
                Holder::Local(l) => format!(" to `{}`", self.name(l)),
                Holder::Control => " as a control".into(),
                Holder::With => " to `with`".into(),
                Holder::Pending => String::new(),
            };
            self.error(
                "N006",
                span,
                format!("`{name}` is moved while it is lent{holder}"),
            )
            .with_label_mut(lend_span, "lent here");
        }
        let linear = self.lin(ty) != Kind::Copyable;
        if linear && self.locals[place.root.index()].loop_depth < self.loops.len() as u32 {
            self.error(
                "N010",
                span,
                format!("`{name}` would be moved on every iteration of the loop"),
            )
            .help = Some("move it after the loop, or create it inside the loop".into());
        }
        if self.lin(ty) == Kind::Linear && !self.reversible.is_empty() {
            let allowed_in_with = self.in_with.last().is_some_and(|&w| {
                self.reversible.last() == Some(&w) && self.locals[place.root.index()].order >= w
            });
            if !allowed_in_with {
                self.error("N013", span, format!("moving `{name}` is not reversible"));
            }
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
        let classical = !self.typed.types.is_pure_quantum(self.typed.types.deref(ty));
        let through_ref = self.place_through_ref(place, true);
        if mutable && through_ref == Some(false) {
            let root_name = self.name(root).to_string();
            let what = if self.is_reference(root) {
                root_name
            } else {
                name.clone()
            };
            self.error(
                "N015",
                span,
                format!("`{what}` is a `ref` reference, so it cannot be used to change `{name}`"),
            );
        } else if mutable && classical {
            if implicit_arg {
                self.error(
                    "N016",
                    span,
                    format!("this call changes `{name}`, which holds classical data"),
                )
                .help = Some(format!("write `mut {name}` to show it is changed"));
            } else if through_ref.is_none() && !self.typed.local(root).mutable {
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
        self.check_lend_conflicts(place, mutable, span, None);
        if mutable {
            self.locals[root.index()].zero = false;
        }
        if !self.with_touched.is_empty() && self.lin(self.typed.types.deref(ty)) != Kind::Copyable {
            let order = self.locals[root.index()].order;
            for (with_order, touched) in &mut self.with_touched {
                if order < *with_order {
                    touched.push((place.clone(), span));
                }
            }
        }
        self.lends.push(Lend {
            place: place.clone(),
            mutable,
            holder,
            span,
            alive: true,
            reborrow: false,
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
                    let Some(&(name, field_ty)) = types.structs[s.index()].fields.get(*i as usize)
                    else {
                        break;
                    };
                    text.push('.');
                    text.push_str(self.ast.name(name));
                    ty = field_ty;
                }
                (TyKind::Tuple(elems), Proj::Field(i)) => {
                    text.push_str(&format!(".{i}"));
                    ty = elems[*i as usize];
                }
                (&TyKind::Array(elem, _) | &TyKind::SizedArray(elem, _), Proj::Index(index)) => {
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
        for &(var, coeff) in form.terms() {
            let name = match var {
                Var::Local(local) => self.name(local),
                Var::Size(size) => &self.typed.types.display(size, self.ast),
            };
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
