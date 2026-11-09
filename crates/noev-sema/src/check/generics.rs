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
        errors("fn f<T: Measure>(x: T) {}"),
        ["cannot find the trait `Measure`"]
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
