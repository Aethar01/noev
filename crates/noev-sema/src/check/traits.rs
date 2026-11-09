//! Traits and `impl`s (`spec/types.md` §6).

use noev_syntax::parse;

use super::*;

fn errors(source: &str) -> Vec<String> {
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (_, diagnostics) = check(&parsed.ast);
    diagnostics.into_iter().map(|d| d.message).collect()
}

fn ok(source: &str) {
    let errs = errors(source);
    assert!(errs.is_empty(), "{errs:?}");
}

/// The ownership checker's errors, as `code message`, for a program that type-checks.
fn ownership(source: &str) -> Vec<String> {
    let parsed = parse(source);
    let (typed, errs) = check(&parsed.ast);
    assert!(errs.is_empty(), "{errs:?}");
    crate::check_ownership(&parsed.ast, &typed)
        .into_iter()
        .filter(|d| d.severity == noev_syntax::Severity::Error)
        .map(|d| format!("{} {}", d.code.unwrap_or(""), d.message))
        .collect()
}

const SHAPE: &str = "trait Shape {\n fn area(ref self) -> Float\n \
                     fn describe(ref self) -> String { \"area {self.area()}\" }\n}\n\
                     struct Square { side: Float }\n";

#[test]
fn impls_and_calls() {
    let square = "impl Shape for Square { fn area(ref self) -> Float { self.side * self.side } }\n";
    ok(&format!(
        "{SHAPE}{square}fn f(s: Square) -> Float {{ s.area() }}"
    ));
    // A default.
    ok(&format!(
        "{SHAPE}{square}fn f(s: Square) -> String {{ s.describe() }}"
    ));
    // Through a bound.
    ok(&format!(
        "{SHAPE}{square}fn f<T: Shape>(s: ref T) -> Float {{ s.area() }}"
    ));
    ok(&format!(
        "{SHAPE}{square}fn f<T>(s: ref T) -> Float where T: Shape {{ s.area() }}"
    ));
    // A generic `impl`.
    ok(&format!(
        "{SHAPE}{square}struct Pair<T> {{ a: T, b: T }}\n\
         impl<T: Shape> Shape for Pair<T> {{ fn area(ref self) -> Float {{ self.a.area() + self.b.area() }} }}\n\
         fn f(p: Pair<Square>) -> Float {{ p.area() }}"
    ));
    // A built-in type.
    ok(
        "trait Flip { fn flip(mut self) }\nimpl Flip for Qubit { fn flip(mut self) { self.x() } }\n\
        fn f(q: mut Qubit) { q.flip() }",
    );
}

#[test]
fn impl_errors() {
    assert_eq!(
        errors(&format!("{SHAPE}impl Shape for Square {{}}")),
        ["this `impl` of `Shape` for `Square` lacks `area`"]
    );
    assert_eq!(
        errors(&format!(
            "{SHAPE}impl Shape for Square {{ fn area(ref self) -> Int {{ 1 }} }}"
        )),
        ["`area` has a different signature than in `Shape`"]
    );
    assert_eq!(
        errors(&format!(
            "{SHAPE}impl Shape for Square {{ fn area(ref self) -> Float {{ 1.0 }}\n fn side(ref self) -> Float {{ 1.0 }} }}"
        )),
        ["`side` is not a function of `Shape`"]
    );
    let area = "fn area(ref self) -> Float { 1.0 }";
    assert_eq!(
        errors(&format!(
            "{SHAPE}impl Shape for Square {{ {area} }}\nimpl Shape for Square {{ {area} }}"
        )),
        ["`Shape` is implemented for `Square` twice"]
    );
    // A generic `impl` overlaps the ones it covers.
    assert_eq!(
        errors(&format!(
            "{SHAPE}struct Pair<T> {{ a: T }}\nimpl Shape for Pair<Int> {{ {area} }}\n\
             impl<T> Shape for Pair<T> {{ {area} }}"
        )),
        ["`Shape` is implemented for `Pair<T>` twice"]
    );
    assert_eq!(
        errors("impl Missing for Int {}"),
        ["cannot find the trait `Missing`"]
    );
}

#[test]
fn bounds() {
    assert_eq!(
        errors(&format!("{SHAPE}fn f<T>(s: ref T) -> Float {{ s.area() }}")),
        ["`T` has no method `area`"]
    );
    assert_eq!(
        errors(&format!(
            "{SHAPE}fn f<T: Shape>(s: ref T) -> Float {{ s.area() }}\nfn g(s: Square) -> Float {{ f(ref s) }}"
        )),
        ["`f` needs `T: Shape`, but `Square` does not implement `Shape`"]
    );
}

#[test]
fn reversible_trait_functions() {
    let flip = "trait Flip {\n rev fn flip(mut self)\n fn plain(mut self)\n}\n";
    // An `impl` of a `rev` function is checked as `rev`.
    assert_eq!(
        ownership(&format!(
            "{flip}impl Flip for Qubit {{\n rev fn flip(mut self) {{ self.reset() }}\n fn plain(mut self) {{ self.x() }}\n}}"
        )),
        ["N013 `reset` is not reversible"]
    );
    let impl_ = "impl Flip for Qubit {\n rev fn flip(mut self) { self.x() }\n fn plain(mut self) { self.x() }\n}\n";
    // Through a bound, a `rev` function can be inverted; one that is not cannot, whatever
    // its `impl` does.
    assert_eq!(
        ownership(&format!(
            "{flip}{impl_}fn f<T: Flip>(q: mut T) {{ adj q.flip() }}\nfn g<T: Flip>(q: mut T) {{ adj q.plain() }}"
        )),
        ["N014 only reversible functions can be inverted with `adj`"]
    );
    assert_eq!(
        ownership(&format!(
            "{flip}{impl_}rev fn r(q: mut Qubit) {{ q.plain() }}"
        )),
        ["N013 `plain` is not reversible"]
    );
}
