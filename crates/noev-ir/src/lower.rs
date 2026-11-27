//! Lowers a checked syntax tree to the IR.
//!
//! Runs only on programs that passed type and ownership checking, so it does not report
//! errors. Expressions are lowered into a destination place (or nowhere, when their value is
//! thrown away). Kets, bras and the standard library are expanded here: a ket is allocation
//! plus gates, a measurement test is gates, measurements and comparisons, and `discard` is
//! `reset` then `release`, as its reference definition in `spec/std.md` says.

use noev_sema::check::{Adjust, Callee, LocalId, Res};
use noev_sema::prelude::{Builtin, Gate};
use noev_sema::ty::OpKind;
use noev_sema::{Circuits, Ty, TyKind, Typed, Types};
use noev_syntax::ast::{self as ast, ExprId, ExprKind, OwnKind, StmtKind};
use noev_syntax::state::{Label, labels};
use noev_syntax::{Ast, SourceMap, Span};

use crate::ir::*;

/// What a `std::ops` or `std::cmp` trait's function lowers to on a built-in type.
enum BuiltinOp {
    Unary(UnOp),
    Binary(ast::BinaryOp),
    Assign(ast::BinaryOp),
    Cmp(ast::BinaryOp),
}

/// Lowers every function of a checked program, given which of its runs are circuits
/// ([`noev_sema::check_circuits`]). `source` supplies the text of kets and bras.
pub fn lower(syntax: &Ast, typed: &Typed, circuits: &Circuits, source: &SourceMap) -> Program {
    // Lowering creates reference types for implicit lends, and instances of generic types,
    // so it works on its own copy.
    let mut types = typed.types.clone();
    let mut functions = Vec::with_capacity(syntax.all_fns().len());
    // `sample` runs and the copies of generic functions become functions of their own,
    // numbered after the program's.
    let mut runs = Vec::new();
    let mut instances = Instances::default();
    let first_run = syntax.all_fns().len() as u32;
    let main = typed.main.map(|f| FuncId(f.index() as u32));
    for (index, decl) in syntax.all_fns().iter().enumerate() {
        let f = ast::FnId::from_index(index);
        let name = fn_name(syntax, typed, f);
        // A generic function is lowered once for each list of type arguments it is called
        // with; its own slot holds nothing that runs.
        if !typed.fns[index].generics.is_empty() {
            functions.push(stub(name, decl.span));
            continue;
        }
        let shared = Shared {
            types: &mut types,
            circuits,
            runs: &mut runs,
            first_run,
            instances: &mut instances,
        };
        let mut function = Builder::new(syntax, typed, shared, source, f, name, Vec::new()).build();
        function.sampled = circuits.fns[index].sampled();
        functions.push(function);
    }
    // Copies of generic functions, as calls asked for them; making one can ask for more.
    while let Some((f, args, slot)) = instances.pending.pop() {
        let arg_names: Vec<String> = args.iter().map(|&t| types.display(t, syntax)).collect();
        let name = format!("{}<{}>", fn_name(syntax, typed, f), arg_names.join(", "));
        let shared = Shared {
            types: &mut types,
            circuits,
            runs: &mut runs,
            first_run,
            instances: &mut instances,
        };
        let mut function = Builder::new(syntax, typed, shared, source, f, name, args).build();
        function.sampled = circuits.fns[f.index()].sampled();
        runs[slot] = function;
    }
    functions.extend(runs);
    // Instances of generic types are named with their arguments: `Pair<Int, Bit>`.
    let struct_names = types
        .structs
        .iter()
        .map(|s| {
            let name = syntax.name(s.name).to_string();
            if s.template.is_none() {
                return name;
            }
            let args: Vec<String> = s.args.iter().map(|&t| types.display(t, syntax)).collect();
            format!("{name}<{}>", args.join(", "))
        })
        .collect();
    let struct_fields = types
        .structs
        .iter()
        .map(|s| {
            s.fields
                .iter()
                .map(|&(name, _)| syntax.name(name).to_string())
                .collect()
        })
        .collect();
    let variant_names = types
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
    let variant_fields = types
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

/// A function's name: by its module's path from the root of the package being compiled, or
/// from its dependency's name, and its struct's for a method.
fn fn_name(syntax: &Ast, typed: &Typed, f: ast::FnId) -> String {
    let decl = syntax.function(f);
    let own = syntax.name(decl.name);
    let mut name = if let Some(s) = typed.fns[f.index()].owner {
        format!(
            "{}::{own}",
            syntax.name(typed.types.structs[s.index()].name)
        )
    } else if let Some(&index) = typed.fn_impls.get(&f) {
        // `<Pair<T, T> as Measure>::measure` and `<Parity as Oracle<4>>::apply`, as Rust
        // writes them.
        let i = &typed.impls[index];
        let mut trait_name = syntax
            .name(typed.traits[i.trait_id.index()].name)
            .to_string();
        if !i.trait_args.is_empty() {
            let args: Vec<String> = i
                .trait_args
                .iter()
                .map(|&a| typed.types.display(a, syntax).to_string())
                .collect();
            trait_name = format!("{trait_name}<{}>", args.join(", "));
        }
        format!(
            "<{} as {trait_name}>::{own}",
            typed.types.display(i.self_ty, syntax)
        )
    } else if let Some(&t) = typed.fn_traits.get(&f) {
        format!("{}::{own}", syntax.name(typed.traits[t.index()].name))
    } else {
        own.to_string()
    };
    let module = typed.fn_modules[f.index()];
    let package = syntax.module(module).package;
    let mut path = Vec::new();
    if package.index() != 0 {
        path.push(syntax.name(syntax.package(package).name).to_string());
    }
    if syntax.module(module).parent.is_some() {
        path.push(syntax.module_path(module));
    }
    if !path.is_empty() {
        name = format!("{}::{name}", path.join("::"));
    }
    name
}

/// A function that is never called: a generic function's own slot, or one a copy fills in.
fn stub(name: String, span: Span) -> Function {
    Function {
        name,
        params: 0,
        locals: vec![LocalDecl {
            ty: Ty::UNIT,
            name: None,
        }],
        blocks: vec![BasicBlock {
            stmts: Vec::new(),
            spans: Vec::new(),
            term: Terminator::Unreachable,
        }],
        span,
        sampled: false,
        extern_kind: ExternKind::None,
    }
}

/// The copies of generic functions made so far, and those still to lower.
#[derive(Default)]
struct Instances {
    /// Each copy's function, by the generic function and its type arguments.
    ids: noev_syntax::FxHashMap<(ast::FnId, Vec<Ty>), FuncId>,
    /// The copies asked for and not lowered yet, with where each goes among the runs.
    pending: Vec<(ast::FnId, Vec<Ty>, usize)>,
}

/// One label of a ket or bra: fixed, or a computed value (`spec/types.md` §3).
#[derive(Clone)]
enum LabelOp {
    Fixed(Label),
    Value(Operand),
}

/// What every builder of a program shares.
struct Shared<'a> {
    types: &'a mut Types,
    circuits: &'a Circuits,
    /// The functions made for `sample` runs and copies of generic functions so far.
    runs: &'a mut Vec<Function>,
    /// The id of the first of them.
    first_run: u32,
    /// The copies of generic functions.
    instances: &'a mut Instances,
}

struct Builder<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    types: &'a mut Types,
    circuits: &'a Circuits,
    runs: &'a mut Vec<Function>,
    first_run: u32,
    instances: &'a mut Instances,
    /// In a copy of a generic function, its type parameters, replaced by `args`.
    params: Vec<Ty>,
    args: Vec<Ty>,
    source: &'a SourceMap,
    body: Option<ast::BlockId>,
    function: Function,
    current: Block,
    /// IR locals for source locals, indexed by [`LocalId::index`].
    locals: Vec<Option<Local>>,
    /// `(continue, break, linear_scope_mark)` targets of enclosing loops.
    loops: Vec<(Block, Block, usize)>,
    /// Linear locals currently in scope (excluding `with` blocks, whose ancillas release at
    /// [`Stmt::WithEnd`]), in declaration order, for automatic \|0⟩ release.
    linear_scope: Vec<(LocalId, Local, Ty)>,
    /// Moved linear places (local plus field / constant-index path) in the current flow.
    moved: Vec<(LocalId, Vec<u32>)>,
    /// Nesting depth inside the `with` part of `with`/`do`.
    in_with_block: u32,
    /// Whether control has left the current block via `return`, `break` or `continue`.
    diverged: bool,
    /// The span of the expression being lowered.
    span: Span,
}

impl<'a> Builder<'a> {
    fn new(
        syntax: &'a Ast,
        typed: &'a Typed,
        shared: Shared<'a>,
        source: &'a SourceMap,
        f: ast::FnId,
        name: String,
        args: Vec<Ty>,
    ) -> Self {
        let decl = syntax.function(f);
        let info = &typed.fns[f.index()];
        let params = if args.is_empty() {
            Vec::new()
        } else {
            info.generics.clone()
        };
        let ret = shared.types.subst(info.sig.ret, &params, &args);
        let ret = noev_sema::check::normalize(&typed.impls, shared.types, ret);
        let mut builder = Builder::empty(syntax, typed, shared, source, name, decl.body, ret);
        builder.params = params;
        builder.args = args;
        builder.function.span = decl.span;
        builder.function.extern_kind = decl.extern_kind;
        if decl.is_foreign_import() {
            for (param, &ty) in syntax.list(decl.params).iter().zip(info.sig.params.iter()) {
                builder.function.locals.push(LocalDecl {
                    ty,
                    name: Some(syntax.name(param.name).to_string()),
                });
                builder.function.params += 1;
            }
            builder.function.blocks.clear();
            return builder;
        }
        match (info.self_local, info.receiver_ty) {
            (Some(local), _) => {
                builder.declare(local);
                builder.function.params += 1;
            }
            // A method whose body never mentions `self` still takes the receiver.
            (None, Some(ty)) => {
                let ty = builder.subst(ty);
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
        source: &'a SourceMap,
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
            instances: shared.instances,
            params: Vec::new(),
            args: Vec::new(),
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
                extern_kind: ExternKind::None,
            },
            current: Block(0),
            locals: vec![None; typed.locals.len()],
            loops: Vec::new(),
            linear_scope: Vec::new(),
            moved: Vec::new(),
            in_with_block: 0,
            diverged: false,
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
            instances: &mut *self.instances,
        };
        let ret = shared.types.subst(run.ty, &self.params, &self.args);
        let ret = noev_sema::check::normalize(&self.typed.impls, shared.types, ret);
        let (params, args) = (self.params.clone(), self.args.clone());
        let mut builder = Builder::empty(
            self.ast,
            self.typed,
            shared,
            self.source,
            name,
            Some(body),
            ret,
        );
        // A run in a copy of a generic function is part of the copy.
        builder.params = params;
        builder.args = args;
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
        if matches!(
            self.function.extern_kind,
            ExternKind::Host | ExternKind::CImport
        ) {
            return self.function;
        }
        if let Some(body) = self.body {
            let dest = (self.function.locals[0].ty != Ty::UNIT).then(|| Place::local(Local(0)));
            self.block_into(body, dest);
            if !self.diverged {
                self.release_scope(0);
            }
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
        let ty = self.local_ty(local);
        let source = self.typed.local(local);
        self.function.locals.push(LocalDecl {
            ty,
            name: Some(self.ast.name(source.name).to_string()),
        });
        let ir = Local(self.function.locals.len() as u32 - 1);
        self.locals[local.index()] = Some(ir);
        if self.in_with_block == 0 && self.types.linearity(ty) == noev_sema::ty::Kind::Linear {
            self.linear_scope.push((local, ir, ty));
        }
        ir
    }

    /// The static path of a place expression (local plus field / tuple / constant array
    /// indices), for tracking which linear parts have been moved before scope exit.
    fn static_place_path(&mut self, id: ExprId) -> Option<(LocalId, Vec<u32>)> {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Own { place, .. } => self.static_place_path(place),
            ExprKind::Name(_) => match self.typed.res[id.index()] {
                Res::Local(local) => Some((local, Vec::new())),
                _ => None,
            },
            ExprKind::Field { base, name } => {
                let (root, mut path) = self.static_place_path(base)?;
                let ty = {
                    let ty = self.ty(base);
                    self.types.deref(ty)
                };
                let TyKind::Struct(s) = *self.types.kind(ty) else {
                    return None;
                };
                let index = self.types.structs[s.index()]
                    .fields
                    .iter()
                    .position(|(n, _)| *n == name)?;
                path.push(index as u32);
                Some((root, path))
            }
            ExprKind::TupleIndex { base, index } => {
                let (root, mut path) = self.static_place_path(base)?;
                path.push(index);
                Some((root, path))
            }
            ExprKind::Index { base, index } => {
                let (root, mut path) = self.static_place_path(base)?;
                let ExprKind::Int(k) = ast.expr(index).kind else {
                    return None;
                };
                path.push(u32::try_from(k).ok()?);
                Some((root, path))
            }
            _ => None,
        }
    }

    fn mark_moved(&mut self, place: ExprId) {
        if let Some(entry) = self.static_place_path(place) {
            self.moved.push(entry);
        }
    }

    fn unmark_moved(&mut self, place: ExprId) {
        if let Some((root, path)) = self.static_place_path(place) {
            self.moved
                .retain(|(l, p)| *l != root || !p.starts_with(&path));
        }
    }

    /// Releases every unmoved qubit owned by linear locals declared since `mark` (proven \|0⟩
    /// by the ownership checker).
    fn release_scope(&mut self, mark: usize) {
        let entries: Vec<(LocalId, Local, Ty)> = self.linear_scope[mark..].to_vec();
        let mut path = Vec::new();
        for (local, ir, ty) in entries.into_iter().rev() {
            path.clear();
            self.release_unmoved(local, &mut path, Place::local(ir), ty);
        }
    }

    fn release_unmoved(&mut self, local: LocalId, path: &mut Vec<u32>, place: Place, ty: Ty) {
        if self.types.linearity(ty) != noev_sema::ty::Kind::Linear {
            return;
        }
        if self
            .moved
            .iter()
            .any(|(l, p)| *l == local && path.starts_with(p))
        {
            return;
        }
        let has_partial = self
            .moved
            .iter()
            .any(|(l, p)| *l == local && p.starts_with(path));
        if !has_partial {
            self.release_value(place, ty);
            return;
        }
        match self.types.kind(ty).clone() {
            TyKind::Array(elem, len) => {
                for k in 0..len {
                    path.push(k as u32);
                    let index = Proj::Index(Operand::Const(Const::Int(k as i64)));
                    self.release_unmoved(local, path, place.with(index), elem);
                    path.pop();
                }
            }
            TyKind::Tuple(elems) => {
                for (i, &elem) in elems.iter().enumerate() {
                    path.push(i as u32);
                    self.release_unmoved(local, path, place.with(Proj::Field(i as u32)), elem);
                    path.pop();
                }
            }
            TyKind::Struct(s) if self.types.structs[s.index()].variants.is_none() => {
                let fields: Vec<Ty> = self.types.structs[s.index()]
                    .fields
                    .iter()
                    .map(|&(_, t)| t)
                    .collect();
                for (i, fty) in fields.into_iter().enumerate() {
                    path.push(i as u32);
                    self.release_unmoved(local, path, place.with(Proj::Field(i as u32)), fty);
                    path.pop();
                }
            }
            _ => self.release_value(place, ty),
        }
    }

    /// Emits [`Stmt::Release`] for every qubit in `place` of type `ty`.
    fn release_value(&mut self, place: Place, ty: Ty) {
        if self.types.linearity(ty) != noev_sema::ty::Kind::Linear {
            return;
        }
        match self.types.kind(ty).clone() {
            TyKind::Qubit => self.push(Stmt::Release(place)),
            TyKind::Array(elem, len) => {
                for k in 0..len {
                    let index = Proj::Index(Operand::Const(Const::Int(k as i64)));
                    self.release_value(place.with(index), elem);
                }
            }
            TyKind::Tuple(elems) => {
                for (i, &elem) in elems.iter().enumerate() {
                    self.release_value(place.with(Proj::Field(i as u32)), elem);
                }
            }
            TyKind::Struct(s) => match self.types.structs[s.index()].variants.clone() {
                None => {
                    let fields: Vec<Ty> = self.types.structs[s.index()]
                        .fields
                        .iter()
                        .map(|&(_, t)| t)
                        .collect();
                    for (i, fty) in fields.into_iter().enumerate() {
                        self.release_value(place.with(Proj::Field(i as u32)), fty);
                    }
                }
                Some(variants) => {
                    let tag = self.temp(Ty::INT);
                    self.assign(tag.clone(), Rvalue::Tag(Operand::Copy(place.clone())));
                    let join = self.new_block();
                    for (idx, variant) in variants.iter().enumerate() {
                        if !variant
                            .data
                            .iter()
                            .any(|&t| self.types.linearity(t) == noev_sema::ty::Kind::Linear)
                        {
                            continue;
                        }
                        let pass = self.new_block();
                        let next = self.new_block();
                        let cond = self.compare(
                            Operand::Copy(tag.clone()),
                            Operand::Const(Const::Int(idx as i64)),
                        );
                        self.terminate_into(
                            Terminator::Branch {
                                cond,
                                then: pass,
                                otherwise: next,
                                control: None,
                            },
                            pass,
                        );
                        for (fi, &fty) in variant.data.iter().enumerate() {
                            self.release_value(place.with(Proj::Field(fi as u32)), fty);
                        }
                        self.terminate_into(Terminator::Goto(join), next);
                    }
                    self.terminate_into(Terminator::Goto(join), join);
                }
            },
            _ => {}
        }
    }

    fn local(&self, local: LocalId) -> Local {
        self.locals[local.index()].expect("locals are declared before use")
    }

    fn ty(&mut self, id: ExprId) -> Ty {
        let ty = self.typed.ty(id);
        self.subst(ty)
    }

    /// `ty` in this copy of a generic function: its type parameters replaced.
    fn subst(&mut self, ty: Ty) -> Ty {
        let ty = self.types.subst(ty, &self.params, &self.args);
        // With its base known, an associated type is what the base's `impl` says.
        noev_sema::check::normalize(&self.typed.impls, self.types, ty)
    }

    /// The type of the source local `local`, in this copy.
    fn local_ty(&mut self, local: LocalId) -> Ty {
        let ty = self.typed.local(local).ty;
        self.subst(ty)
    }

    /// For a call `call` of a `std::ops` or `std::cmp` trait's function on a built-in type
    /// (in a copy of generic code, `T: Add` given `Int`), the built-in operator it is
    /// (`spec/types.md` §6).
    fn builtin_operator(&mut self, f: ast::FnId, call: ExprId) -> Option<BuiltinOp> {
        let &t = self.typed.fn_traits.get(&f)?;
        let kind = self.types.op_trait(t.index() as u32)?;
        let args: Vec<Ty> = self.typed.type_args.get(&call)?.clone();
        let args: Vec<Ty> = args.into_iter().map(|a| self.subst(a)).collect();
        let (&self_ty, rest) = args.split_first()?;
        let n = self.typed.traits[t.index()].params.len();
        let trait_args = rest.get(..n)?;
        if self
            .typed
            .impl_for(self.types, t, trait_args, self_ty)
            .is_some()
        {
            return None;
        }
        self.types
            .builtin_op(t.index() as u32, self_ty, Some(trait_args))?;
        let op = match kind {
            OpKind::Neg => BuiltinOp::Unary(UnOp::Neg),
            OpKind::BitNot => BuiltinOp::Unary(UnOp::BitNot),
            OpKind::Binary(op) => BuiltinOp::Binary(op),
            OpKind::Assign(op) => BuiltinOp::Assign(op),
            OpKind::Eq => {
                let idx = self.typed.traits[t.index()]
                    .fns
                    .iter()
                    .position(|&tf| tf == f)?;
                BuiltinOp::Cmp(if idx == 0 {
                    ast::BinaryOp::Eq
                } else {
                    ast::BinaryOp::NotEq
                })
            }
            OpKind::Ord => {
                let idx = self.typed.traits[t.index()]
                    .fns
                    .iter()
                    .position(|&tf| tf == f)?;
                let op = match idx {
                    0 => ast::BinaryOp::Lt,
                    1 => ast::BinaryOp::Le,
                    2 => ast::BinaryOp::Gt,
                    _ => ast::BinaryOp::Ge,
                };
                BuiltinOp::Cmp(op)
            }
        };
        Some(op)
    }

    fn measured_by_parts(&mut self, f: ast::FnId, call: ExprId) -> Option<Ty> {
        let &t = self.typed.fn_traits.get(&f)?;
        if Some(t.index() as u32) != self.types.measure {
            return None;
        }
        let &self_ty = self.typed.type_args.get(&call)?.first()?;
        let self_ty = self.subst(self_ty);
        let by_parts = self.types.is_measurable(self_ty)
            || matches!(
                self.types.kind(self_ty),
                TyKind::Tuple(_) | TyKind::Array(..)
            );
        by_parts.then_some(self_ty)
    }

    /// Measures the value at `source`, of type `ty`, into `dest`: qubits by the built-in
    /// measurement, tuples and arrays element by element, and anything else by its `impl` of
    /// `Measure`, whose function is `measure`.
    fn measure_into(&mut self, dest: Place, source: Place, ty: Ty, measure: ast::FnId) {
        match self.types.kind(ty).clone() {
            TyKind::Qubit => self.push(Stmt::Measure {
                dest,
                qubit: source,
            }),
            TyKind::Array(elem, len) => {
                for k in 0..len {
                    let index = Proj::Index(Operand::Const(Const::Int(k as i64)));
                    self.measure_into(dest.with(index.clone()), source.with(index), elem, measure);
                }
            }
            TyKind::Tuple(elems) => {
                for (i, &elem) in elems.iter().enumerate() {
                    let field = Proj::Field(i as u32);
                    self.measure_into(dest.with(field.clone()), source.with(field), elem, measure);
                }
            }
            _ => {
                let func = self.func_for(measure, vec![ty]);
                let next = self.new_block();
                self.terminate_into(
                    Terminator::Call {
                        func,
                        args: vec![Operand::Move(source)],
                        dest,
                        adjoint: false,
                        next,
                    },
                    next,
                );
            }
        }
    }

    /// The function a call `call` of `f` runs: `f` itself, or the copy of it for the call's
    /// type arguments, made if this is the first call to ask for it.
    fn func(&mut self, f: ast::FnId, call: ExprId) -> FuncId {
        if self.typed.fns[f.index()].generics.is_empty() {
            return FuncId(f.index() as u32);
        }
        let args: Vec<Ty> = self.typed.type_args.get(&call).cloned().unwrap_or_default();
        let args: Vec<Ty> = args.into_iter().map(|t| self.subst(t)).collect();
        self.func_for(f, args)
    }

    /// The function `f` with the type arguments `args`, with no parameters left: `f`, the
    /// `impl`'s function it stands for, or a copy of either, made if this is the first call
    /// to ask for it.
    fn func_for(&mut self, f: ast::FnId, args: Vec<Ty>) -> FuncId {
        // A trait's function runs its `impl`'s, or its own default, for this `Self`.
        let (f, args) = self
            .typed
            .dispatch(self.types, f, &args)
            .unwrap_or((f, args));
        if self.typed.fns[f.index()].generics.is_empty() {
            return FuncId(f.index() as u32);
        }
        if let Some(&id) = self.instances.ids.get(&(f, args.clone())) {
            return id;
        }
        let slot = self.runs.len();
        self.runs.push(stub(String::new(), Span::default()));
        let id = FuncId(self.first_run + slot as u32);
        self.instances.ids.insert((f, args.clone()), id);
        self.instances.pending.push((f, args, slot));
        id
    }

    fn is_ref(&self, ty: Ty) -> bool {
        matches!(self.types.kind(ty), TyKind::Ref { .. })
    }

    fn copyable(&self, ty: Ty) -> bool {
        self.types.linearity(ty) == noev_sema::ty::Kind::Copyable
    }

    fn text(&self, span: Span) -> &'a str {
        self.source.text_of(span)
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
        | Terminator::CallIndirect { next, .. }
        | Terminator::Undo { next, .. }
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
        | Terminator::CallIndirect { next, .. }
        | Terminator::Undo { next, .. }
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
        let scope_mark = self.linear_scope.len();
        let stmts = ast.list(ast.block(block).stmts);
        for (i, &stmt) in stmts.iter().enumerate() {
            if self.diverged {
                break;
            }
            match ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                    self.expr_into_or_release(expr, dest.clone());
                }
                _ => self.stmt(stmt),
            }
        }
        if !self.diverged {
            self.release_scope(scope_mark);
        }
        self.linear_scope.truncate(scope_mark);
    }

    /// Lowers `expr` into `dest`, or if `dest` is `None` and `expr` produces a linear value
    /// (proven \|0⟩ by the ownership checker), lowers into a temporary and releases its qubits.
    fn expr_into_or_release(&mut self, expr: ExprId, dest: Option<Place>) {
        if dest.is_none() && !matches!(self.ast.expr(expr).kind, ExprKind::With { .. }) {
            let ty = self.ty(expr);
            if self.types.linearity(ty) == noev_sema::ty::Kind::Linear {
                let temp = self.temp(ty);
                self.expr_into(expr, Some(temp.clone()));
                self.release_value(temp, ty);
                return;
            }
        }
        self.expr_into(expr, dest);
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
                    let temp = {
                        let ty = self.ty(init);
                        self.temp(ty)
                    };
                    self.expr_into(init, Some(temp.clone()));
                    self.bind(pat, temp, false);
                }
            },
            // `a += b` on a type of the program's own calls `AddAssign::add_assign`.
            StmtKind::Assign { place, .. } if self.typed.callees[place.index()].is_some() => {
                self.call(place, None, false);
            }
            StmtKind::Assign { op, place, value } => {
                let target = self.place_deref(place);
                let value = self.operand(value);
                if op == ast::AssignOp::Assign
                    && let Some((root, path)) = self.static_place_path(place)
                    && let Some((_, ir, _)) = self
                        .linear_scope
                        .iter()
                        .find(|&&(l, _, _)| l == root)
                        .copied()
                {
                    let ty = self.ty(place);
                    let mut p = path;
                    let mut place_ir = Place::local(ir);
                    for &step in &p {
                        place_ir = place_ir.with(Proj::Field(step));
                    }
                    self.release_unmoved(root, &mut p, place_ir, ty);
                }
                let rvalue = match op {
                    ast::AssignOp::Assign => Rvalue::Use(value),
                    ast::AssignOp::Compound(op) => {
                        Rvalue::Binary(op, Operand::Copy(target.clone()), value)
                    }
                };
                // `d /= 0` fails here, so errors point at the statement.
                let outer = std::mem::replace(&mut self.span, stmt.span);
                self.assign(target, rvalue);
                self.span = outer;
                if op == ast::AssignOp::Assign {
                    self.unmark_moved(place);
                }
            }
            StmtKind::Return(value) => {
                if let Some(value) = value {
                    self.expr_into(value, Some(Place::local(Local(0))));
                }
                self.release_scope(0);
                self.terminate(Terminator::Return);
                self.diverged = true;
            }
            StmtKind::Break | StmtKind::Continue => {
                let &(cont, brk, scope_mark) = self.loops.last().expect("checked: inside a loop");
                self.release_scope(scope_mark);
                let target = if stmt.kind == StmtKind::Break {
                    brk
                } else {
                    cont
                };
                self.terminate(Terminator::Goto(target));
                self.diverged = true;
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
            StmtKind::Expr { expr, .. } => self.expr_into_or_release(expr, None),
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
        let lent = !matches!(
            self.ast.expr(scrutinee).kind,
            ExprKind::Own {
                kind: OwnKind::Mv,
                ..
            }
        ) && (self.is_ref(ty) || self.is_place_expr(scrutinee));
        let moved_before = self.moved.clone();
        let mut arm_moved = None;
        let mut all_diverged = true;
        for arm in self.ast.list(arms).to_vec() {
            self.moved = moved_before.clone();
            self.diverged = false;
            let scope_mark = self.linear_scope.len();
            let next = self.new_block();
            self.test_or_jump(arm.pat, &value, next, &control);
            self.bind(arm.pat, value.clone(), lent);
            self.expr_into_or_release(arm.body, dest.clone());
            if !self.diverged {
                self.release_scope(scope_mark);
                if arm_moved.is_none() {
                    arm_moved = Some(self.moved.clone());
                }
            }
            all_diverged &= self.diverged;
            self.linear_scope.truncate(scope_mark);
            self.terminate_into(Terminator::Goto(join), next);
        }
        self.diverged = all_diverged;
        self.moved = arm_moved.unwrap_or(moved_before);
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
                    ast::PatLiteral::Int(v) => match self.types.deref(ty).width() {
                        Some(w) => Operand::Const(Const::Sized(i128::from(v), w)),
                        None => Operand::Const(Const::Int(v)),
                    },
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
    fn bind(&mut self, pat: ast::PatId, value: Place, lent: bool) {
        match self.ast.pat(pat).kind {
            ast::PatKind::Name(_) => {
                let local = self.typed.pat_locals[pat.index()].expect("checked");
                let ty = self.local_ty(local);
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
            ast::PatKind::Wildcard => {
                if !lent {
                    let ty = self.subst(self.typed.pat_types[pat.index()]);
                    if self.types.linearity(ty) == noev_sema::ty::Kind::Linear {
                        self.release_value(value, ty);
                    }
                }
            }
            ast::PatKind::Tuple(pats) => {
                for (i, &p) in self.ast.list(pats).iter().enumerate() {
                    self.bind(p, value.with(Proj::Field(i as u32)), lent);
                }
            }
            ast::PatKind::Array(pats) => {
                for (i, &p) in self.ast.list(pats).iter().enumerate() {
                    let index = Operand::Const(Const::Int(i as i64));
                    self.bind(p, value.with(Proj::Index(index)), lent);
                }
            }
            ast::PatKind::Literal(_) => {}
            ast::PatKind::Variant { data, .. } => {
                for (i, p) in self.variant_parts(pat, data) {
                    self.bind(p, value.with(Proj::Field(i)), lent);
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
            ExprKind::Field { base, name } => {
                let base_place = self.place_deref(base);
                let struct_ty = {
                    let ty = self.ty(base);
                    self.types.deref(ty)
                };
                if let TyKind::Struct(s) = *self.types.kind(struct_ty) {
                    let index = self.types.structs[s.index()]
                        .fields
                        .iter()
                        .position(|(n, _)| *n == name)
                        .expect("checked");
                    return base_place.with(Proj::Field(index as u32));
                }
            }
            // Into a place, or into a temporary holding a value such as a call's (`g()[1]`).
            ExprKind::TupleIndex { base, index } => {
                return self.place_deref(base).with(Proj::Field(index));
            }
            ExprKind::Index { base, index } => {
                let base_place = self.place_deref(base);
                let index = self.index_operand(index);
                return base_place.with(Proj::Index(index));
            }
            _ => {}
        }
        let temp = {
            let ty = self.ty(id);
            self.temp(ty)
        };
        self.expr_into(id, Some(temp.clone()));
        temp
    }

    /// Like [`Builder::place`], but through a reference: the place a reference refers to.
    fn place_deref(&mut self, id: ExprId) -> Place {
        let place = self.place(id);
        let ty = self.ty(id);
        if self.is_ref(ty) {
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

    /// A constant or size-parameter expression as an operand, without applying `Adjust::Lend`.
    fn const_operand(&mut self, id: ExprId) -> Option<Operand> {
        let expr = *self.ast.expr(id);
        match expr.kind {
            // A size parameter: its value in this copy.
            ExprKind::Name(_) if let Res::Size(size) = self.typed.res[id.index()] => {
                let size = self.subst(size);
                let TyKind::Size(n) = *self.types.kind(size) else {
                    unreachable!("a size parameter is a size in each copy");
                };
                Some(Operand::Const(Const::Int(n as i64)))
            }
            ExprKind::Name(_) | ExprKind::Path(_)
                if let Res::FnPtr(f) = self.typed.res[id.index()] =>
            {
                let ty = self.ty(id);
                let temp = self.temp(ty);
                let func = self.func(f, id);
                self.assign(temp.clone(), Rvalue::FnPtr(func));
                Some(Operand::Copy(temp))
            }
            ExprKind::Int(v) if self.ty(id) == Ty::BIT => Some(Operand::Const(Const::Bit(v == 1))),
            ExprKind::Int(v) if let Some(w) = self.ty(id).width() => {
                Some(Operand::Const(Const::Sized(i128::from(v), w)))
            }
            ExprKind::Int(v) if let Ok(v) = i64::try_from(v) => Some(Operand::Const(Const::Int(v))),
            // Past `i64`: computed from its text (spec/types.md §1).
            ExprKind::Int(_) | ExprKind::BigInt => {
                let temp = self.temp(Ty::INT);
                let text = self.text(expr.span).to_string();
                self.assign(temp.clone(), Rvalue::BigInt(text));
                Some(Operand::Copy(temp))
            }
            // A float or imaginary literal, of the type it took.
            ExprKind::Float(v) => Some(Operand::Const(match self.ty(id) {
                Ty::F32 => Const::F32(v as f32),
                Ty::COMPLEX => Const::Complex(v, 0.0),
                Ty::C32 => Const::C32(v as f32, 0.0),
                _ => Const::Float(v),
            })),
            ExprKind::Imag(v) => Some(Operand::Const(match self.ty(id) {
                Ty::C32 => Const::C32(0.0, v as f32),
                _ => Const::Complex(0.0, v),
            })),
            ExprKind::Bool(v) => Some(Operand::Const(Const::Bool(v))),
            ExprKind::Unit => Some(Operand::Const(Const::Unit)),
            _ => None,
        }
    }

    /// An expression's value as an operand, applying any implicit lend or dereference.
    fn operand(&mut self, id: ExprId) -> Operand {
        let expr = *self.ast.expr(id);
        let adjust = self.typed.adjust[id.index()];
        if let Adjust::Lend { mutable } = adjust {
            let place = if let Some(op) = self.const_operand(id) {
                let ty = self.ty(id);
                let temp = self.temp(ty);
                self.assign(temp.clone(), Rvalue::Use(op));
                temp
            } else {
                self.lend_target(id)
            };
            let ref_ty = {
                let ty = self.ty(id);
                self.types.reference(mutable, ty)
            };
            let temp = self.temp(ref_ty);
            self.assign(temp.clone(), Rvalue::Ref { mutable, place });
            return Operand::Copy(temp);
        }
        if let Some(op) = self.const_operand(id) {
            return op;
        }
        // A projection is read from its place, which `place` makes from a temporary when its
        // base is a value such as a call's result.
        let projection = matches!(
            expr.kind,
            ExprKind::Field { .. } | ExprKind::TupleIndex { .. } | ExprKind::Index { .. }
        );
        if self.is_place_expr(id) || projection {
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
                self.mark_moved(place);
                let ty = self.ty(place);
                let p = self.place(place);
                return self.read(p, ty);
            }
            ExprKind::Own { kind, place } => {
                let mutable = kind == OwnKind::Mut;
                let target = self.place_deref(place);
                let temp = {
                    let ty = self.ty(id);
                    self.temp(ty)
                };
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

    /// An expression's value, following any references (including an implicit `Adjust::Lend`
    /// that was recorded for a `ref` parameter), for built-in comparison operators.
    fn deref_operand(&mut self, id: ExprId) -> Operand {
        let op = self.operand(id);
        let mut ty = self.ty(id);
        match self.typed.adjust[id.index()] {
            Adjust::Lend { mutable } => ty = self.types.reference(mutable, ty),
            Adjust::Deref => ty = self.types.deref(ty),
            Adjust::None => {}
        }
        if !self.is_ref(ty) {
            return op;
        }
        let mut place = match op {
            Operand::Copy(place) | Operand::Move(place) => place,
            constant @ Operand::Const(_) => return constant,
        };
        while let TyKind::Ref { inner, .. } = *self.types.kind(ty) {
            place = place.with(Proj::Deref);
            ty = inner;
        }
        Operand::Copy(place)
    }

    /// An expression's value, following any references, for printing.
    fn value_operand(&mut self, id: ExprId) -> Operand {
        let op = self.operand(id);
        if !{
            let ty = self.ty(id);
            self.is_ref(ty)
        } {
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
            let temp = {
                let ty = self.ty(id);
                self.temp(ty)
            };
            self.expr_into(id, Some(temp.clone()));
            temp
        }
    }

    /// The qubit place a gate acts on, given its target expression: a place, a reference,
    /// `mut q`, or a chain of gate calls.
    fn qubit_place(&mut self, id: ExprId) -> Place {
        let ast = self.ast;
        match ast.expr(id).kind {
            ExprKind::Own { kind, place } => {
                if kind == OwnKind::Mv {
                    self.mark_moved(place);
                }
                self.qubit_place(place)
            }
            ExprKind::MethodCall { .. } if self.is_gate_chain(id) => self.gate_call(id),
            _ if self.is_place_expr(id) => self.place_deref(id),
            _ => {
                let temp = {
                    let ty = self.ty(id);
                    self.temp(ty)
                };
                self.expr_into(id, Some(temp.clone()));
                let ty = self.ty(id);
                if self.is_ref(ty) {
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
            ExprKind::Error => {}
            ExprKind::With { with, body } => {
                self.push(Stmt::WithStart);
                self.in_with_block += 1;
                let stmts = ast.list(ast.block(with).stmts);
                for (i, &stmt) in stmts.iter().enumerate() {
                    if self.diverged {
                        break;
                    }
                    match ast.stmt(stmt).kind {
                        StmtKind::Expr { expr, semi: false } if i + 1 == stmts.len() => {
                            self.expr_into(expr, None);
                        }
                        _ => self.stmt(stmt),
                    }
                }
                self.in_with_block -= 1;
                self.push(Stmt::WithDo);
                let outer_with = std::mem::replace(&mut self.in_with_block, 0);
                self.block_into(body, dest);
                self.in_with_block = outer_with;
                self.push(Stmt::WithEnd);
            }
            ExprKind::Int(_)
            | ExprKind::BigInt
            | ExprKind::Float(_)
            | ExprKind::Imag(_)
            | ExprKind::Bool(_)
            | ExprKind::Unit => {
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
                            let ty = {
                                let ty = self.ty(expr);
                                self.types.deref(ty)
                            };
                            let op = self.value_operand(expr);
                            out.push(FormatPart::Value(op, ty, spec));
                        }
                    }
                }
                self.set(&dest, Rvalue::Format(out));
            }
            ExprKind::Ket { parts, .. } => {
                let state = self.text(expr.span)[1..expr.span.len() as usize - 1].to_string();
                let labels = self.labels(&state, parts);
                let dest = dest.unwrap_or_else(|| {
                    let ty = self.ty(id);
                    self.temp(ty)
                });
                self.ket(&labels, &dest);
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
            ExprKind::ArrayRepeat { value, .. } => {
                // The length, from the type: a literal, or a size parameter's in this copy.
                let TyKind::Array(_, count) = *({
                    let ty = self.ty(id);
                    self.types.kind(ty)
                }) else {
                    return;
                };
                if let ExprKind::Ket { parts, .. } = ast.expr(value).kind {
                    let span = ast.expr(value).span;
                    let state = self.text(span)[1..span.len() as usize - 1].to_string();
                    let dest = dest.unwrap_or_else(|| {
                        let ty = self.ty(id);
                        self.temp(ty)
                    });
                    // Each element's computed labels are evaluated afresh, as the ket is.
                    for k in 0..count {
                        let labels = self.labels(&state, parts);
                        let element = dest.with(Proj::Index(Operand::Const(Const::Int(k as i64))));
                        self.ket(&labels, &element);
                    }
                } else {
                    let op = self.operand(value);
                    self.set(&dest, Rvalue::Repeat(op, count));
                }
            }
            ExprKind::StructLit { fields, .. } => self.struct_lit(id, fields, dest),
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => self.call(id, dest, false),
            // `a + b` on a type of the program's own calls `Add::add`.
            ExprKind::Unary { .. } | ExprKind::Binary { .. }
                if self.typed.is_operator_call(ast, id) =>
            {
                self.call(id, dest, false)
            }
            ExprKind::Adj(call) => self.call(call, dest, true),
            ExprKind::Unary { op, operand } => {
                let x = self.deref_operand(operand);
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
                let a = self.deref_operand(lhs);
                let b = self.deref_operand(rhs);
                self.set(&dest, Rvalue::Binary(op, a, b));
            }
            ExprKind::PrepTest { prep, operand } => {
                let (ExprKind::Call { args, .. }, Some(Callee::Fn(f))) =
                    (ast.expr(prep).kind, self.typed.callees[prep.index()])
                else {
                    unreachable!("checked: a call to a preparation")
                };
                let args = ast.list(args).iter().map(|&a| self.operand(a)).collect();
                let target = self.qubit_place(operand);
                let next = self.new_block();
                let func = self.func(f, prep);
                self.terminate_into(
                    Terminator::Undo {
                        func,
                        args,
                        target: target.clone(),
                        next,
                    },
                    next,
                );
                // Undone, the qubits are all |0⟩ exactly when they held the state.
                let qubits = {
                    let ty = self.ty(operand);
                    let ty = self.types.deref(ty);
                    self.types.qubit_count(ty)
                };
                let zeros = vec![LabelOp::Fixed(Label::Zero); qubits.unwrap_or(1) as usize];
                self.bra_test(&zeros, target, dest);
            }
            ExprKind::BraTest {
                state,
                operand,
                parts,
                ..
            } => {
                let state = self.text(state)[1..state.len() as usize - 1].to_string();
                let labels = self.labels(&state, parts);
                let base = self.qubit_place(operand);
                self.bra_test(&labels, base, dest);
            }
            ExprKind::Overlap {
                bra,
                ket,
                observable,
                precision,
            } => {
                let precision = precision.map(|p| self.operand(p));
                let (Some(bra), Some(ket)) = (self.prep(bra), self.prep(ket)) else {
                    return;
                };
                let observable = observable.map(|o| self.operand(o));
                let dest = dest.unwrap_or_else(|| self.temp(Ty::COMPLEX));
                self.push(Stmt::Overlap {
                    dest,
                    bra,
                    ket,
                    observable,
                    precision,
                });
            }
            ExprKind::Expectation {
                prep,
                observable,
                precision,
            } => {
                let precision = precision.map(|p| self.operand(p));
                let Some(prep) = self.prep(prep) else {
                    return;
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
                        let ty = self.local_ty(l);
                        self.read(Place::local(self.local(l)), ty)
                    })
                    .collect();
                let shots = self.operand(shots);
                let TyKind::Distribution(elem) = *({
                    let ty = self.ty(id);
                    self.types.kind(ty)
                }) else {
                    return;
                };
                let dest = dest.unwrap_or_else(|| {
                    let ty = self.ty(id);
                    self.temp(ty)
                });
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
                let moved_before = self.moved.clone();
                self.diverged = false;
                self.block_into(then, dest.clone());
                let then_diverged = self.diverged;
                let then_moved = std::mem::replace(&mut self.moved, moved_before);
                self.terminate_into(Terminator::Goto(join), else_block);
                self.diverged = false;
                if let Some(otherwise) = otherwise {
                    self.expr_into_or_release(otherwise, dest);
                }
                let else_diverged = self.diverged;
                let else_moved = std::mem::take(&mut self.moved);
                self.diverged = then_diverged && else_diverged;
                self.moved = if !then_diverged {
                    then_moved
                } else {
                    else_moved
                };
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
                let scope_mark = self.linear_scope.len();
                self.loops.push((head, exit, scope_mark));
                self.diverged = false;
                self.block_into(body, None);
                self.loops.pop();
                self.diverged = false;
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

    /// A ket's or bra's labels in order: its text's fixed labels, or with computed ones
    /// (`parts`), each value's (`spec/types.md` §3), an array's one per element.
    fn labels(&mut self, text: &str, parts: ast::List<ast::KetPart>) -> Vec<LabelOp> {
        let ast = self.ast;
        if parts.is_empty() {
            return labels(text).map(LabelOp::Fixed).collect();
        }
        let mut out = Vec::new();
        for &part in ast.list(parts) {
            let value = match part {
                ast::KetPart::Label(label) => {
                    out.push(LabelOp::Fixed(label));
                    continue;
                }
                ast::KetPart::Value(value) => value,
            };
            let ty = {
                let ty = self.ty(value);
                self.types.deref(ty)
            };
            let op = self.operand(value);
            let TyKind::Array(_, len) = *self.types.kind(ty) else {
                out.push(LabelOp::Value(op));
                continue;
            };
            let held = self.temp(ty);
            self.assign(held.clone(), Rvalue::Use(op));
            for k in 0..len {
                let element = held.with(Proj::Index(Operand::Const(Const::Int(k as i64))));
                out.push(LabelOp::Value(Operand::Copy(element)));
            }
        }
        out
    }

    /// Allocates qubits for a ket's labels at `dest`: one qubit, or an array for several.
    fn ket(&mut self, labels: &[LabelOp], dest: &Place) {
        let array = labels.len() > 1;
        for (k, label) in labels.iter().enumerate() {
            let qubit = if array {
                dest.with(Proj::Index(Operand::Const(Const::Int(k as i64))))
            } else {
                dest.clone()
            };
            self.push(Stmt::Alloc(qubit.clone()));
            let label = match label {
                LabelOp::Fixed(label) => *label,
                LabelOp::Value(value) => {
                    self.push(Stmt::Prepare {
                        qubit,
                        label: value.clone(),
                    });
                    continue;
                }
            };
            for &gate in label_gates(label) {
                self.push(Stmt::Gate {
                    gate,
                    adjoint: false,
                    qubits: vec![qubit.clone()],
                    params: Vec::new(),
                });
            }
        }
    }

    /// `<s| x`, with `x` at `base`: measure each qubit in its label's basis and compare.
    fn bra_test(&mut self, labels: &[LabelOp], base: Place, dest: Option<Place>) {
        let array = labels.len() > 1;
        let mut result: Option<Operand> = None;
        for (k, label) in labels.iter().enumerate() {
            let qubit = if array {
                base.with(Proj::Index(Operand::Const(Const::Int(k as i64))))
            } else {
                base.clone()
            };
            // Into the Z basis, as `measure_in` does: `h` for X, `adj s` then `h` for Y.
            let expected = match label {
                LabelOp::Fixed(label) => {
                    for &(gate, adjoint) in into_z(*label) {
                        self.push(Stmt::Gate {
                            gate,
                            adjoint,
                            qubits: vec![qubit.clone()],
                            params: Vec::new(),
                        });
                    }
                    Operand::Const(Const::Bit(label.outcome()))
                }
                LabelOp::Value(value) => {
                    self.push(Stmt::IntoZ {
                        qubit: qubit.clone(),
                        label: value.clone(),
                    });
                    let bit = self.temp(Ty::BIT);
                    self.assign(bit.clone(), Rvalue::LabelBit(value.clone()));
                    Operand::Copy(bit)
                }
            };
            let bit = self.temp(Ty::BIT);
            self.push(Stmt::Measure {
                dest: bit.clone(),
                qubit,
            });
            let matches = self.temp(Ty::BOOL);
            self.assign(
                matches.clone(),
                Rvalue::Binary(ast::BinaryOp::Eq, Operand::Copy(bit), expected),
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

    /// A preparation: a ket's fixed labels, or a call with classical arguments.
    fn prep(&mut self, prep: ExprId) -> Option<Prep> {
        let ast = self.ast;
        match ast.expr(prep).kind {
            ExprKind::Ket { .. } => {
                let span = ast.expr(prep).span;
                Some(Prep::Ket(
                    self.text(span)[1..span.len() as usize - 1].to_string(),
                ))
            }
            ExprKind::Call { args, .. } => {
                let Some(Callee::Fn(f)) = self.typed.callees[prep.index()] else {
                    return None;
                };
                let args = ast.list(args).iter().map(|&a| self.operand(a)).collect();
                Some(Prep::Call(self.func(f, prep), args))
            }
            _ => None,
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
        let scope_mark = self.linear_scope.len();
        self.loops.push((step, exit, scope_mark));
        self.diverged = false;
        self.block_into(body, None);
        self.loops.pop();
        self.diverged = false;
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
        let TyKind::Struct(s) = *({
            let ty = self.ty(id);
            self.types.kind(ty)
        }) else {
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
                    let ty = self.local_ty(local);
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
        let dest = dest.unwrap_or_else(|| {
            let ty = self.ty(id);
            self.temp(ty)
        });
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
        let Some((receiver, args)) = self.typed.call_parts(ast, id) else {
            return;
        };
        let args = &*args;
        let Some(callee) = self.typed.callees[id.index()] else {
            return;
        };
        // `Measure::measure` of a qubit, or an array of them, is the built-in measurement;
        // of a tuple, or an array of values of our own, it is each element's.
        if let Callee::Fn(f) = callee
            && let Some(self_ty) = self.measured_by_parts(f, id)
        {
            if self.types.is_measurable(self_ty) {
                return self.builtin(id, Builtin::Measure, receiver, args, dest, adjoint);
            }
            let Some(&first) = receiver.as_ref().or(args.first()) else {
                return;
            };
            let source = self.qubit_place(first);
            let dest = dest.unwrap_or_else(|| {
                let ty = self.ty(id);
                self.temp(ty)
            });
            let measure = *self.typed.traits[self.types.measure.expect("named") as usize]
                .fns
                .first()
                .expect("`measure`");
            return self.measure_into(dest, source, self_ty, measure);
        }
        // A `std::ops` or `std::cmp` trait's function on a built-in type is its built-in operator.
        if let Callee::Fn(f) = callee
            && let Some(kind) = self.builtin_operator(f, id)
        {
            let (first, rest) = match receiver {
                Some(r) => (r, args),
                None => (args[0], &args[1..]),
            };
            match kind {
                BuiltinOp::Unary(op) => {
                    let x = self.operand(first);
                    self.set(&dest, Rvalue::Unary(op, x));
                }
                BuiltinOp::Binary(op) => {
                    let (a, b) = (self.operand(first), self.operand(rest[0]));
                    self.set(&dest, Rvalue::Binary(op, a, b));
                }
                BuiltinOp::Cmp(op) => {
                    let (a, b) = (self.deref_operand(first), self.deref_operand(rest[0]));
                    self.set(&dest, Rvalue::Binary(op, a, b));
                }
                BuiltinOp::Assign(op) => {
                    let place = match ast.expr(first).kind {
                        ExprKind::Own { place, .. } => place,
                        _ => first,
                    };
                    let value = self.operand(rest[0]);
                    let target = self.place_deref(place);
                    self.assign(
                        target.clone(),
                        Rvalue::Binary(op, Operand::Copy(target), value),
                    );
                }
            }
            return;
        }
        match callee {
            Callee::Fn(f) => {
                let mut ops = Vec::new();
                if let Some(receiver) = receiver {
                    ops.push(self.receiver_operand(id, receiver));
                }
                ops.extend(args.iter().map(|&a| self.operand(a)));
                let dest = dest.unwrap_or_else(|| {
                    let ty = self.ty(id);
                    self.temp(ty)
                });
                let next = self.new_block();
                let func = self.func(f, id);
                self.terminate_into(
                    Terminator::Call {
                        func,
                        args: ops,
                        dest,
                        adjoint,
                        next,
                    },
                    next,
                );
            }
            Callee::FnPtr => {
                let ExprKind::Call {
                    callee: callee_expr,
                    ..
                } = self.ast.expr(id).kind
                else {
                    unreachable!("Callee::FnPtr only comes from ExprKind::Call");
                };
                let callee_op = self.operand(callee_expr);
                let ops = args.iter().map(|&a| self.operand(a)).collect();
                let dest = dest.unwrap_or_else(|| {
                    let ty = self.ty(id);
                    self.temp(ty)
                });
                let next = self.new_block();
                self.terminate_into(
                    Terminator::CallIndirect {
                        callee: callee_op,
                        args: ops,
                        dest,
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
            Callee::TryConvert(target) => {
                let op = self.operand(args[0]);
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::TryConvert(target, op));
                }
            }
            Callee::Variant(_, index) => {
                let ops = args.iter().map(|&a| self.operand(a)).collect();
                if let Some(dest) = dest {
                    self.assign(dest, Rvalue::Variant(index, ops));
                } else {
                    // Thrown away; still built, since its values were moved into it.
                    let temp = {
                        let ty = self.ty(id);
                        self.temp(ty)
                    };
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
            Builtin::Amplitudes => {
                let Some(first) = first else { return };
                let register = self.operand(first);
                self.set(&dest, Rvalue::Amplitudes(register));
            }
            Builtin::Measure => {
                let Some(first) = first else { return };
                let target = self.qubit_place(first);
                let dest = dest.unwrap_or_else(|| {
                    let ty = self.ty(id);
                    self.temp(ty)
                });
                match *({
                    let ty = self.ty(first);
                    let ty = self.types.deref(ty);
                    self.types.kind(ty)
                }) {
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
                let dest = dest.unwrap_or_else(|| {
                    let ty = self.ty(id);
                    self.temp(ty)
                });
                let len = match *({
                    let ty = self.ty(first);
                    let ty = self.types.deref(ty);
                    self.types.kind(ty)
                }) {
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
                let TyKind::Distribution(elem) = *({
                    let ty = self.ty(first);
                    let ty = self.types.deref(ty);
                    self.types.kind(ty)
                }) else {
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
                        let ty = {
                            let ty = self.ty(a);
                            self.types.deref(ty)
                        };
                        (self.value_operand(a), ty)
                    })
                    .collect();
                self.push(Stmt::Print { stderr, args });
            }
            Builtin::Convert(_) | Builtin::Const(_) => {}
        }
    }
}
