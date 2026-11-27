//! End-to-end tests for C & C++ interop in both directions (`spec/interop.md` §3):
//! 1. Noev calling C (`extern "C"` imports, `ref`/`mut` arrays, `extern "C" struct` by value &
//!    reference including >16B structs, checked `Int <-> int64_t`, and `extern "C" fn` callbacks
//!    driving quantum expectation values) under BOTH Cranelift and LLVM code generators.
//! 2. C99 and C++17 host programs embedding Noev through `include/noev.h` and `libnoev_c.a`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use noev::{Error, Program};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn target_debug_dir() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_noev"))
        .parent()
        .expect("bin dir")
        .to_path_buf()
}

fn ensure_staticlib(pkg: &str, lib_name: &str) -> PathBuf {
    static BUILT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "noev-runtime", "-p", "noev-c"])
            .current_dir(repo_root())
            .status()
            .expect("cargo build runs");
        assert!(
            status.success(),
            "cargo build -p noev-runtime -p noev-c failed"
        );
    });
    let lib = target_debug_dir().join(lib_name);
    assert!(
        lib.is_file(),
        "expected static library for {pkg} at {}",
        lib.display()
    );
    lib
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noev_c_interop_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn noev_calls_libm_erf_both_backends() {
    ensure_staticlib("noev-runtime", "libnoev_runtime.a");
    let dir = temp_dir("erf");
    let nv = dir.join("erf.nv");
    fs::write(
        &nv,
        r#"
extern "C" {
    fn erf(x: Float) -> Float
}

fn main() {
    let y = unsafe { erf(1.0) }
    print("{y:.6}")
}
"#,
    )
    .unwrap();

    for backend in ["cranelift", "llvm"] {
        let bin = dir.join(format!("erf_{backend}"));
        let build = Command::new(env!("CARGO_BIN_EXE_noev"))
            .args(["build", "--codegen", backend, "-o"])
            .arg(&bin)
            .arg(&nv)
            .output()
            .expect("noev build runs");
        assert!(
            build.status.success(),
            "{backend} build failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = Command::new(&bin).output().expect("binary runs");
        assert!(
            run.status.success(),
            "{backend} binary failed:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "0.842701");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn noev_calls_c_arrays_structs_and_quantum_callbacks_both_backends() {
    ensure_staticlib("noev-runtime", "libnoev_runtime.a");
    let dir = temp_dir("full_c");

    // Real C library compiled with `cc -c`: arrays by ref/mut, <=16B struct by value,
    // >16B struct (byval/sret), struct by mut ref, checked int64_t, and VQE callback!
    let c_src = dir.join("helper.c");
    let c_obj = dir.join("helper.o");
    fs::write(
        &c_src,
        r#"
#include <math.h>
#include <stdint.h>

typedef struct {
    double x;
    double y;
} Point;

typedef struct {
    double x;
    double y;
    double z;
} Vec3;

void normalize3(double *v) {
    double n = sqrt(v[0]*v[0] + v[1]*v[1] + v[2]*v[2]);
    if (n > 0.0) {
        v[0] /= n;
        v[1] /= n;
        v[2] /= n;
    }
}

double dot3(const double *a, const double *b) {
    return a[0]*b[0] + a[1]*b[1] + a[2]*b[2];
}

Point point_add(Point a, Point b) {
    Point r = { a.x + b.x, a.y + b.y };
    return r;
}

void point_scale(Point *p, double s) {
    p->x *= s;
    p->y *= s;
}

Vec3 vec3_add(Vec3 a, Vec3 b) {
    Vec3 r = { a.x + b.x, a.y + b.y, a.z + b.z };
    return r;
}

int64_t add_i64(int64_t a, int64_t b) {
    return a + b;
}

double minimize(double (*cost)(double), double start) {
    double theta = start;
    double eps = 0.05;
    for (int i = 0; i < 30; i++) {
        double ep = cost(theta + eps);
        double em = cost(theta - eps);
        double grad = (ep - em) / (2.0 * eps);
        theta -= 0.5 * grad;
    }
    return cost(theta);
}

static double square_fn(double x) {
    return x * x;
}

double (*get_square_fn(void))(double) {
    return square_fn;
}
"#,
    )
    .unwrap();

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let cc_out = Command::new(&cc)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-c"])
        .arg(&c_src)
        .arg("-o")
        .arg(&c_obj)
        .output()
        .expect("cc runs");
    assert!(
        cc_out.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&cc_out.stderr)
    );

    let nv = dir.join("prog.nv");
    fs::write(
        &nv,
        r#"
extern "C" struct Point {
    x: Float,
    y: Float,
}

extern "C" struct Vec3 {
    x: Float,
    y: Float,
    z: Float,
}

extern "C" {
    fn normalize3(v: mut [Float; 3])
    fn dot3(a: ref [Float; 3], b: ref [Float; 3]) -> Float
    fn point_add(a: Point, b: Point) -> Point
    fn point_scale(p: mut Point, s: Float)
    fn vec3_add(a: Vec3, b: Vec3) -> Vec3
    fn add_i64(a: Int, b: Int) -> Int
    fn minimize(cost: extern "C" fn(Float) -> Float, start: Float) -> Float
    fn get_square_fn() -> extern "C" fn(Float) -> Float
}

fn ansatz(theta: Float) -> Qubit {
    let q = |0>
    q.ry(theta)
    mv q
}

extern "C" fn energy(theta: Float) -> Float {
    <ansatz(theta)| Z(0) |ansatz(theta)>
}

fn main() {
    let mut v = [3.0, 4.0, 0.0]
    let w = [1.0, 2.0, 3.0]
    let mut p = Point { x: 1.5, y: 2.5 }
    let q = Point { x: 0.5, y: 1.5 }
    let a = Vec3 { x: 1.0, y: 2.0, z: 3.0 }
    let b = Vec3 { x: 4.0, y: 5.0, z: 6.0 }
    unsafe {
        normalize3(mut v)
        let d = dot3(ref v, ref w)
        let sum_p = point_add(p, q)
        point_scale(mut p, 2.0)
        let sum_v = vec3_add(a, b)
        let sum_i = add_i64(100, 23)
        let min_e = minimize(energy, 0.3)
        let sq = get_square_fn()
        let sq5 = sq(5.0)
        print("{v[0]:.2} {v[1]:.2} {d:.2}")
        print("{sum_p.x:.1} {sum_p.y:.1} {p.x:.1} {p.y:.1}")
        print("{sum_v.x:.1} {sum_v.y:.1} {sum_v.z:.1}")
        print("{sum_i} {min_e:.4} {sq5:.1}")
    }
}
"#,
    )
    .unwrap();

    let expected = "\
0.60 0.80 2.20
2.0 4.0 3.0 5.0
5.0 7.0 9.0
123 -1.0000 25.0
";

    for backend in ["cranelift", "llvm"] {
        let bin = dir.join(format!("prog_{backend}"));
        let build = Command::new(env!("CARGO_BIN_EXE_noev"))
            .args(["build", "--codegen", backend, "--link"])
            .arg(&c_obj)
            .arg("-o")
            .arg(&bin)
            .arg(&nv)
            .output()
            .expect("noev build runs");
        assert!(
            build.status.success(),
            "{backend} build failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let run = Command::new(&bin).output().expect("binary runs");
        assert!(
            run.status.success(),
            "{backend} binary failed:\n{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&run.stdout),
            expected,
            "{backend} output mismatch"
        );
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn noev_c_int_overflow_checked_at_boundary() {
    ensure_staticlib("noev-runtime", "libnoev_runtime.a");
    let dir = temp_dir("int_overflow");
    let c_src = dir.join("id.c");
    let c_obj = dir.join("id.o");
    fs::write(
        &c_src,
        "#include <stdint.h>\nint64_t id64(int64_t x) { return x; }\n",
    )
    .unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    assert!(
        Command::new(&cc)
            .args(["-c"])
            .arg(&c_src)
            .arg("-o")
            .arg(&c_obj)
            .status()
            .unwrap()
            .success()
    );

    let nv = dir.join("overflow.nv");
    fs::write(
        &nv,
        r#"
extern "C" {
    fn id64(x: Int) -> Int
}

fn main() {
    let big = 1 << 80
    let y = unsafe { id64(big) }
    print(y)
}
"#,
    )
    .unwrap();

    for backend in ["cranelift", "llvm"] {
        let bin = dir.join(format!("overflow_{backend}"));
        let build = Command::new(env!("CARGO_BIN_EXE_noev"))
            .args(["build", "--codegen", backend, "--link"])
            .arg(&c_obj)
            .arg("-o")
            .arg(&bin)
            .arg(&nv)
            .output()
            .unwrap();
        assert!(build.status.success());
        let run = Command::new(&bin).output().unwrap();
        assert!(!run.status.success(), "{backend} should fail on Int > i64");
        let stderr = String::from_utf8_lossy(&run.stderr);
        assert!(
            stderr.contains("64-bit"),
            "{backend} unexpected stderr: {stderr}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compile_time_extern_c_declaration_checks() {
    for bad in [
        "extern \"C\" { fn bad(a: [Float; 3]) }",
        "extern \"C\" { fn bad() -> [Float; 3] }",
        "extern \"C\" { fn bad(q: Qubit) }",
        "extern \"C\" { fn bad() -> (Float, Float) }",
        "struct Reg { x: Float }\nextern \"C\" { fn bad(r: Reg) }",
        "extern \"C\" struct Bad { q: Qubit }",
        "extern \"C\" struct Bad { s: String }",
        "extern \"C\" struct Bad<T> { x: T }",
        "extern \"C\" { fn bad(x: Float) { } }",
    ] {
        let res = Program::compile_source(bad);
        assert!(
            matches!(res, Err(Error::Compile(_))),
            "expected compile error for `{bad}`, got {res:?}"
        );
    }
}

#[test]
fn c99_and_cpp17_hosts_embed_noev_via_header() {
    let lib_c = ensure_staticlib("noev-c", "libnoev_c.a");
    let include_dir = repo_root().join("include");
    let dir = temp_dir("c_host");

    // 1. C99 host program testing noev.h end-to-end (quantum VQE, re-entrant callback,
    //    extern "C" struct via NOEV_VAL_BYTES, checked Int overflow, validation error).
    let host_c = dir.join("host.c");
    let host_c_bin = dir.join("host_c");
    fs::write(
        &host_c,
        r#"
#include "noev.h"
#include <assert.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

typedef struct {
    double x;
    double y;
} Point;

typedef struct {
    int evals;
} OptState;

static NoevStatus c_optimize(
    NoevHostContext *ctx,
    const NoevValue *args,
    size_t argc,
    NoevValue *out,
    void *user_data
) {
    assert(argc == 1);
    assert(args[0].kind == NOEV_VAL_FLOAT);
    OptState *st = (OptState *)user_data;
    double theta = args[0].as.f64;
    double eps = 0.05;
    for (int i = 0; i < 30; i++) {
        NoevValue ap = noev_val_float(theta + eps);
        NoevValue am = noev_val_float(theta - eps);
        NoevValue ep = noev_val_unit();
        NoevValue em = noev_val_unit();
        NoevStatus s1 = noev_host_ctx_call(ctx, "energy", &ap, 1, &ep);
        if (s1 != NOEV_OK) return s1;
        NoevStatus s2 = noev_host_ctx_call(ctx, "energy", &am, 1, &em);
        if (s2 != NOEV_OK) return s2;
        st->evals += 2;
        double grad = (ep.as.f64 - em.as.f64) / (2.0 * eps);
        theta -= 0.5 * grad;
    }
    *out = noev_val_float(theta);
    return NOEV_OK;
}

int main(void) {
    const char *src =
        "extern \"C\" struct Point { x: Float, y: Float }\n"
        "extern fn optimize(start: Float) -> Float\n"
        "fn ansatz(theta: Float) -> Qubit { let q = |0>\n q.ry(theta)\n mv q }\n"
        "fn energy(theta: Float) -> Float { <ansatz(theta)| Z(0) |ansatz(theta)> }\n"
        "fn vqe() -> Float { let t = optimize(0.3)\n energy(t) }\n"
        "fn shift_point(p: Point) -> Point { Point { x: p.x + 1.0, y: p.y * 2.0 } }\n"
        "fn huge() -> Int { 1 << 80 }\n";

    NoevProgram *prog = NULL;
    assert(noev_program_compile_source(src, &prog) == NOEV_OK);

    NoevHostRuntime *rt = NULL;
    assert(noev_runtime_new(prog, &rt) == NOEV_OK);
    noev_runtime_set_seed(rt, 42);

    // Before registering `optimize`, validation must report NOEV_ERR_MISSING_HOST_FN.
    assert(noev_runtime_validate(rt) == NOEV_ERR_MISSING_HOST_FN);

    OptState state = { 0 };
    assert(noev_runtime_register_host_fn(rt, "optimize", c_optimize, &state) == NOEV_OK);
    assert(noev_runtime_validate(rt) == NOEV_OK);

    NoevValue out = noev_val_unit();
    assert(noev_runtime_call(rt, "vqe", NULL, 0, &out) == NOEV_OK);
    assert(out.kind == NOEV_VAL_FLOAT);
    assert(state.evals == 60);
    assert(fabs(out.as.f64 + 1.0) < 1e-6);

    // Pass and receive `extern "C" struct Point` as raw bytes.
    Point pt = { 2.5, 3.0 };
    NoevValue pt_arg = noev_val_bytes(&pt, sizeof(pt));
    NoevValue pt_out = noev_val_unit();
    assert(noev_runtime_call(rt, "shift_point", &pt_arg, 1, &pt_out) == NOEV_OK);
    assert(pt_out.kind == NOEV_VAL_BYTES && pt_out.as.bytes.len == sizeof(Point));
    Point res;
    memcpy(&res, pt_out.as.bytes.ptr, sizeof(Point));
    assert(fabs(res.x - 3.5) < 1e-12 && fabs(res.y - 6.0) < 1e-12);
    noev_value_free(&pt_out);

    // Int outside int64_t range fails with NOEV_ERR_CONVERSION.
    NoevValue big_out = noev_val_unit();
    assert(noev_runtime_call(rt, "huge", NULL, 0, &big_out) == NOEV_ERR_CONVERSION);
    assert(strstr(noev_last_error(), "64-bit") != NULL);

    noev_runtime_free(rt);
    noev_program_free(prog);
    printf("c99 ok\n");
    return 0;
}
"#,
    )
    .unwrap();

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let mut c_cmd = Command::new(&cc);
    c_cmd
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(&include_dir)
        .arg(&host_c)
        .arg(&lib_c)
        .arg("-o")
        .arg(&host_c_bin);
    #[cfg(target_os = "linux")]
    c_cmd.args(["-lm", "-lpthread", "-ldl"]);
    #[cfg(target_os = "macos")]
    c_cmd.args(["-lm", "-lpthread"]);
    let c_build = c_cmd.output().expect("cc runs");
    assert!(
        c_build.status.success(),
        "C99 host compile failed:\n{}",
        String::from_utf8_lossy(&c_build.stderr)
    );
    let c_run = Command::new(&host_c_bin).output().expect("host_c runs");
    assert!(
        c_run.status.success(),
        "C99 host failed:\n{}",
        String::from_utf8_lossy(&c_run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&c_run.stdout).trim(), "c99 ok");

    // 2. C++17 host program verifying `include/noev.h` compiles cleanly as C++.
    let host_cpp = dir.join("host.cpp");
    let host_cpp_bin = dir.join("host_cpp");
    fs::write(
        &host_cpp,
        r#"
#include "noev.h"
#include <cassert>
#include <cmath>
#include <iostream>

int main() {
    const char *src =
        "fn ansatz(t: Float) -> Qubit { let q = |0>\n q.ry(t)\n mv q }\n"
        "fn energy(t: Float) -> Float { <ansatz(t)| Z(0) |ansatz(t)> }\n";
    NoevProgram *prog = nullptr;
    assert(noev_program_compile_source(src, &prog) == NOEV_OK);
    NoevHostRuntime *rt = nullptr;
    assert(noev_runtime_new(prog, &rt) == NOEV_OK);
    NoevValue arg = noev_val_float(3.14159265358979323846);
    NoevValue out = noev_val_unit();
    assert(noev_runtime_call(rt, "energy", &arg, 1, &out) == NOEV_OK);
    assert(out.kind == NOEV_VAL_FLOAT);
    assert(std::fabs(out.as.f64 + 1.0) < 1e-12);
    noev_runtime_free(rt);
    noev_program_free(prog);
    std::cout << "cpp17 ok\n";
    return 0;
}
"#,
    )
    .unwrap();

    let cxx = std::env::var("CXX").unwrap_or_else(|_| "c++".into());
    if let Ok(cxx_build) = {
        let mut cmd = Command::new(&cxx);
        cmd.args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(&include_dir)
            .arg(&host_cpp)
            .arg(&lib_c)
            .arg("-o")
            .arg(&host_cpp_bin);
        #[cfg(target_os = "linux")]
        cmd.args(["-lm", "-lpthread", "-ldl"]);
        #[cfg(target_os = "macos")]
        cmd.args(["-lm", "-lpthread"]);
        cmd.output()
    } {
        assert!(
            cxx_build.status.success(),
            "C++17 host compile failed:\n{}",
            String::from_utf8_lossy(&cxx_build.stderr)
        );
        let cpp_run = Command::new(&host_cpp_bin).output().expect("host_cpp runs");
        assert!(
            cpp_run.status.success(),
            "C++17 host failed:\n{}",
            String::from_utf8_lossy(&cpp_run.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&cpp_run.stdout).trim(), "cpp17 ok");
    }

    let _ = fs::remove_dir_all(&dir);
}
