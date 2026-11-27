//! Generic functions, structs and enums (`spec/types.md` §6).

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

const SWAP: &str = "fn swap<T>(p: (T, T)) -> (T, T) { let (a, b) = mv p\n (mv b, mv a) }\n";

#[test]
fn type_arguments_are_inferred_or_given() {
    ok(&format!("{SWAP}fn f() -> (Int, Int) {{ swap((1, 2)) }}"));
    ok(&format!(
        "{SWAP}fn f() -> (Qubit, Qubit) {{ swap((|0>, |1>)) }}"
    ));
    // Given, a literal takes the type given.
    ok(&format!(
        "{SWAP}fn f() -> (Bit, Bit) {{ swap::<Bit>((0, 1)) }}"
    ));
    assert_eq!(
        errors(&format!("{SWAP}fn f() -> (Int, Int) {{ swap((1, 2.0)) }}")),
        ["mismatched types: expected `(Int, Int)`, found `(Int, Float)`"]
    );
    assert_eq!(
        errors(&format!(
            "{SWAP}fn f() {{ let p = swap::<Int, Int>((1, 2)) }}"
        )),
        ["this takes 1 type argument, but 2 were given"]
    );
    assert_eq!(
        errors("fn none<T>() -> Int { 1 }\nfn f() -> Int { none() }"),
        ["cannot tell what `T` is in this call"]
    );
    assert_eq!(
        errors("fn g(x: Int) {}\nfn f() { g::<Int>(1) }"),
        ["this function takes no type arguments"]
    );
}

#[test]
fn recursive_calls_infer_their_own_parameters() {
    ok("fn f<T>(s: ref T, n: Int) -> Int { if n == 0 { 1 } else { f(s, n - 1) } }");
    ok("fn g<T>(x: T, n: Int) -> T { if n == 0 { mv x } else { g(mv x, n - 1) } }");
}

#[test]
fn copy_bounds() {
    let twice = "fn twice<T: Copy>(x: T) -> (T, T) { (x, x) }\n";
    ok(&format!("{twice}fn f() -> (Float, Float) {{ twice(1.5) }}"));
    // A shared reference is `Copy`, even to a qubit.
    ok(&format!("{twice}fn f(q: ref Qubit) {{ let p = twice(q) }}"));
    assert_eq!(
        errors(&format!("{twice}fn f() {{ let p = twice(|0>) }}")),
        ["`twice` needs `T: Copy`, but here `T` is `Qubit`"]
    );
    // In `where`, too.
    assert_eq!(
        errors("fn first<T>(x: T) -> T where T: Copy { x }\nfn f() { let q = first(|0>) }"),
        ["`first` needs `T: Copy`, but here `T` is `Qubit`"]
    );
    assert_eq!(
        errors("fn f<T: Missing>(x: T) {}"),
        ["cannot find the trait `Missing`"]
    );
    assert_eq!(
        errors("fn f<T>(x: T) where U: Copy {}"),
        ["`U` is not a type parameter here"]
    );
}

#[test]
fn generic_structs_and_enums() {
    let pair = "struct Pair<T, U> {\n first: T\n second: U\n \
                fn new(first: T, second: U) -> Self { Pair { first: mv first, second: mv second } }\n \
                fn flip(mv self) -> Pair<U, T> { Pair { first: mv self.second, second: mv self.first } }\n}\n";
    ok(&format!(
        "{pair}fn f() -> Pair<Bool, Int> {{ let p = Pair {{ first: 1, second: true }}\n mv p.flip() }}"
    ));
    ok(&format!(
        "{pair}fn f() -> Pair<Int, Qubit> {{ Pair::new(1, |0>) }}"
    ));
    ok(&format!(
        "{pair}fn f(p: Pair<Int, Bit>) -> Bit {{ p.second }}"
    ));
    assert_eq!(
        errors(&format!("{pair}fn f(p: Pair) {{}}")),
        ["`Pair` is generic: give its type arguments, as in `Pair<…>`"]
    );
    assert_eq!(
        errors(&format!("{pair}fn f(p: Pair<Int>) {{}}")),
        ["`Pair` takes 2 type arguments, but 1 was given"]
    );
    let maybe = "enum Maybe<T> { Just(T), Nothing }\n";
    ok(&format!("{maybe}fn f() -> Maybe<Int> {{ Maybe::Just(3) }}"));
    ok(&format!("{maybe}fn f() -> Maybe<Int> {{ Maybe::Nothing }}"));
    ok(&format!(
        "{maybe}fn f(m: Maybe<Int>) -> Int {{ match m {{ Maybe::Just(x) => x, Maybe::Nothing => 0 }} }}"
    ));
    assert_eq!(
        errors(&format!("{maybe}fn f() {{ let m = Maybe::Nothing }}")),
        ["cannot tell what `T` is in this variant"]
    );
}

#[test]
fn generic_code_knows_nothing_of_t() {
    // A `T` has no methods and cannot be printed, compared or measured.
    assert_eq!(
        errors("fn f<T>(x: ref T) { x.h() }"),
        ["`T` has no method `h`"]
    );
    assert_eq!(
        errors("fn f<T: Copy>(x: T) { print(x) }"),
        ["`T` cannot be printed"]
    );
}

#[test]
fn size_parameters() {
    let count = "fn count<T, const N: Int>(xs: ref [T; N]) -> Int { N }\n";
    ok(&format!(
        "{count}fn f(r: [Qubit; 3]) -> Int {{ count(ref r) }}"
    ));
    // From the type expected, or given.
    let fresh = "fn fresh<const N: Int>() -> [Qubit; N] { [|0>; N] }\n";
    ok(&format!("{fresh}fn f() -> [Qubit; 2] {{ fresh() }}"));
    ok(&format!(
        "{fresh}fn f() -> [Qubit; 4] {{ let r = fresh::<4>()\n mv r }}"
    ));
    ok("struct Reg<const N: Int> { qs: [Qubit; N] }\nfn f() -> Reg<2> { Reg { qs: |01> } }");
    assert_eq!(
        errors(
            "fn same<const N: Int>(a: [Int; N], b: [Int; N]) {}\nfn f() { same([1, 2], [1, 2, 3]) }"
        ),
        ["mismatched types: expected `[Int; 2]`, found `[Int; 3]`"]
    );
    assert_eq!(
        errors("fn f<const N: Bool>() {}"),
        ["a size parameter is an `Int`: `const N: Int`"]
    );
    assert_eq!(
        errors("fn f(r: [Qubit; M]) {}"),
        ["cannot find the size `M`; declare it with `const M: Int`"]
    );
    assert_eq!(
        errors("fn f<const N: Int>(x: N) {}"),
        ["`N` is a size, not a type"]
    );
    assert_eq!(
        errors("struct Reg<const N: Int> { qs: [Qubit; N] }\nfn f(r: Reg<Int>) {}"),
        ["`N` is a size, but `Int` is a type"]
    );
    assert_eq!(
        errors(&format!("{SWAP}fn f() {{ let p = swap::<3>((1, 2)) }}")),
        ["`T` is a type, but `3` is a size"]
    );
    assert_eq!(
        errors("fn f<const N: Int, const N: Int>() {}"),
        ["the parameter `N` is declared twice"]
    );
}

#[test]
fn the_type_expected_tells_type_arguments() {
    ok("fn none<T>() -> Option<T> { None }\nfn f() -> Option<Int> { none() }");
    ok("fn none<T>() -> Option<T> { None }\nfn f() { let x: Option<Bit> = none() }");
}

/// Sizes can be sums of size parameters and numbers, compared after normalising, and a call
/// solves the one parameter a sum leaves unknown.
#[test]
fn arithmetic_in_sizes() {
    let widen = "fn widen<const N: Int>(r: [Qubit; N]) -> [Qubit; N + 1] { mv r.measure()\n [|0>; N + 1] }\n";
    ok(&format!(
        "{widen}fn f(r: [Qubit; 3]) -> [Qubit; 4] {{ widen(mv r) }}"
    ));
    // `1 + N` and `N + 1` are the same size, as are `2 * N` and `N + N`.
    ok(
        "fn f<const N: Int>(r: [Int; 1 + N]) -> [Int; N + 1] { r }\n\
        fn g<const N: Int>(r: [Int; N + N]) -> [Int; 2 * N] { r }",
    );
    // Solved from an argument: `N + 1` given 5 is 4, `2 * N` given 6 is 3.
    ok("fn f<const N: Int>(r: ref [Int; N + 1]) -> Int { N }\n\
        fn g<const N: Int>(r: ref [Int; 2 * N]) -> Int { N }\n\
        fn h(a: [Int; 5], b: [Int; 6]) -> Int { f(ref a) + g(ref b) }");
    ok("struct R<const N: Int> { v: [Int; N] }\n\
        fn f<const N: Int>(r: R<N>) -> R<{N + 1}> { R { v: [0; N + 1] } }\n\
        fn g(r: R<2>) -> R<3> { f(r) }");
    assert_eq!(
        errors(&format!(
            "{widen}fn f(r: [Qubit; 3]) -> [Qubit; 3] {{ widen(mv r) }}"
        )),
        ["mismatched types: expected `[Qubit; 3]`, found `[Qubit; 4]`"]
    );
    // No `N` makes `2 * N` odd.
    assert_eq!(
        errors(
            "fn g<const N: Int>(r: ref [Int; 2 * N]) -> Int { N }\n\
                fn h(b: [Int; 5]) -> Int { g(ref b) }"
        )
        .first()
        .map(String::as_str),
        Some("mismatched types: expected `ref [Int; 2 * N]`, found `ref [Int; 5]`")
    );
    // Subtraction takes away a number, or terms the left side has.
    ok(
        "fn f<const N: Int>(r: [Int; N + 1 - 1]) -> [Int; N] { r }\n\
        fn g<const N: Int, const M: Int>(r: [Int; N + M - M]) -> [Int; N] { r }\n\
        fn h<const N: Int>(r: ref [Int; N - 1]) -> Int { N }\n\
        fn k(a: [Int; 4]) -> Int { h(ref a) }",
    );
    assert_eq!(
        errors("fn f<const N: Int, const M: Int>(r: [Int; N - M]) {}"),
        ["a size can only subtract a number, or sizes it adds"]
    );
    assert_eq!(
        errors("fn f(r: [Int; 1 - 2]) {}"),
        ["this size is negative"]
    );
    assert_eq!(
        errors("fn f<const N: Int, const M: Int>(r: [Int; N * M]) {}"),
        ["sizes can only be multiplied by a number"]
    );
    assert_eq!(
        errors("fn f<const N: Int>(r: [Int; N + K]) {}"),
        ["cannot find the size `K`; declare it with `const K: Int`"]
    );
}
