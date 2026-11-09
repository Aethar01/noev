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
