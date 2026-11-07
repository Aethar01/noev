//! Name resolution and type checking.
//!
//! Runs in two passes. The first collects every struct and function and resolves their
//! signatures, so items can be used before they are declared. The second checks each function
//! body, recording what every expression is and refers to in flat tables indexed by expression
//! id ([`Typed`]), which the ownership checker reads.

use noev_syntax::ast::*;
use noev_syntax::{Diagnostic, FxHashMap, Span, Symbol};

use crate::prelude::{self, Builtin, Reversible, Sig};
use crate::ty::{Kind, StructDef, StructId, Ty, TyKind, Types, VariantDef};

/// A local variable: a `let` binding, parameter, `self`, or loop variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(u32);

impl LocalId {
    /// The local's position in [`Typed::locals`].
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The local at `index` in [`Typed::locals`].
    pub fn from_index(index: usize) -> Self {
        LocalId(u32::try_from(index).expect("fewer than 2³² locals"))
    }
}

/// A local variable.
#[derive(Clone, Copy, Debug)]
pub struct Local {
    /// Its name.
    pub name: Symbol,
    /// Its type.
    pub ty: Ty,
    /// Whether it was declared `let mut`.
    pub mutable: bool,
    /// Where it is declared.
    pub span: Span,
}

/// What a name or path refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Res {
    /// Not a name, or unresolved (already reported).
    #[default]
    None,
    /// A local variable.
    Local(LocalId),
    /// A built-in constant.
    Builtin(Builtin),
    /// A variant of an enum, as a value: `Basis::X`.
    Variant(StructId, u32),
}

/// What a call calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callee {
    /// A function or method in the program.
    Fn(FnId),
    /// A built-in.
    Builtin(Builtin),
    /// A conversion, such as `Float(n)`.
    Convert(Ty),
    /// An enum variant with values, such as `Outcome::Success(b)`.
    Variant(StructId, u32),
}

/// An implicit conversion applied to an expression's value where it is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Adjust {
    /// Used as it is.
    #[default]
    None,
    /// Lent implicitly: `h(q)` means `h(mut q)` (`spec/ownership.md` §4).
    Lend {
        /// Whether it is lent for changing.
        mutable: bool,
    },
    /// A reference used as the value it refers to.
    Deref,
}

/// A function's resolved signature and where it lives.
#[derive(Clone, Debug)]
pub struct FnInfo {
    /// Its signature.
    pub sig: Sig,
    /// The struct it is a method or associated function of.
    pub owner: Option<StructId>,
    /// Its parameters as locals, in order, after `self`.
    pub params: Vec<LocalId>,
    /// `self`, for methods whose body uses it.
    pub self_local: Option<LocalId>,
    /// The receiver's type (`Self`, `ref Self` or `mut Self`), for methods.
    pub receiver_ty: Option<Ty>,
}

/// Everything the type checker learned about a program.
#[derive(Debug, Default)]
pub struct Typed {
    /// Every type.
    pub types: Types,
    /// Each expression's type, before any [`Adjust`], indexed by [`ExprId::index`].
    pub expr_types: Vec<Ty>,
    /// What each name, path and `for` loop refers to, indexed by [`ExprId::index`].
    pub res: Vec<Res>,
    /// What each call and method call calls, indexed by [`ExprId::index`].
    pub callees: Vec<Option<Callee>>,
    /// Implicit conversions, indexed by [`ExprId::index`].
    pub adjust: Vec<Adjust>,
    /// How each method call takes its receiver, indexed by [`ExprId::index`].
    pub receivers: Vec<Option<OwnKind>>,
    /// The variable each name pattern binds, indexed by [`PatId::index`].
    pub pat_locals: Vec<Option<LocalId>>,
    /// The type of the value each pattern matches, indexed by [`PatId::index`].
    pub pat_types: Vec<Ty>,
    /// The variant each variant pattern names, indexed by [`PatId::index`].
    pub pat_variants: Vec<Option<u32>>,
    /// Every local variable.
    pub locals: Vec<Local>,
    /// Every function, indexed by [`FnId::index`].
    pub fns: Vec<FnInfo>,
    /// Each `sample` expression's run.
    pub runs: FxHashMap<ExprId, Run>,
}

/// The block of a `sample` expression, which is run separately from the code around it.
#[derive(Clone, Copy, Debug)]
pub struct Run {
    /// The first local declared inside the run: locals before it belong to the code around
    /// it, and are copied into each run.
    pub first_local: u32,
    /// The block's type, before returned qubits are measured.
    pub ty: Ty,
}

impl Typed {
    /// The type of `expr`.
    pub fn ty(&self, expr: ExprId) -> Ty {
        self.expr_types[expr.index()]
    }

    /// A local variable.
    pub fn local(&self, id: LocalId) -> &Local {
        &self.locals[id.index()]
    }
}

/// What the surrounding code wants from an expression.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Nothing in particular.
    None,
    /// A value of this type (coerced if possible, reported if not).
    Ty(Ty),
    /// A hint for integer literals only: `0` and `1` become `Bit` next to a `Bit`.
    Hint(Ty),
    /// The value is thrown away, as in a statement or a block of type `()`.
    Discard,
}

impl Expect {
    fn ty(self) -> Option<Ty> {
        match self {
            Expect::Ty(ty) | Expect::Hint(ty) => Some(ty),
            Expect::None | Expect::Discard => None,
        }
    }
}

/// Resolves names and checks types in `ast`.
pub fn check(ast: &Ast) -> (Typed, Vec<Diagnostic>) {
    let exprs = ast.all_exprs().len();
    let mut checker = Checker {
        ast,
        t: Typed {
            expr_types: vec![Ty::ERROR; exprs],
            res: vec![Res::None; exprs],
            callees: vec![None; exprs],
            adjust: vec![Adjust::None; exprs],
            receivers: vec![None; exprs],
            pat_locals: vec![None; ast.all_pats().len()],
            pat_types: vec![Ty::ERROR; ast.all_pats().len()],
            pat_variants: vec![None; ast.all_pats().len()],
            ..Typed::default()
        },
        diagnostics: Vec::new(),
        fn_names: FxHashMap::default(),
        struct_names: FxHashMap::default(),
        methods: FxHashMap::default(),
        scope: Vec::new(),
        self_ty: None,
        ret: Ty::UNIT,
        loops: 0,
        runs: 0,
        lent_bindings: None,
        std_enums: FxHashMap::default(),
    };
    checker.collect();
    checker.check_bodies();
    (checker.t, checker.diagnostics)
}

struct Checker<'a> {
    ast: &'a Ast,
    t: Typed,
    diagnostics: Vec<Diagnostic>,
    fn_names: FxHashMap<Symbol, FnId>,
    struct_names: FxHashMap<Symbol, StructId>,
    methods: FxHashMap<(StructId, Symbol), FnId>,
    /// Locals in scope, innermost last.
    scope: Vec<(Symbol, LocalId)>,
    /// `Self` inside a struct's methods.
    self_ty: Option<Ty>,
    /// The current function's return type.
    ret: Ty,
    /// How many loops enclose the current code.
    loops: u32,
    /// How many `sample` runs enclose the current code.
    runs: u32,
    /// While binding a `match` arm's pattern on a lent value: whether the lend is `mut`.
    lent_bindings: Option<bool>,
    /// The `Option` and `Result` types made so far, by name and type arguments.
    std_enums: FxHashMap<(Symbol, Vec<Ty>), StructId>,
}

// Collecting items.
impl Checker<'_> {
    fn error(&mut self, span: Span, message: impl Into<String>) -> &mut Diagnostic {
        self.diagnostics.push(Diagnostic::error(span, message));
        self.diagnostics.last_mut().expect("just pushed")
    }

    fn name(&self, symbol: Symbol) -> &str {
        self.ast.name(symbol)
    }

    fn show(&self, ty: Ty) -> String {
        format!("`{}`", self.t.types.display(ty, self.ast))
    }

    fn collect(&mut self) {
        let ast = self.ast;
        let items = ast.list(ast.root);

        // The standard library's types first: `enum Basis { X, Y, Z }`. A program's own type
        // of the same name takes the name; `measure_in` still uses this one.
        let std_name = |text: &str| ast.symbols.get(text).expect("interned by the parser");
        let variant = |text: &str| VariantDef {
            name: std_name(text),
            span: Span::default(),
            data: Vec::new(),
            names: Vec::new(),
            named: false,
        };
        let basis = StructId(self.t.types.structs.len() as u32);
        self.t.types.structs.push(StructDef {
            name: std_name("Basis"),
            span: Span::default(),
            fields: Vec::new(),
            variants: Some(vec![variant("X"), variant("Y"), variant("Z")]),
            args: Vec::new(),
            bare: false,
        });
        self.struct_names.insert(std_name("Basis"), basis);
        let basis_ty = self.t.types.struct_ty(basis);
        self.t.types.basis = Some(basis_ty);
        let std_types = self.t.types.structs.len() as u32;

        // Struct and enum names next, so field and parameter types can refer to any of them.
        for &item in items {
            let (name, name_span, is_enum) = match ast.item(item).kind {
                ItemKind::Struct {
                    name, name_span, ..
                } => (name, name_span, false),
                ItemKind::Enum {
                    name, name_span, ..
                } => (name, name_span, true),
                _ => continue,
            };
            let id = StructId(self.t.types.structs.len() as u32);
            let earlier = self.struct_names.insert(name, id);
            if earlier.is_some_and(|e| e.0 >= std_types) {
                let message = format!("the type `{}` is defined twice", self.name(name));
                self.error(name_span, message);
            }
            self.t.types.structs.push(StructDef {
                name,
                span: name_span,
                fields: Vec::new(),
                variants: is_enum.then(Vec::new),
                args: Vec::new(),
                bare: false,
            });
        }

        self.t.fns = ast
            .all_fns()
            .iter()
            .map(|_| FnInfo {
                sig: Sig {
                    receiver: None,
                    params: Vec::new(),
                    ret: Ty::UNIT,
                    is_unsafe: false,
                    reversible: Reversible::Infer,
                },
                owner: None,
                params: Vec::new(),
                self_local: None,
                receiver_ty: None,
            })
            .collect();

        let mut next_struct = std_types;
        for &item in items {
            let item = ast.item(item);
            match item.kind {
                ItemKind::Fn(f) => {
                    let decl = ast.function(f);
                    if self.fn_names.insert(decl.name, f).is_some() {
                        let message =
                            format!("the function `{}` is defined twice", self.name(decl.name));
                        self.error(decl.name_span, message);
                    }
                    self.collect_fn(f, None);
                    if self.name(decl.name) == "main" {
                        self.check_main(f);
                    }
                }
                ItemKind::Struct {
                    fields, methods, ..
                } => {
                    let id = StructId(next_struct);
                    next_struct += 1;
                    let self_ty = self.t.types.struct_ty(id);
                    self.self_ty = Some(self_ty);
                    let mut resolved: Vec<(Symbol, Ty)> = Vec::new();
                    for field in ast.list(fields) {
                        if resolved.iter().any(|(name, _)| *name == field.name) {
                            let message =
                                format!("the field `{}` is declared twice", self.name(field.name));
                            self.error(field.span, message);
                        }
                        let ty = self.resolve_type(field.ty);
                        resolved.push((field.name, ty));
                    }
                    self.t.types.structs[id.index()].fields = resolved;
                    for &method in ast.list(methods) {
                        let name = ast.function(method).name;
                        if self.methods.insert((id, name), method).is_some() {
                            let message = format!("`{}` is defined twice", self.name(name));
                            let span = ast.function(method).name_span;
                            self.error(span, message);
                        }
                        self.collect_fn(method, Some(id));
                    }
                    self.self_ty = None;
                }
                ItemKind::Enum {
                    variants, methods, ..
                } => {
                    let id = StructId(next_struct);
                    next_struct += 1;
                    let self_ty = self.t.types.struct_ty(id);
                    self.self_ty = Some(self_ty);
                    let mut resolved: Vec<VariantDef> = Vec::new();
                    for variant in ast.list(variants) {
                        if resolved.iter().any(|v| v.name == variant.name) {
                            let message = format!(
                                "the variant `{}` is declared twice",
                                self.name(variant.name)
                            );
                            self.error(variant.span, message);
                            continue;
                        }
                        let (data, names, named) = match variant.data {
                            VariantData::Unit => (Vec::new(), Vec::new(), false),
                            VariantData::Tuple(types) => {
                                let data = ast
                                    .list(types)
                                    .iter()
                                    .map(|&t| self.resolve_type(t))
                                    .collect();
                                (data, Vec::new(), false)
                            }
                            VariantData::Fields(fields) => {
                                let fields = ast.list(fields);
                                let data = fields.iter().map(|f| self.resolve_type(f.ty)).collect();
                                (data, fields.iter().map(|f| f.name).collect(), true)
                            }
                        };
                        resolved.push(VariantDef {
                            name: variant.name,
                            span: variant.span,
                            data,
                            names,
                            named,
                        });
                    }
                    if resolved.is_empty() {
                        self.error(item.span, "an enum needs at least one variant");
                    }
                    self.t.types.structs[id.index()].variants = Some(resolved);
                    for &method in ast.list(methods) {
                        let name = ast.function(method).name;
                        let clash = self.methods.insert((id, name), method).is_some()
                            || self.t.types.structs[id.index()]
                                .variants
                                .iter()
                                .flatten()
                                .any(|v| v.name == name);
                        if clash {
                            let message = format!("`{}` is defined twice", self.name(name));
                            let span = ast.function(method).name_span;
                            self.error(span, message);
                        }
                        self.collect_fn(method, Some(id));
                    }
                    self.self_ty = None;
                }
                ItemKind::Trait { .. } => {
                    self.error(item.span, "traits are not supported yet").help =
                        Some("they come after v0.1; see spec/types.md §6".into());
                }
                ItemKind::Impl { .. } => {
                    self.error(item.span, "`impl` blocks are not supported yet")
                        .help = Some("methods go inside the struct".into());
                }
                ItemKind::Error => {}
            }
        }
    }

    /// `main` takes no parameters and returns `()` (`spec/execution.md` §6).
    fn check_main(&mut self, f: FnId) {
        let decl = *self.ast.function(f);
        let sig = &self.t.fns[f.index()].sig;
        if !sig.params.is_empty() || sig.ret != Ty::UNIT {
            self.error(
                decl.name_span,
                "`main` must take no parameters and return `()`",
            )
            .help = Some("print results instead of returning them".into());
        }
    }

    fn collect_fn(&mut self, f: FnId, owner: Option<StructId>) {
        let decl = *self.ast.function(f);
        let params = self
            .ast
            .list(decl.params)
            .iter()
            .map(|p| self.resolve_type(p.ty))
            .collect();
        let ret = decl.ret.map_or(Ty::UNIT, |ty| self.resolve_type(ty));
        if decl.receiver.is_some() && owner.is_none() {
            let span = decl.receiver.map_or(decl.name_span, |r| r.span);
            self.error(
                span,
                "`self` is only allowed in methods inside a struct or enum",
            );
        }
        let info = &mut self.t.fns[f.index()];
        info.owner = owner;
        info.sig = Sig {
            receiver: decl.receiver.map(|r| r.kind),
            params,
            ret,
            is_unsafe: decl.is_unsafe,
            reversible: if decl.is_rev {
                Reversible::Yes
            } else {
                Reversible::Infer
            },
        };
    }

    fn resolve_type(&mut self, id: TypeId) -> Ty {
        let ast = self.ast;
        let node = ast.ty(id);
        match node.kind {
            TypeKind::Error => Ty::ERROR,
            TypeKind::Unit => Ty::UNIT,
            TypeKind::Ref { mutable, inner } => {
                let inner = self.resolve_type(inner);
                self.t.types.reference(mutable, inner)
            }
            TypeKind::Tuple(elems) => {
                let elems: Box<[Ty]> = ast
                    .list(elems)
                    .iter()
                    .map(|&t| self.resolve_type(t))
                    .collect();
                self.t.types.intern(TyKind::Tuple(elems))
            }
            TypeKind::Array { elem, len } => {
                let elem = self.resolve_type(elem);
                self.t.types.array(elem, len)
            }
            TypeKind::Generic { path, args } => {
                let args: Vec<Ty> = ast
                    .list(args)
                    .iter()
                    .map(|&a| self.resolve_type(a))
                    .collect();
                let name = self.path_text(path);
                match (name.as_str(), args.as_slice()) {
                    ("Distribution", &[elem]) => {
                        if !self.t.types.is_wildcard(elem) && !self.t.types.is_printable(elem) {
                            let message = format!(
                                "a `Distribution` holds classical values, but {} is not one",
                                self.show(elem)
                            );
                            self.error(node.span, message);
                        }
                        self.t.types.intern(TyKind::Distribution(elem))
                    }
                    ("Distribution", _) => {
                        self.error(node.span, "`Distribution` takes one type argument");
                        Ty::ERROR
                    }
                    ("Option", &[value]) => self.std_enum(vec![value]),
                    ("Result", &[value, error]) => self.std_enum(vec![value, error]),
                    ("Option", _) => {
                        self.error(node.span, "`Option` takes one type argument");
                        Ty::ERROR
                    }
                    ("Result", _) => {
                        self.error(node.span, "`Result` takes two type arguments")
                            .help =
                            Some("write `Result<T, E>`: the value's type, then the error's".into());
                        Ty::ERROR
                    }
                    _ => {
                        let message = format!("`{name}` takes no type arguments");
                        self.error(node.span, message);
                        Ty::ERROR
                    }
                }
            }
            TypeKind::Path(segments) => {
                if let [single] = ast.list(segments) {
                    let name = self.name(*single);
                    if let Some(ty) = prelude::builtin_type(name) {
                        return ty;
                    }
                    if name == "Self" {
                        if let Some(ty) = self.self_ty {
                            return ty;
                        }
                        self.error(node.span, "`Self` is only available inside a struct");
                        return Ty::ERROR;
                    }
                    if let Some(&id) = self.struct_names.get(single) {
                        return self.t.types.struct_ty(id);
                    }
                    if name == "Option" || name == "Result" {
                        let example = if name == "Option" {
                            "Option<Int>"
                        } else {
                            "Result<Int, String>"
                        };
                        let message =
                            format!("`{name}` needs its type arguments, as in `{example}`");
                        self.error(node.span, message);
                        return Ty::ERROR;
                    }
                }
                let text = self.path_text(segments);
                let message = format!("cannot find the type `{text}`");
                self.error(node.span, message);
                Ty::ERROR
            }
        }
    }
}

// Function bodies and statements.
impl Checker<'_> {
    fn check_bodies(&mut self) {
        let ast = self.ast;
        for (index, decl) in ast.all_fns().iter().enumerate() {
            let f = FnId::from_index(index);
            let Some(body) = decl.body else { continue };
            let info = self.t.fns[index].clone();
            self.self_ty = info.owner.map(|id| self.t.types.struct_ty(id));
            self.ret = info.sig.ret;
            self.scope.clear();
            self.loops = 0;
            self.runs = 0;

            if let (Some(receiver), Some(self_ty)) = (decl.receiver, self.self_ty) {
                let ty = match receiver.kind {
                    OwnKind::Mv => self_ty,
                    OwnKind::Ref => self.t.types.reference(false, self_ty),
                    OwnKind::Mut => self.t.types.reference(true, self_ty),
                };
                self.t.fns[f.index()].receiver_ty = Some(ty);
                // `self` is only interned if some body uses it; if none does, no body can
                // refer to it, so it needs no local.
                if let Some(name) = self.ast.symbols.get("self") {
                    let local = self.declare(name, ty, false, receiver.span);
                    self.t.fns[f.index()].self_local = Some(local);
                }
            }
            let mut params = Vec::new();
            for (param, &ty) in ast.list(decl.params).iter().zip(&info.sig.params) {
                if params
                    .iter()
                    .any(|&l: &LocalId| self.t.local(l).name == param.name)
                {
                    let message = format!(
                        "the parameter `{}` is declared twice",
                        self.name(param.name)
                    );
                    self.error(param.span, message);
                }
                params.push(self.declare(param.name, ty, false, param.span));
            }
            self.t.fns[f.index()].params = params;

            if self.ret == Ty::UNIT {
                self.block(body, Expect::Discard);
            } else {
                let ty = self.block(body, Expect::Ty(self.ret));
                let block = ast.block(body);
                let has_tail = matches!(
                    ast.list(block.stmts).last().map(|&s| ast.stmt(s).kind),
                    Some(StmtKind::Expr { semi: false, .. })
                );
                if !has_tail && !self.t.types.is_wildcard(ty) {
                    let message = format!(
                        "`{}` must return {}, but its body ends without a value",
                        self.name(decl.name),
                        self.show(self.ret)
                    );
                    let span = Span::new(block.span.end.saturating_sub(1), block.span.end);
                    self.error(span, message)
                        .with_label_mut(decl.name_span, "declared here");
                }
            }
        }
        self.self_ty = None;
    }

    fn declare(&mut self, name: Symbol, ty: Ty, mutable: bool, span: Span) -> LocalId {
        let id = LocalId(self.t.locals.len() as u32);
        self.t.locals.push(Local {
            name,
            ty,
            mutable,
            span,
        });
        self.scope.push((name, id));
        id
    }

    fn lookup(&self, name: Symbol) -> Option<LocalId> {
        self.scope
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|&(_, id)| id)
    }

    /// Checks a block. Its type is its tail expression's, `()` without one, or `Never` if it
    /// ends in `return`, `break` or `continue`.
    fn block(&mut self, id: BlockId, expect: Expect) -> Ty {
        let ast = self.ast;
        let mark = self.scope.len();
        let stmts = ast.list(ast.block(id).stmts);
        let mut ty = Ty::UNIT;
        let mut diverges = false;
        for (i, &stmt) in stmts.iter().enumerate() {
            let last = i + 1 == stmts.len();
            match ast.stmt(stmt).kind {
                StmtKind::Expr { expr, semi: false } if last => {
                    ty = self.expr(expr, expect);
                }
                _ => {
                    if self.stmt(stmt) {
                        diverges = true;
                    }
                }
            }
        }
        self.scope.truncate(mark);
        if diverges && ty == Ty::UNIT {
            return Ty::NEVER;
        }
        if expect == Expect::Discard && ty != Ty::NEVER {
            return Ty::UNIT;
        }
        ty
    }

    /// Checks a statement, returning whether control never continues past it.
    fn stmt(&mut self, id: StmtId) -> bool {
        let ast = self.ast;
        let stmt = *ast.stmt(id);
        match stmt.kind {
            StmtKind::Let {
                mutable,
                pat,
                ty,
                init,
            } => {
                let annotated = ty.map(|t| self.resolve_type(t));
                let expect = annotated.map_or(Expect::None, Expect::Ty);
                let init_ty = self.expr(init, expect);
                self.bind(pat, annotated.unwrap_or(init_ty), mutable);
                if self.refutable(pat) {
                    let span = ast.pat(pat).span;
                    self.error(
                        span,
                        "this pattern might not match, so it cannot be used in `let`",
                    )
                    .help = Some("use `match` to handle every case".into());
                }
                false
            }
            StmtKind::Assign { op, place, value } => {
                let place_ty = self.expr(place, Expect::None);
                let target = self.t.types.deref(place_ty);
                match op {
                    AssignOp::Assign => {
                        self.expr(value, Expect::Ty(target));
                    }
                    AssignOp::Compound(op) => {
                        let value_ty = self.expr(value, Expect::Hint(target));
                        let result = self.binary_type(op, target, value_ty, stmt.span);
                        if !self.t.types.is_wildcard(result) && result != target {
                            let message = format!(
                                "`{}=` on {} gives {}, which cannot be stored back",
                                op.as_str(),
                                self.show(target),
                                self.show(result)
                            );
                            self.error(stmt.span, message);
                        }
                    }
                }
                false
            }
            StmtKind::Return(value) if self.runs > 0 => {
                if let Some(value) = value {
                    self.expr(value, Expect::None);
                }
                self.error(stmt.span, "`return` cannot leave a `sample` run")
                    .help = Some("the run's result is its block's last expression".into());
                true
            }
            StmtKind::Return(value) => {
                match value {
                    Some(value) => {
                        self.expr(value, Expect::Ty(self.ret));
                    }
                    None if self.ret != Ty::UNIT => {
                        let message = format!("this function must return {}", self.show(self.ret));
                        self.error(stmt.span, message);
                    }
                    None => {}
                }
                true
            }
            StmtKind::Break | StmtKind::Continue => {
                if self.loops == 0 {
                    let word = if stmt.kind == StmtKind::Break {
                        "break"
                    } else {
                        "continue"
                    };
                    let message = format!("`{word}` outside a loop");
                    self.error(stmt.span, message);
                }
                true
            }
            StmtKind::Assert(cond) => {
                self.expr(cond, Expect::Ty(Ty::BOOL));
                false
            }
            StmtKind::Expr { expr, .. } => self.expr(expr, Expect::Discard) == Ty::NEVER,
        }
    }

    fn bind(&mut self, pat: PatId, ty: Ty, mutable: bool) {
        let ast = self.ast;
        let node = *ast.pat(pat);
        self.t.pat_types[pat.index()] = ty;
        match node.kind {
            PatKind::Variant { path, data } => self.bind_variant(pat, path, data, ty, mutable),
            PatKind::Literal(literal) => {
                let fits = match literal {
                    _ if self.t.types.is_wildcard(ty) => true,
                    PatLiteral::Int(v) => ty == Ty::INT || (ty == Ty::BIT && (v == 0 || v == 1)),
                    PatLiteral::Bool(_) => ty == Ty::BOOL,
                    PatLiteral::Str(_) => ty == Ty::STRING,
                };
                if !fits {
                    let message = format!("this pattern cannot match {}", self.show(ty));
                    self.error(node.span, message);
                }
            }
            PatKind::Name(name) => {
                // Matching a lent value: names of parts that can't be copied are references.
                let ty = match self.lent_bindings {
                    Some(lent_mut)
                        if !self.t.types.is_wildcard(ty)
                            && self.t.types.linearity(ty) != Kind::Copyable =>
                    {
                        self.t.types.reference(lent_mut, ty)
                    }
                    _ => ty,
                };
                let local = self.declare(name, ty, mutable, node.span);
                self.t.pat_locals[pat.index()] = Some(local);
            }
            PatKind::Wildcard => {}
            PatKind::Tuple(pats) => {
                let pats = ast.list(pats);
                let elems = match self.t.types.kind(ty) {
                    TyKind::Tuple(elems) if elems.len() == pats.len() => elems.clone(),
                    _ if self.t.types.is_wildcard(ty) => vec![Ty::ERROR; pats.len()].into(),
                    _ => {
                        let message = format!(
                            "this pattern takes a tuple of {}, but the value is {}",
                            pats.len(),
                            self.show(ty)
                        );
                        self.error(node.span, message);
                        vec![Ty::ERROR; pats.len()].into()
                    }
                };
                for (&pat, &ty) in pats.iter().zip(elems.iter()) {
                    self.bind(pat, ty, mutable);
                }
            }
            PatKind::Array(pats) => {
                let pats = ast.list(pats);
                let elem = match *self.t.types.kind(ty) {
                    TyKind::Array(elem, len) if len as usize == pats.len() => elem,
                    _ if self.t.types.is_wildcard(ty) => Ty::ERROR,
                    _ => {
                        let message = format!(
                            "this pattern takes an array of {}, but the value is {}",
                            pats.len(),
                            self.show(ty)
                        );
                        self.error(node.span, message);
                        Ty::ERROR
                    }
                };
                for &pat in pats {
                    self.bind(pat, elem, mutable);
                }
            }
        }
    }

    /// `Enum::Variant`, and the patterns for its data.
    fn bind_variant(
        &mut self,
        pat: PatId,
        path: List<Symbol>,
        data: PatData,
        ty: Ty,
        mutable: bool,
    ) {
        let ast = self.ast;
        let span = ast.pat(pat).span;
        let segments = ast.list(path);
        let found = match *segments {
            [enum_name, name] => self.path_type(enum_name).map(|t| (t, name)),
            [name] => {
                let text = self.name(name).to_string();
                let bare = match *self.t.types.kind(ty) {
                    TyKind::Struct(s) => self.t.types.structs[s.index()].bare,
                    _ => false,
                };
                if bare && self.variant_index(ty, name).is_some() {
                    Some((ty, name))
                } else if BARE_VARIANTS.contains(&text.as_str()) {
                    if !self.t.types.is_wildcard(ty) {
                        let owner = self.bare_owner(name);
                        let message = format!(
                            "`{text}` is a variant of `{owner}`, but the value is {}",
                            self.show(ty)
                        );
                        self.error(span, message);
                    }
                    None
                } else {
                    let message = format!("cannot find the variant `{text}`");
                    self.error(span, message).help = Some(format!(
                        "write its enum's name before it, as in `Outcome::{text}`"
                    ));
                    None
                }
            }
            _ => None,
        };
        let Some((enum_ty, name)) = found else {
            self.bind_data_wildcard(data, mutable);
            return;
        };
        if !self.t.types.is_wildcard(ty) && enum_ty != ty {
            let message = format!(
                "this pattern is {}, but the value is {}",
                self.show(enum_ty),
                self.show(ty)
            );
            self.error(span, message);
            self.bind_data_wildcard(data, mutable);
            return;
        }
        let Some(index) = self.variant_index(enum_ty, name) else {
            let message = format!(
                "{} has no variant `{}`",
                self.show(enum_ty),
                self.name(name)
            );
            self.error(span, message);
            self.bind_data_wildcard(data, mutable);
            return;
        };
        self.t.pat_variants[pat.index()] = Some(index);
        let variant = self.t.types.variants(enum_ty).expect("an enum")[index as usize].clone();
        let path_text = self.path_text(path);
        match data {
            PatData::None => {
                if !variant.data.is_empty() {
                    let message = if variant.named {
                        format!("`{path_text}` holds data; take it with `{path_text} {{ .. }}`")
                    } else {
                        let blanks = vec!["_"; variant.data.len()].join(", ");
                        format!("`{path_text}` holds data; take it with `{path_text}({blanks})`")
                    };
                    self.error(span, message);
                }
            }
            PatData::Tuple(pats) => {
                let pats = ast.list(pats);
                if variant.named || pats.len() != variant.data.len() {
                    let message = if variant.named {
                        format!("`{path_text}` has named fields; write `{path_text} {{ … }}`")
                    } else if variant.data.is_empty() {
                        format!("`{path_text}` holds nothing to match")
                    } else {
                        format!(
                            "`{path_text}` holds {} value{}, but the pattern has {}",
                            variant.data.len(),
                            if variant.data.len() == 1 { "" } else { "s" },
                            pats.len()
                        )
                    };
                    self.error(span, message);
                    self.bind_data_wildcard(data, mutable);
                    return;
                }
                for (&p, &ty) in pats.iter().zip(&variant.data) {
                    self.bind(p, ty, mutable);
                }
            }
            PatData::Fields { fields, rest } => {
                if !variant.named {
                    let message = format!("`{path_text}` has no named fields");
                    self.error(span, message);
                    self.bind_data_wildcard(data, mutable);
                    return;
                }
                let mut seen = Vec::new();
                for field in ast.list(fields) {
                    let Some(i) = variant.names.iter().position(|&n| n == field.name) else {
                        let message =
                            format!("`{path_text}` has no field `{}`", self.name(field.name));
                        self.error(field.span, message);
                        if let Some(p) = field.pat {
                            self.bind(p, Ty::ERROR, mutable);
                        }
                        continue;
                    };
                    seen.push(field.name);
                    if let Some(p) = field.pat {
                        self.bind(p, variant.data[i], mutable);
                    }
                }
                let missing: Vec<&str> = variant
                    .names
                    .iter()
                    .filter(|n| !seen.contains(n))
                    .map(|&n| self.name(n))
                    .collect();
                if !rest && !missing.is_empty() {
                    let message = format!("missing fields: `{}`", missing.join("`, `"));
                    self.error(span, message).help =
                        Some("name them, or end the pattern with `..` to ignore the rest".into());
                }
            }
        }
    }

    /// Checks the sub-patterns of a variant's data against unknown types, so names in them
    /// are still declared.
    fn bind_data_wildcard(&mut self, data: PatData, mutable: bool) {
        match data {
            PatData::None => {}
            PatData::Tuple(pats) => {
                for &p in self.ast.list(pats) {
                    self.bind(p, Ty::ERROR, mutable);
                }
            }
            PatData::Fields { fields, .. } => {
                for field in self.ast.list(fields) {
                    if let Some(p) = field.pat {
                        self.bind(p, Ty::ERROR, mutable);
                    }
                }
            }
        }
    }

    /// Whether `pat` can fail to match a value of its type.
    fn refutable(&self, pat: PatId) -> bool {
        let ast = self.ast;
        match ast.pat(pat).kind {
            PatKind::Name(_) | PatKind::Wildcard => false,
            PatKind::Tuple(pats) | PatKind::Array(pats) => {
                ast.list(pats).iter().any(|&p| self.refutable(p))
            }
            PatKind::Literal(_) => true,
            PatKind::Variant { .. } => {
                let ty = self.t.pat_types[pat.index()];
                self.t.types.variants(ty).is_none_or(|v| v.len() > 1) || self.data_refutable(pat)
            }
        }
    }

    /// Whether a variant pattern's data patterns can fail to match.
    fn data_refutable(&self, pat: PatId) -> bool {
        let ast = self.ast;
        let PatKind::Variant { data, .. } = ast.pat(pat).kind else {
            return false;
        };
        match data {
            PatData::None => false,
            PatData::Tuple(pats) => ast.list(pats).iter().any(|&p| self.refutable(p)),
            PatData::Fields { fields, .. } => ast
                .list(fields)
                .iter()
                .filter_map(|f| f.pat)
                .any(|p| self.refutable(p)),
        }
    }
}

// Expressions.
impl Checker<'_> {
    /// Checks an expression, applying `expect`, and records its type.
    fn expr(&mut self, id: ExprId, expect: Expect) -> Ty {
        let ty = self.expr_kind(id, expect);
        self.t.expr_types[id.index()] = ty;
        match expect {
            Expect::Ty(want) => {
                self.coerce(id, ty, want);
                want
            }
            _ => ty,
        }
    }

    /// Makes a value of type `found` usable where `want` is expected, recording any implicit
    /// lend or dereference, or reports a mismatch.
    fn coerce(&mut self, id: ExprId, found: Ty, want: Ty) {
        let types = &self.t.types;
        if found == want || types.is_wildcard(found) || types.is_wildcard(want) {
            return;
        }
        let as_ref = |ty| match *types.kind(ty) {
            TyKind::Ref { mutable, inner } => Some((mutable, inner)),
            _ => None,
        };
        let adjust = match (as_ref(found), as_ref(want)) {
            (_, Some((mutable, inner))) if inner == found => Some(Adjust::Lend { mutable }),
            (Some((true, a)), Some((false, b))) if a == b => Some(Adjust::None),
            (Some((_, inner)), _) if inner == want => Some(Adjust::Deref),
            _ => None,
        };
        let adjust = match adjust {
            Some(adjust) => adjust,
            None => {
                let span = self.ast.expr(id).span;
                let message = format!(
                    "mismatched types: expected {}, found {}",
                    self.show(want),
                    self.show(found)
                );
                let hint = (found == Ty::INT
                    && want == Ty::FLOAT
                    && matches!(self.ast.expr(id).kind, ExprKind::Int(_)))
                .then(|| "write it as a float, such as `1.0`".to_string());
                let diagnostic = self.error(span, message);
                diagnostic.help = hint;
                return;
            }
        };
        self.t.adjust[id.index()] = adjust;
    }

    fn expr_kind(&mut self, id: ExprId, expect: Expect) -> Ty {
        let ast = self.ast;
        let expr = *ast.expr(id);
        match expr.kind {
            ExprKind::Error => Ty::ERROR,
            ExprKind::Int(value) => {
                let wants_bit = expect.ty().map(|t| self.t.types.deref(t)) == Some(Ty::BIT);
                if wants_bit && value <= 1 {
                    Ty::BIT
                } else {
                    Ty::INT
                }
            }
            ExprKind::Float(_) => Ty::FLOAT,
            ExprKind::Str(parts) => self.string(parts),
            ExprKind::Bool(_) => Ty::BOOL,
            ExprKind::Ket { qubits } => {
                let qubits = u64::from(qubits);
                if qubits == 1 {
                    Ty::QUBIT
                } else {
                    self.t.types.array(Ty::QUBIT, qubits)
                }
            }
            ExprKind::Name(name) => self.name_expr(id, name, expr.span),
            ExprKind::Path(segments) => self.path_value(id, segments, expect, expr.span),
            ExprKind::Unit => Ty::UNIT,
            ExprKind::Tuple(elems) => {
                let wanted: Option<Box<[Ty]>> =
                    expect.ty().and_then(|t| match self.t.types.kind(t) {
                        TyKind::Tuple(e) => Some(e.clone()),
                        _ => None,
                    });
                let elems: Box<[Ty]> = ast
                    .list(elems)
                    .iter()
                    .enumerate()
                    .map(|(i, &e)| {
                        let hint = wanted.as_ref().and_then(|w| w.get(i)).copied();
                        self.expr(e, hint.map_or(Expect::None, Expect::Hint))
                    })
                    .collect();
                self.t.types.intern(TyKind::Tuple(elems))
            }
            ExprKind::Array(elems) => self.array_expr(elems, expect, expr.span),
            ExprKind::ArrayRepeat { value, count } => {
                let elem_hint = expect.ty().and_then(|t| self.array_of(t)).map(|(e, _)| e);
                let elem = self.expr(value, elem_hint.map_or(Expect::None, Expect::Hint));
                self.expr(count, Expect::Ty(Ty::INT));
                let len = match ast.expr(count).kind {
                    ExprKind::Int(n) => n,
                    _ => {
                        let span = ast.expr(count).span;
                        self.error(span, "the length of an array must be an integer literal");
                        0
                    }
                };
                if self.t.types.linearity(elem) != crate::ty::Kind::Copyable
                    && !matches!(ast.expr(value).kind, ExprKind::Ket { .. })
                {
                    let span = ast.expr(value).span;
                    self.error(
                        span,
                        "only kets can be repeated: anything else would be copied",
                    )
                    .help =
                        Some("write the elements out, or repeat a ket such as `[|0>; 3]`".into());
                }
                self.t.types.array(elem, len)
            }
            ExprKind::StructLit { path, fields } => self.struct_lit(path, fields, expr.span),
            ExprKind::Field { base, name } => {
                let base_ty = self.expr(base, Expect::None);
                let ty = self.t.types.deref(base_ty);
                if self.t.types.is_wildcard(ty) {
                    return Ty::ERROR;
                }
                if let Some(id) = self.struct_of(ty)
                    && let Some(&(_, field_ty)) = self.t.types.structs[id.index()]
                        .fields
                        .iter()
                        .find(|(n, _)| *n == name)
                {
                    return field_ty;
                }
                let message = format!("{} has no field `{}`", self.show(ty), self.name(name));
                self.error(expr.span, message);
                Ty::ERROR
            }
            ExprKind::TupleIndex { base, index } => {
                let base_ty = self.expr(base, Expect::None);
                let ty = self.t.types.deref(base_ty);
                if let Some(elem) = self.tuple_elem(ty, index as usize) {
                    return elem;
                }
                if !self.t.types.is_wildcard(ty) {
                    let message = format!("{} has no element {index}", self.show(ty));
                    self.error(expr.span, message);
                }
                Ty::ERROR
            }
            ExprKind::Index { base, index } => {
                let base_ty = self.expr(base, Expect::None);
                self.expr(index, Expect::Ty(Ty::INT));
                let ty = self.t.types.deref(base_ty);
                if let Some((elem, _)) = self.array_of(ty) {
                    return elem;
                }
                if !self.t.types.is_wildcard(ty) {
                    let message = format!("{} cannot be indexed", self.show(ty));
                    self.error(expr.span, message);
                }
                Ty::ERROR
            }
            ExprKind::Call { callee, args } => self.call(id, callee, args, expect, expr.span),
            ExprKind::MethodCall {
                receiver,
                method,
                args,
            } => self.method_call(id, receiver, method, args, expr.span),
            ExprKind::Unary { op, operand } => {
                let hint = expect.ty().map_or(Expect::None, Expect::Hint);
                let ty = self.expr(operand, hint);
                let ty = self.t.types.deref(ty);
                let ok = match op {
                    UnaryOp::Neg => matches!(ty, Ty::INT | Ty::FLOAT | Ty::OBSERVABLE),
                    UnaryOp::BitNot => matches!(ty, Ty::INT | Ty::BIT),
                    UnaryOp::Not => ty == Ty::BOOL,
                };
                if ok || self.t.types.is_wildcard(ty) {
                    ty
                } else {
                    let word = match op {
                        UnaryOp::Neg => "-",
                        UnaryOp::BitNot => "~",
                        UnaryOp::Not => "not",
                    };
                    let message = format!("`{word}` cannot be applied to {}", self.show(ty));
                    self.error(expr.span, message);
                    Ty::ERROR
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Type a literal side from the other side, so `m == 1` with `m: Bit` works.
                let (lhs_ty, rhs_ty) = if matches!(ast.expr(lhs).kind, ExprKind::Int(_)) {
                    let rhs_ty = self.expr(rhs, Expect::None);
                    let hint = self.t.types.deref(rhs_ty);
                    (self.expr(lhs, Expect::Hint(hint)), rhs_ty)
                } else {
                    let lhs_ty = self.expr(lhs, Expect::None);
                    let hint = self.t.types.deref(lhs_ty);
                    (lhs_ty, self.expr(rhs, Expect::Hint(hint)))
                };
                let (lhs_ty, rhs_ty) = (self.t.types.deref(lhs_ty), self.t.types.deref(rhs_ty));
                self.binary_type(op, lhs_ty, rhs_ty, expr.span)
            }
            ExprKind::Own { kind, place } => {
                let ty = self.expr(place, Expect::None);
                match kind {
                    OwnKind::Mv => ty,
                    OwnKind::Ref | OwnKind::Mut => {
                        let mutable = kind == OwnKind::Mut;
                        let inner = match *self.t.types.kind(ty) {
                            // Lending a reference lends what it refers to.
                            TyKind::Ref { inner, .. } => inner,
                            _ => ty,
                        };
                        self.t.types.reference(mutable, inner)
                    }
                }
            }
            ExprKind::Adj(call) => self.expr(call, Expect::None),
            ExprKind::BraTest {
                qubits, operand, ..
            } => {
                let ty = self.expr(operand, Expect::None);
                let qubits = u64::from(qubits);
                let want = if qubits == 1 {
                    Ty::QUBIT
                } else {
                    self.t.types.array(Ty::QUBIT, qubits)
                };
                if ty != want && !self.t.types.is_wildcard(ty) {
                    let message = format!(
                        "this bra tests {qubits} qubit{}, so it needs {}, but the value is {}",
                        if qubits == 1 { "" } else { "s" },
                        self.show(want),
                        self.show(ty)
                    );
                    let span = self.ast.expr(operand).span;
                    self.error(span, message);
                }
                Ty::BOOL
            }
            ExprKind::Expectation {
                prep,
                observable,
                precision,
            } => {
                if let Some(precision) = precision {
                    self.expr(precision, Expect::Ty(Ty::FLOAT));
                }
                let ty = self.expr(prep, Expect::None);
                if !self.t.types.is_wildcard(ty) && !self.t.types.is_pure_quantum(ty) {
                    let message = format!(
                        "a preparation must produce qubits, but this produces {}",
                        self.show(ty)
                    );
                    let span = self.ast.expr(prep).span;
                    self.error(span, message);
                }
                self.expr(observable, Expect::Ty(Ty::OBSERVABLE));
                Ty::FLOAT
            }
            ExprKind::Block(block) => self.block(block, expect),
            ExprKind::Unsafe(block) => self.block(block, expect),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => self.if_expr(cond, then, otherwise, expect, expr.span),
            ExprKind::Sample { shots, body } => self.sample(id, shots, body),
            ExprKind::Match { scrutinee, arms } => {
                self.match_expr(scrutinee, arms, expect, expr.span)
            }
            ExprKind::While { cond, body } => {
                self.condition(cond);
                self.loops += 1;
                self.block(body, Expect::Discard);
                self.loops -= 1;
                Ty::UNIT
            }
            ExprKind::For {
                var,
                start,
                end,
                body,
            } => {
                self.expr(start, Expect::Ty(Ty::INT));
                self.expr(end, Expect::Ty(Ty::INT));
                let mark = self.scope.len();
                let local = self.declare(var, Ty::INT, false, expr.span);
                self.t.res[id.index()] = Res::Local(local);
                self.loops += 1;
                self.block(body, Expect::Discard);
                self.loops -= 1;
                self.scope.truncate(mark);
                Ty::UNIT
            }
            ExprKind::Ctrl { controls, body } => {
                for &control in ast.list(controls) {
                    let ty = self.expr(control, Expect::None);
                    let ty = self.t.types.deref(ty);
                    if !self.t.types.is_wildcard(ty) && !self.t.types.is_pure_quantum(ty) {
                        let message = format!(
                            "`ctrl` needs qubits as controls, but this is {}",
                            self.show(ty)
                        );
                        let span = ast.expr(control).span;
                        let diagnostic = self.error(span, message);
                        if matches!(ty, Ty::BOOL | Ty::BIT) {
                            diagnostic.help =
                                Some("use `if` to branch on a classical value".into());
                        }
                    }
                }
                self.block(body, Expect::Discard);
                Ty::UNIT
            }
            ExprKind::With { .. } => {
                self.error(expr.span, "`with`/`do` is not supported yet")
                    .help = Some("it comes after v0.1; see spec/ownership.md §8".into());
                Ty::UNIT
            }
        }
    }

    fn array_of(&self, ty: Ty) -> Option<(Ty, u64)> {
        match *self.t.types.kind(ty) {
            TyKind::Array(elem, len) => Some((elem, len)),
            _ => None,
        }
    }

    fn struct_of(&self, ty: Ty) -> Option<StructId> {
        match *self.t.types.kind(ty) {
            TyKind::Struct(id) => Some(id),
            _ => None,
        }
    }

    fn tuple_elem(&self, ty: Ty, index: usize) -> Option<Ty> {
        match self.t.types.kind(ty) {
            TyKind::Tuple(elems) => elems.get(index).copied(),
            _ => None,
        }
    }

    fn name_expr(&mut self, id: ExprId, name: Symbol, span: Span) -> Ty {
        if let Some(local) = self.lookup(name) {
            self.t.res[id.index()] = Res::Local(local);
            return self.t.local(local).ty;
        }
        let text = self.name(name).to_string();
        let message = if self.fn_names.contains_key(&name) {
            format!("`{text}` is a function; call it with `{text}(…)`")
        } else if self.struct_names.contains_key(&name) || prelude::builtin_type(&text).is_some() {
            format!("`{text}` is a type, not a value")
        } else {
            format!("cannot find `{text}` in this scope")
        };
        self.error(span, message);
        Ty::ERROR
    }

    /// The type a `Type::` path names, if it is a struct, `Self` or a built-in type.
    fn path_type(&mut self, name: Symbol) -> Option<Ty> {
        let text = self.name(name);
        if let Some(ty) = prelude::builtin_type(text) {
            return Some(ty);
        }
        if text == "Self" {
            return self.self_ty;
        }
        let id = *self.struct_names.get(&name)?;
        Some(self.t.types.struct_ty(id))
    }

    /// A path used as a value: a variant such as `Basis::X`, or a constant such as `Float::PI`.
    fn path_value(&mut self, id: ExprId, segments: List<Symbol>, expect: Expect, span: Span) -> Ty {
        if let [name] = *self.ast.list(segments) {
            return self.bare_value(id, name, expect, span);
        }
        if let [ty_name, name] = *self.ast.list(segments)
            && let Some(ty) = self.path_type(ty_name)
            && let Some(index) = self.variant_index(ty, name)
        {
            let TyKind::Struct(s) = *self.t.types.kind(ty) else {
                unreachable!("only enums have variants")
            };
            self.t.res[id.index()] = Res::Variant(s, index);
            let variant = &self.t.types.variants(ty).expect("an enum")[index as usize];
            if !variant.data.is_empty() {
                let path = self.path_text(segments);
                let message = if variant.named {
                    format!("`{path}` holds data; write `{path} {{ … }}`")
                } else {
                    format!("`{path}` holds data; write `{path}(…)`")
                };
                self.error(span, message);
            }
            return ty;
        }
        if let [ty_name, name] = *self.ast.list(segments)
            && let Some(ty) = self.path_type(ty_name)
            && let Some(constant) = prelude::constant(ty, self.name(name))
        {
            self.t.res[id.index()] = Res::Builtin(constant);
            return Ty::FLOAT;
        }
        let text = self.path_text(segments);
        let message = format!("cannot find the value `{text}`");
        self.error(span, message);
        Ty::ERROR
    }

    /// The index of the variant `name` of `ty`, if `ty` is an enum with one.
    fn variant_index(&self, ty: Ty, name: Symbol) -> Option<u32> {
        let variants = self.t.types.variants(ty)?;
        variants
            .iter()
            .position(|v| v.name == name)
            .map(|i| i as u32)
    }

    fn path_text(&self, segments: List<Symbol>) -> String {
        self.ast
            .list(segments)
            .iter()
            .map(|&s| self.name(s))
            .collect::<Vec<_>>()
            .join("::")
    }

    fn array_expr(&mut self, elems: List<ExprId>, expect: Expect, span: Span) -> Ty {
        let elems = self.ast.list(elems);
        let wanted = expect.ty().and_then(|t| self.array_of(t)).map(|(e, _)| e);
        let Some((&first, rest)) = elems.split_first() else {
            return match wanted {
                Some(elem) => self.t.types.array(elem, 0),
                None => {
                    self.error(span, "cannot tell the element type of an empty array")
                        .help =
                        Some("add a type annotation, such as `let a: [Int; 0] = []`".into());
                    Ty::ERROR
                }
            };
        };
        let elem = self.expr(first, wanted.map_or(Expect::None, Expect::Hint));
        for &e in rest {
            self.expr(e, Expect::Ty(elem));
        }
        self.t.types.array(elem, elems.len() as u64)
    }

    fn struct_lit(&mut self, path: ExprId, inits: List<FieldInit>, span: Span) -> Ty {
        let ast = self.ast;
        // `Enum::Variant { … }`: a variant with named fields.
        if let ExprKind::Path(segments) = ast.expr(path).kind
            && let [enum_name, name] = *ast.list(segments)
            && let Some(enum_ty) = self.path_type(enum_name)
            && let Some(index) = self.variant_index(enum_ty, name)
        {
            let TyKind::Struct(s) = *self.t.types.kind(enum_ty) else {
                unreachable!("only enums have variants")
            };
            self.t.res[path.index()] = Res::Variant(s, index);
            let variant = self.t.types.variants(enum_ty).expect("an enum")[index as usize].clone();
            if !variant.named {
                let message = if variant.data.is_empty() {
                    format!(
                        "`{}` holds nothing; write it without braces",
                        self.path_text(segments)
                    )
                } else {
                    format!(
                        "`{}` holds values by position; write `{}(…)`",
                        self.path_text(segments),
                        self.path_text(segments)
                    )
                };
                self.error(span, message);
                self.skip_inits(inits);
                return enum_ty;
            }
            let fields: Vec<(Symbol, Ty)> = variant
                .names
                .iter()
                .copied()
                .zip(variant.data.iter().copied())
                .collect();
            self.field_inits(inits, &fields, enum_ty, span);
            return enum_ty;
        }
        let struct_ty = match ast.expr(path).kind {
            ExprKind::Name(name) => self.path_type(name),
            _ => None,
        };
        let Some(struct_ty) = struct_ty else {
            let span = ast.expr(path).span;
            self.error(span, "cannot find this struct");
            self.skip_inits(inits);
            return Ty::ERROR;
        };
        let Some(id) = self.struct_of(struct_ty) else {
            let message = format!("{} is not a struct", self.show(struct_ty));
            self.error(span, message);
            return Ty::ERROR;
        };
        if self.t.types.structs[id.index()].variants.is_some() {
            let message = format!(
                "{} is an enum; write one of its variants, such as `{}::…`",
                self.show(struct_ty),
                self.t.types.display(struct_ty, ast)
            );
            self.error(span, message);
            self.skip_inits(inits);
            return Ty::ERROR;
        }
        let fields = self.t.types.structs[id.index()].fields.clone();
        self.field_inits(inits, &fields, struct_ty, span);
        struct_ty
    }

    fn skip_inits(&mut self, inits: List<FieldInit>) {
        for init in self.ast.list(inits) {
            if let Some(value) = init.value {
                self.expr(value, Expect::None);
            }
        }
    }

    /// Checks `field: value` (or `field` alone, a variable of that name) against `fields`,
    /// each given exactly once.
    fn field_inits(
        &mut self,
        inits: List<FieldInit>,
        fields: &[(Symbol, Ty)],
        owner: Ty,
        span: Span,
    ) {
        let ast = self.ast;
        let mut seen: Vec<Symbol> = Vec::new();
        for init in ast.list(inits) {
            let Some(&(_, field_ty)) = fields.iter().find(|(n, _)| *n == init.name) else {
                let message = format!(
                    "{} has no field `{}`",
                    self.show(owner),
                    self.name(init.name)
                );
                self.error(init.span, message);
                if let Some(value) = init.value {
                    self.expr(value, Expect::None);
                }
                continue;
            };
            if seen.contains(&init.name) {
                let message = format!("the field `{}` is given twice", self.name(init.name));
                self.error(init.span, message);
            }
            seen.push(init.name);
            match init.value {
                Some(value) => {
                    self.expr(value, Expect::Ty(field_ty));
                }
                None => match self.lookup(init.name) {
                    Some(local) => {
                        let ty = self.t.local(local).ty;
                        if ty != field_ty && !self.t.types.is_wildcard(ty) {
                            let message = format!(
                                "mismatched types: the field `{}` is {}, but the variable is {}",
                                self.name(init.name),
                                self.show(field_ty),
                                self.show(ty)
                            );
                            self.error(init.span, message);
                        }
                    }
                    None => {
                        let message =
                            format!("cannot find `{}` in this scope", self.name(init.name));
                        self.error(init.span, message);
                    }
                },
            }
        }
        let missing: Vec<&str> = fields
            .iter()
            .filter(|(n, _)| !seen.contains(n))
            .map(|(n, _)| self.name(*n))
            .collect();
        if !missing.is_empty() {
            let message = format!("missing fields: `{}`", missing.join("`, `"));
            self.error(span, message);
        }
    }

    fn condition(&mut self, cond: ExprId) {
        let ty = self.expr(cond, Expect::None);
        let ty = self.t.types.deref(ty);
        if matches!(ty, Ty::BOOL | Ty::BIT) || self.t.types.is_wildcard(ty) {
            return;
        }
        let span = self.ast.expr(cond).span;
        if self.t.types.is_pure_quantum(ty) {
            self.error(span, "a qubit cannot be a condition").help = Some(
                "measure it first (`if <1| mv q { … }`), or use `ctrl` to control on it".into(),
            );
        } else {
            let message = format!(
                "a condition must be `Bool` or `Bit`, but this is {}",
                self.show(ty)
            );
            self.error(span, message);
        }
    }

    fn if_expr(
        &mut self,
        cond: ExprId,
        then: BlockId,
        otherwise: Option<ExprId>,
        expect: Expect,
        span: Span,
    ) -> Ty {
        self.condition(cond);
        // Branches check against what the `if` is expected to be; without an expectation, the
        // `else` branch checks against the `then` branch.
        let then_ty = self.block(then, expect);
        let Some(otherwise) = otherwise else {
            let unit = expect == Expect::Discard
                || then_ty == Ty::UNIT
                || self.t.types.is_wildcard(then_ty);
            if !unit {
                self.error(
                    span,
                    "an `if` without `else` has no value when the condition is false",
                )
                .help = Some("add an `else` branch".into());
                return Ty::ERROR;
            }
            return Ty::UNIT;
        };
        match expect {
            Expect::Discard => {
                let else_ty = self.expr(otherwise, Expect::Discard);
                if then_ty == Ty::NEVER && else_ty == Ty::NEVER {
                    Ty::NEVER
                } else {
                    Ty::UNIT
                }
            }
            Expect::Ty(want) => {
                self.expr(otherwise, Expect::Ty(want));
                want
            }
            Expect::None | Expect::Hint(_) => {
                if then_ty == Ty::NEVER {
                    return self.expr(otherwise, expect);
                }
                let else_expect = if self.t.types.is_wildcard(then_ty) {
                    Expect::None
                } else {
                    Expect::Ty(then_ty)
                };
                let else_ty = self.expr(otherwise, else_expect);
                if self.t.types.is_wildcard(then_ty) {
                    else_ty
                } else {
                    then_ty
                }
            }
        }
    }

    /// `match x { p => a, … }`: each arm's pattern against `x`'s type, the arms' values
    /// combined as an `if`'s branches are, and every value covered.
    fn match_expr(&mut self, scrutinee: ExprId, arms: List<Arm>, expect: Expect, span: Span) -> Ty {
        let ast = self.ast;
        let scrutinee_ty = self.expr(scrutinee, Expect::None);
        let ty = self.t.types.deref(scrutinee_ty);
        // `match mv x` and temporaries give the arms the parts; a place or a reference lends
        // them (`spec/ownership.md` §13).
        let lent = match ast.expr(scrutinee).kind {
            ExprKind::Own {
                kind: OwnKind::Mv, ..
            } => None,
            _ => match *self.t.types.kind(scrutinee_ty) {
                TyKind::Ref { mutable, .. } => Some(mutable),
                _ if self.is_place(scrutinee) => Some(false),
                _ => None,
            },
        };
        let arms = ast.list(arms);
        if arms.is_empty() {
            self.error(span, "a `match` needs at least one arm");
            return Ty::ERROR;
        }
        let mut result: Option<Ty> = None;
        let mut all_never = true;
        for arm in arms {
            let mark = self.scope.len();
            self.lent_bindings = lent;
            self.bind(arm.pat, ty, false);
            self.lent_bindings = None;
            let arm_expect = match expect {
                Expect::Discard | Expect::Ty(_) => expect,
                Expect::None | Expect::Hint(_) => match result {
                    Some(t) if !self.t.types.is_wildcard(t) => Expect::Ty(t),
                    _ => expect,
                },
            };
            let arm_ty = self.expr(arm.body, arm_expect);
            self.scope.truncate(mark);
            if arm_ty != Ty::NEVER {
                all_never = false;
                if result.is_none_or(|t| self.t.types.is_wildcard(t)) {
                    result = Some(arm_ty);
                }
            }
        }
        self.check_exhaustive(ty, arms, span);
        match expect {
            _ if all_never => Ty::NEVER,
            Expect::Discard => Ty::UNIT,
            Expect::Ty(want) => want,
            Expect::None | Expect::Hint(_) => result.unwrap_or(Ty::UNIT),
        }
    }

    /// Whether `id` names a place: a variable, or a field or element of one.
    fn is_place(&self, id: ExprId) -> bool {
        match self.ast.expr(id).kind {
            ExprKind::Name(_) => matches!(self.t.res[id.index()], Res::Local(_)),
            ExprKind::Field { base, .. }
            | ExprKind::TupleIndex { base, .. }
            | ExprKind::Index { base, .. } => self.is_place(base),
            ExprKind::Own { place, .. } => self.is_place(place),
            _ => false,
        }
    }

    /// Reports the values of `ty` no arm covers.
    fn check_exhaustive(&mut self, ty: Ty, arms: &[Arm], span: Span) {
        if self.t.types.is_wildcard(ty) || arms.iter().any(|a| !self.refutable(a.pat)) {
            return;
        }
        let pats: Vec<PatId> = arms.iter().map(|a| a.pat).collect();
        let missing = self.uncovered(ty, &pats);
        if let [coverage::Witness::Any] = missing.as_slice() {
            let message = format!("this `match` does not cover every {}", self.show(ty));
            self.error(span, message).help = Some("end it with a catch-all arm: `_ => …`".into());
            return;
        }
        if missing.is_empty() {
            return;
        }
        let shown: Vec<String> = missing
            .iter()
            .map(|w| format!("`{}`", self.show_witness(w, ty)))
            .collect();
        let message = format!("this `match` does not cover {}", shown.join(", "));
        self.error(span, message).help =
            Some("add an arm for each, or a catch-all arm: `_ => …`".into());
    }

    fn binary_type(&mut self, op: BinaryOp, lhs: Ty, rhs: Ty, span: Span) -> Ty {
        use BinaryOp::*;
        if self.t.types.is_wildcard(lhs) || self.t.types.is_wildcard(rhs) {
            return match op {
                Or | And | Eq | NotEq | Lt | Le | Gt | Ge => Ty::BOOL,
                _ => Ty::ERROR,
            };
        }
        let result = match op {
            Or | And => (lhs == Ty::BOOL && rhs == Ty::BOOL).then_some(Ty::BOOL),
            Eq | NotEq => (lhs == rhs && self.t.types.is_comparable(lhs)).then_some(Ty::BOOL),
            Lt | Le | Gt | Ge => {
                (lhs == rhs && matches!(lhs, Ty::INT | Ty::FLOAT)).then_some(Ty::BOOL)
            }
            BitOr | BitXor | BitAnd => {
                (lhs == rhs && matches!(lhs, Ty::INT | Ty::BIT)).then_some(lhs)
            }
            Shl | Shr => (lhs == Ty::INT && rhs == Ty::INT).then_some(Ty::INT),
            Add | Sub => match (lhs, rhs) {
                (Ty::INT, Ty::INT) => Some(Ty::INT),
                (Ty::FLOAT, Ty::FLOAT) => Some(Ty::FLOAT),
                (Ty::OBSERVABLE, Ty::OBSERVABLE) => Some(Ty::OBSERVABLE),
                _ => None,
            },
            Mul => match (lhs, rhs) {
                (Ty::INT, Ty::INT) => Some(Ty::INT),
                (Ty::FLOAT, Ty::FLOAT) => Some(Ty::FLOAT),
                (Ty::OBSERVABLE | Ty::FLOAT, Ty::OBSERVABLE) | (Ty::OBSERVABLE, Ty::FLOAT) => {
                    Some(Ty::OBSERVABLE)
                }
                _ => None,
            },
            Div | Rem => match (lhs, rhs) {
                (Ty::INT, Ty::INT) => Some(Ty::INT),
                (Ty::FLOAT, Ty::FLOAT) => Some(Ty::FLOAT),
                _ => None,
            },
        };
        if let Some(ty) = result {
            return ty;
        }
        let message = format!(
            "`{}` cannot be applied to {} and {}",
            op.as_str(),
            self.show(lhs),
            self.show(rhs)
        );
        let diagnostic = self.error(span, message);
        if matches!((lhs, rhs), (Ty::INT, Ty::FLOAT) | (Ty::FLOAT, Ty::INT)) {
            diagnostic.help = Some(
                "Noev does not convert between `Int` and `Float` implicitly; use `Float(n)` or a float literal such as `2.0`".into(),
            );
        }
        match op {
            Or | And | Eq | NotEq | Lt | Le | Gt | Ge => Ty::BOOL,
            _ => Ty::ERROR,
        }
    }
}

// Calls.
impl Checker<'_> {
    fn call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        args: List<ExprId>,
        expect: Expect,
        span: Span,
    ) -> Ty {
        let ast = self.ast;
        let callee_expr = *ast.expr(callee);
        let args = ast.list(args);
        match callee_expr.kind {
            ExprKind::Name(name) => {
                if let Some(local) = self.lookup(name) {
                    let message = format!("`{}` is a variable, not a function", self.name(name));
                    self.t.res[callee.index()] = Res::Local(local);
                    self.error(callee_expr.span, message);
                    return self.skip_args(args);
                }
                if let Some(&f) = self.fn_names.get(&name) {
                    let sig = self.t.fns[f.index()].sig.clone();
                    self.t.callees[id.index()] = Some(Callee::Fn(f));
                    return self.check_args(&sig, None, args, span);
                }
                let text = self.name(name).to_string();
                if let Some(target) = prelude::builtin_type(&text) {
                    return self.conversion(id, target, args, span);
                }
                if text == "print" || text == "eprint" {
                    return self.print_call(id, text == "eprint", args);
                }
                if text == "measure" {
                    return self.measure_fn(id, args, span);
                }
                if let Some((builtin, sig)) = prelude::function(&mut self.t.types, &text) {
                    self.t.callees[id.index()] = Some(Callee::Builtin(builtin));
                    return self.check_args(&sig, None, args, span);
                }
                let message = if self.struct_names.contains_key(&name) {
                    format!("`{text}` is a struct; create one with `{text} {{ … }}`")
                } else {
                    format!("cannot find the function `{text}`")
                };
                self.error(callee_expr.span, message);
                self.skip_args(args)
            }
            ExprKind::Path(segments) => {
                if let [name] = *ast.list(segments) {
                    return self.bare_call(id, name, args, expect, span);
                }
                if let [ty_name, name] = *ast.list(segments)
                    && let Some(ty) = self.path_type(ty_name)
                    && let Some(index) = self.variant_index(ty, name)
                {
                    let path = self.path_text(segments);
                    return self.variant_call(id, ty, index, &path, args, span);
                }
                if let [ty_name, name] = *ast.list(segments)
                    && let Some(ty) = self.path_type(ty_name)
                {
                    if let Some((callee_kind, sig)) = self.lookup_method(ty, name) {
                        self.t.callees[id.index()] = Some(callee_kind);
                        // `Type::method(x, …)` passes the receiver as the first argument.
                        let receiver = sig.receiver.map(|kind| match kind {
                            OwnKind::Mv => ty,
                            OwnKind::Ref => self.t.types.reference(false, ty),
                            OwnKind::Mut => self.t.types.reference(true, ty),
                        });
                        return self.check_args(&sig, receiver, args, span);
                    }
                    let message =
                        format!("{} has no function `{}`", self.show(ty), self.name(name));
                    self.error(callee_expr.span, message);
                    return self.skip_args(args);
                }
                let message = format!("cannot find the function `{}`", self.path_text(segments));
                self.error(callee_expr.span, message);
                self.skip_args(args)
            }
            _ => {
                self.expr(callee, Expect::None);
                self.error(callee_expr.span, "only functions can be called");
                self.skip_args(args)
            }
        }
    }

    /// `Enum::Variant(values…)`: like a call to a function taking the variant's data.
    fn variant_call(
        &mut self,
        id: ExprId,
        enum_ty: Ty,
        index: u32,
        path: &str,
        args: &[ExprId],
        span: Span,
    ) -> Ty {
        let TyKind::Struct(s) = *self.t.types.kind(enum_ty) else {
            unreachable!("only enums have variants")
        };
        let variant = self.t.types.variants(enum_ty).expect("an enum")[index as usize].clone();
        if variant.named || variant.data.is_empty() {
            let message = if variant.named {
                format!("`{path}` has named fields; write `{path} {{ … }}`")
            } else {
                format!("`{path}` holds nothing; write it without `(…)`")
            };
            self.error(span, message);
            self.skip_args(args);
            return enum_ty;
        }
        self.t.callees[id.index()] = Some(Callee::Variant(s, index));
        let sig = Sig {
            receiver: None,
            params: variant.data.clone(),
            ret: enum_ty,
            is_unsafe: false,
            reversible: Reversible::Yes,
        };
        self.check_args(&sig, None, args, span);
        enum_ty
    }

    /// The standard library's `Option<T>` (one type argument) or `Result<T, E>` (two): an enum
    /// made the first time each is used (`spec/types.md` §2).
    fn std_enum(&mut self, args: Vec<Ty>) -> Ty {
        let ast = self.ast;
        let symbol = |text: &str| ast.symbols.get(text).expect("interned by the parser");
        let variant = |text: &str, data: Vec<Ty>| VariantDef {
            name: symbol(text),
            span: Span::default(),
            data,
            names: Vec::new(),
            named: false,
        };
        let (name, variants) = match *args.as_slice() {
            [value] => (
                "Option",
                vec![variant("Some", vec![value]), variant("None", Vec::new())],
            ),
            [value, error] => (
                "Result",
                vec![variant("Ok", vec![value]), variant("Err", vec![error])],
            ),
            _ => unreachable!("`Option` takes one type argument, `Result` two"),
        };
        let name = symbol(name);
        if let Some(&id) = self.std_enums.get(&(name, args.clone())) {
            return self.t.types.struct_ty(id);
        }
        let id = StructId(self.t.types.structs.len() as u32);
        self.t.types.structs.push(StructDef {
            name,
            span: Span::default(),
            fields: Vec::new(),
            variants: Some(variants),
            args: args.clone(),
            bare: true,
        });
        self.std_enums.insert((name, args), id);
        self.t.types.struct_ty(id)
    }

    /// The `Option` or `Result` that `expect` asks for, if it has the variant `name`, and the
    /// variant's index.
    fn expected_variant(&self, name: Symbol, expect: Expect) -> Option<(Ty, u32)> {
        let ty = self.t.types.deref(expect.ty()?);
        let TyKind::Struct(s) = *self.t.types.kind(ty) else {
            return None;
        };
        if !self.t.types.structs[s.index()].bare {
            return None;
        }
        Some((ty, self.variant_index(ty, name)?))
    }

    /// The enum `Some`, `None`, `Ok` or `Err` belongs to.
    fn bare_owner(&self, name: Symbol) -> &'static str {
        if matches!(self.name(name), "Some" | "None") {
            "Option"
        } else {
            "Result"
        }
    }

    /// Reports a bare variant whose enum cannot be told from what is expected.
    fn unknown_bare(&mut self, name: Symbol, expect: Expect, span: Span) -> Ty {
        let text = self.name(name).to_string();
        let owner = self.bare_owner(name);
        match expect.ty() {
            Some(want) if self.t.types.is_wildcard(want) => {}
            Some(want) => {
                let message = format!(
                    "mismatched types: expected {}, found `{text}`, a variant of `{owner}`",
                    self.show(want)
                );
                self.error(span, message);
            }
            None => {
                let example = match text.as_str() {
                    "None" => "let x: Option<Int> = None",
                    "Ok" => "let r: Result<Int, String> = Ok(1)",
                    _ => "let r: Result<Int, String> = Err(\"failed\")",
                };
                let message = format!("cannot tell which `{owner}` `{text}` is");
                self.error(span, message).help = Some(format!("write its type, as in `{example}`"));
            }
        }
        Ty::ERROR
    }

    /// `None`, of the `Option` expected.
    fn bare_value(&mut self, id: ExprId, name: Symbol, expect: Expect, span: Span) -> Ty {
        let text = self.name(name).to_string();
        if text != "None" {
            self.error(span, format!("`{text}` holds data; write `{text}(…)`"));
            return Ty::ERROR;
        }
        let Some((ty, index)) = self.expected_variant(name, expect) else {
            return self.unknown_bare(name, expect, span);
        };
        let TyKind::Struct(s) = *self.t.types.kind(ty) else {
            unreachable!("only enums have variants")
        };
        self.t.res[id.index()] = Res::Variant(s, index);
        ty
    }

    /// `Some(x)`, `Ok(x)` or `Err(e)`, of the enum expected; without one, `Some(x)` is the
    /// `Option` of `x`'s type.
    fn bare_call(
        &mut self,
        id: ExprId,
        name: Symbol,
        args: &[ExprId],
        expect: Expect,
        span: Span,
    ) -> Ty {
        let text = self.name(name).to_string();
        if let Some((ty, index)) = self.expected_variant(name, expect) {
            return self.variant_call(id, ty, index, &text, args, span);
        }
        if text != "Some" {
            self.skip_args(args);
            return self.unknown_bare(name, expect, span);
        }
        let [arg] = *args else {
            self.error(span, "`Some(…)` holds one value");
            return self.skip_args(args);
        };
        let found = self.expr(arg, Expect::None);
        if self.t.types.is_wildcard(found) {
            return Ty::ERROR;
        }
        // A reference to a copyable value holds a copy, as when it is passed by value.
        let value = match *self.t.types.kind(found) {
            TyKind::Ref { inner, .. } if self.t.types.linearity(inner) == Kind::Copyable => {
                self.coerce(arg, found, inner);
                inner
            }
            _ => found,
        };
        let ty = self.std_enum(vec![value]);
        let TyKind::Struct(s) = *self.t.types.kind(ty) else {
            unreachable!("`Option` is an enum")
        };
        self.t.callees[id.index()] = Some(Callee::Variant(s, 0));
        ty
    }

    /// A method or associated function of `ty`, built-in or declared in a struct.
    fn lookup_method(&mut self, ty: Ty, name: Symbol) -> Option<(Callee, Sig)> {
        if let Some(id) = self.struct_of(ty) {
            let &f = self.methods.get(&(id, name))?;
            return Some((Callee::Fn(f), self.t.fns[f.index()].sig.clone()));
        }
        let text = self.ast.name(name);
        prelude::method(&mut self.t.types, ty, text)
            .map(|(builtin, sig)| (Callee::Builtin(builtin), sig))
    }

    fn method_call(
        &mut self,
        id: ExprId,
        receiver: ExprId,
        method: Symbol,
        args: List<ExprId>,
        span: Span,
    ) -> Ty {
        let args = self.ast.list(args);
        let receiver_ty = self.expr(receiver, Expect::None);
        let base = self.t.types.deref(receiver_ty);
        if self.t.types.is_wildcard(base) {
            return self.skip_args(args);
        }
        let Some((callee, sig)) = self.lookup_method(base, method) else {
            let message = format!("{} has no method `{}`", self.show(base), self.name(method));
            self.error(span, message);
            return self.skip_args(args);
        };
        if sig.receiver.is_none() {
            let message = format!(
                "`{0}` is an associated function, not a method; call it as `{1}::{0}(…)`",
                self.name(method),
                self.t.types.display(base, self.ast)
            );
            self.error(span, message);
            return self.skip_args(args);
        }
        self.t.callees[id.index()] = Some(callee);
        self.t.receivers[id.index()] = sig.receiver;
        if base != receiver_ty {
            self.t.adjust[receiver.index()] = Adjust::Deref;
        }
        self.check_args(&sig, None, args, span)
    }

    /// Checks arguments against `sig`'s parameters (preceded by `receiver`, for
    /// `Type::method(x, …)`), returning the call's type.
    fn check_args(&mut self, sig: &Sig, receiver: Option<Ty>, args: &[ExprId], span: Span) -> Ty {
        let params: Vec<Ty> = receiver
            .into_iter()
            .chain(sig.params.iter().copied())
            .collect();
        if params.len() != args.len() {
            let message = format!(
                "this takes {} argument{}, but {} {} given",
                params.len(),
                if params.len() == 1 { "" } else { "s" },
                args.len(),
                if args.len() == 1 { "was" } else { "were" }
            );
            self.error(span, message);
        }
        for (i, &arg) in args.iter().enumerate() {
            match params.get(i) {
                Some(&param) => {
                    self.expr(arg, Expect::Ty(param));
                }
                None => {
                    self.expr(arg, Expect::None);
                }
            }
        }
        sig.ret
    }

    fn skip_args(&mut self, args: &[ExprId]) -> Ty {
        for &arg in args {
            self.expr(arg, Expect::None);
        }
        Ty::ERROR
    }

    /// `Float(x)` and the other conversions.
    fn conversion(&mut self, id: ExprId, target: Ty, args: &[ExprId], span: Span) -> Ty {
        self.t.callees[id.index()] = Some(Callee::Convert(target));
        let [arg] = *args else {
            let message = format!("{} takes one value to convert", self.show(target));
            self.error(span, message);
            self.skip_args(args);
            return target;
        };
        let ty = self.expr(arg, Expect::None);
        let from = self.t.types.deref(ty);
        if !self.t.types.is_wildcard(from) && !prelude::converts(from, target) {
            let message = format!(
                "{} cannot be converted to {}",
                self.show(from),
                self.show(target)
            );
            let diagnostic = self.error(span, message);
            if from == Ty::FLOAT && target == Ty::INT {
                diagnostic.help = Some(
                    "say how to round: `x.trunc()`, `x.round()`, `x.floor()` or `x.ceil()`".into(),
                );
            }
        }
        target
    }

    /// `sample n { … }`: a `Distribution` of the block's results, with returned qubits
    /// measured.
    fn sample(&mut self, id: ExprId, shots: ExprId, body: BlockId) -> Ty {
        self.expr(shots, Expect::Ty(Ty::INT));
        let first_local = self.t.locals.len() as u32;
        // `break` and `continue` cannot leave the run either.
        let loops = std::mem::replace(&mut self.loops, 0);
        self.runs += 1;
        let ty = self.block(body, Expect::None);
        self.runs -= 1;
        self.loops = loops;
        self.t.runs.insert(id, Run { first_local, ty });
        if self.t.types.is_wildcard(ty) {
            return Ty::ERROR;
        }
        match self.measured_type(ty) {
            Ok(elem) => self.t.types.intern(TyKind::Distribution(elem)),
            Err(problem) => {
                let span = self.ast.block(body).span;
                let message = format!("`sample` cannot count results of type {}", self.show(ty));
                self.error(span, message).help = Some(problem.into());
                Ty::ERROR
            }
        }
    }

    /// A run's result type once its returned qubits are measured: each `Qubit` becomes a `Bit`.
    fn measured_type(&mut self, ty: Ty) -> Result<Ty, &'static str> {
        match self.t.types.kind(ty).clone() {
            TyKind::Qubit => Ok(Ty::BIT),
            TyKind::Array(elem, len) => {
                let elem = self.measured_type(elem)?;
                Ok(self.t.types.array(elem, len))
            }
            TyKind::Tuple(elems) => {
                let elems = elems
                    .iter()
                    .map(|&t| self.measured_type(t))
                    .collect::<Result<Box<[Ty]>, _>>()?;
                Ok(self.t.types.intern(TyKind::Tuple(elems)))
            }
            TyKind::Struct(_) if !self.t.types.is_printable(ty) => Err(
                "a struct holding qubits cannot be counted; return its qubits in a tuple or array",
            ),
            TyKind::Ref { .. } => Err("references cannot be counted; return values"),
            _ if self.t.types.is_printable(ty) => Ok(ty),
            _ => Err("only classical values and qubits can be counted"),
        }
    }

    /// `print(…)` and `eprint(…)`: any number of printable values.
    fn print_call(&mut self, id: ExprId, stderr: bool, args: &[ExprId]) -> Ty {
        self.t.callees[id.index()] = Some(Callee::Builtin(Builtin::Print { stderr }));
        for &arg in args {
            let ty = self.expr(arg, Expect::None);
            self.require_printable(arg, ty);
        }
        Ty::UNIT
    }

    fn require_printable(&mut self, expr: ExprId, ty: Ty) {
        if self.t.types.is_wildcard(ty) || self.t.types.is_printable(ty) {
            return;
        }
        let span = self.ast.expr(expr).span;
        let message = format!("{} cannot be printed", self.show(ty));
        let quantum = self.t.types.linearity(self.t.types.deref(ty)) == crate::ty::Kind::Linear;
        let diagnostic = self.error(span, message);
        if quantum {
            diagnostic.help = Some("measure qubits first, and print the result".into());
        }
    }

    /// A string literal's interpolated values, checked against their format specs.
    fn string(&mut self, parts: List<StrPart>) -> Ty {
        for part in self.ast.list(parts) {
            let StrPart::Value { expr, spec } = *part else {
                continue;
            };
            let ty = self.expr(expr, Expect::None);
            self.require_printable(expr, ty);
            let ty = self.t.types.deref(ty);
            let Some(spec) = spec else { continue };
            if self.t.types.is_wildcard(ty) {
                continue;
            }
            let numeric = matches!(ty, Ty::INT | Ty::FLOAT);
            let problem = match spec.kind {
                Some(FormatKind::Exponent) if ty != Ty::FLOAT => Some("`e` applies to `Float`"),
                Some(
                    FormatKind::Binary | FormatKind::Octal | FormatKind::Hex | FormatKind::HexUpper,
                ) if ty != Ty::INT => Some("`b`, `o`, `x` and `X` apply to `Int`"),
                _ if spec.precision.is_some() && ty != Ty::FLOAT => {
                    Some("a precision applies to `Float`")
                }
                _ if spec.plus && !numeric => Some("`+` applies to `Int` and `Float`"),
                _ if spec.zero && !numeric => Some("`0` padding applies to `Int` and `Float`"),
                _ => None,
            };
            if let Some(problem) = problem {
                let message = format!("this format spec does not apply to {}", self.show(ty));
                self.error(spec.span, message).help = Some(problem.into());
            }
        }
        Ty::STRING
    }

    /// The free function `measure`, on a qubit or an array of qubits.
    fn measure_fn(&mut self, id: ExprId, args: &[ExprId], span: Span) -> Ty {
        self.t.callees[id.index()] = Some(Callee::Builtin(Builtin::Measure));
        let [arg] = *args else {
            self.error(span, "`measure` takes one qubit or array of qubits");
            self.skip_args(args);
            return Ty::ERROR;
        };
        let ty = self.expr(arg, Expect::None);
        match (ty, self.array_of(ty)) {
            (Ty::QUBIT, _) => Ty::BIT,
            (_, Some((Ty::QUBIT, len))) => self.t.types.array(Ty::BIT, len),
            _ if self.t.types.is_wildcard(ty) => Ty::ERROR,
            _ => {
                let message = format!(
                    "`measure` takes a qubit or an array of qubits, but this is {}",
                    self.show(ty)
                );
                let span = self.ast.expr(arg).span;
                self.error(span, message);
                Ty::ERROR
            }
        }
    }
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

mod coverage;

#[cfg(test)]
mod tests;
