//! Cranelift & LLVM native codegen throughput benchmark.
#![allow(missing_docs)]

use criterion::{BatchSize, Criterion, Throughput, black_box, criterion_group, criterion_main};
use noev_codegen::{CodeGenerator, CodegenOptions, OptLevel};

fn lower_source(source: &str) -> noev_ir::Program {
    let sources = noev_syntax::SourceMap::single("bench.nv", source);
    let parsed = noev_syntax::parse(source);
    assert!(
        parsed.diagnostics.is_empty(),
        "parse errors: {:?}",
        parsed.diagnostics
    );
    let ast = parsed.ast;
    let (typed, type_diags) = noev_sema::check(&ast);
    assert!(type_diags.is_empty(), "type errors: {type_diags:?}");
    let own_diags = noev_sema::check_ownership(&ast, &typed);
    assert!(own_diags.is_empty(), "ownership errors: {own_diags:?}");
    let (circuits, _) = noev_sema::check_circuits(&ast, &typed);
    noev_ir::lower(&ast, &typed, &circuits, &sources)
}

fn generated_source(functions: usize) -> String {
    let mut out = String::new();
    for i in 0..functions {
        out.push_str(&format!(
            "fn work_{i}(n: Int) -> Int {{\n\
             \tlet mut acc = 0\n\
             \tlet mut k = 0\n\
             \twhile k < n {{\n\
             \t\tacc = acc + k * 3 - 1\n\
             \t\tk = k + 1\n\
             \t}}\n\
             \tacc\n\
             }}\n\
             fn bell_{i}(theta: Float) -> [Bit; 2] {{\n\
             \tlet mut q = |00>\n\
             \tq[0].h()\n\
             \tq[0].rz(theta)\n\
             \tctrl q[0] {{ q[1].x() }}\n\
             \tmv q.measure()\n\
             }}\n"
        ));
    }
    out.push_str("fn main() {\n\tlet _ = work_0(10)\n\tlet _ = bell_0(0.5)\n}\n");
    out
}

fn bench_codegen(c: &mut Criterion) {
    let src = generated_source(50);
    let program = lower_source(&src);
    let fn_count = program.functions.len() as u64;

    let mut group = c.benchmark_group("codegen");
    group.throughput(Throughput::Elements(fn_count));
    #[cfg(feature = "cranelift")]
    group.bench_function("cranelift_100_fns", |b| {
        b.iter_batched(
            || &program,
            |prog| {
                let obj = noev_codegen::CraneliftCodegen
                    .emit_object(prog, CodegenOptions::default())
                    .expect("cranelift emit_object");
                black_box(obj.len())
            },
            BatchSize::SmallInput,
        );
    });
    #[cfg(feature = "llvm")]
    group.bench_function("llvm_ir_100_fns", |b| {
        b.iter_batched(
            || &program,
            |prog| {
                let ir = noev_codegen::llvm::LlvmCodegen
                    .emit_llvm_ir(prog, CodegenOptions::default())
                    .expect("llvm emit_llvm_ir");
                black_box(ir.len())
            },
            BatchSize::SmallInput,
        );
    });
    #[cfg(feature = "llvm")]
    group.bench_function("llvm_O2_obj_100_fns", |b| {
        b.iter_batched(
            || &program,
            |prog| {
                let obj = noev_codegen::llvm::LlvmCodegen
                    .emit_object(
                        prog,
                        CodegenOptions {
                            opt: OptLevel::Speed,
                            ..CodegenOptions::default()
                        },
                    )
                    .expect("llvm emit_object");
                black_box(obj.len())
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_codegen);
criterion_main!(benches);
