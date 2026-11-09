//! Names across modules and packages (`spec/modules.md`).

use noev_syntax::ast::{Ast, ModuleId};
use noev_syntax::parse_module;

use super::*;

/// Checks a package built from `(module, source)` pairs. A module is a path such as `a::b`,
/// or `""` for the root; `dep/` before it puts it in the dependency `dep`, which the main
/// package depends on.
fn errors(files: &[(&str, &str)]) -> Vec<String> {
    let ast = package(files);
    let (_, diagnostics) = check(&ast);
    diagnostics.into_iter().map(|d| d.message).collect()
}

/// The diagnostics of types and then ownership, each as `code message`, or `warning message`.
fn ownership(files: &[(&str, &str)]) -> Vec<String> {
    let ast = package(files);
    let (typed, diagnostics) = check(&ast);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    crate::check_ownership(&ast, &typed)
        .into_iter()
        .map(|d| match (d.severity, d.code) {
            (noev_syntax::Severity::Warning, _) => format!("warning {}", d.message),
            (_, Some(code)) => format!("{code} {}", d.message),
            (_, None) => d.message,
        })
        .collect()
}

fn package(files: &[(&str, &str)]) -> Ast {
    let mut ast = Ast::new();
    let main = ast.add_package("main");
    let mut modules: Vec<(String, ModuleId)> = vec![(String::new(), ast.package(main).root)];
    let mut base = 0;
    let mut diagnostics = Vec::new();
    for &(path, source) in files {
        let (package, path) = match path.split_once('/') {
            Some((dep, path)) => (Some(dep), path),
            None => (None, path),
        };
        let key = |p: &str| match package {
            Some(dep) => format!("{dep}/{p}"),
            None => p.to_string(),
        };
        let mut module = match modules.iter().find(|(k, _)| *k == key("")) {
            Some(&(_, m)) => m,
            None => {
                let dep = ast.add_package(package.expect("a dependency"));
                ast.add_dependency(main, package.unwrap(), dep);
                let root = ast.package(dep).root;
                modules.push((key(""), root));
                root
            }
        };
        let mut so_far = String::new();
        for segment in path.split("::").filter(|s| !s.is_empty()) {
            if !so_far.is_empty() {
                so_far.push_str("::");
            }
            so_far.push_str(segment);
            module = match modules.iter().find(|(k, _)| *k == key(&so_far)) {
                Some(&(_, m)) => m,
                None => {
                    let m = ast.add_module(module, segment);
                    modules.push((key(&so_far), m));
                    m
                }
            };
        }
        diagnostics.extend(parse_module(&mut ast, module, source, base));
        base += source.len() as u32 + 1;
    }
    ast.collect_root();
    assert!(diagnostics.is_empty(), "syntax errors: {diagnostics:?}");
    ast
}

fn ok(files: &[(&str, &str)]) {
    let errs = errors(files);
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn items_are_imported_and_found_by_path() {
    ok(&[
        (
            "",
            "use ansatz::ry_layer\nuse ansatz::Layer as L\n\
             fn main() { let q = |0>\n ry_layer(q, 0.5)\n let l = L { depth: 1 }\n \
             let d = pkg::ansatz::depth(l)\n discard(mv q) }",
        ),
        (
            "ansatz",
            "pub struct Layer { pub depth: Int }\n\
             pub fn ry_layer(q: mut Qubit, t: Float) { q.ry(t) }\n\
             pub fn depth(l: Layer) -> Int { l.depth }",
        ),
    ]);
}

#[test]
fn modules_do_not_see_their_parents_names() {
    assert_eq!(
        errors(&[
            ("", "fn helper() {}\nfn main() { a::f() }"),
            ("a", "pub fn f() { helper() }"),
        ]),
        ["cannot find the function `helper`"]
    );
    ok(&[
        ("", "pub(pkg) fn helper() {}\nfn main() { a::f() }"),
        ("a", "pub fn f() { super::helper() }"),
    ]);
}

#[test]
fn private_items_are_reported_where_used() {
    assert_eq!(
        errors(&[("", "fn main() { a::f() }"), ("a", "fn f() {}")]),
        ["`f` is private"]
    );
    assert_eq!(
        errors(&[("", "use a::f\nfn main() { f() }"), ("a", "fn f() {}")]),
        ["`f` is private"]
    );
    // A private item is visible in the modules inside its own.
    ok(&[
        ("", "fn main() {}"),
        ("a", "fn f() {}"),
        ("a::b", "fn g() { super::f() }"),
    ]);
}

#[test]
fn visibility_levels() {
    // `pub(super)`: the parent and the modules inside it.
    ok(&[
        ("", "fn main() {}"),
        ("a", "pub fn g() { b::f() }"),
        ("a::b", "pub(super) fn f() {}"),
        ("a::c", "pub fn h() { super::b::f() }"),
    ]);
    assert_eq!(
        errors(&[
            ("", "fn main() {}"),
            ("a", "pub fn g() { b::f() }"),
            ("a::b", "pub(super) fn f() {}"),
            ("c", "pub fn h() { pkg::a::b::f() }"),
        ]),
        ["`f` is private"]
    );
    // `pub(in path)` must name a module that contains this one.
    ok(&[
        ("", "fn main() {}"),
        ("a", "pub fn g() { b::c::f() }"),
        ("a::b::c", "pub(in pkg::a) fn f() {}"),
    ]);
    assert_eq!(
        errors(&[
            ("", "fn main() {}"),
            ("a", "pub fn g() {}"),
            ("b", "pub(in pkg::a) fn f() {}"),
        ]),
        ["`pub(in …)` must name a module that contains this one"]
    );
    // `pub(pkg)` is visible across the package, but not to its dependents.
    assert_eq!(
        errors(&[
            ("", "fn main() { qlib::shared()\n qlib::inner() }"),
            ("qlib/", "pub fn shared() {}\npub(pkg) fn inner() {}"),
        ]),
        ["`inner` is private"]
    );
}

#[test]
fn imports_resolve_in_any_order() {
    // `b` imports what `c` re-exports from `a`, declared after it.
    ok(&[
        ("", "use b::f\nfn main() { f() }"),
        ("b", "pub use pkg::c::f"),
        ("c", "pub use pkg::a::f"),
        ("a", "pub fn f() {}"),
    ]);
}

#[test]
fn glob_imports() {
    ok(&[
        ("", "use a::*\nfn main() { f()\n let s = S { x: 1 } }"),
        (
            "a",
            "pub fn f() {}\npub struct S { pub x: Int }\nfn hidden() {}",
        ),
    ]);
    // A glob does not import private items.
    assert_eq!(
        errors(&[
            ("", "use a::*\nfn main() { hidden() }"),
            ("a", "fn hidden() {}"),
        ]),
        ["cannot find the function `hidden`"]
    );
    // The module's own names and explicit imports take precedence over globs.
    ok(&[
        (
            "",
            "use a::*\nuse b::f\nfn main() { f()\n g(1) }\nfn g(x: Int) {}",
        ),
        ("a", "pub fn f(x: Int) {}\npub fn g() {}"),
        ("b", "pub fn f() {}"),
    ]);
    // Two globs bringing different items under one name are an error only where it is
    // used.
    ok(&[
        ("", "use a::*\nuse b::*\nfn main() { g() }"),
        ("a", "pub fn f() {}\npub fn g() {}"),
        ("b", "pub fn f() {}"),
    ]);
    assert_eq!(
        errors(&[
            ("", "use a::*\nuse b::*\nfn main() { f() }"),
            ("a", "pub fn f() {}"),
            ("b", "pub fn f() {}"),
        ]),
        ["`f` is ambiguous: glob imports bring two different items with that name"]
    );
    // The same item through two globs is not ambiguous; globs that import each other end.
    ok(&[
        ("", "use a::*\nuse b::*\nfn main() { f() }"),
        ("a", "pub use pkg::b::*\npub fn f() {}"),
        ("b", "pub use pkg::a::*"),
    ]);
}

#[test]
fn use_trees() {
    ok(&[
        (
            "",
            "use a::{self, f as first, g}\nfn main() { first()\n g()\n a::f() }",
        ),
        ("a", "pub fn f() {}\npub fn g() {}"),
    ]);
}

#[test]
fn unresolved_imports() {
    assert_eq!(
        errors(&[("", "use a::missing\nfn main() {}"), ("a", "")]),
        ["cannot find `missing` in `a`"]
    );
    assert_eq!(
        errors(&[("", "use nowhere::x\nfn main() {}")]),
        ["cannot find the module `nowhere`"]
    );
    assert_eq!(
        errors(&[("", "use super::x\nfn main() {}")]),
        ["`super` in a package's root module, which has no parent"]
    );
    assert_eq!(
        errors(&[("", "use a::f::g\nfn main() {}"), ("a", "pub fn f() {}")]),
        ["`a::f` is not a module"]
    );
}

#[test]
fn reexports_are_no_wider_than_the_item() {
    assert_eq!(
        errors(&[
            ("", "fn main() {}"),
            ("a", "pub(pkg) fn f() {}"),
            ("b", "pub use pkg::a::f"),
        ]),
        ["`f` cannot be re-exported more widely than it is visible"]
    );
}

#[test]
fn names_defined_twice() {
    assert_eq!(
        errors(&[("", "fn f() {}\nfn f() {}\nfn main() {}")]),
        ["the function `f` is defined twice"]
    );
    assert_eq!(
        errors(&[
            ("", "use a::f\nfn f() {}\nfn main() {}"),
            ("a", "pub fn f() {}")
        ]),
        ["the name `f` is defined twice"]
    );
    assert_eq!(
        errors(&[("", "struct a {}\nfn main() {}"), ("a", "")]),
        ["the type `a` is defined twice"]
    );
    // A function and a type can share a name: they are in different namespaces.
    ok(&[("", "struct f {}\nfn f() {}\nfn main() {}")]);
}

#[test]
fn dependencies() {
    ok(&[
        ("", "use qlib::qft::qft\nfn main() { qft()\n qlib::flip() }"),
        ("qlib/", "pub fn flip() {}"),
        ("qlib/qft", "pub fn qft() {}"),
    ]);
}

#[test]
fn types_and_variants_by_path() {
    ok(&[
        (
            "",
            "fn main() { let o = a::Outcome::Hit\n let p: a::Point = a::Point { x: 1 }\n \
             match o { a::Outcome::Hit => {}, a::Outcome::Miss => {} } }",
        ),
        (
            "a",
            "pub enum Outcome { Hit, Miss }\npub struct Point { pub x: Int }",
        ),
    ]);
}

#[test]
fn main_is_the_root_modules() {
    // A `main` in another module is an ordinary function.
    ok(&[
        ("", "fn main() {}"),
        ("a", "pub fn main(x: Int) -> Int { x }"),
    ]);
}

#[test]
fn fields_and_methods_are_private_to_their_module() {
    let a = "pub struct Register {\n pub size: Int\n count: Int\n \
             pub fn new() -> Self { Register { size: 1, count: 0 } }\n \
             fn secret(ref self) -> Int { self.count }\n \
             pub fn total(ref self) -> Int { self.secret() + self.size }\n}";
    ok(&[
        (
            "",
            "fn main() { let r = a::Register::new()\n let t = r.total() + r.size }",
        ),
        ("a", a),
    ]);
    assert_eq!(
        errors(&[
            (
                "",
                "fn main() { let r = a::Register::new()\n let c = r.count }"
            ),
            ("a", a)
        ]),
        ["the field `count` of `Register` is private"]
    );
    assert_eq!(
        errors(&[
            (
                "",
                "fn main() { let r = a::Register { size: 1, count: 2 } }"
            ),
            ("a", a)
        ]),
        ["the field `count` of `Register` is private"]
    );
    assert_eq!(
        errors(&[
            (
                "",
                "fn main() { let r = a::Register::new()\n let s = r.secret() }"
            ),
            ("a", a)
        ]),
        ["`secret` is private"]
    );
    // Inside the module, and the modules in it, everything is visible.
    ok(&[
        ("", "fn main() {}"),
        ("a", a),
        (
            "a::b",
            "fn f() { let r = super::Register { size: 1, count: 2 }\n let s = r.secret() }",
        ),
    ]);
}

#[test]
fn reversible_across_modules_only_if_declared() {
    // Inside its module, `flip` is inferred reversible.
    assert_eq!(
        ownership(&[
            ("", "fn main() {}"),
            (
                "a",
                "fn flip(q: mut Qubit) { q.x() }\nrev fn twice(q: mut Qubit) { flip(q)\n adj flip(q) }"
            ),
        ]),
        Vec::<String>::new()
    );
    // From another module, only `rev fn` is.
    assert_eq!(
        ownership(&[
            (
                "",
                "rev fn twice(q: mut Qubit) { a::flip(q) }\nfn undo(q: mut Qubit) { adj a::flip(q) }\nfn main() {}"
            ),
            ("a", "pub fn flip(q: mut Qubit) { q.x() }"),
        ]),
        [
            "N013 `flip` is not reversible",
            "N014 only reversible functions can be inverted with `adj`",
            "warning `flip` is reversible, but other modules can use it as reversible only if it is declared `rev fn`",
        ]
    );
    assert_eq!(
        ownership(&[
            (
                "",
                "rev fn twice(q: mut Qubit) { a::flip(q)\n adj a::flip(q) }\nfn main() {}"
            ),
            ("a", "pub rev fn flip(q: mut Qubit) { q.x() }"),
        ]),
        Vec::<String>::new()
    );
    // Inference through a function in another module stops there too.
    assert_eq!(
        ownership(&[
            (
                "",
                "rev fn r(q: mut Qubit) { wrap(q) }\nfn wrap(q: mut Qubit) { a::plain(q) }\nfn main() {}"
            ),
            ("a", "pub fn plain(q: mut Qubit) { q.x() }"),
        ]),
        [
            "N013 `wrap` is not reversible",
            "warning `plain` is reversible, but other modules can use it as reversible only if it is declared `rev fn`",
        ]
    );
}
