//! Name resolution and type checking.
//!
//! Runs in two passes. The first collects every struct and function and resolves their
//! signatures, so items can be used before they are declared. The second checks each function
//! body, recording what every expression is and refers to in flat tables indexed by expression
//! id ([`Typed`]), which the ownership checker reads.

use noev_syntax::ast::*;
use noev_syntax::{Diagnostic, FxHashMap, Span, Symbol};

use crate::prelude::{self, Builtin, Math, Reversible, Sig};
use crate::resolve::{self, Binding, Entry, Found, Ns, PathError, PathWords, Scopes, Visible};
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
    /// A conversion that can fail, such as `u8::try_from(n)`: a `Result<T, String>`.
    TryConvert(Ty),
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

/// A trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TraitId(u32);

impl TraitId {
    /// The trait's position in [`Typed::traits`].
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A trait's name and functions (`spec/types.md` §6).
#[derive(Clone, Debug)]
pub struct TraitInfo {
    /// Its name.
    pub name: Symbol,
    /// Its item.
    pub item: ItemId,
    /// `Self` in its functions: a type parameter, the first of each function's.
    pub self_param: Ty,
    /// Its functions, with or without default bodies.
    pub fns: Vec<FnId>,
    /// Its associated types, each with whether it must be `Copy`.
    pub assoc: Vec<(Symbol, bool)>,
}

/// An `impl` of a trait for a type.
#[derive(Clone, Debug)]
pub struct ImplInfo {
    /// The trait.
    pub trait_id: TraitId,
    /// The type, which can mention the `impl`'s type parameters.
    pub self_ty: Ty,
    /// The `impl`'s type parameters: `impl<T> … for Pair<T, T>`.
    pub generics: Vec<Ty>,
    /// The functions it provides; the trait's defaults stand for the others.
    pub fns: Vec<FnId>,
    /// What it says each associated type is: `type Result = Bit`.
    pub assoc: Vec<(Symbol, Ty)>,
    /// Where it is.
    pub span: Span,
}

/// `ty` with each associated type whose base has an `impl` replaced by what the `impl` says
/// it is, as `<Qubit as Measure>::Result` is `Bit` (`spec/types.md` §6).
pub fn normalize(impls: &[ImplInfo], types: &mut Types, ty: Ty) -> Ty {
    match types.kind(ty).clone() {
        TyKind::Assoc {
            base,
            trait_index,
            name,
            copy,
        } => {
            let base = normalize(impls, types, base);
            // Qubits measure to bits by the built-in measurement.
            if Some(trait_index) == types.measure
                && let Some(result) = types.measured(base)
            {
                return result;
            }
            let found = impls.iter().find_map(|i| {
                if i.trait_id.0 != trait_index {
                    return None;
                }
                let mut found = vec![None; i.generics.len()];
                if !types.match_params(i.self_ty, base, &i.generics, &mut found) {
                    return None;
                }
                let args = found.into_iter().collect::<Option<Vec<Ty>>>()?;
                let &(_, given) = i.assoc.iter().find(|(n, _)| *n == name)?;
                Some((given, i.generics.clone(), args))
            });
            match found {
                Some((given, from, to)) => {
                    let given = types.subst(given, &from, &to);
                    normalize(impls, types, given)
                }
                None => types.intern(TyKind::Assoc {
                    base,
                    trait_index,
                    name,
                    copy,
                }),
            }
        }
        TyKind::Tuple(elems) => {
            let elems: Box<[Ty]> = elems.iter().map(|&t| normalize(impls, types, t)).collect();
            types.intern(TyKind::Tuple(elems))
        }
        TyKind::Array(elem, len) => {
            let elem = normalize(impls, types, elem);
            types.array(elem, len)
        }
        TyKind::Ref { mutable, inner } => {
            let inner = normalize(impls, types, inner);
            types.reference(mutable, inner)
        }
        _ => ty,
    }
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
    /// Its type parameters: for a method, its struct's first, then its own; for a trait's
    /// function, `Self` first; for an `impl`'s, the `impl`'s first. Empty if it is not
    /// generic.
    pub generics: Vec<Ty>,
    /// `Self` in a trait's or an `impl`'s function.
    pub self_ty: Option<Ty>,
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
    /// The program's `fn main`, in the root module of the package being compiled.
    pub main: Option<FnId>,
    /// The module each function is in, methods included, indexed by [`FnId::index`].
    pub fn_modules: Vec<ModuleId>,
    /// The type arguments of each call of a generic function or method, in the order of
    /// [`FnInfo::generics`], by the call's expression. They can mention the calling
    /// function's own type parameters.
    pub type_args: FxHashMap<ExprId, Vec<Ty>>,
    /// Every trait.
    pub traits: Vec<TraitInfo>,
    /// Every `impl`.
    pub impls: Vec<ImplInfo>,
    /// The trait each trait function belongs to.
    pub fn_traits: FxHashMap<FnId, TraitId>,
    /// The `impl` each `impl` function belongs to, by its index in [`Typed::impls`].
    pub fn_impls: FxHashMap<FnId, usize>,
    /// The name of each trait and `impl` function, to match one with the other.
    impl_names: FxHashMap<FnId, Symbol>,
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

    /// The `impl` of `trait_id` for `ty`, a type without parameters in `types` (this
    /// table's, or one made from it), and the `impl`'s type arguments, if there is one.
    pub fn impl_for(&self, types: &Types, trait_id: TraitId, ty: Ty) -> Option<(usize, Vec<Ty>)> {
        self.impls.iter().enumerate().find_map(|(index, i)| {
            if i.trait_id != trait_id {
                return None;
            }
            let mut found = vec![None; i.generics.len()];
            if !types.match_params(i.self_ty, ty, &i.generics, &mut found) {
                return None;
            }
            let args = found.into_iter().collect::<Option<Vec<Ty>>>()?;
            Some((index, args))
        })
    }

    /// What a call of the trait function `f` runs, given its type arguments (`Self`'s
    /// first, with no parameters left, in `types`): the `impl`'s function, or the trait's
    /// default, and that function's type arguments. `None` if `f` is not a trait's function,
    /// or there is no `impl` for `Self`.
    pub fn dispatch(&self, types: &Types, f: FnId, args: &[Ty]) -> Option<(FnId, Vec<Ty>)> {
        let &trait_id = self.fn_traits.get(&f)?;
        let (&self_ty, rest) = args.split_first()?;
        let (index, impl_args) = self.impl_for(types, trait_id, self_ty)?;
        let provided = self.impls[index]
            .fns
            .iter()
            .copied()
            .find(|&g| self.same_fn(g, f));
        match provided {
            Some(g) => {
                let mut all = impl_args;
                all.extend_from_slice(rest);
                Some((g, all))
            }
            None => Some((f, args.to_vec())),
        }
    }

    /// Whether the `impl` function `g` provides the trait function `f`.
    fn same_fn(&self, g: FnId, f: FnId) -> bool {
        self.impl_names.get(&g) == self.impl_names.get(&f)
    }
}

/// A function a name refers to.
#[derive(Clone, Copy, Debug)]
enum FnRef {
    /// One of the program's.
    Fn(FnId),
    /// A built-in standard-library function, by its name there.
    Std(&'static str),
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
    let (mut scopes, diagnostics) = resolve::resolve(ast);
    let mut checker = Checker {
        ast,
        words: PathWords::new(ast),
        module: ModuleId::from_index(0),
        t: Typed {
            fn_modules: scopes.take_fn_modules(),
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
        scopes,
        diagnostics,
        item_structs: vec![None; ast.all_items().len()],
        std_types: FxHashMap::default(),
        field_vis: FxHashMap::default(),
        methods: FxHashMap::default(),
        scope: Vec::new(),
        self_ty: None,
        ret: Ty::UNIT,
        loops: 0,
        runs: 0,
        lent_bindings: None,
        negating: false,
        std_enums: FxHashMap::default(),
        aliases: FxHashMap::default(),
        alias_types: FxHashMap::default(),
        resolving_aliases: Vec::new(),
        type_params: Vec::new(),
        struct_params: FxHashMap::default(),
        inference: Vec::new(),
        item_traits: FxHashMap::default(),
        param_bounds: FxHashMap::default(),
        param_assoc: FxHashMap::default(),
        impl_assoc: Vec::new(),
    };
    checker.collect();
    checker.check_bodies();
    (checker.t, checker.diagnostics)
}

struct Checker<'a> {
    ast: &'a Ast,
    t: Typed,
    /// What each name means in each module.
    scopes: Scopes<'a>,
    /// `pkg`, `super` and `self`, which can start a path.
    words: PathWords,
    /// The module of the code being checked.
    module: ModuleId,
    diagnostics: Vec<Diagnostic>,
    /// The struct or enum each struct or enum item declares, by [`ItemId::index`].
    item_structs: Vec<Option<StructId>>,
    /// The standard library's types in the prelude: `Basis` and `State`.
    std_types: FxHashMap<Symbol, StructId>,
    /// Where each field of each struct can be used from, in declaration order.
    field_vis: FxHashMap<StructId, Vec<Visible>>,
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
    /// While checking the operand of `-`: a literal there may reach a signed type's minimum.
    negating: bool,
    /// The generic definitions of `Option` and `Result`, by name, once made.
    std_enums: FxHashMap<Symbol, StructId>,
    /// Type aliases: the type each names, and where its name is.
    aliases: FxHashMap<ItemId, (TypeId, Span)>,
    /// The aliases resolved so far.
    alias_types: FxHashMap<ItemId, Ty>,
    /// The aliases being resolved, innermost last, to catch one that names itself.
    resolving_aliases: Vec<ItemId>,
    /// The type parameters in scope, by name.
    type_params: Vec<(Symbol, Ty)>,
    /// The type parameters of each generic struct or enum, by name.
    struct_params: FxHashMap<StructId, Vec<(Symbol, Ty)>>,
    /// The generic calls whose type arguments are being inferred, innermost last.
    inference: Vec<Inference>,
    /// The trait each trait item declares.
    item_traits: FxHashMap<ItemId, TraitId>,
    /// The traits each type parameter is bound by.
    param_bounds: FxHashMap<Ty, Vec<TraitId>>,
    /// The associated types bounds fix, as `T: Measure<Result = Bit>`, by parameter, trait
    /// and name.
    param_assoc: FxHashMap<(Ty, TraitId, Symbol), Ty>,
    /// While `impl`s are collected, what each says its associated types are, so their
    /// signatures can name them through `Self`.
    impl_assoc: Vec<ImplAssoc>,
}

/// An `impl`'s type, trait and associated types, while `impl`s are collected.
type ImplAssoc = (Ty, TraitId, Vec<(Symbol, Ty)>);

/// The type parameters of a generic call or literal, and what each is found to be so far.
struct Inference {
    params: Vec<Ty>,
    found: Vec<Option<Ty>>,
}

// Collecting items.
impl<'a> Checker<'a> {
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
            template: None,
        });
        self.std_types.insert(std_name("Basis"), basis);
        let basis_ty = self.t.types.struct_ty(basis);
        self.t.types.basis = Some(basis_ty);
        // `enum State { Zero, One, Plus, Minus, PlusI, MinusI }`, in `state::Label`'s order.
        let state = StructId(self.t.types.structs.len() as u32);
        let labels = ["Zero", "One", "Plus", "Minus", "PlusI", "MinusI"];
        self.t.types.structs.push(StructDef {
            name: std_name("State"),
            span: Span::default(),
            fields: Vec::new(),
            variants: Some(labels.iter().map(|l| variant(l)).collect()),
            args: Vec::new(),
            bare: false,
            template: None,
        });
        self.std_types.insert(std_name("State"), state);
        let state_ty = self.t.types.struct_ty(state);
        self.t.types.state = Some(state_ty);
        let std_types = self.t.types.structs.len() as u32;

        // Traits first, so any bound can name one.
        for &item in items {
            let ItemKind::Trait {
                name,
                generics,
                fns,
                types,
                ..
            } = ast.item(item).kind
            else {
                continue;
            };
            if !generics.params.is_empty() {
                self.error(ast.item(item).span, "generic traits are not supported yet")
                    .help = Some("see spec/types.md §6 for what is planned".into());
            }
            let id = TraitId(self.t.traits.len() as u32);
            // `Self` in the trait's functions: a type parameter bound by the trait.
            let self_name = ast.symbols.get("Self").unwrap_or(name);
            let self_param = self.t.types.param(self_name, false);
            self.param_bounds.insert(self_param, vec![id]);
            self.item_traits.insert(item, id);
            self.module = self.scopes.item_module(item);
            let assoc = ast
                .list(types)
                .iter()
                .map(|a| {
                    let copy = ast
                        .list(a.bounds)
                        .iter()
                        .any(|b| matches!(ast.list(b.path), [n] if ast.name(*n) == "Copy"));
                    if ast.list(a.bounds).len() > usize::from(copy) {
                        self.error(
                            ast.item(item).span,
                            "bounds on associated types other than `Copy` are not supported yet",
                        );
                    }
                    (a.name, copy)
                })
                .collect();
            self.t.traits.push(TraitInfo {
                name,
                item,
                self_param,
                fns: ast.list(fns).to_vec(),
                assoc,
            });
            if Some(ast.module(self.module).package) == ast.std_package()
                && ast.name(name) == "Measure"
            {
                self.t.types.measure = Some(id.0);
            }
            for &f in ast.list(fns) {
                self.t.fn_traits.insert(f, id);
                self.t.impl_names.insert(f, ast.function(f).name);
            }
        }

        // Struct, enum and alias names next, so field and parameter types can refer to any
        // of them.
        for &item in items {
            if let ItemKind::Type {
                name,
                name_span,
                ty,
            } = ast.item(item).kind
            {
                let text = self.name(name).to_string();
                if prelude::builtin_type(&text).is_some() {
                    let message =
                        format!("`{text}` is a built-in type; an alias needs a name of its own");
                    self.error(name_span, message);
                } else {
                    self.aliases.insert(item, (ty, name_span));
                }
                continue;
            }
            let (name, name_span, is_enum, generics) = match ast.item(item).kind {
                ItemKind::Struct {
                    name,
                    name_span,
                    generics,
                    ..
                } => (name, name_span, false, generics),
                ItemKind::Enum {
                    name,
                    name_span,
                    generics,
                    ..
                } => (name, name_span, true, generics),
                _ => continue,
            };
            let id = StructId(self.t.types.structs.len() as u32);
            self.item_structs[item.index()] = Some(id);
            // A generic struct or enum is a template: its arguments are its parameters.
            self.module = self.scopes.item_module(item);
            let params = self.generic_params(generics);
            self.t.types.structs.push(StructDef {
                name,
                span: name_span,
                fields: Vec::new(),
                variants: is_enum.then(Vec::new),
                args: params.iter().map(|&(_, t)| t).collect(),
                bare: false,
                template: None,
            });
            if !params.is_empty() {
                self.struct_params.insert(id, params);
            }
        }

        // Every alias, so a broken one is reported even if nothing uses it.
        let mut aliases: Vec<ItemId> = self.aliases.keys().copied().collect();
        aliases.sort_by_key(|a| self.aliases[a].1.start);
        for alias in aliases {
            self.alias_type(alias);
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
                generics: Vec::new(),
                self_ty: None,
            })
            .collect();

        let mut next_struct = std_types;
        let main_root = ast.packages().first().map(|p| p.root);
        for &item_id in items {
            let item = ast.item(item_id);
            self.module = self.scopes.item_module(item_id);
            match item.kind {
                ItemKind::Fn(f) => {
                    let decl = ast.function(f);
                    self.collect_fn(f, None, Vec::new(), None);
                    if self.name(decl.name) == "main" && Some(self.module) == main_root {
                        self.t.main = Some(f);
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
                    self.type_params = self.struct_params.get(&id).cloned().unwrap_or_default();
                    let mut resolved: Vec<(Symbol, Ty)> = Vec::new();
                    let mut visible = Vec::new();
                    for field in ast.list(fields) {
                        if resolved.iter().any(|(name, _)| *name == field.name) {
                            let message =
                                format!("the field `{}` is declared twice", self.name(field.name));
                            self.error(field.span, message);
                        }
                        let ty = self.resolve_type(field.ty);
                        resolved.push((field.name, ty));
                        visible.push(self.visibility(field.vis));
                    }
                    self.t.types.structs[id.index()].fields = resolved;
                    self.field_vis.insert(id, visible);
                    for &method in ast.list(methods) {
                        self.visibility(ast.function(method).vis);
                        let name = ast.function(method).name;
                        if self.methods.insert((id, name), method).is_some() {
                            let message = format!("`{}` is defined twice", self.name(name));
                            let span = ast.function(method).name_span;
                            self.error(span, message);
                        }
                        self.collect_fn(method, Some(id), Vec::new(), None);
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
                    self.type_params = self.struct_params.get(&id).cloned().unwrap_or_default();
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
                        self.visibility(ast.function(method).vis);
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
                        self.collect_fn(method, Some(id), Vec::new(), None);
                    }
                    self.self_ty = None;
                }
                ItemKind::Trait { fns, .. } => {
                    let id = self.item_traits[&item_id];
                    let self_param = self.t.traits[id.index()].self_param;
                    let self_name = self.t.types.params[self
                        .t
                        .types
                        .as_param(self_param)
                        .expect("a parameter")
                        .index()]
                    .name;
                    for &f in ast.list(fns) {
                        self.collect_fn(f, None, vec![(self_name, self_param)], Some(self_param));
                    }
                }
                ItemKind::Impl {
                    generics,
                    bound,
                    ty,
                    fns,
                    types,
                } => {
                    let params = self.generic_params(generics);
                    self.type_params = params.clone();
                    let trait_id = self.bound_trait(&bound);
                    let self_ty = self.resolve_type(ty);
                    self.type_params.clear();
                    self.type_params = params.clone();
                    let assoc: Vec<(Symbol, Ty)> = ast
                        .list(types)
                        .iter()
                        .map(|a| (a.name, self.resolve_type(a.ty)))
                        .collect();
                    self.type_params.clear();
                    if let Some(trait_id) = trait_id {
                        // Signatures can name the associated types, through `Self`.
                        self.impl_assoc.push((self_ty, trait_id, assoc.clone()));
                    }
                    for &f in ast.list(fns) {
                        self.collect_fn(f, None, params.clone(), Some(self_ty));
                        self.t.impl_names.insert(f, ast.function(f).name);
                    }
                    if let Some(trait_id) = trait_id {
                        let index = self.t.impls.len();
                        for &f in ast.list(fns) {
                            self.t.fn_impls.insert(f, index);
                        }
                        self.t.impls.push(ImplInfo {
                            trait_id,
                            self_ty,
                            generics: params.iter().map(|&(_, t)| t).collect(),
                            fns: ast.list(fns).to_vec(),
                            assoc,
                            span: item.span,
                        });
                    }
                }
                ItemKind::Use(_) | ItemKind::Type { .. } | ItemKind::Error => {}
            }
            self.type_params.clear();
        }
        // Instances made while fields were being resolved may have missed some.
        self.t.types.refill_instances();
        self.impl_assoc.clear();
        for index in 0..self.t.impls.len() {
            self.check_impl(index);
        }
    }

    /// The trait `bound` names, reporting one that is not a trait or is not supported yet.
    fn bound_trait(&mut self, bound: &Bound) -> Option<TraitId> {
        let span = bound.span;
        let (at, rest) = self.qualify(bound.path, span)?;
        let [name] = *rest else {
            let message = format!("cannot find the trait `{}`", self.path_text(bound.path));
            self.error(span, message);
            return None;
        };
        let found = match at {
            None => self
                .scopes
                .lexical(self.module, name, Ns::Type)
                .or_else(|| self.prelude_trait(name)),
            Some(module) => self.scopes.qualified(self.module, module, name, Ns::Type),
        };
        let trait_id = match self.found(found, name, span) {
            Some(Binding::Item(item)) => self.item_traits.get(&item).copied(),
            _ => None,
        };
        let Some(trait_id) = trait_id else {
            let message = format!("cannot find the trait `{}`", self.path_text(bound.path));
            self.error(span, message);
            return None;
        };
        if self
            .ast
            .list(bound.args)
            .iter()
            .any(|a| matches!(a, BoundArg::Type(_)))
        {
            self.error(span, "generic traits are not supported yet")
                .help = Some("see spec/types.md §6 for what is planned".into());
        }
        Some(trait_id)
    }

    /// The associated types `bound` fixes, as `Measure<Result = Bit>`, reporting names the
    /// trait does not have.
    fn bound_assoc(&mut self, bound: &Bound, trait_id: TraitId) -> Vec<(Symbol, Ty)> {
        let mut fixed = Vec::new();
        for arg in self.ast.list(bound.args) {
            let BoundArg::Assoc { name, ty } = *arg else {
                continue;
            };
            let known = self.t.traits[trait_id.index()]
                .assoc
                .iter()
                .any(|(n, _)| *n == name);
            if !known {
                let trait_name = self.name(self.t.traits[trait_id.index()].name).to_string();
                let message = format!(
                    "`{}` is not an associated type of `{trait_name}`",
                    self.name(name)
                );
                self.error(bound.span, message);
                continue;
            }
            let ty = self.resolve_type(ty);
            fixed.push((name, ty));
        }
        fixed
    }

    /// Checks an `impl` against its trait: every function the trait has no default for is
    /// there, with the trait's signature for `Self`; it is in the trait's package or the
    /// type's; and no other `impl` gives the same trait for the same type.
    fn check_impl(&mut self, index: usize) {
        let ast = self.ast;
        let info = self.t.impls[index].clone();
        let trait_info = self.t.traits[info.trait_id.index()].clone();
        let trait_name = self.name(trait_info.name).to_string();
        let shown = self.show(info.self_ty);
        for &g in &info.fns {
            let name = ast.function(g).name;
            let Some(&f) = trait_info
                .fns
                .iter()
                .find(|&&f| ast.function(f).name == name)
            else {
                let message = format!("`{}` is not a function of `{trait_name}`", self.name(name));
                self.error(ast.function(g).name_span, message);
                continue;
            };
            self.check_impl_fn(f, g, &info, &trait_name);
        }
        for &(name, copy) in &trait_info.assoc {
            match info.assoc.iter().find(|(n, _)| *n == name) {
                None => {
                    let message = format!(
                        "this `impl` of `{trait_name}` for {shown} lacks `type {}`",
                        self.name(name)
                    );
                    self.error(info.span, message);
                }
                Some(&(_, ty)) if copy && self.t.types.linearity(ty) != Kind::Copyable => {
                    let message = format!(
                        "`{trait_name}` needs `{}` to be `Copy`, but {} is not",
                        self.name(name),
                        self.show(ty)
                    );
                    self.error(info.span, message);
                }
                Some(_) => {}
            }
        }
        for &(name, _) in &info.assoc {
            if !trait_info.assoc.iter().any(|(n, _)| *n == name) {
                let message = format!(
                    "`{}` is not an associated type of `{trait_name}`",
                    self.name(name)
                );
                self.error(info.span, message);
            }
        }
        for &f in &trait_info.fns {
            let decl = ast.function(f);
            let provided = info.fns.iter().any(|&g| ast.function(g).name == decl.name);
            if !provided && decl.body.is_none() {
                let message = format!(
                    "this `impl` of `{trait_name}` for {shown} lacks `{}`",
                    self.name(decl.name)
                );
                self.error(info.span, message);
            }
        }
        // Coherence: the `impl` is in the package of the trait or of the type.
        let impl_module = self.scopes.item_module(
            ast.list(ast.root)
                .iter()
                .copied()
                .find(|&i| ast.item(i).span == info.span)
                .expect("the impl's item"),
        );
        let package = ast.module(impl_module).package;
        let trait_package = ast.module(self.scopes.item_module(trait_info.item)).package;
        let type_package = self.struct_of(info.self_ty).and_then(|s| {
            let template = self.t.types.structs[s.index()].template.unwrap_or(s);
            let item = self
                .item_structs
                .iter()
                .position(|&id| id == Some(template))?;
            Some(
                ast.module(self.scopes.item_module(ItemId::from_index(item)))
                    .package,
            )
        });
        if package != trait_package && type_package != Some(package) {
            let message = format!(
                "an `impl` of `{trait_name}` for {shown} must be in the package of `{trait_name}` or of the type"
            );
            self.error(info.span, message).help =
                Some("so two packages can never give the same one".into());
        }
        // No two `impl`s of the trait for the same type.
        for earlier in 0..index {
            let other = &self.t.impls[earlier];
            if other.trait_id != info.trait_id {
                continue;
            }
            let mut params = info.generics.clone();
            params.extend(&other.generics);
            let mut found = vec![None; params.len()];
            let overlap =
                self.t
                    .types
                    .match_params(other.self_ty, info.self_ty, &params, &mut found)
                    || {
                        let mut found = vec![None; params.len()];
                        self.t
                            .types
                            .match_params(info.self_ty, other.self_ty, &params, &mut found)
                    };
            if overlap {
                let message = format!("`{trait_name}` is implemented for {shown} twice");
                let span = other.span;
                self.error(info.span, message)
                    .labels
                    .push((span, "first here".into()));
            }
        }
    }

    /// Checks that the `impl` function `g` has the signature of the trait function `f`
    /// for the `impl`'s type, and makes it reversible if `f` is declared so.
    fn check_impl_fn(&mut self, f: FnId, g: FnId, info: &ImplInfo, trait_name: &str) {
        let trait_sig = self.t.fns[f.index()].sig.clone();
        let impl_sig = self.t.fns[g.index()].sig.clone();
        let trait_generics = self.t.fns[f.index()].generics.clone();
        let impl_generics = self.t.fns[g.index()].generics.clone();
        // `Self`, then the trait function's own parameters, for the `impl`'s.
        let own = &impl_generics[info.generics.len()..];
        let span = self.ast.function(g).name_span;
        let name = self.name(self.ast.function(f).name).to_string();
        if own.len() + 1 != trait_generics.len() {
            let message = format!(
                "`{name}` takes {} type parameter{} in `{trait_name}`",
                trait_generics.len() - 1,
                if trait_generics.len() == 2 { "" } else { "s" }
            );
            self.error(span, message);
            return;
        }
        let mut to = vec![info.self_ty];
        to.extend_from_slice(own);
        let impls = std::slice::from_ref(info);
        let types = &mut self.t.types;
        let want_params: Vec<Ty> = trait_sig
            .params
            .iter()
            .map(|&t| {
                let t = types.subst(t, &trait_generics, &to);
                normalize(impls, types, t)
            })
            .collect();
        let want_ret = types.subst(trait_sig.ret, &trait_generics, &to);
        let want_ret = normalize(impls, types, want_ret);
        if trait_sig.receiver != impl_sig.receiver
            || want_params != impl_sig.params
            || want_ret != impl_sig.ret
        {
            let message = format!("`{name}` has a different signature than in `{trait_name}`");
            self.error(span, message);
        }
        if trait_sig.reversible == Reversible::Yes {
            self.t.fns[g.index()].sig.reversible = Reversible::Yes;
        }
    }

    /// Makes the type parameters of `generics`, written in the current module, reporting
    /// what is not supported yet.
    fn generic_params(&mut self, generics: Generics) -> Vec<(Symbol, Ty)> {
        let ast = self.ast;
        let mut params = Vec::new();
        for param in ast.list(generics.params) {
            match param.kind {
                GenericKind::Size { .. } => {
                    self.error(param.span, "size parameters are not supported yet")
                        .help = Some("see spec/types.md §6 for what is planned".into());
                }
                GenericKind::Type { bounds } => {
                    if params.iter().any(|&(n, _)| n == param.name) {
                        let message = format!(
                            "the type parameter `{}` is declared twice",
                            self.name(param.name)
                        );
                        self.error(param.span, message);
                        continue;
                    }
                    let (mut copy, mut traits) = self.bounds_of(bounds);
                    let mut lists = vec![bounds];
                    for pred in ast.list(generics.wheres) {
                        if pred.name == param.name {
                            let (c, t) = self.bounds_of(pred.bounds);
                            copy |= c;
                            traits.extend(t);
                            lists.push(pred.bounds);
                        }
                    }
                    let ty = self.t.types.param(param.name, copy);
                    if !traits.is_empty() {
                        self.param_bounds.insert(ty, traits);
                    }
                    // `T: Measure<Result = Bit>`: the associated types the bounds fix.
                    for list in lists {
                        for bound in ast.list(list) {
                            let Some(trait_id) = self.trait_named(bound) else {
                                continue;
                            };
                            for (name, fixed) in self.bound_assoc(bound, trait_id) {
                                self.param_assoc.insert((ty, trait_id, name), fixed);
                            }
                        }
                    }
                    params.push((param.name, ty));
                }
            }
        }
        for pred in ast.list(generics.wheres) {
            if !ast
                .list(generics.params)
                .iter()
                .any(|p| p.name == pred.name)
            {
                let message = format!("`{}` is not a type parameter here", self.name(pred.name));
                self.error(pred.span, message);
            }
        }
        params
    }

    /// Whether `bounds` include `Copy`, and the traits they name.
    fn bounds_of(&mut self, bounds: List<Bound>) -> (bool, Vec<TraitId>) {
        let mut copy = false;
        let mut traits = Vec::new();
        for bound in self.ast.list(bounds) {
            if let [name] = self.ast.list(bound.path)
                && self.name(*name) == "Copy"
                && bound.args.is_empty()
            {
                copy = true;
            } else if let Some(t) = self.bound_trait(bound) {
                traits.push(t);
            }
        }
        (copy, traits)
    }

    /// The trait `bound` names, quietly: `None` for `Copy`, or a name already reported.
    fn trait_named(&mut self, bound: &Bound) -> Option<TraitId> {
        let ast = self.ast;
        let [name] = *ast.list(bound.path) else {
            return None;
        };
        if ast.name(name) == "Copy" {
            return None;
        }
        let found = self
            .scopes
            .lexical(self.module, name, Ns::Type)
            .or_else(|| self.prelude_trait(name))?;
        match found {
            Found::Entry(Entry {
                binding: Binding::Item(item),
                ..
            }) => self.item_traits.get(&item).copied(),
            _ => None,
        }
    }

    /// A trait of the prelude named `name`: one in the standard library's root module, such
    /// as `Measure`.
    fn prelude_trait(&self, name: Symbol) -> Option<Found> {
        let std = self.ast.std_package()?;
        let root = self.ast.package(std).root;
        self.scopes.qualified(self.module, root, name, Ns::Type)
    }

    /// Resolves associated types in `ty` that are known: through an `impl`, a bound that
    /// fixes one (`T: Measure<Result = Bit>`), or an `impl` being collected.
    fn resolve_assoc(&mut self, ty: Ty) -> Ty {
        let ty = normalize(&self.t.impls, &mut self.t.types, ty);
        let TyKind::Assoc {
            base,
            trait_index,
            name,
            ..
        } = *self.t.types.kind(ty)
        else {
            return ty;
        };
        if let Some(&fixed) = self.param_assoc.get(&(base, TraitId(trait_index), name)) {
            return fixed;
        }
        let found = self.impl_assoc.iter().find_map(|(self_ty, t, assoc)| {
            (*self_ty == base && t.0 == trait_index)
                .then(|| assoc.iter().find(|(n, _)| *n == name).map(|&(_, t)| t))
                .flatten()
        });
        found.unwrap_or(ty)
    }

    /// The associated type `name` of `base`, through the traits it implements.
    fn assoc_type(&mut self, base: Ty, name: Symbol, span: Span) -> Ty {
        let found = (0..self.t.traits.len())
            .map(|t| TraitId(t as u32))
            .filter(|&t| self.implements(base, t))
            .find_map(|t| {
                let &(_, copy) = self.t.traits[t.index()]
                    .assoc
                    .iter()
                    .find(|(n, _)| *n == name)?;
                Some((t, copy))
            });
        let Some((trait_id, copy)) = found else {
            let message = format!(
                "{} has no associated type `{}`",
                self.show(base),
                self.name(name)
            );
            self.error(span, message);
            return Ty::ERROR;
        };
        let ty = self.t.types.intern(TyKind::Assoc {
            base,
            trait_index: trait_id.0,
            name,
            copy,
        });
        self.resolve_assoc(ty)
    }

    /// Whether `ty` implements `trait_id`: a type parameter bound by it, or a type with an
    /// `impl` of it.
    fn implements(&self, ty: Ty, trait_id: TraitId) -> bool {
        if self.t.types.is_wildcard(ty) {
            return true;
        }
        if self.t.types.as_param(ty).is_some() {
            return self
                .param_bounds
                .get(&ty)
                .is_some_and(|b| b.contains(&trait_id));
        }
        if Some(trait_id.0) == self.t.types.measure && self.t.types.is_measurable(ty) {
            return true;
        }
        // An `impl` for a generic type matches instances whose arguments are parameters too.
        self.t.impls.iter().any(|i| {
            if i.trait_id != trait_id {
                return false;
            }
            let mut found = vec![None; i.generics.len()];
            self.t
                .types
                .match_params(i.self_ty, ty, &i.generics, &mut found)
        })
    }

    /// The type the alias `item` names, if it is one (`spec/types.md` §2).
    fn alias_type(&mut self, item: ItemId) -> Option<Ty> {
        if let Some(&ty) = self.alias_types.get(&item) {
            return Some(ty);
        }
        let &(id, span) = self.aliases.get(&item)?;
        if self.resolving_aliases.contains(&item) {
            let ItemKind::Type { name, .. } = self.ast.item(item).kind else {
                unreachable!("an alias")
            };
            let message = format!("the alias `{}` names itself", self.name(name));
            self.error(span, message).help = Some(
                "an alias names another type; for a type that holds itself, use an enum".into(),
            );
            self.alias_types.insert(item, Ty::ERROR);
            return Some(Ty::ERROR);
        }
        // The alias's type is written in its own module.
        let module = std::mem::replace(&mut self.module, self.scopes.item_module(item));
        self.resolving_aliases.push(item);
        let ty = self.resolve_type(id);
        self.resolving_aliases.pop();
        self.module = module;
        // A cycle through this alias was reported where it closed.
        let ty = *self.alias_types.entry(item).or_insert(ty);
        Some(ty)
    }

    /// The type parameters in scope in `f`'s body, by name.
    fn fn_type_params(&self, f: FnId) -> Vec<(Symbol, Ty)> {
        let generics = &self.t.fns[f.index()].generics;
        generics
            .iter()
            .map(|&t| {
                let p = self.t.types.as_param(t).expect("a type parameter");
                (self.t.types.params[p.index()].name, t)
            })
            .collect()
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

    /// Resolves `f`'s signature. A method of `owner` sees its struct's type parameters; a
    /// trait's or an `impl`'s function sees `prefix` first, and `Self` is `self_ty`.
    fn collect_fn(
        &mut self,
        f: FnId,
        owner: Option<StructId>,
        prefix: Vec<(Symbol, Ty)>,
        self_ty: Option<Ty>,
    ) {
        let decl = *self.ast.function(f);
        let outer = std::mem::take(&mut self.type_params);
        let outer_self = self.self_ty;
        if self_ty.is_some() {
            self.self_ty = self_ty;
        }
        let mut scope = owner
            .and_then(|o| self.struct_params.get(&o).cloned())
            .unwrap_or_default();
        scope.extend(prefix);
        self.type_params = scope;
        let own = self.generic_params(decl.generics);
        self.type_params.extend(own);
        let params = self
            .ast
            .list(decl.params)
            .iter()
            .map(|p| self.resolve_type(p.ty))
            .collect();
        let ret = decl.ret.map_or(Ty::UNIT, |ty| self.resolve_type(ty));
        if decl.receiver.is_some() && owner.is_none() && self_ty.is_none() {
            let span = decl.receiver.map_or(decl.name_span, |r| r.span);
            self.error(
                span,
                "`self` is only allowed in methods inside a struct or enum",
            );
        }
        let generics = self.type_params.iter().map(|&(_, t)| t).collect();
        self.type_params = outer;
        self.self_ty = outer_self;
        // Generic code cannot see an `impl`'s body, so a trait's function is reversible only
        // if declared so.
        let in_trait = self.t.fn_traits.contains_key(&f);
        let info = &mut self.t.fns[f.index()];
        info.owner = owner;
        info.generics = generics;
        info.self_ty = self_ty;
        info.sig = Sig {
            receiver: decl.receiver.map(|r| r.kind),
            params,
            ret,
            is_unsafe: decl.is_unsafe,
            reversible: if decl.is_rev {
                Reversible::Yes
            } else if in_trait {
                Reversible::No
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
                match len {
                    Len::Int(len) => self.t.types.array(elem, len),
                    Len::Param(_) => {
                        self.error(node.span, "size parameters are not supported yet")
                            .help = Some("see spec/types.md §6 for what is planned".into());
                        Ty::ERROR
                    }
                }
            }
            TypeKind::Size(_) => {
                self.error(node.span, "a size is not a type");
                Ty::ERROR
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
                    _ if let Some(template) = self.generic_struct(path, node.span) => {
                        let expected = self.t.types.structs[template.index()].args.len();
                        if args.len() != expected {
                            let message = format!(
                                "`{name}` takes {expected} type argument{}, but {} {} given",
                                if expected == 1 { "" } else { "s" },
                                args.len(),
                                if args.len() == 1 { "was" } else { "were" }
                            );
                            self.error(node.span, message);
                            return Ty::ERROR;
                        }
                        let id = self.t.types.instantiate(template, args);
                        self.t.types.struct_ty(id)
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
                let Some((at, rest)) = self.qualify(segments, node.span) else {
                    return Ty::ERROR;
                };
                if let [single] = rest {
                    let name = ast.name(*single);
                    if name == "Self" && at.is_none() && self.self_ty.is_none() {
                        self.error(node.span, "`Self` is only available inside a struct");
                        return Ty::ERROR;
                    }
                    if let Some(ty) = self.path_type(at, *single, node.span) {
                        if let TyKind::Struct(id) = *self.t.types.kind(ty)
                            && self.struct_params.contains_key(&id)
                            && Some(ty) != self.self_ty
                        {
                            let message = format!(
                                "`{name}` is generic: give its type arguments, as in `{name}<…>`"
                            );
                            self.error(node.span, message);
                            return Ty::ERROR;
                        }
                        return ty;
                    }
                    if at.is_none() && (name == "Option" || name == "Result") {
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
                // `T::Result`, `Self::Result`: an associated type.
                if at.is_none()
                    && let [base, name] = *rest
                    && let Some(base_ty) = self.path_type(None, base, node.span)
                {
                    return self.assoc_type(base_ty, name, node.span);
                }
                let text = self.path_text(segments);
                let message = format!("cannot find the type `{text}`");
                let help = self.std_help(ast.list(segments)[0]);
                self.error(node.span, message).help = help;
                Ty::ERROR
            }
        }
    }
}

// Function bodies and statements.
impl<'a> Checker<'a> {
    fn check_bodies(&mut self) {
        let ast = self.ast;
        for (index, decl) in ast.all_fns().iter().enumerate() {
            let f = FnId::from_index(index);
            let Some(body) = decl.body else { continue };
            self.module = self.t.fn_modules[f.index()];
            self.type_params = self.fn_type_params(f);
            let info = self.t.fns[index].clone();
            self.self_ty = info
                .owner
                .map(|id| self.t.types.struct_ty(id))
                .or(info.self_ty);
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
                    PatLiteral::Int(v) => {
                        ty == Ty::INT
                            || (ty == Ty::BIT && (v == 0 || v == 1))
                            || ty.width().is_some_and(|w| w.holds(i128::from(v)))
                    }
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
        let (at, segments) = self.qualify(path, span).unwrap_or((None, &[]));
        let found = match *segments {
            [enum_name, name] => self.path_type(at, enum_name, span).map(|t| (t, name)),
            [name] if at.is_none() => {
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
        // `Maybe::Just(x)` names the generic enum; the value is one of its instances.
        let enum_ty = match (self.struct_of(enum_ty), self.struct_of(ty)) {
            (Some(e), Some(v))
                if self.struct_params.contains_key(&e)
                    && self.t.types.structs[v.index()].template == Some(e) =>
            {
                ty
            }
            _ => enum_ty,
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
impl<'a> Checker<'a> {
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
        // In a generic call, the parameters being inferred match what they are given.
        if !self.inference.is_empty() && self.t.types.has_params(want) {
            if self.bind_param(want, found) {
                return;
            }
            if let TyKind::Ref { mutable, inner } = *self.t.types.kind(want)
                && !matches!(self.t.types.kind(found), TyKind::Ref { .. })
                && self.bind_param(inner, found)
            {
                self.t.adjust[id.index()] = Adjust::Lend { mutable };
                return;
            }
        }
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
                // In a generic call, what is known of the parameters so far.
                let want = self.known(want);
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
            ExprKind::Int(value) => self.int_literal(Some(value), expect, expr.span),
            ExprKind::BigInt => self.int_literal(None, expect, expr.span),
            ExprKind::Float(_) => match expect.ty().map(|t| self.t.types.deref(t)) {
                Some(t @ (Ty::F32 | Ty::COMPLEX | Ty::C32)) => t,
                _ => Ty::FLOAT,
            },
            ExprKind::Imag(_) => match expect.ty().map(|t| self.t.types.deref(t)) {
                Some(Ty::C32) => Ty::C32,
                _ => Ty::COMPLEX,
            },
            ExprKind::Str(parts) => self.string(parts),
            ExprKind::Bool(_) => Ty::BOOL,
            ExprKind::Ket { qubits, parts } => {
                let Some(computed) = self.computed_labels(parts) else {
                    return Ty::ERROR;
                };
                let qubits = u64::from(qubits) + computed;
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
                    && let Some(index) = self.t.types.structs[id.index()]
                        .fields
                        .iter()
                        .position(|(n, _)| *n == name)
                {
                    self.field_visible(id, index, expr.span);
                    return self.t.types.structs[id.index()].fields[index].1;
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
                // `-128` is an `i8` although `128` alone is not.
                self.negating = op == UnaryOp::Neg
                    && matches!(ast.expr(operand).kind, ExprKind::Int(_) | ExprKind::BigInt);
                let ty = self.expr(operand, hint);
                self.negating = false;
                let ty = self.t.types.deref(ty);
                let ok = match op {
                    UnaryOp::Neg => {
                        matches!(ty, Ty::INT | Ty::OBSERVABLE)
                            || ty.is_float()
                            || ty.is_complex()
                            || ty.width().is_some_and(|w| w.signed)
                    }
                    UnaryOp::BitNot => ty.is_integer() || ty == Ty::BIT,
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
                // Type a literal side from the other side, so `m == 1` with `m: Bit` and
                // `x + 1` with `x: u8` work; arithmetic also takes the type expected of it,
                // so `let y: u8 = 1 + 2` does.
                use BinaryOp::*;
                let arithmetic = !matches!(op, Eq | NotEq | Lt | Le | Gt | Ge | And | Or);
                let outer = expect
                    .ty()
                    .map(|t| self.t.types.deref(t))
                    .filter(|&t| arithmetic && (t.is_number() || t.is_complex() || t == Ty::BIT))
                    .map_or(Expect::None, Expect::Hint);
                let (lhs_ty, rhs_ty) = if self.is_literal(lhs) {
                    let rhs_ty = self.expr(rhs, outer);
                    let hint = self.t.types.deref(rhs_ty);
                    (self.expr(lhs, Expect::Hint(hint)), rhs_ty)
                } else {
                    let lhs_ty = self.expr(lhs, outer);
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
                qubits,
                parts,
                operand,
                ..
            } => {
                let computed = self.computed_labels(parts);
                let ty = self.expr(operand, Expect::None);
                let Some(computed) = computed else {
                    return Ty::BOOL;
                };
                let qubits = u64::from(qubits) + computed;
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
            ExprKind::PrepTest { prep, operand } => {
                let prep_ty = self.expr(prep, Expect::None);
                let qubits = |types: &Types, t: Ty| {
                    t == Ty::QUBIT || matches!(*types.kind(t), TyKind::Array(Ty::QUBIT, _))
                };
                let is_fn = matches!(self.t.callees[prep.index()], Some(Callee::Fn(_)));
                let valid = is_fn && qubits(&self.t.types, prep_ty);
                if !self.t.types.is_wildcard(prep_ty) && !valid {
                    let span = self.ast.expr(prep).span;
                    self.error(
                        span,
                        "a bra holds labels such as `<01|`, or a call to a function that prepares a `Qubit` or `[Qubit; N]`",
                    );
                }
                let ty = self.expr(operand, Expect::None);
                let ty = self.t.types.deref(ty);
                let wild = self.t.types.is_wildcard(ty) || self.t.types.is_wildcard(prep_ty);
                if !wild && valid && ty != prep_ty {
                    let message = format!(
                        "this bra's preparation makes {}, but the value is {}",
                        self.show(prep_ty),
                        self.show(ty)
                    );
                    let span = self.ast.expr(operand).span;
                    self.error(span, message);
                }
                Ty::BOOL
            }
            ExprKind::Overlap {
                bra,
                ket,
                observable,
                precision,
            } => {
                if let Some(precision) = precision {
                    self.expr(precision, Expect::Ty(Ty::FLOAT));
                }
                let bra_ty = self.preparation_type(bra);
                let ket_ty = self.preparation_type(ket);
                if let (Some(a), Some(b)) = (bra_ty, ket_ty)
                    && a != b
                {
                    let message = format!(
                        "both sides must prepare the same qubits: {} and {}",
                        self.show(a),
                        self.show(b)
                    );
                    self.error(expr.span, message);
                }
                if let Some(observable) = observable {
                    self.expr(observable, Expect::Ty(Ty::OBSERVABLE));
                }
                Ty::COMPLEX
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
                    .help = Some("see spec/ownership.md §8".into());
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
        let message = if self.names(name, Ns::Value) {
            format!("`{text}` is a function; call it with `{text}(…)`")
        } else if self.names(name, Ns::Type)
            || self.std_types.contains_key(&name)
            || prelude::builtin_type(&text).is_some()
        {
            format!("`{text}` is a type, not a value")
        } else {
            format!("cannot find `{text}` in this scope")
        };
        self.error(span, message);
        Ty::ERROR
    }

    /// The type `name` names in the module `at`, or as written here when `at` is `None`,
    /// where it can also be `Self`, a built-in type or one of the prelude's.
    fn path_type(&mut self, at: Option<ModuleId>, name: Symbol, span: Span) -> Option<Ty> {
        let found = match at {
            None => {
                if let Some(&(_, ty)) = self.type_params.iter().rev().find(|&&(n, _)| n == name) {
                    return Some(ty);
                }
                let text = self.name(name);
                if let Some(ty) = prelude::builtin_type(text) {
                    return Some(ty);
                }
                if text == "Self" {
                    return self.self_ty;
                }
                self.scopes.lexical(self.module, name, Ns::Type)
            }
            Some(module) => self.scopes.qualified(self.module, module, name, Ns::Type),
        };
        let Some(binding) = self.found(found, name, span) else {
            // The prelude's types, which a module's own names take precedence over; `State`
            // is in `std::state`.
            let in_prelude = at.is_none() && resolve::std_module_of(self.name(name)).is_none();
            let id = *self.std_types.get(&name).filter(|_| in_prelude)?;
            return Some(self.t.types.struct_ty(id));
        };
        match binding {
            Binding::Item(item) => match self.item_structs[item.index()] {
                Some(id) => Some(self.t.types.struct_ty(id)),
                None => self.alias_type(item),
            },
            Binding::Std(text) => {
                let id = *self.std_types.get(&self.ast.symbols.get(text)?)?;
                Some(self.t.types.struct_ty(id))
            }
            Binding::Module(_) | Binding::Fn(_) => None,
        }
    }

    /// Matches `found` against `want`, binding the type parameters being inferred that `want`
    /// mentions. Returns whether they match.
    fn bind_param(&mut self, want: Ty, found: Ty) -> bool {
        // Before comparing: a parameter can be given itself, as a struct's `T` is in its own
        // methods.
        if let Some(inference) = self.inference.last_mut()
            && let Some(i) = inference.params.iter().position(|&p| p == want)
        {
            return match inference.found[i] {
                None => {
                    inference.found[i] = Some(found);
                    true
                }
                Some(earlier) => {
                    earlier == found
                        || self.t.types.is_wildcard(earlier)
                        || self.t.types.is_wildcard(found)
                }
            };
        }
        let types = &self.t.types;
        if want == found || types.is_wildcard(found) {
            return true;
        }
        match (types.kind(want).clone(), types.kind(found).clone()) {
            (TyKind::Tuple(a), TyKind::Tuple(b)) if a.len() == b.len() => {
                a.iter().zip(b.iter()).all(|(&w, &f)| self.bind_param(w, f))
            }
            (TyKind::Array(w, n), TyKind::Array(f, m)) if n == m => self.bind_param(w, f),
            (TyKind::Distribution(w), TyKind::Distribution(f)) => self.bind_param(w, f),
            (
                TyKind::Ref {
                    mutable: a,
                    inner: w,
                },
                TyKind::Ref {
                    mutable: b,
                    inner: f,
                },
            ) if a == b => self.bind_param(w, f),
            (TyKind::Struct(a), TyKind::Struct(b)) => {
                let (da, db) = (&types.structs[a.index()], &types.structs[b.index()]);
                if da.template.unwrap_or(a) != db.template.unwrap_or(b)
                    || da.args.len() != db.args.len()
                {
                    return false;
                }
                let (wa, fa) = (da.args.clone(), db.args.clone());
                wa.iter().zip(&fa).all(|(&w, &f)| self.bind_param(w, f))
            }
            _ => false,
        }
    }

    /// `ty` with the type parameters being inferred that are already known replaced, so
    /// that literals take the type expected (`swap::<Bit>((0, 1))`).
    fn known(&mut self, ty: Ty) -> Ty {
        let Some(inference) = self.inference.last() else {
            return ty;
        };
        if inference.found.iter().all(Option::is_none) || !self.t.types.has_params(ty) {
            return ty;
        }
        let (from, to): (Vec<Ty>, Vec<Ty>) = inference
            .params
            .iter()
            .zip(&inference.found)
            .filter_map(|(&p, f)| Some((p, (*f)?)))
            .unzip();
        let ty = self.t.types.subst(ty, &from, &to);
        self.resolve_assoc(ty)
    }

    /// Runs `check` with `params` being inferred, starting from `given`, and returns what
    /// each was found to be, or `None` for one nothing determined.
    fn infer(
        &mut self,
        params: &[Ty],
        given: Vec<Option<Ty>>,
        check: impl FnOnce(&mut Self),
    ) -> Vec<Option<Ty>> {
        self.inference.push(Inference {
            params: params.to_vec(),
            found: given,
        });
        check(self);
        self.inference.pop().expect("pushed").found
    }

    /// The type arguments a generic call or literal ends with: what was found, with each
    /// parameter nothing determined reported and left unknown.
    fn settle(&mut self, params: &[Ty], found: Vec<Option<Ty>>, what: &str, span: Span) -> Vec<Ty> {
        params
            .iter()
            .zip(found)
            .map(|(&p, found)| {
                found.unwrap_or_else(|| {
                    let message = format!(
                        "cannot tell what `{}` is in this {what}",
                        self.t.types.display(p, self.ast)
                    );
                    self.error(span, message).help =
                        Some("give it with `::<…>`, or a type annotation".into());
                    Ty::ERROR
                })
            })
            .collect()
    }

    /// Reports each type argument that does not meet its parameter's bound.
    fn check_bounds(&mut self, params: &[Ty], args: &[Ty], callee: &str, span: Span) {
        for (&p, &arg) in params.iter().zip(args) {
            let Some(id) = self.t.types.as_param(p) else {
                continue;
            };
            let def = self.t.types.params[id.index()];
            if def.copy
                && !self.t.types.is_wildcard(arg)
                && self.t.types.linearity(arg) != Kind::Copyable
            {
                let message = format!(
                    "`{callee}` needs `{}: Copy`, but here `{}` is {}",
                    self.name(def.name),
                    self.name(def.name),
                    self.show(arg)
                );
                self.error(span, message);
            }
            for trait_id in self.param_bounds.get(&p).cloned().unwrap_or_default() {
                let trait_name = self.name(self.t.traits[trait_id.index()].name).to_string();
                let fixed: Vec<(Symbol, Ty)> = self
                    .param_assoc
                    .iter()
                    .filter(|((q, t, _), _)| *q == p && *t == trait_id)
                    .map(|((_, _, n), &ty)| (*n, ty))
                    .collect();
                for (name, want) in fixed {
                    if !self.implements(arg, trait_id) {
                        break;
                    }
                    let want = self.t.types.subst(want, params, args);
                    let found = self.assoc_type(arg, name, span);
                    if found != want && !self.t.types.is_wildcard(found) {
                        let message = format!(
                            "`{callee}` needs `{}::{}` to be {}, but it is {}",
                            self.show(arg).trim_matches('`'),
                            self.name(name),
                            self.show(want),
                            self.show(found)
                        );
                        self.error(span, message);
                    }
                }
                let _ = &trait_name;
                if !self.implements(arg, trait_id) {
                    let trait_name = self.name(self.t.traits[trait_id.index()].name).to_string();
                    let message = format!(
                        "`{callee}` needs `{}: {trait_name}`, but {} does not implement `{trait_name}`",
                        self.name(def.name),
                        self.show(arg)
                    );
                    self.error(span, message);
                }
            }
        }
    }

    /// For `ty`, a generic enum named without arguments (`Maybe::Nothing`), the instance the
    /// surrounding code expects; otherwise `ty`.
    fn instance_expected(&mut self, ty: Ty, expect: Expect, span: Span) -> Ty {
        let Some(s) = self.struct_of(ty) else {
            return ty;
        };
        if !self.struct_params.contains_key(&s) {
            return ty;
        }
        if let Some(want) = expect.ty()
            && let Some(w) = self.struct_of(want)
            && self.t.types.structs[w.index()].template == Some(s)
        {
            return want;
        }
        let params = self.t.types.structs[s.index()].args.clone();
        let args = self.settle(&params, vec![None; params.len()], "variant", span);
        let instance = self.t.types.instantiate(s, args);
        self.t.types.struct_ty(instance)
    }

    /// The generic struct or enum `path` names, if it names one.
    fn generic_struct(&mut self, path: List<Symbol>, span: Span) -> Option<StructId> {
        let (at, rest) = self.qualify(path, span)?;
        let [name] = *rest else { return None };
        let ty = self.path_type(at, name, span)?;
        let TyKind::Struct(id) = *self.t.types.kind(ty) else {
            return None;
        };
        self.struct_params.contains_key(&id).then_some(id)
    }

    /// The help for a standard-library name used without its `use`, if it is one.
    fn std_help(&self, name: Symbol) -> Option<String> {
        let text = self.name(name);
        let module = resolve::std_module_of(text)?;
        Some(format!(
            "`{text}` is in `std::{module}`, which the prelude does not import: add \
             `use std::{module}::{text}`"
        ))
    }

    /// The function `name` names in the module `at`, or as written here when `at` is `None`:
    /// the program's, or a standard-library one imported by name. `Err` if the name is
    /// ambiguous, which is reported.
    fn path_fn(
        &mut self,
        at: Option<ModuleId>,
        name: Symbol,
        span: Span,
    ) -> Result<Option<FnRef>, ()> {
        let found = match at {
            None => self.scopes.lexical(self.module, name, Ns::Value),
            Some(module) => self.scopes.qualified(self.module, module, name, Ns::Value),
        };
        let ambiguous = matches!(found, Some(Found::Ambiguous(..)));
        match self.found(found, name, span) {
            Some(Binding::Fn(f)) => Ok(Some(FnRef::Fn(f))),
            Some(Binding::Std(text)) => Ok(Some(FnRef::Std(text))),
            _ if ambiguous => Err(()),
            _ => Ok(None),
        }
    }

    /// What a lookup found, reporting a name that is private or ambiguous.
    fn found(&mut self, found: Option<Found>, name: Symbol, span: Span) -> Option<Binding> {
        match found? {
            Found::Entry(entry) => Some(entry.binding),
            Found::Private(entry) => {
                let message = format!("`{}` is private", self.name(name));
                self.error(span, message);
                Some(entry.binding)
            }
            Found::Ambiguous(a, b) => {
                let message = format!(
                    "`{}` is ambiguous: glob imports bring two different items with that name",
                    self.name(name)
                );
                let labels = &mut self.error(span, message).labels;
                labels.push((a, "one comes from this import".into()));
                labels.push((b, "another from this one".into()));
                None
            }
        }
    }

    /// Whether `name`, written here, names something in `ns`, without reporting anything.
    fn names(&self, name: Symbol, ns: Ns) -> bool {
        matches!(
            self.scopes.lexical(self.module, name, ns),
            Some(Found::Entry(_) | Found::Ambiguous(..))
        )
    }

    /// Splits off the modules a path starts with: the module the rest is in (`None` if the
    /// path does not start with one) and the rest. `None` if the path is broken, which is
    /// reported.
    fn qualify(
        &mut self,
        segments: List<Symbol>,
        span: Span,
    ) -> Option<(Option<ModuleId>, &'a [Symbol])> {
        let path: &'a [Symbol] = self.ast.list(segments);
        if path.len() < 2 {
            return Some((None, path));
        }
        let mut module = match self.scopes.path_start(self.words, self.module, path[0]) {
            None => return Some((None, path)),
            Some(Ok(module)) => module,
            Some(Err(error)) => {
                let message = resolve::path_message(self.ast, path, &error);
                self.error(span, message);
                return None;
            }
        };
        let mut at = 1;
        while at + 1 < path.len() {
            if self.words.is_super(path[at]) {
                let Some(parent) = self.scopes.parent(module) else {
                    let error = PathError::NoParent { at };
                    let message = resolve::path_message(self.ast, path, &error);
                    self.error(span, message);
                    return None;
                };
                module = parent;
            } else {
                match self
                    .scopes
                    .qualified(self.module, module, path[at], Ns::Type)
                {
                    Some(Found::Entry(Entry {
                        binding: Binding::Module(inner),
                        ..
                    })) => module = inner,
                    _ => break,
                }
            }
            at += 1;
        }
        Some((Some(module), &path[at..]))
    }

    /// A path used as a value: a variant such as `Basis::X`, or a constant such as `Float::PI`.
    fn path_value(&mut self, id: ExprId, segments: List<Symbol>, expect: Expect, span: Span) -> Ty {
        let Some((at, rest)) = self.qualify(segments, span) else {
            return Ty::ERROR;
        };
        if let ([name], None) = (rest, at) {
            return self.bare_value(id, *name, expect, span);
        }
        if let ([name], Some(module)) = (rest, at) {
            let path = self.path_text(segments);
            let message = if !matches!(self.path_fn(Some(module), *name, span), Ok(None)) {
                format!("`{path}` is a function; call it with `{path}(…)`")
            } else if self.path_type(Some(module), *name, span).is_some() {
                format!("`{path}` is a type, not a value")
            } else {
                format!("cannot find the value `{path}`")
            };
            self.error(span, message);
            return Ty::ERROR;
        }
        if let [ty_name, name] = *rest
            && let Some(ty) = self.path_type(at, ty_name, span)
            && let Some(index) = self.variant_index(ty, name)
        {
            let written_self = self.name(ty_name) == "Self";
            let ty = if written_self {
                ty
            } else {
                self.instance_expected(ty, expect, span)
            };
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
        if let [ty_name, name] = *rest
            && let Some(ty) = self.path_type(at, ty_name, span)
            && let Some(constant) = prelude::constant(ty, self.name(name))
        {
            self.t.res[id.index()] = Res::Builtin(constant);
            return Ty::FLOAT;
        }
        let text = self.path_text(segments);
        let message = format!("cannot find the value `{text}`");
        let help = self.std_help(self.ast.list(segments)[0]);
        self.error(span, message).help = help;
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
        let path_span = ast.expr(path).span;
        let qualified = match ast.expr(path).kind {
            ExprKind::Path(segments) => Some(self.qualify(segments, path_span)),
            _ => None,
        };
        if let Some(None) = qualified {
            self.skip_inits(inits);
            return Ty::ERROR;
        }
        // `Enum::Variant { … }`: a variant with named fields.
        if let ExprKind::Path(segments) = ast.expr(path).kind
            && let Some(Some((at, &[enum_name, name]))) = qualified
            && let Some(enum_ty) = self.path_type(at, enum_name, path_span)
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
        let struct_ty = match (ast.expr(path).kind, qualified) {
            (ExprKind::Name(name), _) => self.path_type(None, name, path_span),
            (_, Some(Some((at, &[name])))) => self.path_type(at, name, path_span),
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
        // Other modules cannot create a struct whose fields they cannot see.
        for init in ast.list(inits) {
            if let Some(index) = fields.iter().position(|(n, _)| *n == init.name) {
                self.field_visible(id, index, init.span);
            }
        }
        // `Pair { first: 1, second: |0> }`: a generic struct's arguments, from its fields.
        // `Self { … }` is the struct's own type, in its methods.
        let written_self =
            matches!(ast.expr(path).kind, ExprKind::Name(n) if self.name(n) == "Self");
        if self.struct_params.contains_key(&id) && !written_self {
            let params = self.t.types.structs[id.index()].args.clone();
            let found = self.infer(&params, vec![None; params.len()], |this| {
                this.field_inits(inits, &fields, struct_ty, span);
            });
            let args = self.settle(&params, found, "struct literal", path_span);
            let instance = self.t.types.instantiate(id, args);
            return self.t.types.struct_ty(instance);
        }
        self.field_inits(inits, &fields, struct_ty, span);
        struct_ty
    }

    /// Where `vis`, written in the current module, can be used from, reporting a `pub(in …)`
    /// that names no module containing it.
    fn visibility(&mut self, vis: Vis) -> Visible {
        match self.scopes.visibility(self.words, self.module, vis) {
            Ok(visible) => visible,
            Err((span, message)) => {
                self.error(span, message);
                Visible::Within(self.module)
            }
        }
    }

    /// Reports the field `index` of the struct `id` if the current module cannot see it.
    fn field_visible(&mut self, id: StructId, index: usize, span: Span) {
        let id = self.t.types.structs[id.index()].template.unwrap_or(id);
        let Some(&visible) = self.field_vis.get(&id).and_then(|v| v.get(index)) else {
            return;
        };
        if !self.scopes.can_see(self.module, visible) {
            let def = &self.t.types.structs[id.index()];
            let message = format!(
                "the field `{}` of `{}` is private",
                self.name(def.fields[index].0),
                self.name(def.name)
            );
            self.error(span, message);
        }
    }

    /// Reports the method `f` if the current module cannot see it.
    fn method_visible(&mut self, f: FnId, span: Span) {
        let decl = self.ast.function(f);
        let module = self.t.fn_modules[f.index()];
        let visible = self
            .scopes
            .visibility(self.words, module, decl.vis)
            .unwrap_or(Visible::Within(module));
        if !self.scopes.can_see(self.module, visible) {
            let message = format!("`{}` is private", self.name(decl.name));
            self.error(span, message);
        }
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
            let field_ty = self.known(field_ty);
            match init.value {
                Some(value) => {
                    self.expr(value, Expect::Ty(field_ty));
                }
                None => match self.lookup(init.name) {
                    Some(local) => {
                        let ty = self.t.local(local).ty;
                        let inferred = !self.inference.is_empty() && self.bind_param(field_ty, ty);
                        if ty != field_ty && !inferred && !self.t.types.is_wildcard(ty) {
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

    /// How many qubits a ket's or bra's computed labels stand for: one for a `Bit` or
    /// `State`, N for an array of them (`spec/types.md` §3). `None` if one was reported.
    fn computed_labels(&mut self, parts: List<KetPart>) -> Option<u64> {
        let mut qubits = Some(0);
        for part in self.ast.list(parts) {
            let KetPart::Value(value) = *part else {
                continue;
            };
            // `{[0, 1]}` is bits, as `{1}` is a bit.
            let hint = match self.ast.expr(value).kind {
                ExprKind::Array(elems) => {
                    let len = self.ast.list(elems).len() as u64;
                    self.t.types.array(Ty::BIT, len)
                }
                _ => Ty::BIT,
            };
            let ty = self.expr(value, Expect::Hint(hint));
            let ty = self.t.types.deref(ty);
            let label = |t: Ty| t == Ty::BIT || Some(t) == self.t.types.state;
            let count = match *self.t.types.kind(ty) {
                _ if label(ty) => Some(1),
                TyKind::Array(elem, len) if label(elem) => Some(len),
                _ if self.t.types.is_wildcard(ty) => None,
                _ => {
                    let message = format!(
                        "a computed label is a `Bit`, `State` or an array of them, not {}",
                        self.show(ty)
                    );
                    let span = self.ast.expr(value).span;
                    self.error(span, message);
                    None
                }
            };
            qubits = qubits.zip(count).map(|(a, b)| a + b);
        }
        qubits
    }

    /// The type a side of an amplitude prepares, if it is a preparation: fixed labels, or a
    /// call to a function that gives a `Qubit` or `[Qubit; N]`.
    fn preparation_type(&mut self, prep: ExprId) -> Option<Ty> {
        let ty = self.expr(prep, Expect::None);
        if self.t.types.is_wildcard(ty) {
            return None;
        }
        let qubits =
            ty == Ty::QUBIT || matches!(*self.t.types.kind(ty), TyKind::Array(Ty::QUBIT, _));
        let shape = match self.ast.expr(prep).kind {
            ExprKind::Ket { .. } => true,
            _ => matches!(self.t.callees[prep.index()], Some(Callee::Fn(_))),
        };
        if qubits && shape {
            return Some(ty);
        }
        let span = self.ast.expr(prep).span;
        self.error(
            span,
            "a preparation is a ket's labels, or a call to a function that prepares a `Qubit` or `[Qubit; N]`",
        );
        None
    }

    /// An integer literal: `value`, or one too large for a `u64`. It is the sized integer or
    /// `Bit` expected, if it fits, and otherwise an `Int`.
    fn int_literal(&mut self, value: Option<u64>, expect: Expect, span: Span) -> Ty {
        let want = expect.ty().map(|t| self.t.types.deref(t));
        if want == Some(Ty::BIT) && value.is_some_and(|v| v <= 1) {
            return Ty::BIT;
        }
        let Some((ty, width)) = want.and_then(|t| Some((t, t.width()?))) else {
            return Ty::INT;
        };
        // After `-`, a signed type's minimum: `-128` as an `i8`.
        let limit = if self.negating {
            -width.min()
        } else {
            width.max()
        };
        if value.is_none_or(|v| i128::from(v) > limit) {
            let message = format!(
                "this literal does not fit in `{}`, which holds {} to {}",
                width.name(),
                width.min(),
                width.max()
            );
            self.error(span, message);
        }
        ty
    }

    /// Whether `id` is a number literal, maybe negated, which takes its type from the other
    /// side of an operator.
    fn is_literal(&self, id: ExprId) -> bool {
        match self.ast.expr(id).kind {
            ExprKind::Int(_) | ExprKind::BigInt | ExprKind::Float(_) | ExprKind::Imag(_) => true,
            ExprKind::Unary {
                op: UnaryOp::Neg,
                operand,
            } => self.is_literal(operand),
            _ => false,
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
            Lt | Le | Gt | Ge => (lhs == rhs && lhs.is_number()).then_some(Ty::BOOL),
            BitOr | BitXor | BitAnd => {
                (lhs == rhs && (lhs.is_integer() || lhs == Ty::BIT)).then_some(lhs)
            }
            // The shift amount is the same type, or an `Int`.
            Shl | Shr => (lhs.is_integer() && (rhs == lhs || rhs == Ty::INT)).then_some(lhs),
            Add | Sub => match (lhs, rhs) {
                (a, b) if a == b && (a.is_number() || a.is_complex()) => Some(a),
                (Ty::OBSERVABLE, Ty::OBSERVABLE) => Some(Ty::OBSERVABLE),
                _ => None,
            },
            Mul => match (lhs, rhs) {
                (a, b) if a == b && (a.is_number() || a.is_complex()) => Some(a),
                (Ty::OBSERVABLE | Ty::FLOAT, Ty::OBSERVABLE) | (Ty::OBSERVABLE, Ty::FLOAT) => {
                    Some(Ty::OBSERVABLE)
                }
                _ => None,
            },
            Div => (lhs == rhs && (lhs.is_number() || lhs.is_complex())).then_some(lhs),
            Rem => (lhs == rhs && lhs.is_number()).then_some(lhs),
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
        let mixes_complex =
            (lhs.is_complex() && rhs.is_number()) || (lhs.is_number() && rhs.is_complex());
        let help = if matches!((lhs, rhs), (Ty::INT, Ty::FLOAT) | (Ty::FLOAT, Ty::INT)) {
            Some("Noev does not convert between `Int` and `Float` implicitly; use `Float(n)` or a float literal such as `2.0`".to_string())
        } else if mixes_complex {
            let complex = self
                .t
                .types
                .display(if lhs.is_complex() { lhs } else { rhs }, self.ast);
            Some(format!(
                "Noev does not convert to complex numbers implicitly; write `{complex}(x)`"
            ))
        } else if lhs.is_number() && rhs.is_number() {
            Some("Noev does not convert between number types implicitly; convert one side, as in `i64(x)` or `Int(x)`".to_string())
        } else {
            None
        };
        self.error(span, message).help = help;
        match op {
            Or | And | Eq | NotEq | Lt | Le | Gt | Ge => Ty::BOOL,
            _ => Ty::ERROR,
        }
    }
}

// Calls.
impl<'a> Checker<'a> {
    /// A call of the program's function `f`, through the path `callee`.
    fn fn_call(&mut self, id: ExprId, callee: ExprId, f: FnId, args: &[ExprId], span: Span) -> Ty {
        let sig = self.t.fns[f.index()].sig.clone();
        self.t.callees[id.index()] = Some(Callee::Fn(f));
        let generics = self.t.fns[f.index()].generics.clone();
        let given = self.ast.list(self.ast.path_args(callee)).to_vec();
        if generics.is_empty() {
            if !given.is_empty() {
                let span = self.ast.expr(callee).span;
                self.error(span, "this function takes no type arguments");
            }
            return self.check_args(&sig, None, args, span);
        }
        let name = self.name(self.ast.function(f).name).to_string();
        let given = self.given_args(&generics, &given, callee);
        let found = self.infer(&generics, given, |this| {
            this.check_args(&sig, None, args, span);
        });
        let type_args = self.settle(&generics, found, "call", self.ast.expr(callee).span);
        self.check_bounds(&generics, &type_args, &name, span);
        let ret = self.t.types.subst(sig.ret, &generics, &type_args);
        let ret = self.resolve_assoc(ret);
        self.t.type_args.insert(id, type_args);
        ret
    }

    /// The type arguments written with `::<…>` on `callee`, for `generics`, each `None` if
    /// none is written.
    fn given_args(&mut self, generics: &[Ty], given: &[TypeId], callee: ExprId) -> Vec<Option<Ty>> {
        if given.is_empty() {
            return vec![None; generics.len()];
        }
        if given.len() != generics.len() {
            let span = self.ast.expr(callee).span;
            let message = format!(
                "this takes {} type argument{}, but {} {} given",
                generics.len(),
                if generics.len() == 1 { "" } else { "s" },
                given.len(),
                if given.len() == 1 { "was" } else { "were" }
            );
            self.error(span, message);
        }
        let mut args: Vec<Option<Ty>> = given.iter().map(|&t| Some(self.resolve_type(t))).collect();
        args.resize(generics.len(), None);
        args
    }

    /// A call of the built-in function or conversion named `text`, if there is one.
    fn builtin_call(&mut self, id: ExprId, text: &str, args: &[ExprId], span: Span) -> Option<Ty> {
        if let Some(target) = prelude::builtin_type(text) {
            return Some(self.conversion(id, target, args, span));
        }
        Some(match text {
            "print" | "eprint" => self.print_call(id, text == "eprint", args),
            "measure" => self.measure_fn(id, args, span),
            "amplitudes" => self.amplitudes_fn(id, args, span),
            "fft" | "ifft" => self.fourier_fn(id, text == "ifft", args, span),
            _ => {
                let (builtin, sig) = prelude::function(&mut self.t.types, text)?;
                self.t.callees[id.index()] = Some(Callee::Builtin(builtin));
                self.check_args(&sig, None, args, span)
            }
        })
    }

    /// A call of a function named by one name, `name`, through `callee`.
    fn name_call(
        &mut self,
        id: ExprId,
        callee: ExprId,
        name: Symbol,
        args: &[ExprId],
        span: Span,
    ) -> Ty {
        let callee_expr = *self.ast.expr(callee);
        if let Some(local) = self.lookup(name) {
            let message = format!("`{}` is a variable, not a function", self.name(name));
            self.t.res[callee.index()] = Res::Local(local);
            self.error(callee_expr.span, message);
            return self.skip_args(args);
        }
        let Ok(found) = self.path_fn(None, name, callee_expr.span) else {
            return self.skip_args(args);
        };
        match found {
            Some(FnRef::Fn(f)) => return self.fn_call(id, callee, f, args, span),
            Some(FnRef::Std(text)) => {
                if let Some(ty) = self.builtin_call(id, text, args, span) {
                    return ty;
                }
            }
            None => {}
        }
        let text = self.name(name).to_string();
        if let Some(help) = self.std_help(name) {
            let message = format!("cannot find the function `{text}`");
            self.error(callee_expr.span, message).help = Some(help);
            return self.skip_args(args);
        }
        if let Some(ty) = self.builtin_call(id, &text, args, span) {
            return ty;
        }
        let message = if self.names(name, Ns::Type) {
            format!("`{text}` is a struct; create one with `{text} {{ … }}`")
        } else {
            format!("cannot find the function `{text}`")
        };
        self.error(callee_expr.span, message);
        self.skip_args(args)
    }

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
            ExprKind::Name(name) => self.name_call(id, callee, name, args, span),
            ExprKind::Path(segments) => {
                let path_span = callee_expr.span;
                let Some((at, rest)) = self.qualify(segments, path_span) else {
                    return self.skip_args(args);
                };
                if let ([name], None) = (rest, at) {
                    // `swap::<Int>`: a path only for its arguments.
                    if !BARE_VARIANTS.contains(&self.name(*name)) {
                        return self.name_call(id, callee, *name, args, span);
                    }
                    return self.bare_call(id, *name, args, expect, span);
                }
                if let ([name], Some(module)) = (rest, at) {
                    let Ok(found) = self.path_fn(Some(module), *name, path_span) else {
                        return self.skip_args(args);
                    };
                    match found {
                        Some(FnRef::Fn(f)) => return self.fn_call(id, callee, f, args, span),
                        Some(FnRef::Std(text)) => {
                            if let Some(ty) = self.builtin_call(id, text, args, span) {
                                return ty;
                            }
                        }
                        None => {}
                    }
                    let message =
                        format!("cannot find the function `{}`", self.path_text(segments));
                    self.error(path_span, message);
                    return self.skip_args(args);
                }
                if let [ty_name, name] = *rest
                    && let Some(ty) = self.path_type(at, ty_name, path_span)
                    && let Some(index) = self.variant_index(ty, name)
                {
                    let path = self.path_text(segments);
                    return self.variant_call(id, ty, index, &path, args, span);
                }
                if let [ty_name, name] = *rest
                    && let Some(target) = self.path_type(at, ty_name, path_span)
                    && target.width().is_some()
                    && self.name(name) == "try_from"
                {
                    return self.try_conversion(id, target, args, span);
                }
                if let [ty_name, name] = *rest
                    && let Some(ty) = self.path_type(at, ty_name, path_span)
                {
                    if let Some((callee_kind, sig)) = self.lookup_method(ty, name) {
                        if let Callee::Fn(f) = callee_kind {
                            self.method_visible(f, path_span);
                        }
                        self.t.callees[id.index()] = Some(callee_kind);
                        // `Type::method(x, …)` passes the receiver as the first argument.
                        let receiver = sig.receiver.map(|kind| match kind {
                            OwnKind::Mv => ty,
                            OwnKind::Ref => self.t.types.reference(false, ty),
                            OwnKind::Mut => self.t.types.reference(true, ty),
                        });
                        return match callee_kind {
                            Callee::Fn(f) => {
                                self.method_args(id, f, &sig, ty, receiver, args, span)
                            }
                            _ => self.check_args(&sig, receiver, args, span),
                        };
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
        let sig = Sig {
            receiver: None,
            params: variant.data.clone(),
            ret: enum_ty,
            is_unsafe: false,
            reversible: Reversible::Yes,
        };
        // `Maybe::Just(x)`: a generic enum's arguments, from the variant's data. `Self::Just(x)`
        // is the enum's own type, in its methods.
        if self.struct_params.contains_key(&s) && !path.starts_with("Self::") {
            let params = self.t.types.structs[s.index()].args.clone();
            let found = self.infer(&params, vec![None; params.len()], |this| {
                this.check_args(&sig, None, args, span);
            });
            let type_args = self.settle(&params, found, "variant", span);
            let instance = self.t.types.instantiate(s, type_args);
            self.t.callees[id.index()] = Some(Callee::Variant(instance, index));
            return self.t.types.struct_ty(instance);
        }
        self.t.callees[id.index()] = Some(Callee::Variant(s, index));
        self.check_args(&sig, None, args, span);
        enum_ty
    }

    /// The standard library's `Option<T>` (one type argument) or `Result<T, E>` (two): an enum
    /// made the first time each is used (`spec/types.md` §2).
    fn std_enum(&mut self, args: Vec<Ty>) -> Ty {
        let template = self.std_template(args.len() == 2);
        let id = self.t.types.instantiate(template, args);
        self.t.types.struct_ty(id)
    }

    /// The generic definition of `Option<T>`, or of `Result<T, E>` if `result`, made the
    /// first time it is needed.
    fn std_template(&mut self, result: bool) -> StructId {
        let ast = self.ast;
        let symbol = |text: &str| ast.symbols.get(text).expect("interned by the parser");
        let name = symbol(if result { "Result" } else { "Option" });
        if let Some(&id) = self.std_enums.get(&name) {
            return id;
        }
        let variant = |text: &str, data: Vec<Ty>| VariantDef {
            name: symbol(text),
            span: Span::default(),
            data,
            names: Vec::new(),
            named: false,
        };
        // The parameters are only ever shown replaced, so they can share the enum's name.
        let value = self.t.types.param(name, false);
        let (args, variants) = if result {
            let error = self.t.types.param(name, false);
            (
                vec![value, error],
                vec![variant("Ok", vec![value]), variant("Err", vec![error])],
            )
        } else {
            (
                vec![value],
                vec![variant("Some", vec![value]), variant("None", Vec::new())],
            )
        };
        let id = StructId(self.t.types.structs.len() as u32);
        self.t.types.structs.push(StructDef {
            name,
            span: Span::default(),
            fields: Vec::new(),
            variants: Some(variants),
            args,
            bare: true,
            template: None,
        });
        self.std_enums.insert(name, id);
        id
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
            // An instance's methods are its generic definition's.
            let id = self.t.types.structs[id.index()].template.unwrap_or(id);
            if let Some(&f) = self.methods.get(&(id, name)) {
                return Some((Callee::Fn(f), self.t.fns[f.index()].sig.clone()));
            }
        }
        let text = self.ast.name(name);
        if let Some((builtin, sig)) = prelude::method(&mut self.t.types, ty, text) {
            return Some((Callee::Builtin(builtin), sig));
        }
        // A trait's function, for a type that implements the trait.
        let f = self.trait_fn(ty, name)?;
        Some((Callee::Fn(f), self.t.fns[f.index()].sig.clone()))
    }

    /// The function `name` of a trait `ty` implements, if exactly one has it.
    fn trait_fn(&mut self, ty: Ty, name: Symbol) -> Option<FnId> {
        let ast = self.ast;
        let candidates: Vec<FnId> = (0..self.t.traits.len())
            .filter(|&t| self.implements(ty, TraitId(t as u32)))
            .filter_map(|t| {
                self.t.traits[t]
                    .fns
                    .iter()
                    .copied()
                    .find(|&f| ast.function(f).name == name)
            })
            .collect();
        candidates.first().copied()
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
        if let Callee::Fn(f) = callee {
            self.method_visible(f, span);
        }
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
        match callee {
            Callee::Fn(f) => self.method_args(id, f, &sig, base, None, args, span),
            _ => self.check_args(&sig, None, args, span),
        }
    }

    /// The arguments of a call `id` of the method or associated function `f` of `owner`'s
    /// type, and the call's type. For a generic struct's method, `owner`'s type arguments
    /// are the struct's; the method's own are inferred.
    #[allow(clippy::too_many_arguments)]
    fn method_args(
        &mut self,
        id: ExprId,
        f: FnId,
        sig: &Sig,
        owner: Ty,
        receiver: Option<Ty>,
        args: &[ExprId],
        span: Span,
    ) -> Ty {
        let generics = self.t.fns[f.index()].generics.clone();
        if generics.is_empty() {
            return self.check_args(sig, receiver, args, span);
        }
        // The struct's arguments, when `owner` is an instance rather than its definition;
        // for a trait's function, `Self` is `owner`.
        let mut known = vec![None; generics.len()];
        if self.t.fn_traits.contains_key(&f) {
            known[0] = Some(owner);
        } else if let Some(s) = self.struct_of(owner) {
            let def = &self.t.types.structs[s.index()];
            if def.template.is_some() {
                for (slot, &arg) in known.iter_mut().zip(&def.args) {
                    *slot = Some(arg);
                }
            }
        }
        let name = self.name(self.ast.function(f).name).to_string();
        let found = self.infer(&generics, known, |this| {
            this.check_args(sig, receiver, args, span);
        });
        let type_args = self.settle(&generics, found, "call", span);
        self.check_bounds(&generics, &type_args, &name, span);
        let ret = self.t.types.subst(sig.ret, &generics, &type_args);
        let ret = self.resolve_assoc(ret);
        self.t.type_args.insert(id, type_args);
        ret
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
                    let param = self.known(param);
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
        // A literal takes the target type when it fits: `u8(5)`.
        let ty = self.expr(arg, Expect::Hint(target));
        let from = self.t.types.deref(ty);
        if !self.t.types.is_wildcard(from) && !prelude::converts(&self.t.types, from, target) {
            let message = format!(
                "{} cannot be converted to {}",
                self.show(from),
                self.show(target)
            );
            let help = if from.is_float() && target.is_integer() {
                Some("say how to round: `x.trunc()`, `x.round()`, `x.floor()` or `x.ceil()`".into())
            } else if from.is_complex() {
                Some("say which real number: `z.re()`, `z.im()` or `z.abs()`".into())
            } else if from.is_integer() && target.width().is_some() {
                let name = self.t.types.display(target, self.ast);
                Some(format!(
                    "not every value fits; `{name}::try_from(x)` gives a `Result` that says whether it did"
                ))
            } else {
                None
            };
            self.error(span, message).help = help;
        }
        target
    }

    /// `u8::try_from(n)`: any integer to a sized integer, as a `Result` (`spec/types.md` §4).
    fn try_conversion(&mut self, id: ExprId, target: Ty, args: &[ExprId], span: Span) -> Ty {
        self.t.callees[id.index()] = Some(Callee::TryConvert(target));
        let result = self.std_enum(vec![target, Ty::STRING]);
        let name = self.show(target);
        let [arg] = *args else {
            let message = format!("{name}::try_from takes one value to convert");
            self.error(span, message);
            self.skip_args(args);
            return result;
        };
        let ty = self.expr(arg, Expect::None);
        let from = self.t.types.deref(ty);
        if !self.t.types.is_wildcard(from) && !from.is_integer() {
            let message = format!(
                "only integers convert with `try_from`, not {}",
                self.show(from)
            );
            let diagnostic = self.error(span, message);
            if from.is_float() {
                diagnostic.help = Some("round it first, as in `x.round()`".into());
            }
        }
        result
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
            let numeric = ty.is_number();
            let problem = match spec.kind {
                Some(FormatKind::Exponent) if !ty.is_float() => Some("`e` applies to floats"),
                Some(
                    FormatKind::Binary | FormatKind::Octal | FormatKind::Hex | FormatKind::HexUpper,
                ) if !ty.is_integer() => Some("`b`, `o`, `x` and `X` apply to integers"),
                _ if spec.precision.is_some() && !ty.is_float() && !ty.is_complex() => {
                    Some("a precision applies to floats and complex numbers")
                }
                _ if spec.plus && !numeric => Some("`+` applies to numbers"),
                _ if spec.zero && !numeric => Some("`0` padding applies to numbers"),
                _ => None,
            };
            if let Some(problem) = problem {
                let message = format!("this format spec does not apply to {}", self.show(ty));
                self.error(spec.span, message).help = Some(problem.into());
            }
        }
        Ty::STRING
    }

    /// `amplitudes(r)`: lends a qubit or an array of at most 20, and gives its 2^N amplitudes
    /// (`spec/std.md` §2).
    fn amplitudes_fn(&mut self, id: ExprId, args: &[ExprId], span: Span) -> Ty {
        self.t.callees[id.index()] = Some(Callee::Builtin(Builtin::Amplitudes));
        let [arg] = *args else {
            self.error(span, "`amplitudes` takes one qubit or array of qubits");
            self.skip_args(args);
            return Ty::ERROR;
        };
        let found = self.expr(arg, Expect::None);
        let ty = self.t.types.deref(found);
        let qubits = match (ty, self.array_of(ty)) {
            (Ty::QUBIT, _) => 1,
            (_, Some((Ty::QUBIT, len))) => len,
            _ if self.t.types.is_wildcard(ty) => return Ty::ERROR,
            _ => {
                let message = format!(
                    "`amplitudes` takes a qubit or an array of qubits, but this is {}",
                    self.show(ty)
                );
                let span = self.ast.expr(arg).span;
                self.error(span, message);
                return Ty::ERROR;
            }
        };
        if qubits > 20 {
            let span = self.ast.expr(arg).span;
            self.error(
                span,
                "`amplitudes` shows at most 20 qubits: 2^20 amplitudes",
            );
            return Ty::ERROR;
        }
        // Lent, as `describe(e)` lends `e`.
        let lent = self.t.types.reference(false, ty);
        self.coerce(arg, found, lent);
        self.t.types.array(Ty::COMPLEX, 1 << qubits)
    }

    /// `fft(x)` and `ifft(x)`: a `[Complex; N]` or `[c32; N]` to the same (`spec/std.md` §3).
    fn fourier_fn(&mut self, id: ExprId, inverse: bool, args: &[ExprId], span: Span) -> Ty {
        let math = if inverse { Math::Ifft } else { Math::Fft };
        self.t.callees[id.index()] = Some(Callee::Builtin(Builtin::Math(math)));
        let name = if inverse { "ifft" } else { "fft" };
        let [arg] = *args else {
            self.error(span, format!("`{name}` takes one array of complex numbers"));
            self.skip_args(args);
            return Ty::ERROR;
        };
        // `fft([1.0, 0.0])`: literals take the type expected, `Complex` unless said otherwise.
        let hint = match self.ast.expr(arg).kind {
            ExprKind::Array(elems) => {
                let len = self.ast.list(elems).len() as u64;
                Expect::Hint(self.t.types.array(Ty::COMPLEX, len))
            }
            _ => Expect::None,
        };
        let found = self.expr(arg, hint);
        let ty = self.t.types.deref(found);
        match self.array_of(ty) {
            Some((elem, len)) if elem.is_complex() && len > 0 => {
                self.coerce(arg, found, ty);
                ty
            }
            _ if self.t.types.is_wildcard(ty) => Ty::ERROR,
            _ => {
                let message = format!(
                    "`{name}` takes a non-empty `[Complex; N]` or `[c32; N]`, but this is {}",
                    self.show(ty)
                );
                let span = self.ast.expr(arg).span;
                self.error(span, message);
                Ty::ERROR
            }
        }
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
        // `measure(x)` for anything that implements `Measure`: the trait's function.
        if let Some(m) = self.t.types.measure
            && !self.t.types.is_measurable(ty)
            && self.implements(ty, TraitId(m))
        {
            let f = self.t.traits[m as usize].fns[0];
            self.t.callees[id.index()] = Some(Callee::Fn(f));
            self.t.type_args.insert(id, vec![ty]);
            let result = self.t.traits[m as usize].assoc[0].0;
            let ret = self.t.types.intern(TyKind::Assoc {
                base: ty,
                trait_index: m,
                name: result,
                copy: true,
            });
            return self.resolve_assoc(ret);
        }
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
                self.error(span, message).help =
                    Some("a type of your own can measure with an `impl` of `Measure`".into());
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
mod generics;
#[cfg(test)]
mod modules;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod traits;
