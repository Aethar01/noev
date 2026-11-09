//! Module scopes: what each name means in each module (`spec/modules.md` §2–§4).
//!
//! Every module has its own scope, in two namespaces as in Rust: types (structs, enums,
//! aliases and modules) and values (functions). A name is looked up, in order, among the
//! module's own items and explicit imports, then its glob imports, then the packages it
//! depends on. The prelude comes last; the type checker handles it, since it holds the
//! built-in names. A module does not see its parent's names: it imports them with `super`.
//!
//! Imports are resolved together, repeating until nothing changes, since one import can
//! depend on another, in any module and in any order.

use noev_syntax::ast::*;
use noev_syntax::{Diagnostic, FxHashMap, Span, Symbol};

/// What a name in a scope refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A function.
    Fn(FnId),
    /// A struct, enum or type alias.
    Item(ItemId),
    /// A module.
    Module(ModuleId),
}

/// Where a name can be used from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visible {
    /// Every module of every package.
    Everywhere,
    /// Every module of one package.
    Package(PackageId),
    /// One module and the modules inside it.
    Within(ModuleId),
}

/// The two namespaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ns {
    /// Structs, enums, aliases and modules.
    Type,
    /// Functions.
    Value,
}

/// A name in a scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What it refers to.
    pub binding: Binding,
    /// Where it can be used from.
    pub visible: Visible,
    /// Where it is declared or imported.
    pub span: Span,
}

/// A name brought in by glob imports: one entry, or several different ones, which is an
/// error only where the name is used.
#[derive(Clone, Copy, Debug)]
enum Glob {
    One(Entry),
    Ambiguous(Entry, Entry),
}

/// What looking up a name found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// The name refers to this.
    Entry(Entry),
    /// Two glob imports bring different items under this name.
    Ambiguous(Span, Span),
    /// The name exists there, but is not visible here.
    Private(Entry),
}

#[derive(Default)]
struct Scope {
    names: [FxHashMap<Symbol, Entry>; 2],
    globs: [FxHashMap<Symbol, Glob>; 2],
}

/// Every module's scope.
pub struct Scopes<'a> {
    ast: &'a Ast,
    scopes: Vec<Scope>,
    /// The module each item is in, by [`ItemId::index`].
    item_modules: Vec<ModuleId>,
    /// The module each function is in, methods included, by [`FnId::index`].
    fn_modules: Vec<ModuleId>,
}

fn ns_index(ns: Ns) -> usize {
    match ns {
        Ns::Type => 0,
        Ns::Value => 1,
    }
}

impl Scopes<'_> {
    fn package(&self, module: ModuleId) -> PackageId {
        self.ast.module(module).package
    }

    /// The module `item` is in.
    pub fn item_module(&self, item: ItemId) -> ModuleId {
        self.item_modules[item.index()]
    }

    /// Every function's module, by [`FnId::index`], taken out of the scopes.
    pub fn take_fn_modules(&mut self) -> Vec<ModuleId> {
        std::mem::take(&mut self.fn_modules)
    }

    /// The root module of `module`'s package.
    pub fn root_of(&self, module: ModuleId) -> ModuleId {
        self.ast.package(self.package(module)).root
    }

    /// The module `module` is in, if it is not a root.
    pub fn parent(&self, module: ModuleId) -> Option<ModuleId> {
        self.ast.module(module).parent
    }

    /// Whether `inner` is `outer` or inside it.
    pub fn is_within(&self, inner: ModuleId, outer: ModuleId) -> bool {
        let mut module = Some(inner);
        while let Some(m) = module {
            if m == outer {
                return true;
            }
            module = self.ast.module(m).parent;
        }
        false
    }

    /// Whether something visible as `visible` can be used from `from`.
    pub fn can_see(&self, from: ModuleId, visible: Visible) -> bool {
        match visible {
            Visible::Everywhere => true,
            Visible::Package(p) => self.package(from) == p,
            Visible::Within(m) => self.is_within(from, m),
        }
    }

    /// Where something marked `vis` in `module` can be used from. `Err` with the path's span
    /// if `pub(in path)` does not name a module containing this one; it is then private.
    pub fn visibility(
        &self,
        words: PathWords,
        module: ModuleId,
        vis: Vis,
    ) -> Result<Visible, (Span, String)> {
        Ok(match vis {
            Vis::Private => Visible::Within(module),
            Vis::Pub => Visible::Everywhere,
            Vis::Pkg => Visible::Package(self.package(module)),
            // Rust allows `pub(super)` only where there is a parent.
            Vis::Super => Visible::Within(self.parent(module).unwrap_or(module)),
            Vis::In { path, span } => {
                let segments = self.ast.list(path);
                match self.module_path(words, module, segments) {
                    Ok(target) if self.is_within(module, target) => Visible::Within(target),
                    Ok(_) => {
                        let message = "`pub(in …)` must name a module that contains this one";
                        return Err((span, message.into()));
                    }
                    Err(error) => return Err((span, path_message(self.ast, segments, &error))),
                }
            }
        })
    }

    /// Whether everywhere `inner` reaches, `outer` does too.
    fn covers(&self, outer: Visible, inner: Visible) -> bool {
        match (outer, inner) {
            (Visible::Everywhere, _) => true,
            (_, Visible::Everywhere) => false,
            (Visible::Package(p), Visible::Package(q)) => p == q,
            (Visible::Package(p), Visible::Within(m)) => self.package(m) == p,
            (Visible::Within(_), Visible::Package(_)) => false,
            (Visible::Within(m), Visible::Within(n)) => self.is_within(n, m),
        }
    }

    /// `name` as written in `module`: its items and imports, then its glob imports, then the
    /// packages it depends on. `None` if none has it: then it may be in the prelude.
    pub fn lexical(&self, module: ModuleId, name: Symbol, ns: Ns) -> Option<Found> {
        let scope = &self.scopes[module.index()];
        let n = ns_index(ns);
        if let Some(&entry) = scope.names[n].get(&name) {
            return Some(Found::Entry(entry));
        }
        // Most modules have no glob imports and most packages no dependencies, and most
        // names looked up are built-ins found in none of these.
        if !scope.globs[n].is_empty()
            && let Some(&glob) = scope.globs[n].get(&name)
        {
            return Some(match glob {
                Glob::One(entry) => Found::Entry(entry),
                Glob::Ambiguous(a, b) => Found::Ambiguous(a.span, b.span),
            });
        }
        if ns == Ns::Type {
            let deps = &self.ast.package(self.package(module)).deps;
            if let Some(&(_, dep)) = deps.iter().find(|(dep, _)| *dep == name) {
                let root = self.ast.package(dep).root;
                return Some(Found::Entry(Entry {
                    binding: Binding::Module(root),
                    visible: Visible::Everywhere,
                    span: Span::default(),
                }));
            }
        }
        None
    }

    /// `name` in the module `target`, as seen from `from`: through a path such as `a::name`.
    pub fn qualified(
        &self,
        from: ModuleId,
        target: ModuleId,
        name: Symbol,
        ns: Ns,
    ) -> Option<Found> {
        let scope = &self.scopes[target.index()];
        let n = ns_index(ns);
        let found = match scope.names[n].get(&name) {
            Some(&entry) => entry,
            None => match scope.globs[n].get(&name)? {
                Glob::One(entry) => *entry,
                Glob::Ambiguous(a, b) => return Some(Found::Ambiguous(a.span, b.span)),
            },
        };
        Some(if self.can_see(from, found.visible) {
            Found::Entry(found)
        } else {
            Found::Private(found)
        })
    }

    /// Every name visible from `from` in `target`'s scope, for glob imports.
    fn visible_names(&self, from: ModuleId, target: ModuleId) -> Vec<(usize, Symbol, Entry)> {
        let scope = &self.scopes[target.index()];
        let mut names = Vec::new();
        for n in 0..2 {
            for (&name, &entry) in &scope.names[n] {
                if self.can_see(from, entry.visible) {
                    names.push((n, name, entry));
                }
            }
            for (&name, glob) in &scope.globs[n] {
                if let Glob::One(entry) = *glob
                    && self.can_see(from, entry.visible)
                    && !scope.names[n].contains_key(&name)
                {
                    names.push((n, name, entry));
                }
            }
        }
        names
    }
}

/// Why a module path could not be resolved.
pub enum PathError {
    /// Nothing is found yet; imports still being resolved may provide it.
    NotFound {
        /// The segment that was not found.
        at: usize,
    },
    /// A segment is not a module.
    NotModule {
        /// The segment.
        at: usize,
    },
    /// A segment is a module that is not visible here.
    Private {
        /// The segment.
        at: usize,
    },
    /// `super` in a root module.
    NoParent {
        /// The segment.
        at: usize,
    },
    /// Two glob imports bring different modules under one name.
    Ambiguous {
        /// The segment.
        at: usize,
    },
}

/// What is wrong with a module path that could not be resolved, for a message.
pub fn path_message(ast: &Ast, path: &[Symbol], error: &PathError) -> String {
    let text = |end: usize| {
        let names: Vec<&str> = path[..=end].iter().map(|&s| ast.name(s)).collect();
        names.join("::")
    };
    match *error {
        PathError::NotFound { at } => format!("cannot find the module `{}`", text(at)),
        PathError::NotModule { at } => format!("`{}` is not a module", text(at)),
        PathError::Private { at } => format!("the module `{}` is private", text(at)),
        PathError::NoParent { .. } => {
            "`super` in a package's root module, which has no parent".to_string()
        }
        PathError::Ambiguous { at } => format!(
            "`{}` is ambiguous: glob imports bring two different items with that name",
            text(at)
        ),
    }
}

/// The words a path can start with, as symbols, if the program interned them.
#[derive(Clone, Copy)]
pub struct PathWords {
    pkg: Option<Symbol>,
    sup: Option<Symbol>,
    this: Option<Symbol>,
}

impl PathWords {
    /// Looks the words up in `ast`'s symbols.
    pub fn new(ast: &Ast) -> Self {
        PathWords {
            pkg: ast.symbols.get("pkg"),
            sup: ast.symbols.get("super"),
            this: ast.symbols.get("self"),
        }
    }

    /// Whether `name` is `super`.
    pub fn is_super(self, name: Symbol) -> bool {
        Some(name) == self.sup
    }
}

impl Scopes<'_> {
    /// The module the first segment of a path names, written in `from`: `pkg`, `super`,
    /// `self`, or a module in scope. `None` if it names none.
    pub fn path_start(
        &self,
        words: PathWords,
        from: ModuleId,
        first: Symbol,
    ) -> Option<Result<ModuleId, PathError>> {
        if Some(first) == words.pkg {
            return Some(Ok(self.root_of(from)));
        }
        if Some(first) == words.sup {
            return Some(self.parent(from).ok_or(PathError::NoParent { at: 0 }));
        }
        if Some(first) == words.this {
            return Some(Ok(from));
        }
        match self.lexical(from, first, Ns::Type)? {
            Found::Entry(Entry {
                binding: Binding::Module(m),
                ..
            }) => Some(Ok(m)),
            Found::Ambiguous(..) => Some(Err(PathError::Ambiguous { at: 0 })),
            _ => None,
        }
    }

    /// The module `path` names, written in `from`.
    pub fn module_path(
        &self,
        words: PathWords,
        from: ModuleId,
        path: &[Symbol],
    ) -> Result<ModuleId, PathError> {
        let mut module = match self.path_start(words, from, path[0]) {
            Some(start) => start?,
            None if self.lexical(from, path[0], Ns::Type).is_some()
                || self.lexical(from, path[0], Ns::Value).is_some() =>
            {
                return Err(PathError::NotModule { at: 0 });
            }
            None => return Err(PathError::NotFound { at: 0 }),
        };
        for (at, &segment) in path.iter().enumerate().skip(1) {
            if Some(segment) == words.sup {
                module = self.parent(module).ok_or(PathError::NoParent { at })?;
                continue;
            }
            module = match self.qualified(from, module, segment, Ns::Type) {
                Some(Found::Entry(Entry {
                    binding: Binding::Module(m),
                    ..
                })) => m,
                Some(Found::Entry(_)) => return Err(PathError::NotModule { at }),
                Some(Found::Private(_)) => return Err(PathError::Private { at }),
                Some(Found::Ambiguous(..)) => return Err(PathError::Ambiguous { at }),
                None if self.qualified(from, module, segment, Ns::Value).is_some() => {
                    return Err(PathError::NotModule { at });
                }
                None => return Err(PathError::NotFound { at }),
            };
        }
        Ok(module)
    }
}

#[derive(Clone, Copy)]
struct Pending {
    module: ModuleId,
    import: Import,
    visible: Visible,
    done: bool,
    /// Whether it is being resolved, further up the stack: an import that leads back to
    /// itself waits for the next round.
    busy: bool,
}

/// Builds every module's scope, reporting names defined twice and imports that cannot be
/// resolved.
pub fn resolve(ast: &Ast) -> (Scopes<'_>, Vec<Diagnostic>) {
    let modules = ast.modules();
    let mut scopes = Scopes {
        ast,
        scopes: modules.iter().map(|_| Scope::default()).collect(),
        item_modules: vec![ModuleId::from_index(0); ast.all_items().len()],
        fn_modules: vec![ModuleId::from_index(0); ast.all_fns().len()],
    };
    let mut resolver = Resolver {
        ast,
        scopes,
        words: PathWords::new(ast),
        diagnostics: Vec::new(),
        pending: Vec::new(),
        by_name: FxHashMap::default(),
    };
    resolver.define();
    resolver.imports();
    scopes = resolver.scopes;
    (scopes, resolver.diagnostics)
}

struct Resolver<'a> {
    ast: &'a Ast,
    scopes: Scopes<'a>,
    words: PathWords,
    diagnostics: Vec<Diagnostic>,
    /// Every import, glob or not.
    pending: Vec<Pending>,
    /// The explicit imports that bind each name in each module, by index into `pending`.
    by_name: FxHashMap<(ModuleId, Symbol), Vec<usize>>,
}

impl Resolver<'_> {
    /// Resolves a visibility written in `module`, reporting a `pub(in …)` that names no module
    /// containing it.
    fn visible(&mut self, module: ModuleId, vis: Vis) -> Visible {
        self.scopes
            .visibility(self.words, module, vis)
            .unwrap_or_else(|(span, message)| {
                self.diagnostics.push(Diagnostic::error(span, message));
                Visible::Within(module)
            })
    }

    fn path_text(&self, segments: &[Symbol]) -> String {
        let names: Vec<&str> = segments.iter().map(|&s| self.ast.name(s)).collect();
        names.join("::")
    }

    /// Adds `entry` to `module`'s scope as `name`, unless the name is already there.
    fn insert(&mut self, module: ModuleId, ns: Ns, name: Symbol, entry: Entry, what: &str) {
        let names = &mut self.scopes.scopes[module.index()].names[ns_index(ns)];
        match names.get(&name) {
            Some(earlier) if earlier.binding != entry.binding => {
                let text = self.ast.name(name);
                let message = format!("{what} `{text}` is defined twice");
                let earlier = earlier.span;
                let mut diagnostic = Diagnostic::error(entry.span, message);
                if earlier != Span::default() {
                    diagnostic = diagnostic.with_label(earlier, "first defined here");
                }
                self.diagnostics.push(diagnostic);
            }
            Some(_) => {}
            None => {
                names.insert(name, entry);
            }
        }
    }

    /// Puts every item and module into its scope, and collects the imports.
    fn define(&mut self) {
        let ast = self.ast;
        // Modules first, as names in their parents, so items' `pub(in …)` paths resolve.
        for (index, module) in ast.modules().iter().enumerate() {
            if let Some(parent) = module.parent {
                let entry = Entry {
                    binding: Binding::Module(ModuleId::from_index(index)),
                    visible: Visible::Everywhere,
                    span: Span::default(),
                };
                self.insert(parent, Ns::Type, module.name, entry, "the module");
            }
        }
        for (index, module) in ast.modules().iter().enumerate() {
            let id = ModuleId::from_index(index);
            for &item in ast.list(module.items) {
                self.scopes.item_modules[item.index()] = id;
                let node = *ast.item(item);
                let visible = self.visible(id, node.vis);
                match node.kind {
                    ItemKind::Fn(f) => {
                        self.scopes.fn_modules[f.index()] = id;
                        let decl = ast.function(f);
                        let entry = Entry {
                            binding: Binding::Fn(f),
                            visible,
                            span: decl.name_span,
                        };
                        self.insert(id, Ns::Value, decl.name, entry, "the function");
                    }
                    ItemKind::Struct {
                        name,
                        name_span,
                        methods,
                        ..
                    }
                    | ItemKind::Enum {
                        name,
                        name_span,
                        methods,
                        ..
                    } => {
                        for &method in ast.list(methods) {
                            self.scopes.fn_modules[method.index()] = id;
                        }
                        let entry = Entry {
                            binding: Binding::Item(item),
                            visible,
                            span: name_span,
                        };
                        self.insert(id, Ns::Type, name, entry, "the type");
                    }
                    ItemKind::Type {
                        name, name_span, ..
                    } => {
                        let entry = Entry {
                            binding: Binding::Item(item),
                            visible,
                            span: name_span,
                        };
                        self.insert(id, Ns::Type, name, entry, "the type");
                    }
                    ItemKind::Trait { fns, .. } | ItemKind::Impl { fns, .. } => {
                        for &f in ast.list(fns) {
                            self.scopes.fn_modules[f.index()] = id;
                        }
                    }
                    ItemKind::Use(imports) => {
                        for &import in ast.list(imports) {
                            if !import.glob
                                && let Some(name) = self.import_name(&import)
                            {
                                let index = self.pending.len();
                                self.by_name.entry((id, name)).or_default().push(index);
                            }
                            self.pending.push(Pending {
                                module: id,
                                import,
                                visible,
                                done: false,
                                busy: false,
                            });
                        }
                    }
                    ItemKind::Error => {}
                }
            }
        }
    }

    /// Resolves the imports, repeating until nothing changes, then reports those left.
    fn imports(&mut self) {
        loop {
            let mut changed = false;
            for index in 0..self.pending.len() {
                if self.pending[index].import.glob {
                    changed |= self.glob(index);
                } else if !self.pending[index].done {
                    changed |= self.try_import(index);
                }
            }
            if !changed {
                break;
            }
        }
        for index in 0..self.pending.len() {
            let p = self.pending[index];
            if !p.done {
                self.report(&p);
            }
        }
    }

    /// Tries to resolve the explicit import `index`, unless it is done or already being
    /// resolved. Returns whether it was resolved now.
    fn try_import(&mut self, index: usize) -> bool {
        let p = self.pending[index];
        if p.done || p.busy {
            return false;
        }
        self.pending[index].busy = true;
        let resolved = self.import(&p);
        self.pending[index].busy = false;
        self.pending[index].done = resolved;
        resolved
    }

    /// Resolves the imports in `module` that bind `name`, so a chain of re-exports resolves
    /// in one go, whatever order its modules are in. Returns whether any was resolved.
    fn import_into(&mut self, module: ModuleId, name: Symbol) -> bool {
        let Some(indices) = self.by_name.get(&(module, name)) else {
            return false;
        };
        let mut resolved = false;
        for index in indices.clone() {
            resolved |= self.try_import(index);
        }
        resolved
    }

    /// The name an import binds, or `None` for `as _`.
    fn import_name(&self, import: &Import) -> Option<Symbol> {
        let name = import
            .alias
            .unwrap_or_else(|| *self.ast.list(import.path).last().expect("a path"));
        (self.ast.name(name) != "_").then_some(name)
    }

    /// Tries to resolve an explicit import, adding what it names to its module's scope.
    fn import(&mut self, p: &Pending) -> bool {
        let path = self.ast.list(p.import.path);
        let (last, prefix) = path.split_last().expect("a path");
        // `use a::{self}`, or a path that is only a module, such as `use super::helpers`
        // where `helpers` is a module: both name the module.
        let mut found = Vec::new();
        if prefix.is_empty() {
            match self.scopes.path_start(self.words, p.module, *last) {
                Some(Ok(m)) if self.is_word(*last) => {
                    found.push((
                        Ns::Type,
                        Entry {
                            binding: Binding::Module(m),
                            visible: Visible::Everywhere,
                            span: p.import.span,
                        },
                    ));
                }
                _ => {
                    for ns in [Ns::Type, Ns::Value] {
                        if let Some(Found::Entry(entry)) = self.scopes.lexical(p.module, *last, ns)
                        {
                            found.push((ns, entry));
                        }
                    }
                }
            }
        } else {
            let Ok(target) = self.scopes.module_path(self.words, p.module, prefix) else {
                return false;
            };
            if Some(*last) == self.words.this {
                found.push((
                    Ns::Type,
                    Entry {
                        binding: Binding::Module(target),
                        visible: Visible::Everywhere,
                        span: p.import.span,
                    },
                ));
            } else {
                // The name may come from an import of the target's that is not resolved yet.
                let mut private = false;
                if [Ns::Type, Ns::Value]
                    .iter()
                    .all(|&ns| self.scopes.qualified(p.module, target, *last, ns).is_none())
                {
                    self.import_into(target, *last);
                }
                for ns in [Ns::Type, Ns::Value] {
                    match self.scopes.qualified(p.module, target, *last, ns) {
                        Some(Found::Entry(entry)) => found.push((ns, entry)),
                        // Reported, then imported anyway, so its uses are not reported too.
                        Some(Found::Private(entry)) => {
                            private = true;
                            found.push((
                                ns,
                                Entry {
                                    visible: p.visible,
                                    ..entry
                                },
                            ));
                        }
                        _ => {}
                    }
                }
                if private {
                    let text = self.ast.name(*last);
                    let message = format!("`{text}` is private");
                    self.diagnostics
                        .push(Diagnostic::error(p.import.span, message));
                }
            }
        }
        if found.is_empty() {
            return false;
        }
        let Some(name) = self.import_name(&p.import) else {
            return true;
        };
        for (ns, entry) in found {
            if !self.scopes.covers(entry.visible, p.visible) {
                let text = self.ast.name(name);
                let message =
                    format!("`{text}` cannot be re-exported more widely than it is visible");
                self.diagnostics
                    .push(Diagnostic::error(p.import.span, message));
                return true;
            }
            let imported = Entry {
                visible: p.visible,
                span: p.import.span,
                ..entry
            };
            self.insert(p.module, ns, name, imported, "the name");
        }
        true
    }

    fn is_word(&self, name: Symbol) -> bool {
        Some(name) == self.words.pkg
            || Some(name) == self.words.sup
            || Some(name) == self.words.this
    }

    /// Copies the names a glob import brings into its module's scope. Returns whether it
    /// added any.
    fn glob(&mut self, index: usize) -> bool {
        let p = self.pending[index];
        let path = self.ast.list(p.import.path);
        let Ok(target) = self.scopes.module_path(self.words, p.module, path) else {
            return false;
        };
        self.pending[index].done = true;
        let mut changed = false;
        for (n, name, entry) in self.scopes.visible_names(p.module, target) {
            // A glob re-exports a name only as widely as the name is visible.
            let visible = if self.scopes.covers(entry.visible, p.visible) {
                p.visible
            } else {
                entry.visible
            };
            let imported = Entry {
                visible,
                span: p.import.span,
                ..entry
            };
            let globs = &mut self.scopes.scopes[p.module.index()].globs[n];
            match globs.get(&name) {
                None => {
                    globs.insert(name, Glob::One(imported));
                    changed = true;
                }
                Some(Glob::One(earlier)) if earlier.binding != imported.binding => {
                    globs.insert(name, Glob::Ambiguous(*earlier, imported));
                    changed = true;
                }
                Some(_) => {}
            }
        }
        changed
    }

    /// Reports an import that could not be resolved.
    fn report(&mut self, p: &Pending) {
        let path = self.ast.list(p.import.path);
        let span = p.import.span;
        let check = if p.import.glob {
            path
        } else {
            &path[..path.len() - 1]
        };
        let message = if check.is_empty() {
            format!("cannot find `{}` in this module", self.path_text(path))
        } else {
            match self.scopes.module_path(self.words, p.module, check) {
                Ok(_) => {
                    let (last, prefix) = path.split_last().expect("a path");
                    format!(
                        "cannot find `{}` in `{}`",
                        self.ast.name(*last),
                        self.path_text(prefix)
                    )
                }
                Err(error) => path_message(self.ast, check, &error),
            }
        };
        self.diagnostics.push(Diagnostic::error(span, message));
    }
}
