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
