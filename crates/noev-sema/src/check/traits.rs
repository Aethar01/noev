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
    // A bound written is a promise: `T: Copy` says nothing of `Shape`.
    assert_eq!(
        errors(&format!(
            "{SHAPE}fn f<T: Copy>(s: ref T) -> Float {{ s.area() }}"
        )),
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
    // On a known type the call reaches the `impl`'s function, which is analysed as usual.
    let resets = "impl Flip for Qubit {\n rev fn flip(mut self) { self.x() }\n fn plain(mut self) { self.reset() }\n}\n";
    assert_eq!(
        ownership(&format!(
            "{flip}{resets}rev fn r(q: mut Qubit) {{ q.plain() }}"
        )),
        ["N013 `plain` is not reversible"]
    );
}

const READOUT: &str = "trait Readout {\n type Result: Copy\n fn read(mv self) -> Self::Result\n}\n\
                       impl Readout for Qubit {\n type Result = Bit\n fn read(mv self) -> Bit { mv self.measure() }\n}\n";

#[test]
fn associated_types() {
    ok(&format!(
        "{READOUT}fn f<T: Readout>(x: T) -> T::Result {{ mv x.read() }}"
    ));
    // Known through the `impl`, or through a bound that fixes it.
    ok(&format!("{READOUT}fn f(q: Qubit) -> Bit {{ mv q.read() }}"));
    ok(&format!(
        "{READOUT}fn f<T: Readout<Result = Bit>>(x: T) -> Bit {{ mv x.read() }}"
    ));
    // A `Copy` associated type can be copied in generic code.
    ok(&format!(
        "{READOUT}fn f<T: Readout>(x: T) -> (T::Result, T::Result) {{ let r = mv x.read()\n (r, r) }}"
    ));
    assert_eq!(
        errors(&format!(
            "{READOUT}fn f<T: Readout>(x: T) -> T::Nope {{ mv x.read() }}"
        )),
        ["`T` has no associated type `Nope`"]
    );
    assert_eq!(
        errors(&format!(
            "{READOUT}struct Two {{ q: Qubit }}\nimpl Readout for Two {{ fn read(mv self) -> Bit {{ mv self.q.measure() }} }}"
        )),
        [
            "`read` has a different signature than in `Readout`",
            "this `impl` of `Readout` for `Two` lacks `type Result`"
        ]
    );
    assert_eq!(
        errors(&format!(
            "{READOUT}struct Boxed {{ q: Qubit }}\nimpl Readout for Boxed {{ type Result = Qubit\n fn read(mv self) -> Qubit {{ mv self.q }} }}"
        )),
        ["`Readout` needs `Result` to be `Copy`, but `Qubit` is not"]
    );
    assert_eq!(
        errors(&format!(
            "{READOUT}struct Two {{ a: Qubit, b: Qubit }}\n\
             impl Readout for Two {{ type Result = (Bit, Bit)\n fn read(mv self) -> (Bit, Bit) {{ (mv self.a.measure(), mv self.b.measure()) }} }}\n\
             fn one<T: Readout<Result = Bit>>(x: T) -> Bit {{ mv x.read() }}\n\
             fn f() -> Bit {{ one(Two {{ a: |0>, b: |0> }}) }}"
        )),
        ["`one` needs `Two::Result` to be `Bit`, but it is `(Bit, Bit)`"]
    );
}

#[test]
fn measure_is_a_trait() {
    let experiment = "struct Experiment { angle: Float, state: Qubit }\n\
                      struct Outcome { angle: Float, bit: Bit }\n\
                      impl Measure for Experiment {\n type Result = Outcome\n \
                      fn measure(mv self) -> Outcome { Outcome { angle: self.angle, bit: mv self.state.measure() } }\n}\n";
    ok(&format!(
        "{experiment}fn f(e: Experiment) -> Outcome {{ mv e.measure() }}"
    ));
    ok(&format!(
        "{experiment}fn f(e: Experiment) -> Outcome {{ measure(mv e) }}"
    ));
    // Qubits measure by the built-in measurement, through a bound too.
    ok("fn f<T: Measure>(x: T) -> T::Result { mv x.measure() }\nfn g() -> [Bit; 2] { f(|01>) }");
    ok("fn f<T: Measure<Result = Bit>>(x: T) -> Bit { measure(mv x) }\nfn g() -> Bit { f(|1>) }");
    assert_eq!(
        errors("struct S { x: Int }\nfn f(s: S) { let b = measure(s) }"),
        ["`measure` takes a qubit or an array of qubits, but this is `S`"]
    );
    assert_eq!(
        errors(
            "fn f<T: Measure>(x: T) -> T::Result { mv x.measure() }\nstruct S { x: Int }\nfn g(s: S) { let r = f(s) }"
        ),
        ["`f` needs `T: Measure`, but `S` does not implement `Measure`"]
    );
}

const TWO_NAMES: &str = "trait A { fn name(ref self) -> Int }\ntrait B { fn name(ref self) -> Int }\n\
                         struct S { x: Int }\nimpl A for S { fn name(ref self) -> Int { 1 } }\n\
                         impl B for S { fn name(ref self) -> Int { 2 } }\n";

#[test]
fn ambiguous_methods() {
    assert_eq!(
        errors(&format!("{TWO_NAMES}fn f(s: S) -> Int {{ s.name() }}")),
        ["`name` is a function of `A` and `B`, which `S` implements"]
    );
    ok(&format!(
        "{TWO_NAMES}fn f(s: S) -> Int {{ A::name(ref s) + B::name(ref s) }}"
    ));
}

#[test]
fn concrete_calls_see_the_impl() {
    // A call on a known type reaches the impl's function, so its reversibility is inferred.
    let flip = "trait Flip { fn flip(mut self) }\nimpl Flip for Qubit { fn flip(mut self) { self.x() } }\n";
    assert!(ownership(&format!("{flip}rev fn f(q: mut Qubit) {{ q.flip() }}")).is_empty());
    assert!(
        ownership(&format!(
            "{flip}rev fn f(q: mut Qubit) {{ Flip::flip(mut q) }}"
        ))
        .is_empty()
    );
}

#[test]
fn measure_by_parts() {
    let e = "struct E { state: Qubit, tag: Int }\nstruct R { bit: Bit, tag: Int }\n\
             impl Measure for E {\n type Result = R\n \
             fn measure(mv self) -> R { R { bit: mv self.state.measure(), tag: self.tag } }\n}\n";
    ok(&format!(
        "{e}fn f(p: (E, Qubit)) -> (R, Bit) {{ mv p.measure() }}"
    ));
    ok(&format!(
        "{e}fn f(es: [E; 2]) -> [R; 2] {{ measure(mv es) }}"
    ));
    ok(&format!(
        "{e}fn f<T: Measure>(x: T) -> T::Result {{ mv x.measure() }}\nfn g(p: (E, [Qubit; 2])) -> (R, [Bit; 2]) {{ f(p) }}"
    ));
}

#[test]
fn impls_over_sizes() {
    ok("trait Width { fn width(ref self) -> Int }\n\
        impl<const N: Int> Width for [Qubit; N] { fn width(ref self) -> Int { N } }\n\
        fn f(r: [Qubit; 3]) -> Int { r.width() }");
    ok("struct Reg<const N: Int> { qs: [Qubit; N] }\n\
        impl<const N: Int> Measure for Reg<N> {\n type Result = [Bit; N]\n \
        fn measure(mv self) -> [Bit; N] { mv self.qs.measure() }\n}\n\
        fn f(r: Reg<4>) -> [Bit; 4] { measure(mv r) }");
}

const ORACLE: &str = "trait Oracle<const N: Int> {\n rev fn apply(ref self, r: mut [Qubit; N])\n \
                      fn width(ref self) -> Int { N }\n}\nstruct Flip {}\n\
                      impl Oracle<2> for Flip { rev fn apply(ref self, r: mut [Qubit; 2]) { r[1].x() } }\n";

#[test]
fn generic_traits() {
    // Several `impl`s of one trait for one type, told apart by the arguments.
    ok(&format!(
        "{ORACLE}impl Oracle<3> for Flip {{ rev fn apply(ref self, r: mut [Qubit; 3]) {{ r[2].x() }} }}\n\
         fn f(o: ref Flip, r: mut [Qubit; 3]) {{ o.apply(mut r) }}"
    ));
    // With one `impl`, its arguments are the call's.
    ok(&format!("{ORACLE}fn f(o: ref Flip) -> Int {{ o.width() }}"));
    ok(&format!(
        "{ORACLE}fn f(o: ref Flip) -> Int {{ Oracle::width(o) }}"
    ));
    // Through a bound, from the bound.
    ok(&format!(
        "{ORACLE}fn f<T: Oracle<N>, const N: Int>(o: ref T, r: mut [Qubit; N]) {{ adj o.apply(mut r) }}\n\
         fn g<T: Oracle<N>, const N: Int>(o: ref T, r: ref [Qubit; N]) -> Int {{ o.width() }}"
    ));
    ok(&format!(
        "{ORACLE}fn f<T, const N: Int>(o: ref T, r: mut [Qubit; N]) where T: Oracle<N> {{ o.apply(mut r) }}\n\
         fn g(o: ref Flip, r: mut [Qubit; 2]) {{ f(o, mut r) }}"
    ));
    // A trait over a type, and a generic `impl`.
    ok(
        "trait Scale<T> { fn scale(ref self, x: T) -> T }\nstruct Twice {}\n\
         impl Scale<Int> for Twice { fn scale(ref self, x: Int) -> Int { 2 * x } }\n\
         impl Scale<Float> for Twice { fn scale(ref self, x: Float) -> Float { 2.0 * x } }\n\
         struct Id {}\nimpl<T> Scale<T> for Id { fn scale(ref self, x: T) -> T { mv x } }\n\
         fn f(t: Twice, i: Id) -> (Int, Float, Bool) { (t.scale(1), t.scale(1.0), i.scale(true)) }",
    );
}

#[test]
fn generic_trait_errors() {
    assert_eq!(
        errors(&format!("{ORACLE}impl Oracle for Flip {{}}")),
        ["`Oracle` takes 1 argument, but 0 are given"]
    );
    assert_eq!(
        errors(&format!("{ORACLE}fn f<T: Oracle<2, 3>>(o: ref T) {{}}")),
        ["`Oracle` takes 1 argument, but 2 are given"]
    );
    assert_eq!(
        errors(&format!("{ORACLE}fn f<T: Oracle<Int>>(o: ref T) {{}}")),
        ["`N` is a size, but `Int` is a type"]
    );
    assert_eq!(
        errors(&format!(
            "{ORACLE}impl Oracle<2> for Flip {{ rev fn apply(ref self, r: mut [Qubit; 2]) {{}} }}"
        )),
        ["`Oracle<2>` is implemented for `Flip` twice"]
    );
    assert_eq!(
        errors(&format!(
            "{ORACLE}impl Oracle<3> for Flip {{ rev fn apply(ref self, r: mut [Qubit; 2]) {{}} }}"
        )),
        ["`apply` has a different signature than in `Oracle`"]
    );
    // A call meeting a bound with other arguments.
    assert_eq!(
        errors(&format!(
            "{ORACLE}fn f<T: Oracle<3>>(o: ref T) {{}}\nfn g(o: ref Flip) {{ f(o) }}"
        )),
        ["`f` needs `T: Oracle<3>`, but `Flip` does not implement `Oracle<3>`"]
    );
    // Two `impl`s, and nothing to choose between them.
    assert_eq!(
        errors(&format!(
            "{ORACLE}impl Oracle<3> for Flip {{ rev fn apply(ref self, r: mut [Qubit; 3]) {{}} }}\n\
             fn f(o: ref Flip) -> Int {{ o.width() }}"
        )),
        ["cannot tell what `N` is in this call"]
    );
}

/// Errors from type checking and then ownership, as messages, with the labels of each.
fn all_errors(source: &str) -> Vec<String> {
    let parsed = parse(source);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, mut diagnostics) = check(&parsed.ast);
    if diagnostics.is_empty() {
        diagnostics = crate::check_ownership(&parsed.ast, &typed);
    }
    diagnostics
        .into_iter()
        .filter(|d| d.severity == noev_syntax::Severity::Error)
        .map(|d| {
            let labels: Vec<String> = d
                .labels
                .iter()
                .map(|(span, text)| {
                    format!(
                        "{text} at {}",
                        &source[span.start as usize..span.end as usize]
                    )
                })
                .collect();
            if labels.is_empty() {
                d.message
            } else {
                format!("{} ({})", d.message, labels.join(", "))
            }
        })
        .collect()
}

#[test]
fn inferred_trait_bounds() {
    // From a method, and through a function that passes the parameter on.
    ok(&format!(
        "{SHAPE}impl Shape for Square {{ fn area(ref self) -> Float {{ self.side }} }}\n\
         fn f<T>(s: ref T) -> Float {{ s.area() }}\n\
         fn g<U>(s: ref U) -> Float {{ f(s) + f(s) }}\n\
         fn h(s: Square) -> Float {{ g(ref s) }}"
    ));
    // From a function whose bound is written, and from `measure`.
    ok(&format!(
        "{SHAPE}fn f<T: Shape>(s: ref T) -> Float {{ s.area() }}\nfn g<U>(s: ref U) -> Float {{ f(s) }}"
    ));
    ok("fn f<T>(x: T) { let b = mv x.measure() }\nfn g(q: Qubit) { f(mv q) }");
    // Recursion.
    ok(&format!(
        "{SHAPE}fn f<T>(s: ref T, n: Int) -> Float {{ if n == 0 {{ s.area() }} else {{ f(s, n - 1) }} }}"
    ));
    assert_eq!(
        all_errors(&format!(
            "{SHAPE}fn f<T>(s: ref T) -> Float {{ s.area() }}\n\
             fn g<U>(s: ref U) -> Float {{ f(s) }}\nfn h(n: Int) -> Float {{ g(ref n) }}"
        )),
        [
            "`g` needs `U: Shape`, but `Int` does not implement `Shape` (`U: Shape` is needed here at f(s))"
        ]
    );
    assert_eq!(
        all_errors(&format!(
            "{SHAPE}trait Solid {{ fn area(ref self) -> Float }}\nfn f<T>(s: ref T) -> Float {{ s.area() }}"
        )),
        ["`area` is a function of `Shape` and `Solid`, so `T` needs a bound saying which"]
    );
    // Other packages see only the bounds written: a library's `pub` function infers none.
    assert_eq!(
        all_errors(&format!(
            "{SHAPE}pub fn f<T>(s: ref T) -> Float {{ s.area() }}"
        )),
        ["`T` has no method `area`"]
    );
}

#[test]
fn inferred_copy() {
    ok_owned("fn twice<T>(x: T) -> (T, T) { (x, x) }\nfn f() -> (Int, Int) { twice(1) }");
    ok_owned("fn drop<T>(x: T) {}\nfn pass<U>(x: U) { drop(x) }\nfn f() { pass(1.5) }");
    // A parameter declared `Copy` given a `T`.
    ok_owned(
        "fn twice<T: Copy>(x: T) -> (T, T) { (x, x) }\nfn pass<U>(x: U) -> (U, U) { twice(x) }",
    );
    // Only the parameters copied.
    assert_eq!(
        all_errors(
            "fn first<A, B>(a: A, b: B) -> (A, A, B) { (a, a, mv b) }\n\
             fn f(q: Qubit) -> (Int, Int, Qubit) { first(1, mv q) }\n\
             fn g(q: Qubit) -> (Qubit, Qubit, Int) { first(mv q, 1) }"
        ),
        ["`first` needs `A: Copy`, but here `A` is `Qubit` (`A: Copy` is needed here at a)"]
    );
    assert_eq!(
        all_errors(
            "fn drop<T>(x: T) {}\nfn pass<U>(x: U) { drop(x) }\nfn f(q: Qubit) { pass(mv q) }"
        ),
        ["`pass` needs `U: Copy`, but here `U` is `Qubit` (`U: Copy` is needed here at x)"]
    );
    // Moved on every turn of a loop, as only a copyable value can be.
    assert_eq!(
        all_errors(
            "fn each<T>(x: T, n: Int) { for i in 0..n { let y = x } }\n\
             fn f() { each(1.0, 3) }\nfn g(q: Qubit) { each(mv q, 3) }"
        ),
        ["`each` needs `T: Copy`, but here `T` is `Qubit` (`T: Copy` is needed here at x)"]
    );
    // A bound written is a promise, and so are a library's `pub` function's.
    assert_eq!(
        ownership("fn twice<T: Measure>(x: T) -> (T, T) { (x, x) }")[0]
            .split(' ')
            .next(),
        Some("N001")
    );
    assert_eq!(
        ownership("pub fn twice<T>(x: T) -> (T, T) { (x, x) }")[0]
            .split(' ')
            .next(),
        Some("N001")
    );
}

fn ok_owned(source: &str) {
    let errs = all_errors(source);
    assert!(errs.is_empty(), "{errs:?}");
}

/// Calling a trait's function that is not `rev fn`, through a bound, stops a function being
/// a circuit because the `impl` may measure; the reason says so, not that a result is passed.
#[test]
fn unknown_trait_function_is_not_a_circuit() {
    let source = "trait Oracle { rev fn apply(ref self, q: mut Qubit)\n fn width(ref self) -> Int }\n\
                  fn run<T: Oracle>(o: ref T, q: mut Qubit) -> Int { o.apply(mut q)\n o.width() }\n\
                  fn outer<T: Oracle>(o: ref T, q: mut Qubit) -> Int { run(ref o, mut q) }\n";
    let parsed = parse(source);
    let (typed, errs) = check(&parsed.ast);
    assert!(errs.is_empty(), "{errs:?}");
    let (circuits, _) = crate::check_circuits(&parsed.ast, &typed);
    let why = |name: &str| {
        let f = parsed
            .ast
            .all_fns()
            .iter()
            .position(|d| parsed.ast.name(d.name) == name);
        circuits.fns[f.expect("declared")]
            .not_circuit
            .clone()
            .map(|(_, why)| why)
    };
    let direct = "it calls `width`, a trait's function not declared `rev fn` whose `impl`s are \
                  not known here, so it may measure";
    assert_eq!(why("run").as_deref(), Some(direct));
    assert_eq!(why("outer"), Some(format!("in `run`, {direct}")));
}

/// A generic trait's associated type depends on its arguments: `<Twice as Scale<Int>>::Out`
/// and `<Twice as Scale<Float>>::Out` differ, and a bound picks one.
const SCALE: &str = "trait Scale<T> { type Out\n fn scale(ref self, x: T) -> Self::Out }\n\
                     struct Twice {}\n\
                     impl Scale<Int> for Twice { type Out = Int\n fn scale(ref self, x: Int) -> Int { 2 * x } }\n\
                     impl Scale<Float> for Twice { type Out = String\n fn scale(ref self, x: Float) -> String { \"{x}\" } }\n";

#[test]
fn associated_types_in_generic_traits() {
    ok(&format!(
        "{SCALE}fn f(t: ref Twice) -> (Int, String) {{ (t.scale(2), t.scale(1.5)) }}"
    ));
    // Through a bound, which gives the arguments.
    ok(&format!(
        "{SCALE}fn g<S: Scale<Int>>(s: ref S) -> S::Out {{ s.scale(3) }}\n\
         fn h(t: ref Twice) -> Int {{ g(t) }}"
    ));
    ok(&format!(
        "{SCALE}fn g<S: Scale<Float, Out = String>>(s: ref S) -> String {{ s.scale(3.0) }}\n\
         fn h(t: ref Twice) -> String {{ g(t) }}"
    ));
    assert_eq!(
        errors(&format!(
            "{SCALE}fn g<S: Scale<Float, Out = Int>>(s: ref S) -> Int {{ s.scale(3.0) }}\n\
             fn h(t: ref Twice) -> Int {{ g(t) }}"
        )),
        ["`g` needs `Twice::Out` to be `Int`, but it is `String`"]
    );
    assert_eq!(
        errors(&format!("{SCALE}fn k(t: ref Twice) -> Twice::Out {{ 1 }}")),
        [
            "`Twice` implements `Scale` with several arguments, so `Twice::Out` could be several types"
        ]
    );
}

const OPS: &str = "use std::ops::{Add, Neg}\nstruct P { x: Int }\n";

/// `+` on a type of the program's own calls `Add::add`; without an `impl`, or a bound on a
/// type parameter, it is an error that says which.
#[test]
fn operator_traits() {
    ok(&format!(
        "{OPS}impl Add<P> for P {{ type Output = Int\n fn add(mv self, r: P) -> Int {{ self.x + r.x }} }}\n\
         fn f(a: P, b: P) -> Int {{ a + b + 1 }}\n\
         fn g<T: Add<T, Output = Int>>(a: T, b: T) -> Int {{ mv a + mv b }}"
    ));
    assert_eq!(
        errors(&format!("{OPS}fn f(a: P, b: P) -> P {{ a + b }}")),
        ["`+` cannot be applied to `P`: it does not implement `Add`"]
    );
    assert_eq!(
        errors(&format!("{OPS}fn f(a: P) -> P {{ -a }}")),
        ["`-` cannot be applied to `P`: it does not implement `Neg`"]
    );
    assert_eq!(
        errors(&format!("{OPS}fn f<T>(a: T, b: T) -> T {{ mv a + mv b }}")),
        ["`+` cannot be applied to `T`: it does not implement `Add`"]
    );
    // The right side must be what the `impl` takes: with one `impl`, it is expected.
    assert_eq!(
        errors(&format!(
            "{OPS}impl Add<P> for P {{ type Output = P\n fn add(mv self, r: P) -> P {{ mv r }} }}\n\
             fn f(a: P) -> P {{ a + 1 }}"
        )),
        ["mismatched types: expected `P`, found `Int`"]
    );
    // A bound a call in an operand does not meet is that call's, not the operator's.
    assert_eq!(
        errors(&format!(
            "{OPS}impl Add<Int> for P {{ type Output = Int\n fn add(mv self, r: Int) -> Int {{ r }} }}\n\
             fn g<T: Add<Int>>(t: T) -> Int {{ 1 }}\n\
             fn f(a: P) -> Int {{ a + g(true) }}"
        )),
        ["`g` needs `T: Add<Int>`, but `Bool` does not implement `Add<Int>`"]
    );
    // Without `std::ops`, the operator is the built-in one, and says where to look.
    assert_eq!(
        errors("struct P { x: Int }\nfn f(a: P, b: P) -> P { a + b }"),
        ["`+` cannot be applied to `P` and `P`"]
    );
}

/// The operands of an operator on qubits are moved, so they need `mv`.
#[test]
fn operator_operands_are_moved() {
    let reg = "use std::ops::Add\nstruct R { q: Qubit }\n\
               impl Add<R> for R { type Output = R\n fn add(mv self, r: R) -> R { mv r.q.measure()\n mv self } }\n";
    assert_eq!(
        ownership(&format!("{reg}fn f(a: R, b: R) -> R {{ mv a + mv b }}")),
        Vec::<String>::new()
    );
    assert_eq!(
        ownership(&format!("{reg}fn f(a: R, b: R) -> R {{ a + mv b }}")),
        ["N001 `+` takes ownership of `a`"]
    );
    assert_eq!(
        ownership(&format!("{reg}fn f(a: R, b: R) -> R {{ mv a + b }}")),
        ["N001 `b` holds qubits, which cannot be copied"]
    );
}

/// `a += b` on a type of the program's own calls `AddAssign::add_assign`, lending `a` for
/// changing: a classical one must be `let mut`, and one with only an `impl` for `ref R`
/// lends the right side.
#[test]
fn compound_assignment_traits() {
    let v = "use std::ops::AddAssign\nstruct V { x: Int }\n\
             impl AddAssign<V> for V { fn add_assign(mut self, r: V) { self.x += r.x } }\n";
    assert_eq!(
        ownership(&format!(
            "{v}fn f(w: V) {{ let mut v = V {{ x: 1 }}\n v += w }}"
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        ownership(&format!(
            "{v}fn f(w: V) {{ let v = V {{ x: 1 }}\n v += w }}"
        )),
        ["N015 `v` is not declared `let mut`, so it cannot be changed"]
    );
    assert_eq!(
        errors(&format!(
            "{v}fn f(w: V) {{ let mut v = V {{ x: 1 }}\n v -= w }}"
        )),
        ["`-=` cannot be applied to `V`: it does not implement `SubAssign`"]
    );
    let r = "use std::ops::{AddAssign, SubAssign}\nstruct R { q: Qubit }\n\
             impl AddAssign<ref R> for R { fn add_assign(mut self, r: ref R) { ctrl r.q { self.q.x() } } }\n";
    assert_eq!(
        ownership(&format!("{r}rev fn f(a: mut R, b: ref R) {{ a += b }}")),
        Vec::<String>::new()
    );
    assert_eq!(
        errors(&format!("{r}fn f(a: mut R, b: ref R) {{ a -= b }}")),
        ["`-=` cannot be applied to `R`: it does not implement `SubAssign`"]
    );
}

/// Built-in types implement `std::ops` by their built-in operators, with their `Output`s.
#[test]
fn builtin_types_implement_operator_traits() {
    let sum = "use std::ops::{Add, Mul}\nfn sum<T: Add<T, Output = T>>(a: T, b: T) -> T { mv a + mv b }\n";
    ok(&format!(
        "{sum}fn f() -> (Int, Float, u8) {{ (sum(1, 2), sum(1.0, 2.0), sum(u8(1), u8(2))) }}\n\
         fn g() -> Int {{ Add::add(1, 2) }}"
    ));
    // `Float * Observable` is an `Observable`.
    ok(
        "use std::ops::Mul\nfn s<T: Mul<Observable, Output = Observable>>(k: T) -> Observable { mv k * Z(0) }\n\
        fn f() -> Observable { s(2.0) }",
    );
    assert_eq!(
        errors(&format!("{sum}fn f() -> Bool {{ sum(true, false) }}")),
        ["`sum` needs `T: Add<Bool>`, but `Bool` does not implement `Add<Bool>`"]
    );
    assert_eq!(
        errors(&format!(
            "{sum}fn s<T: Mul<Float, Output = T>>(x: T) -> T {{ mv x * 2.0 }}\nfn f() -> Int {{ s(3) }}"
        )),
        ["`s` needs `T: Mul<Float>`, but `Int` does not implement `Mul<Float>`"]
    );
}

/// Trait parameters can have defaults (`trait Add<Rhs = Self>`), naming `Self` or earlier
/// parameters, and leaving an argument out uses the default in `impl`s and bounds.
#[test]
fn trait_param_defaults() {
    // `Add` defaults `Rhs = Self`, so both `impl Add for V` and `T: Add<Output = T>` work.
    ok("use std::ops::Add\n\
         struct V { x: Int }\n\
         impl Add for V { type Output = V\n fn add(mv self, r: V) -> V { V { x: self.x + r.x } } }\n\
         fn sum<T: Add<Output = T>>(a: T, b: T) -> T { mv a + mv b }\n\
         fn f(a: V, b: V) -> (V, Int) { (sum(a, b), sum(1, 2)) }");
    // Custom traits with type and size defaults, and a later default naming an earlier param.
    ok(
        "trait Pair<A = Self, B = A> { fn pair(ref self, a: A, b: B) -> Int }\n\
         struct S { x: Int }\n\
         impl Pair for S { fn pair(ref self, a: S, b: S) -> Int { self.x + a.x + b.x } }\n\
         impl Pair<Int> for S { fn pair(ref self, a: Int, b: Int) -> Int { self.x + a + b } }\n\
         fn use_same<T: Pair>(x: ref T, a: T, b: T) -> Int { x.pair(mv a, mv b) }\n\
         fn use_int<T: Pair<Int>>(x: ref T, a: Int, b: Int) -> Int { x.pair(a, b) }\n\
         fn f(s: S) -> Int { use_same(ref s, s, s) + use_int(ref s, 1, 2) }",
    );
    ok(
        "trait Pad<const N: Int = 1> { fn pad(ref self) -> [Int; N] }\n\
         struct S {}\n\
         impl Pad for S { fn pad(ref self) -> [Int; 1] { [0] } }\n\
         fn f<T: Pad>(x: ref T) -> [Int; 1] { x.pad() }\n\
         fn g(s: S) -> [Int; 1] { f(ref s) }",
    );
    // Ordering, arity and non-trait errors.
    assert_eq!(
        errors("trait Bad<A = Int, B> { fn f(ref self, a: A, b: B) }"),
        ["a parameter without a default cannot follow one with a default"]
    );
    assert_eq!(
        errors("struct Bad<T = Int> { x: T }"),
        ["defaults are only allowed on trait parameters"]
    );
    assert_eq!(
        errors("fn bad<T = Int>(x: T) -> T { mv x }"),
        ["defaults are only allowed on trait parameters"]
    );
    assert_eq!(
        errors(
            "trait One<A, B = Self> { fn f(ref self, a: A, b: B) }\n\
             struct S {}\n\
             impl One for S { fn f(ref self, a: S, b: S) {} }"
        ),
        ["`One` takes 1 to 2 arguments, but 0 are given"]
    );
}

/// Comparison (`std::cmp::{Eq, Ord}`) and bit operator traits (`std::ops::{BitAnd, ...}`).
#[test]
fn comparison_and_bit_operator_traits() {
    let mask = "use std::ops::{BitAnd, BitOr, BitXor, BitNot, Shl, Shr, BitAndAssign, ShlAssign}\n\
                struct Mask { bits: Int }\n\
                impl BitAnd for Mask { type Output = Mask\n fn bitand(mv self, r: Mask) -> Mask { Mask { bits: self.bits & r.bits } } }\n\
                impl BitOr for Mask { type Output = Mask\n fn bitor(mv self, r: Mask) -> Mask { Mask { bits: self.bits | r.bits } } }\n\
                impl BitXor for Mask { type Output = Mask\n fn bitxor(mv self, r: Mask) -> Mask { Mask { bits: self.bits ^ r.bits } } }\n\
                impl BitNot for Mask { type Output = Mask\n fn bitnot(mv self) -> Mask { Mask { bits: ~self.bits } } }\n\
                impl Shl<Int> for Mask { type Output = Mask\n fn shl(mv self, n: Int) -> Mask { Mask { bits: self.bits << n } } }\n\
                impl Shr<Int> for Mask { type Output = Mask\n fn shr(mv self, n: Int) -> Mask { Mask { bits: self.bits >> n } } }\n\
                impl BitAndAssign for Mask { fn bitand_assign(mut self, r: Mask) { self.bits &= r.bits } }\n\
                impl ShlAssign<Int> for Mask { fn shl_assign(mut self, n: Int) { self.bits <<= n } }\n";
    ok(&format!(
        "{mask}fn f(a: Mask, b: Mask) -> Mask {{ let mut m = ~(a & b | a ^ b)\n m &= a\n m <<= 2\n (m << 1) >> 1 }}"
    ));
    // Comparisons lend both sides, so linear types don't move and copyable types aren't copied.
    let cmp = "use std::cmp::{Eq, Ord}\n\
               struct P { x: Int }\n\
               impl Eq for P { fn eq(ref self, r: ref P) -> Bool { self.x == r.x } }\n\
               impl Ord for P {\n\
                   fn lt(ref self, r: ref P) -> Bool { self.x < r.x }\n\
                   fn le(ref self, r: ref P) -> Bool { self.x <= r.x }\n\
               }\n";
    ok(&format!(
        "{cmp}fn f(a: P, b: P) -> Bool {{ a == b or a != b or a < b or a <= b or a > b or a >= b }}\n\
         fn g<T: Eq + Ord>(a: ref T, b: ref T) -> Bool {{ a == b or a != b or a < b or a >= b }}"
    ));
    // Comparable enums keep built-in `==` even when `std::cmp::Eq` is imported.
    ok("use std::cmp::Eq\n\
         enum Color { Red, Blue }\n\
         fn f(a: Color, b: Color) -> Bool { a == b and a != b }\n\
         fn g<T: Eq>(a: T, b: T) -> Bool { a == b }\n\
         fn h() -> Bool { g(Color::Red, Color::Blue) and g(\"hi\", \"hi\") and g([0, 1], [0, 1]) }");
    // Comparing linear structs lends them, never moves.
    let qcmp = "use std::cmp::Eq\n\
                struct Tagged { id: Int, q: Qubit }\n\
                impl Eq for Tagged { fn eq(ref self, r: ref Tagged) -> Bool { self.id == r.id } }\n";
    assert_eq!(
        ownership(&format!(
            "{qcmp}fn f(a: Tagged, b: Tagged) -> (Bool, Bit, Bit) {{ let eq = a == b\n (eq, mv a.q.measure(), mv b.q.measure()) }}"
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        errors("use std::cmp::Ord\nstruct P { x: Int }\nfn f(a: P, b: P) -> Bool { a >= b }"),
        ["`>=` cannot be applied to `P`: it does not implement `Ord`"]
    );
}
