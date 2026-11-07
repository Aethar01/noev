//! Lowers a checked syntax tree to the IR.
//!
//! Runs only on programs that passed type and ownership checking, so it does not report
//! errors. Expressions are lowered into a destination place (or nowhere, when their value is
//! thrown away). Kets, bras and the standard library are expanded here: a ket is allocation
//! plus gates, a measurement test is gates, measurements and comparisons, and `discard` is
//! `reset` then `release`, as its reference definition in `spec/std.md` says.

use noev_sema::check::{Adjust, Callee, LocalId, Res};
use noev_sema::prelude::{Builtin, Gate};
use noev_sema::{Circuits, Ty, TyKind, Typed, Types};
use noev_syntax::ast::{self as ast, ExprId, ExprKind, OwnKind, StmtKind};
use noev_syntax::state::{Label, Pauli, labels};
use noev_syntax::{Ast, Span};

use crate::ir::*;

/// Lowers every function of a checked program, given which of its runs are circuits
/// ([`noev_sema::check_circuits`]). `source` supplies the text of kets and bras.
pub fn lower(syntax: &Ast, typed: &Typed, circuits: &Circuits, source: &str) -> Program {
    let struct_names = typed
        .types
        .structs
        .iter()
        .map(|s| syntax.name(s.name).to_string())
        .collect();
    let struct_fields = typed
        .types
        .structs
        .iter()
        .map(|s| {
            s.fields
                .iter()
                .map(|&(name, _)| syntax.name(name).to_string())
                .collect()
        })
        .collect();
    let variant_names = typed
        .types
        .structs
        .iter()
        .map(|s| {
            s.variants
                .iter()
                .flatten()
                .map(|v| syntax.name(v.name).to_string())
                .collect()
        })
        .collect();
    let variant_fields = typed
        .types
        .structs
        .iter()
        .map(|s| {
            s.variants
                .iter()
                .flatten()
                .map(|v| {
                    v.names
                        .iter()
                        .map(|&n| syntax.name(n).to_string())
                        .collect()
                })
                .collect()
        })
        .collect();
    // Lowering creates reference types for implicit lends, so it works on its own copy.
    let mut types = typed.types.clone();
    let mut functions = Vec::with_capacity(syntax.all_fns().len());
    // `sample` runs become functions of their own, numbered after the program's.
    let mut runs = Vec::new();
    let mut main = None;
    for (index, decl) in syntax.all_fns().iter().enumerate() {
        let f = ast::FnId::from_index(index);
        let owner = typed.fns[index].owner;
        let name = match owner {
            Some(s) => format!(
                "{}::{}",
                syntax.name(typed.types.structs[s.index()].name),
                syntax.name(decl.name)
            ),
            None => syntax.name(decl.name).to_string(),
        };
        if owner.is_none() && name == "main" {
            main = Some(FuncId(index as u32));
        }
        let shared = Shared {
            types: &mut types,
            circuits,
            runs: &mut runs,
            first_run: syntax.all_fns().len() as u32,
        };
        let mut function = Builder::new(syntax, typed, shared, source, f, name).build();
        function.sampled = circuits.fns[index].sampled();
        functions.push(function);
    }
    functions.extend(runs);
    Program {
        functions,
        main,
        types,
        struct_names,
        struct_fields,
        variant_names,
        variant_fields,
    }
}

/// What every builder of a program shares.
struct Shared<'a> {
    types: &'a mut Types,
    circuits: &'a Circuits,
    /// The functions made for `sample` runs so far.
    runs: &'a mut Vec<Function>,
    /// The id of the first of them.
    first_run: u32,
}

struct Builder<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    types: &'a mut Types,
    circuits: &'a Circuits,
    runs: &'a mut Vec<Function>,
    first_run: u32,
    source: &'a str,
    body: Option<ast::BlockId>,
    function: Function,
    current: Block,
    /// IR locals for source locals, indexed by [`LocalId::index`].
    locals: Vec<Option<Local>>,
    /// `(continue, break)` targets of enclosing loops.
    loops: Vec<(Block, Block)>,
    /// The span of the expression being lowered.
    span: Span,
}

impl<'a> Builder<'a> {
    fn new(
        syntax: &'a Ast,
        typed: &'a Typed,
        shared: Shared<'a>,
        source: &'a str,
        f: ast::FnId,
        name: String,
    ) -> Self {
        let decl = syntax.function(f);
        let info = &typed.fns[f.index()];
        let mut builder =
            Builder::empty(syntax, typed, shared, source, name, decl.body, info.sig.ret);
        builder.function.span = decl.span;
        match (info.self_local, info.receiver_ty) {
            (Some(local), _) => {
                builder.declare(local);
                builder.function.params += 1;
            }
            // A method whose body never mentions `self` still takes the receiver.
            (None, Some(ty)) => {
                builder.function.locals.push(LocalDecl {
                    ty,
                    name: Some("self".into()),
                });
                builder.function.params += 1;
            }
            (None, None) => {}
        }
        for &local in &info.params {
            builder.declare(local);
            builder.function.params += 1;
        }
        builder
    }

    /// A builder for a function returning `ret`, with no parameters yet.
    fn empty(
        syntax: &'a Ast,
        typed: &'a Typed,
        shared: Shared<'a>,
        source: &'a str,
        name: String,
        body: Option<ast::BlockId>,
        ret: Ty,
    ) -> Self {
        let mut builder = Builder {
            ast: syntax,
            typed,
            types: shared.types,
            circuits: shared.circuits,
            runs: shared.runs,
            first_run: shared.first_run,
            source,
            body,
            function: Function {
                name,
                params: 0,
                locals: vec![LocalDecl {
                    ty: ret,
                    name: None,
                }],
                blocks: Vec::new(),
                span: body.map_or(Span::default(), |b| syntax.block(b).span),
                sampled: false,
            },
            current: Block(0),
            locals: vec![None; typed.locals.len()],
            loops: Vec::new(),
            span: Span::default(),
        };
        builder.current = builder.new_block();
        builder
    }

    /// How to record the `if` or `while` `id` as classical control in a circuit, if it
    /// becomes that.
    fn control(&self, id: ExprId, join: Block, repeats: bool) -> Option<Box<Control>> {
        let carried = self.circuits.control(id)?;
        Some(Box::new(Control {
            join,
            carried: carried.iter().map(|&l| self.local(l)).collect(),
            repeats,
        }))
    }

    /// Makes the function for a `sample` run, whose parameters are the locals from outside it
    /// that it reads. Returns it and those locals.
    fn run_function(&mut self, id: ExprId, body: ast::BlockId) -> (FuncId, Vec<LocalId>) {
        let run = self.typed.runs[&id];
        let captures = self.captures(body, run.first_local);
        let name = format!("{}::sample{}", self.function.name, self.runs.len() + 1);
        let shared = Shared {
            types: &mut *self.types,
            circuits: self.circuits,
            runs: &mut *self.runs,
            first_run: self.first_run,
        };
        let mut builder = Builder::empty(
            self.ast,
            self.typed,
            shared,
            self.source,
            name,
            Some(body),
            run.ty,
        );
        builder.function.sampled = self.circuits.runs[&id].sampled();
        for &local in &captures {
            builder.declare(local);
            builder.function.params += 1;
        }
        let function = builder.build();
        self.runs.push(function);
        (
            FuncId(self.first_run + self.runs.len() as u32 - 1),
            captures,
        )
    }

    /// The locals declared before `first` that a block mentions, nested runs included.
    fn captures(&self, body: ast::BlockId, first: u32) -> Vec<LocalId> {
        let span = self.ast.block(body).span;
        let mut found = Vec::new();
        for (index, expr) in self.ast.all_exprs().iter().enumerate() {
            if expr.span.start < span.start || expr.span.end > span.end {
                continue;
            }
            match expr.kind {
                ExprKind::Name(_) => {
                    if let Res::Local(l) = self.typed.res[index]
                        && l.index() < first as usize
                    {
                        found.push(l);
                    }
                }
                ExprKind::StructLit { fields, .. } => {
                    for field in self.ast.list(fields).iter().filter(|f| f.value.is_none()) {
                        let outer = (0..first as usize)
                            .rev()
                            .map(LocalId::from_index)
                            .find(|&l| {
                                self.locals[l.index()].is_some()
                                    && self.typed.local(l).name == field.name
                            });
                        found.extend(outer);
                    }
                }
                _ => {}
            }
        }
        found.sort();
        found.dedup();
        found
    }

    fn build(mut self) -> Function {
        if let Some(body) = self.body {
            let dest = (self.function.locals[0].ty != Ty::UNIT).then(|| Place::local(Local(0)));
            self.block_into(body, dest);
            self.terminate(Terminator::Return);
        }
        remove_unreachable(&mut self.function);
        self.function
    }

    // Building blocks.

    fn new_block(&mut self) -> Block {
        self.function.blocks.push(BasicBlock {
            stmts: Vec::new(),
            spans: Vec::new(),
            term: Terminator::Unreachable,
        });
        Block(self.function.blocks.len() as u32 - 1)
    }

    fn push(&mut self, stmt: Stmt) {
        let block = &mut self.function.blocks[self.current.index()];
        block.stmts.push(stmt);
        block.spans.push(self.span);
    }

    fn assign(&mut self, place: Place, value: Rvalue) {
        self.push(Stmt::Assign(place, value));
    }

    /// Ends the current block. Code after it goes into a fresh block, which is unreachable
    /// unless something jumps to it.
    fn terminate(&mut self, term: Terminator) {
        self.function.blocks[self.current.index()].term = term;
        self.current = self.new_block();
    }

    /// Ends the current block with `term` and continues in `next`.
    fn terminate_into(&mut self, term: Terminator, next: Block) {
        self.function.blocks[self.current.index()].term = term;
        self.current = next;
    }

    fn temp(&mut self, ty: Ty) -> Place {
        self.function.locals.push(LocalDecl { ty, name: None });
        Place::local(Local(self.function.locals.len() as u32 - 1))
    }

    fn declare(&mut self, local: LocalId) -> Local {
        let source = self.typed.local(local);
        self.function.locals.push(LocalDecl {
            ty: source.ty,
            name: Some(self.ast.name(source.name).to_string()),
        });
        let ir = Local(self.function.locals.len() as u32 - 1);
        self.locals[local.index()] = Some(ir);
        ir
    }

    fn local(&self, local: LocalId) -> Local {
        self.locals[local.index()].expect("locals are declared before use")
    }

    fn ty(&self, id: ExprId) -> Ty {
        self.typed.ty(id)
    }

    fn is_ref(&self, ty: Ty) -> bool {
        matches!(self.types.kind(ty), TyKind::Ref { .. })
    }

    fn copyable(&self, ty: Ty) -> bool {
        self.types.linearity(ty) == noev_sema::ty::Kind::Copyable
    }

    fn text(&self, span: Span) -> &'a str {
        let source: &'a str = self.source;
        &source[span.start as usize..span.end as usize]
    }
}

/// Removes blocks that cannot be reached from the entry, renumbering the rest. Lowering
/// leaves one after every `return`, `break` and `continue`.
fn remove_unreachable(function: &mut Function) {
    let blocks = &function.blocks;
    let mut reachable = vec![false; blocks.len()];
    let mut stack = vec![0usize];
    while let Some(b) = stack.pop() {
        if std::mem::replace(&mut reachable[b], true) {
            continue;
        }
        stack.extend(successors(&blocks[b].term).map(|s| s.index()));
    }
    let mut renumber = vec![Block(0); blocks.len()];
    let mut next = 0;
    for (b, &keep) in reachable.iter().enumerate() {
        if keep {
            renumber[b] = Block(next);
            next += 1;
        }
    }
    let old = std::mem::take(&mut function.blocks);
    for (b, mut block) in old.into_iter().enumerate() {
        if !reachable[b] {
            continue;
        }
        for target in successors_mut(&mut block.term) {
            *target = renumber[target.index()];
        }
        function.blocks.push(block);
    }
}

fn successors(term: &Terminator) -> impl Iterator<Item = Block> {
    let (a, b) = match *term {
        Terminator::Goto(next)
        | Terminator::Call { next, .. }
        | Terminator::Assert { next, .. } => (Some(next), None),
        Terminator::Branch {
            then, otherwise, ..
        } => (Some(then), Some(otherwise)),
        Terminator::Return | Terminator::Unreachable => (None, None),
    };
    a.into_iter().chain(b)
}

fn successors_mut(term: &mut Terminator) -> Vec<&mut Block> {
    match term {
        Terminator::Goto(next)
        | Terminator::Call { next, .. }
        | Terminator::Assert { next, .. } => vec![next],
        Terminator::Branch {
            then,
            otherwise,
            control,
            ..
        } => {
            let mut blocks = vec![then, otherwise];
            blocks.extend(control.as_mut().map(|c| &mut c.join));
            blocks
        }
        Terminator::Return | Terminator::Unreachable => vec![],
    }
}

// Blocks and statements.

impl Builder<'_> {
    fn block_into(&mut self, block: ast::BlockId, dest: Option<Place>) {
        let ast = self.ast;
        let stmts = ast.list(ast.block(block).stmts);
        for (i, &stmt) in stmts.iter().enumerate() {
            match ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    self.expr_into(expr, dest.clone());
                }
                _ => self.stmt(stmt),
            }
        }
    }

    fn stmt(&mut self, id: ast::StmtId) {
        let ast = self.ast;
        let stmt = *ast.stmt(id);
        match stmt.kind {
            StmtKind::Let { pat, init, .. } => match ast.pat(pat).kind {
                ast::PatKind::Name(_) => {
                    let local = self.typed.pat_locals[pat.index()].expect("checked");
                    let local = self.declare(local);
                    self.expr_into(init, Some(Place::local(local)));
                }
                _ => {
                    let temp = self.temp(self.ty(init));
                    self.expr_into(init, Some(temp.clone()));
                    self.bind(pat, temp);
                }
            },
            StmtKind::Assign { op, place, value } => {
                let target = self.place_deref(place);
                let value = self.operand(value);
                let rvalue = match op {
                    ast::AssignOp::Assign => Rvalue::Use(value),
                    ast::AssignOp::Compound(op) => {
                        Rvalue::Binary(op, Operand::Copy(target.clone()), value)
                    }
                };
                self.assign(target, rvalue);
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    self.expr_into(value, Some(Place::local(Local(0))));
                }
                self.terminate(Terminator::Return);
            }
            StmtKind::Break | StmtKind::Continue => {
                let &(cont, brk) = self.loops.last().expect("checked: inside a loop");
                let target = if stmt.kind == StmtKind::Break {
                    brk
                } else {
                    cont
                };
                self.terminate(Terminator::Goto(target));
            }
            StmtKind::Assert(cond) => {
                let cond = self.operand(cond);
                let next = self.new_block();
                self.terminate_into(
                    Terminator::Assert {
                        cond,
                        next,
                        span: stmt.span,
                    },
                    next,
                );
            }
            StmtKind::Expr { expr, .. } => self.expr_into(expr, None),
        }
    }

    /// `match`: the scrutinee once, then each arm's test in turn, going to the first arm
    /// that fits. A variant's data is only looked at once its variant has matched. The checker
    /// made sure some arm always fits.
    fn match_into(
        &mut self,
        id: ExprId,
        scrutinee: ExprId,
        arms: ast::List<ast::Arm>,
        dest: Option<Place>,
    ) {
        let ty = self.ty(scrutinee);
        let held = self.temp(ty);
        let op = self.operand(scrutinee);
        self.assign(held.clone(), Rvalue::Use(op));
        // Through a reference, the patterns look at what it refers to.
        let value = if self.is_ref(ty) {
            held.with(Proj::Deref)
        } else {
            held
        };
        let join = self.new_block();
        // On a measurement result in a circuit, each test is an `if` recorded up to `join`.
        let control = self.control(id, join, false);
        for arm in self.ast.list(arms).to_vec() {
            let next = self.new_block();
            self.test_or_jump(arm.pat, &value, next, &control);
            self.bind(arm.pat, value.clone());
            self.expr_into(arm.body, dest.clone());
            self.terminate_into(Terminator::Goto(join), next);
        }
        self.terminate_into(Terminator::Unreachable, join);
    }

    /// Continues in a fresh block if the value at `place` fits `pat`, and goes to `fail` if it
    /// does not. Each test's branch is `control`.
    fn test_or_jump(
        &mut self,
        pat: ast::PatId,
        place: &Place,
        fail: Block,
        control: &Option<Box<Control>>,
    ) {
        let ast = self.ast;
        let ty = self.typed.pat_types[pat.index()];
        let check = |this: &mut Self, cond: Operand| {
            let pass = this.new_block();
            this.terminate_into(
                Terminator::Branch {
                    cond,
                    then: pass,
                    otherwise: fail,
                    control: control.clone(),
                },
                pass,
            );
        };
        match ast.pat(pat).kind {
            ast::PatKind::Name(_) | ast::PatKind::Wildcard => {}
            ast::PatKind::Literal(literal) => {
                let constant = match literal {
                    ast::PatLiteral::Int(v) if self.types.deref(ty) == Ty::BIT => {
                        Operand::Const(Const::Bit(v == 1))
                    }
                    ast::PatLiteral::Int(v) => Operand::Const(Const::Int(v)),
                    ast::PatLiteral::Bool(b) => Operand::Const(Const::Bool(b)),
                    ast::PatLiteral::Str(text) => {
                        let temp = self.temp(Ty::STRING);
                        let text = ast.name(text).to_string();
                        self.assign(temp.clone(), Rvalue::Format(vec![FormatPart::Text(text)]));
                        Operand::Copy(temp)
                    }
                };
                let cond = self.compare(Operand::Copy(place.clone()), constant);
                check(self, cond);
            }
            ast::PatKind::Variant { data, .. } => {
                let Some(index) = self.typed.pat_variants[pat.index()] else {
                    return;
                };
                let tag = self.temp(Ty::INT);
                self.assign(tag.clone(), Rvalue::Tag(Operand::Copy(place.clone())));
                let cond =
                    self.compare(Operand::Copy(tag), Operand::Const(Const::Int(index as i64)));
                check(self, cond);
                for (i, p) in self.variant_parts(pat, data) {
                    self.test_or_jump(p, &place.with(Proj::Field(i)), fail, control);
                }
            }
            ast::PatKind::Tuple(pats) | ast::PatKind::Array(pats) => {
                let is_tuple = matches!(ast.pat(pat).kind, ast::PatKind::Tuple(_));
                for (i, &p) in ast.list(pats).iter().enumerate() {
                    let part = if is_tuple {
                        place.with(Proj::Field(i as u32))
                    } else {
                        place.with(Proj::Index(Operand::Const(Const::Int(i as i64))))
                    };
                    self.test_or_jump(p, &part, fail, control);
                }
            }
        }
    }

    /// The patterns for a variant's data, each with the position of the value it matches.
    fn variant_parts(&self, pat: ast::PatId, data: ast::PatData) -> Vec<(u32, ast::PatId)> {
        let ast = self.ast;
        match data {
            ast::PatData::None => Vec::new(),
            ast::PatData::Tuple(pats) => ast
                .list(pats)
                .iter()
                .enumerate()
                .map(|(i, &p)| (i as u32, p))
                .collect(),
            ast::PatData::Fields { fields, .. } => {
                let ty = self.types.deref(self.typed.pat_types[pat.index()]);
                let Some(index) = self.typed.pat_variants[pat.index()] else {
                    return Vec::new();
                };
                let Some(variants) = self.types.variants(ty) else {
                    return Vec::new();
                };
                let names = &variants[index as usize].names;
                ast.list(fields)
                    .iter()
                    .filter_map(|f| {
                        let i = names.iter().position(|&n| n == f.name)?;
                        Some((i as u32, f.pat?))
                    })
                    .collect()
            }
        }
    }

    /// A gate without angles on one qubit.
    fn gate_on(&mut self, gate: Gate, adjoint: bool, qubit: &Place) {
        self.push(Stmt::Gate {
            gate,
            adjoint,
            qubits: vec![qubit.clone()],
            params: Vec::new(),
        });
    }

    /// `a == b`, as a `Bool` operand.
    fn compare(&mut self, a: Operand, b: Operand) -> Operand {
        let result = self.temp(Ty::BOOL);
        self.assign(result.clone(), Rvalue::Binary(ast::BinaryOp::Eq, a, b));
        Operand::Copy(result)
    }

    /// Binds a destructuring pattern to the parts of `value`.
    fn bind(&mut self, pat: ast::PatId, value: Place) {
        match self.ast.pat(pat).kind {
            ast::PatKind::Name(_) => {
                let local = self.typed.pat_locals[pat.index()].expect("checked");
                let ty = self.typed.local(local).ty;
                // In a lent `match`, a name for a part that can't be copied refers to it.
                let by_ref = match *self.types.kind(ty) {
                    TyKind::Ref { mutable, .. }
                        if !self.is_ref(self.typed.pat_types[pat.index()]) =>
                    {
                        Some(mutable)
                    }
                    _ => None,
                };
                let local = self.declare(local);
                let value = match by_ref {
                    Some(mutable) => Rvalue::Ref {
                        mutable,
                        place: value,
                    },
                    None => Rvalue::Use(self.read(value, ty)),
                };
                self.assign(Place::local(local), value);
            }
            ast::PatKind::Wildcard => {}
            ast::PatKind::Tuple(pats) => {
                for (i, &p) in self.ast.list(pats).iter().enumerate() {
                    self.bind(p, value.with(Proj::Field(i as u32)));
                }
            }
            ast::PatKind::Array(pats) => {
                for (i, &p) in self.ast.list(pats).iter().enumerate() {
                    let index = Operand::Const(Const::Int(i as i64));
                    self.bind(p, value.with(Proj::Index(index)));
                }
            }
            ast::PatKind::Literal(_) => {}
            ast::PatKind::Variant { data, .. } => {
                for (i, p) in self.variant_parts(pat, data) {
                    self.bind(p, value.with(Proj::Field(i)));
                }
            }
        }
    }

    /// Reads a place: copied if its type can be copied, moved otherwise.
    fn read(&self, place: Place, ty: Ty) -> Operand {
        if self.copyable(ty) || self.is_ref(ty) {
            Operand::Copy(place)
        } else {
            Operand::Move(place)
        }
    }
}

// Expressions.

impl Builder<'_> {
    fn is_place_expr(&self, id: ExprId) -> bool {
        match self.ast.expr(id).kind {
            ExprKind::Name(_) => matches!(self.typed.res[id.index()], Res::Local(_)),
            ExprKind::Field { base, .. }
            | ExprKind::TupleIndex { base, .. }
            | ExprKind::Index { base, .. } => self.is_place_expr(base),
            _ => false,
        }
    }

    /// The place an expression names, without following a reference it holds. Expressions
    /// that are not places are computed into a temporary.
    fn place(&mut self, id: ExprId) -> Place {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Name(_) => {
                if let Res::Local(local) = self.typed.res[id.index()] {
                    return Place::local(self.local(local));
                }
            }
            ExprKind::Field { base, name } if self.is_place_expr(base) => {
                let base_place = self.place_deref(base);
                let struct_ty = self.types.deref(self.ty(base));
                if let TyKind::Struct(s) = *self.types.kind(struct_ty) {
                    let index = self.types.structs[s.index()]
                        .fields
                        .iter()
                        .position(|(n, _)| *n == name)
                        .expect("checked");
                    return base_place.with(Proj::Field(index as u32));
                }
            }
            ExprKind::TupleIndex { base, index } if self.is_place_expr(base) => {
                return self.place_deref(base).with(Proj::Field(index));
            }
            ExprKind::Index { base, index } if self.is_place_expr(base) => {
                let base_place = self.place_deref(base);
                let index = self.index_operand(index);
                return base_place.with(Proj::Index(index));
            }
            _ => {}
        }
        let temp = self.temp(self.ty(id));
        self.expr_into(id, Some(temp.clone()));
        temp
    }

    /// Like [`Builder::place`], but through a reference: the place a reference refers to.
    fn place_deref(&mut self, id: ExprId) -> Place {
        let place = self.place(id);
        if self.is_ref(self.ty(id)) {
            place.with(Proj::Deref)
        } else {
            place
        }
    }

    /// An index: a constant, or a local holding the computed value.
    fn index_operand(&mut self, index: ExprId) -> Operand {
        match self.operand(index) {
            op @ Operand::Const(_) => op,
            Operand::Copy(place) if place.proj.is_empty() => Operand::Copy(place),
            other => {
                let temp = self.temp(Ty::INT);
                self.assign(temp.clone(), Rvalue::Use(other));
                Operand::Copy(temp)
            }
        }
    }

    /// An expression's value as an operand, applying any implicit lend or dereference.
    fn operand(&mut self, id: ExprId) -> Operand {
        let ast = self.ast;
        let expr = *ast.expr(id);
        let adjust = self.typed.adjust[id.index()];
        match expr.kind {
            ExprKind::Int(v) if self.ty(id) == Ty::BIT => {
                return Operand::Const(Const::Bit(v == 1));
            }
            ExprKind::Int(v) => return Operand::Const(Const::Int(v as i64)),
            ExprKind::Float(v) => return Operand::Const(Const::Float(v)),
            ExprKind::Bool(v) => return Operand::Const(Const::Bool(v)),
            ExprKind::Unit => return Operand::Const(Const::Unit),
            _ => {}
        }
        if let Adjust::Lend { mutable } = adjust {
            let place = self.lend_target(id);
            let ref_ty = self.types.reference(mutable, self.ty(id));
            let temp = self.temp(ref_ty);
            self.assign(temp.clone(), Rvalue::Ref { mutable, place });
            return Operand::Copy(temp);
        }
        if self.is_place_expr(id) {
            let ty = self.ty(id);
            return match adjust {
                Adjust::Deref => {
                    let target = self.types.deref(ty);
                    let place = self.place(id).with(Proj::Deref);
                    self.read(place, target)
                }
                _ => {
                    let place = self.place(id);
                    self.read(place, ty)
                }
            };
        }
        match expr.kind {
            ExprKind::Own {
                kind: OwnKind::Mv,
                place,
            } => {
                let ty = self.ty(place);
                let p = self.place(place);
                return self.read(p, ty);
            }
            ExprKind::Own { kind, place } => {
                let mutable = kind == OwnKind::Mut;
                let target = self.place_deref(place);
                let temp = self.temp(self.ty(id));
                self.assign(
                    temp.clone(),
                    Rvalue::Ref {
                        mutable,
                        place: target,
                    },
                );
                return Operand::Copy(temp);
            }
            _ => {}
        }
        let ty = self.ty(id);
        let temp = self.temp(ty);
        self.expr_into(id, Some(temp.clone()));
        let ty = match adjust {
            Adjust::Deref => {
                let target = self.types.deref(ty);
                return self.read(temp.with(Proj::Deref), target);
            }
            _ => ty,
        };
        self.read(temp, ty)
    }

    /// An expression's value, following any references, for printing.
    fn value_operand(&mut self, id: ExprId) -> Operand {
        let op = self.operand(id);
        if !self.is_ref(self.ty(id)) {
            return op;
        }
        let mut place = match op {
            Operand::Copy(place) | Operand::Move(place) => place,
            constant @ Operand::Const(_) => return constant,
        };
        // Follow every level of reference: `ref ref Int` prints as an `Int`.
        let mut ty = self.ty(id);
        while let TyKind::Ref { inner, .. } = *self.types.kind(ty) {
            place = place.with(Proj::Deref);
            ty = inner;
        }
        Operand::Copy(place)
    }

    /// The place an implicit lend refers to.
    fn lend_target(&mut self, id: ExprId) -> Place {
        if self.is_place_expr(id) {
            self.place(id)
        } else {
            let temp = self.temp(self.ty(id));
            self.expr_into(id, Some(temp.clone()));
            temp
        }
    }

    /// The qubit place a gate acts on, given its target expression: a place, a reference,
    /// `mut q`, or a chain of gate calls.
    fn qubit_place(&mut self, id: ExprId) -> Place {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Own { place, .. } => self.qubit_place(place),
            ExprKind::MethodCall { .. } if self.is_gate_chain(id) => self.gate_call(id),
            _ if self.is_place_expr(id) => self.place_deref(id),
            _ => {
                let temp = self.temp(self.ty(id));
                self.expr_into(id, Some(temp.clone()));
                if self.is_ref(self.ty(id)) {
                    temp.with(Proj::Deref)
                } else {
                    temp
                }
            }
        }
    }

    fn is_gate_chain(&self, id: ExprId) -> bool {
        matches!(
            self.typed.callees[id.index()],
            Some(Callee::Builtin(Builtin::Gate(_) | Builtin::Reset))
        ) && matches!(self.ast.expr(id).kind, ExprKind::MethodCall { .. })
    }

    /// Lowers a gate or `reset` method call, returning the qubit it acted on.
    fn gate_call(&mut self, id: ExprId) -> Place {
        let ExprKind::MethodCall { receiver, args, .. } = self.ast.expr(id).kind else {
            unreachable!("gate_call on a method call")
        };
        let target = self.qubit_place(receiver);
        let args = self.ast.list(args);
        match self.typed.callees[id.index()] {
            Some(Callee::Builtin(Builtin::Reset)) => self.push(Stmt::Reset(target.clone())),
            Some(Callee::Builtin(Builtin::Gate(gate))) => {
                let mut qubits = vec![target.clone()];
                let mut params = Vec::new();
                for &arg in args {
                    if gate == Gate::Swap {
                        qubits.push(self.qubit_place(arg));
                    } else {
                        params.push(self.operand(arg));
                    }
                }
                self.push(Stmt::Gate {
                    gate,
                    adjoint: false,
                    qubits,
                    params,
                });
            }
            _ => unreachable!("is_gate_chain"),
        }
        target
    }

    /// Computes an expression into `dest`, or for its effects only.
    /// Lowers an expression into `dest`; its statements carry its span, unless a part of it
    /// has its own.
    fn expr_into(&mut self, id: ExprId, dest: Option<Place>) {
        let outer = std::mem::replace(&mut self.span, self.ast.expr(id).span);
        self.expr_parts_into(id, dest);
        self.span = outer;
    }

    fn expr_parts_into(&mut self, id: ExprId, dest: Option<Place>) {
        let ast = self.ast;
        let expr = *ast.expr(id);
        match expr.kind {
            ExprKind::Error | ExprKind::With { .. } => {}
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Unit => {
                let op = self.operand(id);
                self.set(&dest, Rvalue::Use(op));
            }
            ExprKind::Str(parts) => {
                let mut out = Vec::new();
                for part in ast.list(parts) {
                    match *part {
                        ast::StrPart::Text(text) => {
                            out.push(FormatPart::Text(ast.name(text).to_string()))
                        }
                        ast::StrPart::Value { expr, spec } => {
                            let ty = self.types.deref(self.ty(expr));
                            let op = self.value_operand(expr);
                            out.push(FormatPart::Value(op, ty, spec));
                        }
                    }
                }
                self.set(&dest, Rvalue::Format(out));
            }
            ExprKind::Ket { .. } => {
                let state = &self.text(expr.span)[1..expr.span.len() as usize - 1];
                let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
                self.ket(state, &dest);
            }
            ExprKind::Path(_) => match self.typed.res[id.index()] {
                Res::Builtin(Builtin::Const(c)) => self.set(&dest, Rvalue::FloatConst(c)),
                Res::Variant(_, index) => self.set(&dest, Rvalue::Variant(index, Vec::new())),
                _ => {}
            },
            ExprKind::Match { scrutinee, arms } => self.match_into(id, scrutinee, arms, dest),
            ExprKind::Name(_)
            | ExprKind::Field { .. }
            | ExprKind::TupleIndex { .. }
            | ExprKind::Index { .. }
            | ExprKind::Own { .. } => {
                let op = self.operand(id);
                self.set(&dest, Rvalue::Use(op));
            }
            ExprKind::Tuple(elems) | ExprKind::Array(elems) => {
                let ops: Vec<Operand> = ast.list(elems).iter().map(|&e| self.operand(e)).collect();
                self.set(&dest, Rvalue::Aggregate(ops));
            }
            ExprKind::ArrayRepeat { value, count } => {
                let ExprKind::Int(count) = ast.expr(count).kind else {
                    return;
                };
                if matches!(ast.expr(value).kind, ExprKind::Ket { .. }) {
                    let span = ast.expr(value).span;
                    let state = &self.text(span)[1..span.len() as usize - 1];
                    let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
                    for k in 0..count {
                        let element = dest.with(Proj::Index(Operand::Const(Const::Int(k as i64))));
                        self.ket(state, &element);
                    }
                } else {
                    let op = self.operand(value);
                    self.set(&dest, Rvalue::Repeat(op, count));
                }
            }
            ExprKind::StructLit { fields, .. } => self.struct_lit(id, fields, dest),
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => self.call(id, dest, false),
            ExprKind::Adj(call) => self.call(call, dest, true),
            ExprKind::Unary { op, operand } => {
                let x = self.operand(operand);
                let op = match op {
                    ast::UnaryOp::Neg => UnOp::Neg,
                    ast::UnaryOp::BitNot => UnOp::BitNot,
                    ast::UnaryOp::Not => UnOp::Not,
                };
                self.set(&dest, Rvalue::Unary(op, x));
            }
            ExprKind::Binary {
                op: op @ (ast::BinaryOp::And | ast::BinaryOp::Or),
                lhs,
                rhs,
            } => {
                self.short_circuit(op, lhs, rhs, dest);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let a = self.operand(lhs);
                let b = self.operand(rhs);
                self.set(&dest, Rvalue::Binary(op, a, b));
            }
            ExprKind::BraTest { state, operand, .. } => {
                let state = &self.text(state)[1..state.len() as usize - 1];
                self.bra_test(state, operand, dest);
            }
            ExprKind::Expectation {
                prep,
                observable,
                precision,
            } => {
                let precision = precision.map(|p| self.operand(p));
                let prep = match ast.expr(prep).kind {
                    ExprKind::Ket { .. } => {
                        let span = ast.expr(prep).span;
                        Prep::Ket(self.text(span)[1..span.len() as usize - 1].to_string())
                    }
                    ExprKind::Call { args, .. } => {
                        let Some(Callee::Fn(f)) = self.typed.callees[prep.index()] else {
                            return;
                        };
                        let args = ast.list(args).iter().map(|&a| self.operand(a)).collect();
                        Prep::Call(FuncId(f.index() as u32), args)
                    }
                    _ => return,
                };
                let observable = self.operand(observable);
                let dest = dest.unwrap_or_else(|| self.temp(Ty::FLOAT));
                self.push(Stmt::Expectation {
                    dest,
                    prep,
                    observable,
                    precision,
                });
            }
            ExprKind::Sample { shots, body } => {
                let (func, captures) = self.run_function(id, body);
                let args = captures
                    .iter()
                    .map(|&l| {
                        let ty = self.typed.local(l).ty;
                        self.read(Place::local(self.local(l)), ty)
                    })
                    .collect();
                let shots = self.operand(shots);
                let TyKind::Distribution(elem) = *self.types.kind(self.ty(id)) else {
                    return;
                };
                let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
                self.push(Stmt::Sample {
                    dest,
                    func,
                    args,
                    shots,
                    elem,
                });
            }
            ExprKind::Block(block) | ExprKind::Unsafe(block) => self.block_into(block, dest),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                let cond = self.operand(cond);
                let (then_block, else_block, join) =
                    (self.new_block(), self.new_block(), self.new_block());
                let control = self.control(id, join, false);
                self.terminate_into(
                    Terminator::Branch {
                        cond,
                        then: then_block,
                        otherwise: else_block,
                        control,
                    },
                    then_block,
                );
                self.block_into(then, dest.clone());
                self.terminate_into(Terminator::Goto(join), else_block);
                if let Some(otherwise) = otherwise {
                    self.expr_into(otherwise, dest);
                }
                self.terminate_into(Terminator::Goto(join), join);
            }
            ExprKind::While { cond, body } => {
                let (head, body_block, exit) =
                    (self.new_block(), self.new_block(), self.new_block());
                self.terminate_into(Terminator::Goto(head), head);
                let cond = self.operand(cond);
                let control = self.control(id, head, true);
                self.terminate_into(
                    Terminator::Branch {
                        cond,
                        then: body_block,
                        otherwise: exit,
                        control,
                    },
                    body_block,
                );
                self.loops.push((head, exit));
                self.block_into(body, None);
                self.loops.pop();
                self.terminate_into(Terminator::Goto(head), exit);
            }
            ExprKind::For {
                start, end, body, ..
            } => self.for_loop(id, start, end, body),
            ExprKind::Ctrl { controls, body } => {
                let places = ast
                    .list(controls)
                    .iter()
                    .map(|&c| self.place_deref(c))
                    .collect();
                self.push(Stmt::CtrlPush(places));
                self.block_into(body, None);
                self.push(Stmt::CtrlPop);
            }
        }
    }

    /// Assigns `value` to `dest`, if the value is wanted.
    fn set(&mut self, dest: &Option<Place>, value: Rvalue) {
        if let Some(dest) = dest {
            self.assign(dest.clone(), value);
        }
    }

    /// Allocates qubits for a ket's state at `dest`: one qubit, or an array for several.
    fn ket(&mut self, state: &str, dest: &Place) {
        let array = labels(state).count() > 1;
        for (k, label) in labels(state).enumerate() {
            let qubit = if array {
                dest.with(Proj::Index(Operand::Const(Const::Int(k as i64))))
            } else {
                dest.clone()
            };
            self.push(Stmt::Alloc(qubit.clone()));
            let gates: &[Gate] = match label {
                Label::Zero => &[],
                Label::One => &[Gate::X],
                Label::Plus => &[Gate::H],
                Label::Minus => &[Gate::X, Gate::H],
                Label::PlusI => &[Gate::H, Gate::S],
                Label::MinusI => &[Gate::X, Gate::H, Gate::S],
            };
            for &gate in gates {
                self.push(Stmt::Gate {
                    gate,
                    adjoint: false,
                    qubits: vec![qubit.clone()],
                    params: Vec::new(),
                });
            }
        }
    }

    /// `<s| x`: measure each qubit in its label's basis and compare.
    fn bra_test(&mut self, state: &str, operand: ExprId, dest: Option<Place>) {
        let base = self.qubit_place(operand);
        let array = labels(state).count() > 1;
        let mut result: Option<Operand> = None;
        for (k, label) in labels(state).enumerate() {
            let qubit = if array {
                base.with(Proj::Index(Operand::Const(Const::Int(k as i64))))
            } else {
                base.clone()
            };
            // Into the Z basis, as `measure_in` does: `h` for X, `adj s` then `h` for Y.
            let gates: &[(Gate, bool)] = match label.basis() {
                Pauli::Z => &[],
                Pauli::X => &[(Gate::H, false)],
                Pauli::Y => &[(Gate::S, true), (Gate::H, false)],
            };
            for &(gate, adjoint) in gates {
                self.push(Stmt::Gate {
                    gate,
                    adjoint,
                    qubits: vec![qubit.clone()],
                    params: Vec::new(),
                });
            }
            let bit = self.temp(Ty::BIT);
            self.push(Stmt::Measure {
                dest: bit.clone(),
                qubit,
            });
            let matches = self.temp(Ty::BOOL);
            let expected = label.outcome();
            self.assign(
                matches.clone(),
                Rvalue::Binary(
                    ast::BinaryOp::Eq,
                    Operand::Copy(bit),
                    Operand::Const(Const::Bit(expected)),
                ),
            );
            result = Some(match result {
                None => Operand::Copy(matches),
                Some(previous) => {
                    let both = self.temp(Ty::BOOL);
                    self.assign(
                        both.clone(),
                        Rvalue::Binary(ast::BinaryOp::And, previous, Operand::Copy(matches)),
                    );
                    Operand::Copy(both)
                }
            });
        }
        if let (Some(dest), Some(result)) = (dest, result) {
            self.assign(dest, Rvalue::Use(result));
        }
    }

    /// `a and b`, `a or b`: `b` is only evaluated when needed. When evaluating `b` can have no
    /// effect, both are evaluated and combined without a branch, which keeps a condition on
    /// measurement results one value while a circuit is recorded.
    fn short_circuit(&mut self, op: ast::BinaryOp, lhs: ExprId, rhs: ExprId, dest: Option<Place>) {
        let dest = dest.unwrap_or_else(|| self.temp(Ty::BOOL));
        if noev_sema::circuit::is_pure(self.ast, rhs) {
            let (a, b) = (self.operand(lhs), self.operand(rhs));
            self.assign(dest, Rvalue::Binary(op, a, b));
            return;
        }
        let a = self.operand(lhs);
        self.assign(dest.clone(), Rvalue::Use(a));
        let (rest, join) = (self.new_block(), self.new_block());
        let (then, otherwise) = if op == ast::BinaryOp::And {
            (rest, join)
        } else {
            (join, rest)
        };
        self.terminate_into(
            Terminator::Branch {
                cond: Operand::Copy(dest.clone()),
                then,
                otherwise,
                control: None,
            },
            rest,
        );
        self.expr_into(rhs, Some(dest));
        self.terminate_into(Terminator::Goto(join), join);
    }

    fn for_loop(&mut self, id: ExprId, start: ExprId, end: ExprId, body: ast::BlockId) {
        let Res::Local(var) = self.typed.res[id.index()] else {
            return;
        };
        let start = self.operand(start);
        let end_value = self.operand(end);
        let end = self.temp(Ty::INT);
        self.assign(end.clone(), Rvalue::Use(end_value));
        let counter = self.temp(Ty::INT);
        self.assign(counter.clone(), Rvalue::Use(start));

        let (head, body_block, step, exit) = (
            self.new_block(),
            self.new_block(),
            self.new_block(),
            self.new_block(),
        );
        self.terminate_into(Terminator::Goto(head), head);
        let more = self.temp(Ty::BOOL);
        self.assign(
            more.clone(),
            Rvalue::Binary(
                ast::BinaryOp::Lt,
                Operand::Copy(counter.clone()),
                Operand::Copy(end),
            ),
        );
        self.terminate_into(
            Terminator::Branch {
                cond: Operand::Copy(more),
                then: body_block,
                otherwise: exit,
                control: None,
            },
            body_block,
        );

        let var = self.declare(var);
        self.assign(
            Place::local(var),
            Rvalue::Use(Operand::Copy(counter.clone())),
        );
        self.loops.push((step, exit));
        self.block_into(body, None);
        self.loops.pop();
        self.terminate_into(Terminator::Goto(step), step);
        self.assign(
            counter.clone(),
            Rvalue::Binary(
                ast::BinaryOp::Add,
                Operand::Copy(counter),
                Operand::Const(Const::Int(1)),
            ),
        );
        self.terminate_into(Terminator::Goto(head), exit);
    }

    fn struct_lit(&mut self, id: ExprId, fields: ast::List<ast::FieldInit>, dest: Option<Place>) {
        let TyKind::Struct(s) = *self.types.kind(self.ty(id)) else {
            return;
        };
        // A variant with named fields, or a struct.
        let variant = match self.ast.expr(id).kind {
            ExprKind::StructLit { path, .. } => match self.typed.res[path.index()] {
                Res::Variant(_, index) => Some(index),
                _ => None,
            },
            _ => None,
        };
        let decl: Vec<(noev_syntax::Symbol, Ty)> = match variant {
            Some(index) => {
                let v = &self.types.structs[s.index()]
                    .variants
                    .as_ref()
                    .expect("an enum")[index as usize];
                v.names
                    .iter()
                    .copied()
                    .zip(v.data.iter().copied())
                    .collect()
            }
            None => self.types.structs[s.index()].fields.clone(),
        };
        let inits = self.ast.list(fields);
        // Evaluate in source order, then assemble in declaration order.
        let mut values: Vec<Option<Operand>> = vec![None; decl.len()];
        for init in inits {
            let index = decl
                .iter()
                .position(|(n, _)| *n == init.name)
                .expect("checked");
            let op = match init.value {
                Some(value) => self.operand(value),
                None => {
                    let local = self.shorthand_local(init.name);
                    let ty = self.typed.local(local).ty;
                    self.read(Place::local(self.local(local)), ty)
                }
            };
            values[index] = Some(op);
        }
        let ops: Vec<Operand> = values
            .into_iter()
            .map(|v| v.expect("checked: every field"))
            .collect();
        let value = match variant {
            Some(index) => Rvalue::Variant(index, ops),
            None => Rvalue::Aggregate(ops),
        };
        let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
        self.assign(dest, value);
    }

    fn shorthand_local(&self, name: noev_syntax::Symbol) -> LocalId {
        // The most recently declared visible local with this name.
        (0..self.locals.len())
            .rev()
            .map(LocalId::from_index)
            .find(|&l| self.locals[l.index()].is_some() && self.typed.local(l).name == name)
            .expect("checked: the variable exists")
    }
}

// Calls.

impl Builder<'_> {
    fn call(&mut self, id: ExprId, dest: Option<Place>, adjoint: bool) {
        let ast = self.ast;
        let (receiver, args) = match ast.expr(id).kind {
            ExprKind::Call { args, .. } => (None, ast.list(args)),
            ExprKind::MethodCall { receiver, args, .. } => (Some(receiver), ast.list(args)),
            _ => return,
        };
        let Some(callee) = self.typed.callees[id.index()] else {
            return;
        };
        match callee {
            Callee::Fn(f) => {
                let mut ops = Vec::new();
                if let Some(receiver) = receiver {
                    ops.push(self.receiver_operand(id, receiver));
                }
                ops.extend(args.iter().map(|&a| self.operand(a)));
                let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
                let next = self.new_block();
                self.terminate_into(
                    Terminator::Call {
                        func: FuncId(f.index() as u32),
                        args: ops,
                        dest,
                        adjoint,
                        next,
                    },
                    next,
                );
            }
            Callee::Convert(target) => {
                let op = self.operand(args[0]);
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Convert(target, op));
                }
            }
            Callee::Variant(_, index) => {
                let ops = args.iter().map(|&a| self.operand(a)).collect();
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Variant(index, ops));
                } else {
                    // Thrown away; still built, since its values were moved into it.
                    let temp = self.temp(self.ty(id));
                    self.assign(temp, Rvalue::Variant(index, ops));
                }
            }
            Callee::Builtin(builtin) => self.builtin(id, builtin, receiver, args, dest, adjoint),
        }
    }

    /// A user method's receiver, by how the method takes `self`.
    fn receiver_operand(&mut self, call: ExprId, receiver: ExprId) -> Operand {
        let kind = self.typed.receivers[call.index()].unwrap_or(OwnKind::Ref);
        let ty = self.ty(receiver);
        match kind {
            OwnKind::Mv => self.operand(receiver),
            // Already a reference (a variable, or a chain's result): pass it on as it is.
            OwnKind::Ref | OwnKind::Mut if self.is_ref(ty) => Operand::Copy(self.place(receiver)),
            OwnKind::Ref | OwnKind::Mut => {
                let place = self.place(receiver);
                let mutable = kind == OwnKind::Mut;
                let ref_ty = self.types.reference(mutable, ty);
                let temp = self.temp(ref_ty);
                self.assign(temp.clone(), Rvalue::Ref { mutable, place });
                Operand::Copy(temp)
            }
        }
    }

    fn builtin(
        &mut self,
        id: ExprId,
        builtin: Builtin,
        receiver: Option<ExprId>,
        args: &[ExprId],
        dest: Option<Place>,
        adjoint: bool,
    ) {
        // The receiver of a method, or the first argument of a free function.
        let (first, rest): (Option<ExprId>, &[ExprId]) = match receiver {
            Some(r) => (Some(r), args),
            None => (args.first().copied(), args.get(1..).unwrap_or(&[])),
        };
        match builtin {
            Builtin::Gate(gate) => {
                let Some(first) = first else { return };
                let target = self.qubit_place(first);
                let mut qubits = vec![target.clone()];
                let mut params = Vec::new();
                for &arg in rest {
                    if gate == Gate::Swap {
                        qubits.push(self.qubit_place(arg));
                    } else {
                        params.push(self.operand(arg));
                    }
                }
                self.push(Stmt::Gate {
                    gate,
                    adjoint,
                    qubits,
                    params,
                });
                if let Some(dest) = dest
                    && receiver.is_some()
                {
                    self.assign(
                        dest,
                        Rvalue::Ref {
                            mutable: true,
                            place: target,
                        },
                    );
                }
            }
            Builtin::Reset => {
                let Some(first) = first else { return };
                let target = self.qubit_place(first);
                self.push(Stmt::Reset(target.clone()));
                if let Some(dest) = dest
                    && receiver.is_some()
                {
                    self.assign(
                        dest,
                        Rvalue::Ref {
                            mutable: true,
                            place: target,
                        },
                    );
                }
            }
            Builtin::Measure => {
                let Some(first) = first else { return };
                let target = self.qubit_place(first);
                let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
                match *self.types.kind(self.types.deref(self.ty(first))) {
                    TyKind::Array(_, len) => {
                        for k in 0..len {
                            let index = Proj::Index(Operand::Const(Const::Int(k as i64)));
                            self.push(Stmt::Measure {
                                dest: dest.with(index.clone()),
                                qubit: target.with(index),
                            });
                        }
                    }
                    _ => self.push(Stmt::Measure {
                        dest,
                        qubit: target,
                    }),
                }
            }
            // The standard library's `measure_in`: into the Z basis (X: `h`; Y: `adj s`, `h`),
            // then `measure`.
            Builtin::MeasureIn => {
                let (Some(first), Some(&basis)) = (first, rest.first()) else {
                    return;
                };
                let target = self.qubit_place(first);
                let basis = self.operand(basis);
                let dest = dest.unwrap_or_else(|| self.temp(self.ty(id)));
                let len = match *self.types.kind(self.types.deref(self.ty(first))) {
                    TyKind::Array(_, len) => Some(len),
                    _ => None,
                };
                let qubits: Vec<Place> = match len {
                    Some(len) => (0..len)
                        .map(|k| target.with(Proj::Index(Operand::Const(Const::Int(k as i64)))))
                        .collect(),
                    None => vec![target],
                };
                let tag = self.temp(Ty::INT);
                self.assign(tag.clone(), Rvalue::Tag(basis));
                let (x, not_x, y, measure) = (
                    self.new_block(),
                    self.new_block(),
                    self.new_block(),
                    self.new_block(),
                );
                let is_x = self.compare(Operand::Copy(tag.clone()), Operand::Const(Const::Int(0)));
                self.terminate_into(
                    Terminator::Branch {
                        cond: is_x,
                        then: x,
                        otherwise: not_x,
                        control: None,
                    },
                    x,
                );
                for q in &qubits {
                    self.gate_on(Gate::H, false, q);
                }
                self.terminate_into(Terminator::Goto(measure), not_x);
                let is_y = self.compare(Operand::Copy(tag), Operand::Const(Const::Int(1)));
                self.terminate_into(
                    Terminator::Branch {
                        cond: is_y,
                        then: y,
                        otherwise: measure,
                        control: None,
                    },
                    y,
                );
                for q in &qubits {
                    self.gate_on(Gate::S, true, q);
                    self.gate_on(Gate::H, false, q);
                }
                self.terminate_into(Terminator::Goto(measure), measure);
                for (k, qubit) in qubits.into_iter().enumerate() {
                    let dest = match len {
                        Some(_) => dest.with(Proj::Index(Operand::Const(Const::Int(k as i64)))),
                        None => dest.clone(),
                    };
                    self.push(Stmt::Measure { dest, qubit });
                }
            }
            // The standard library's `discard`: reset, then release.
            Builtin::Discard => {
                let Some(first) = first else { return };
                let target = self.qubit_place(first);
                self.push(Stmt::Reset(target.clone()));
                self.push(Stmt::Release(target));
            }
            Builtin::Release => {
                let Some(first) = first else { return };
                let target = self.qubit_place(first);
                self.push(Stmt::Release(target));
            }
            Builtin::Pauli(pauli) => {
                let index = self.operand(args[0]);
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Pauli(pauli, index));
                }
            }
            Builtin::Identity => {
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Identity);
                }
            }
            Builtin::ProjectorZ | Builtin::ProjectorX => {
                let index = self.operand(args[0]);
                let bit = self.operand(args[1]);
                if let Some(dest) = dest {
                    let x_basis = builtin == Builtin::ProjectorX;
                    self.assign(
                        dest,
                        Rvalue::Projector {
                            x_basis,
                            index,
                            bit,
                        },
                    );
                }
            }
            Builtin::Math(math) => {
                let mut ops = Vec::new();
                if let Some(first) = first {
                    ops.push(self.operand(first));
                }
                ops.extend(rest.iter().map(|&a| self.operand(a)));
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Math(math, ops));
                }
            }
            Builtin::Dist(method) => {
                let Some(first) = first else { return };
                let TyKind::Distribution(elem) = *self.types.kind(self.types.deref(self.ty(first)))
                else {
                    return;
                };
                let mut ops = vec![self.value_operand(first)];
                ops.extend(rest.iter().map(|&a| self.operand(a)));
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Dist(method, ops, elem));
                }
            }
            Builtin::Print { stderr } => {
                let args = args
                    .iter()
                    .map(|&a| {
                        let ty = self.types.deref(self.ty(a));
                        (self.value_operand(a), ty)
                    })
                    .collect();
                self.push(Stmt::Print { stderr, args });
            }
            Builtin::Convert(_) | Builtin::Const(_) => {}
        }
    }
}
