//! Throughput benchmarks for type, ownership and circuit checking. Run with `cargo bench -p noev-sema`.
//!
//! Each UI test program is parsed once, then checked many times; repeating programs within
//! one file would define every function twice. Two generated packages of many modules measure
//! name resolution across modules. Dependency-free, like the syntax benchmark: the fastest of
//! several runs is reported.

use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

fn programs() -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/ui")];
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|e| e == "nv") {
            files.push(std::fs::read_to_string(&path).unwrap());
        }
    }
    files.sort();
    files
}

fn fastest(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .unwrap()
}

fn main() {
    let sources = programs();
    let parsed: Vec<_> = sources.iter().map(|s| noev_syntax::parse(s)).collect();
    let repeats = 500;
    let bytes: usize = sources.iter().map(String::len).sum::<usize>() * repeats;
    let lines: usize = sources.iter().map(|s| s.lines().count()).sum::<usize>() * repeats;

    let report = |phase: &str, time: Duration| {
        let secs = time.as_secs_f64();
        let mib = bytes as f64 / (1024.0 * 1024.0);
        println!(
            "{phase:<9} {:>8.1} MiB/s {:>10.0} lines/s  ({:.2} ms for {mib:.1} MiB)",
            mib / secs,
            lines as f64 / secs,
            secs * 1e3,
        );
    };

    let time = fastest(10, || {
        for _ in 0..repeats {
            for p in &parsed {
                black_box(noev_sema::resolve::resolve(black_box(&p.ast)));
            }
        }
    });
    report("resolve", time);

    let time = fastest(10, || {
        for _ in 0..repeats {
            for p in &parsed {
                black_box(noev_sema::check(black_box(&p.ast)));
            }
        }
    });
    report("types", time);

    let typed: Vec<_> = parsed.iter().map(|p| noev_sema::check(&p.ast).0).collect();
    let time = fastest(10, || {
        for _ in 0..repeats {
            for (p, t) in parsed.iter().zip(&typed) {
                black_box(noev_sema::check_ownership(black_box(&p.ast), black_box(t)));
            }
        }
    });
    report("ownership", time);

    // Circuits are only checked in programs that pass the other checks.
    let valid: Vec<_> = parsed
        .iter()
        .zip(&typed)
        .filter(|(p, t)| {
            p.diagnostics.is_empty()
                && noev_sema::check(&p.ast).1.is_empty()
                && noev_sema::check_ownership(&p.ast, t).is_empty()
        })
        .collect();
    let valid_bytes: usize = sources
        .iter()
        .zip(&parsed)
        .filter(|(_, p)| valid.iter().any(|(v, _)| std::ptr::eq(*v, *p)))
        .map(|(s, _)| s.len())
        .sum::<usize>()
        * repeats;
    let time = fastest(10, || {
        for _ in 0..repeats {
            for (p, t) in &valid {
                black_box(noev_sema::check_circuits(black_box(&p.ast), black_box(t)));
            }
        }
    });
    let mib = valid_bytes as f64 / (1024.0 * 1024.0);
    println!(
        "{:<9} {:>8.1} MiB/s  ({:.2} ms for {mib:.2} MiB of valid programs)",
        "circuits",
        mib / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
    );

    impls(400);
    operators(2000);
    with_do(1000);
    lifetimes(1000);
    inferred("inferred", 2000, true);
    inferred("irrev", 2000, false);
    modules("modules", 2000, |i| {
        // Each module uses the one before it by path, by import and by glob.
        let mut text =
            format!("pub fn f{i}() -> Int {{ {i} }}\npub struct S{i} {{ pub x: Int }}\n");
        if i > 0 {
            let p = i - 1;
            text.push_str(&format!(
                "use pkg::m{p}::*\nuse pkg::m{p}::f{p} as g\n\
                 fn h{i}() -> Int {{ g() + f{p}() + pkg::m{p}::f{p}() + S{p} {{ x: 1 }}.x }}\n"
            ));
        }
        text
    });
    modules("reexports", 3000, |i| {
        // A chain of re-exports, each module's declared before the one it re-exports from,
        // the worst order for resolving imports.
        if i == 2999 {
            "pub fn f() {}\n".to_string()
        } else {
            format!("pub use pkg::m{}::f\n", i + 1)
        }
    });
}

/// Checks a package of `count` modules `m0`, `m1`, …, with `source(i)` as module `mi`'s text.
fn modules(phase: &str, count: usize, source: impl Fn(usize) -> String) {
    let texts: Vec<String> = (0..count).map(&source).collect();
    let mut ast = noev_syntax::Ast::new();
    let package = ast.add_package("bench");
    let root = ast.package(package).root;
    let mut base = 0;
    for (i, text) in texts.iter().enumerate() {
        let module = ast.add_module(root, &format!("m{i}"));
        let errors = noev_syntax::parse_module(&mut ast, module, text, base);
        assert!(errors.is_empty(), "{errors:?}");
        base += text.len() as u32 + 1;
    }
    ast.collect_root();
    let (_, errors) = noev_sema::check(&ast);
    assert!(errors.is_empty(), "{:?}", &errors[..errors.len().min(3)]);
    let time = fastest(10, || {
        black_box(noev_sema::check(black_box(&ast)));
    });
    let lines: usize = texts.iter().map(|t| t.lines().count()).sum();
    println!(
        "{phase:<9} {:>8.0} modules/s {:>8.0} lines/s  ({:.2} ms for {count} modules)",
        count as f64 / time.as_secs_f64(),
        lines as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3,
    );
}

/// Checks a program with `count` `impl`s of one generic trait for one type, each called once
/// with its own argument: finding an `impl` looks through the trait's `impl`s.
/// `+` and `*` on a struct of the program's own, each a call of a `std::ops` trait's
/// function picked by the right side's type, checked through to ownership.
fn operators(count: usize) {
    let mut text = String::from(
        "use std::ops::{Add, Mul}\nstruct V { x: Int }\n\
         impl Add<V> for V { type Output = V\n fn add(mv self, r: V) -> V { V { x: self.x + r.x } } }\n\
         impl Mul<Int> for V { type Output = V\n fn mul(mv self, k: Int) -> V { V { x: self.x * k } } }\n\
         fn main() {\n let v = V { x: 1 }\n",
    );
    for i in 0..count {
        text.push_str(&format!(" let v{i} = v + v * {i}\n"));
    }
    text.push_str("}\n");
    let parsed = noev_syntax::parse(&text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let errors = noev_sema::check_ownership(&parsed.ast, &typed);
    assert!(errors.is_empty(), "{errors:?}");
    let types = fastest(5, || {
        black_box(noev_sema::check(black_box(&parsed.ast)));
    });
    let ownership = fastest(5, || {
        black_box(noev_sema::check_ownership(black_box(&parsed.ast), &typed));
    });
    println!(
        "{:<9} {:>10.0} ops/s  ({:.2} ms types, {:.2} ms ownership, for {} operators)",
        "operators",
        (2 * count) as f64 / (types + ownership).as_secs_f64(),
        types.as_secs_f64() * 1e3,
        ownership.as_secs_f64() * 1e3,
        2 * count,
    );
}

/// Checks `count` `with`/`do` blocks with ancillas and outer controls, measuring type and
/// ownership checking throughput.
fn with_do(count: usize) {
    let mut text = String::from("fn f(a: ref Qubit, b: ref Qubit, out: mut Qubit) {\n");
    for i in 0..count {
        text.push_str(&format!(
            " with {{ let t{i} = |0>\n ctrl a, b {{ t{i}.x() }} }} do {{ ctrl t{i} {{ out.x() }} }}\n"
        ));
    }
    text.push_str("}\n");
    let parsed = noev_syntax::parse(&text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let errors = noev_sema::check_ownership(&parsed.ast, &typed);
    assert!(errors.is_empty(), "{errors:?}");
    let types = fastest(5, || {
        black_box(noev_sema::check(black_box(&parsed.ast)));
    });
    let ownership = fastest(5, || {
        black_box(noev_sema::check_ownership(black_box(&parsed.ast), &typed));
    });
    println!(
        "{:<9} {:>10.0} blocks/s  ({:.2} ms types, {:.2} ms ownership, for {count} with/do blocks)",
        "with_do",
        count as f64 / (types + ownership).as_secs_f64(),
        types.as_secs_f64() * 1e3,
        ownership.as_secs_f64() * 1e3,
    );
}

fn impls(count: usize) {
    let mut text = String::from(
        "trait Width<const N: Int> { fn width(ref self, r: ref [Int; N]) -> Int }\nstruct W {}\n",
    );
    for i in 1..=count {
        text.push_str(&format!(
            "impl Width<{i}> for W {{ fn width(ref self, r: ref [Int; {i}]) -> Int {{ {i} }} }}\n"
        ));
    }
    text.push_str("fn main() {\n let w = W {}\n");
    for i in 1..=count {
        text.push_str(&format!(
            " let r{i}: [Int; {i}] = [0; {i}]\n print(w.width(ref r{i}))\n"
        ));
    }
    text.push_str("}\n");
    let parsed = noev_syntax::parse(&text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (_, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let repeats = 5;
    let time = fastest(10, || {
        for _ in 0..repeats {
            black_box(noev_sema::check(black_box(&parsed.ast)));
        }
    });
    println!(
        "{:<9} {:>10.0} calls/s  ({:.2} ms for {count} impls and calls)",
        "impls",
        (count * repeats) as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3 / repeats as f64,
    );
}

/// Checks a chain of `count` generic functions with no bounds written, each passing its `T`
/// to the one before it, the first calling `x.area()` and copying `x`: their bounds are
/// inferred. Declared last to first, the worst order for following needs through calls.
/// Unless `area` is `rev`, every function is irreversible because the first is, which
/// measures reversibility inference in the same order.
fn inferred(phase: &str, count: usize, rev: bool) {
    let mut text = String::from(
        "trait Shape { @fn area(ref self) -> Float }\nstruct Sq {}\n\
         impl Shape for Sq { @fn area(ref self) -> Float { 1.0 } }\n\
         fn main() { print(f%(Sq {})) }\n",
    )
    .replace('%', &(count - 1).to_string())
    .replace('@', if rev { "rev " } else { "" });
    for i in (1..count).rev() {
        text.push_str(&format!("fn f{i}<T>(x: T) -> Float {{ f{}(x) }}\n", i - 1));
    }
    text.push_str("fn f0<T>(x: T) -> Float { let y = x\n y.area() + x.area() }\n");
    let parsed = noev_syntax::parse(&text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let errors = noev_sema::check_ownership(&parsed.ast, &typed);
    assert!(errors.is_empty(), "{errors:?}");
    let types = fastest(5, || {
        black_box(noev_sema::check(black_box(&parsed.ast)));
    });
    let ownership = fastest(5, || {
        black_box(noev_sema::check_ownership(black_box(&parsed.ast), &typed));
    });
    let circuits = fastest(5, || {
        black_box(noev_sema::check_circuits(black_box(&parsed.ast), &typed));
    });
    println!(
        "{:<9} {:>10.0} fns/s  ({:.2} ms types, {:.2} ms ownership, {:.2} ms circuits, for a chain of {count} functions)",
        phase,
        count as f64 / (types + ownership + circuits).as_secs_f64(),
        types.as_secs_f64() * 1e3,
        ownership.as_secs_f64() * 1e3,
        circuits.as_secs_f64() * 1e3,
    );
}

/// Checks `count` functions exercising NLL, Polonius conditional borrow returns, two-phase
/// receiver lends, field-level method lends, and structs holding references.
fn lifetimes(count: usize) {
    let mut text = String::from(
        "struct Pair { a: Qubit\n b: Qubit\n\
           fn flip_b(mut self) { self.b.x() }\n\
           fn a_mut(mut self) -> mut[self.a] Qubit { mut self.a }\n\
           fn b_mut(mut self) -> mut[self.b] Qubit { mut self.b }\n }\n\
         struct Window { q: ref Qubit }\n\
         struct Counter { n: Int\n\
           fn get(ref self) -> Int { self.n }\n\
           fn add(mut self, d: Int) { self.n += d }\n }\n\
         fn pick(a: ref Qubit, b: ref Qubit, flag: Bool) -> ref[a, b] Qubit { if flag { a } else { b } }\n",
    );
    for i in 0..count {
        text.push_str(&format!(
            "fn f{i}(flag: Bool) -> Bit {{\n\
               let mut c = Counter {{ n: {i} }}\n\
               c.add(c.get())\n\
               let p = Pair {{ a: |+>, b: |0> }}\n\
               ctrl p.a {{ p.flip_b() }}\n\
               let w = Window {{ q: ref p.a }}\n\
               let r = pick(w.q, ref p.b, flag)\n\
               ctrl r {{}}\n\
               let a = p.a_mut()\n\
               let b = p.b_mut()\n\
               swap(a, b)\n\
               mv p.a.discard()\n\
               mv p.b.measure()\n\
             }}\n"
        ));
    }
    text.push_str("fn main() { let _ = f0(true) }\n");
    let parsed = noev_syntax::parse(&text);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let (typed, errors) = noev_sema::check(&parsed.ast);
    assert!(errors.is_empty(), "{errors:?}");
    let errors = noev_sema::check_ownership(&parsed.ast, &typed);
    assert!(errors.is_empty(), "{errors:?}");
    let repeats = 5;
    let time = fastest(10, || {
        for _ in 0..repeats {
            black_box(noev_sema::check_ownership(black_box(&parsed.ast), &typed));
        }
    });
    println!(
        "{:<9} {:>10.0} fns/s  ({:.2} ms for {count} functions)",
        "lifetimes",
        (count * repeats) as f64 / time.as_secs_f64(),
        time.as_secs_f64() * 1e3 / repeats as f64,
    );
}
