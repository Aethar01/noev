//! C and C++ embedding API implementing `include/noev.h` (`spec/interop.md` §1, §3).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::rc::Rc;

use noev::embed::{check_boundary_ty, show_ty};
use noev_ir::ir::{FuncId, Function, Program as IrProgram};
use noev_runtime::backend::{Backend, Exporter};
use noev_runtime::host::{HostCallContext, HostTable};
use noev_runtime::int::Int;
use noev_runtime::machine::{Machine, RuntimeError};
use noev_runtime::value::Value;
use noev_sema::ty::Width;
use noev_sema::{Ty, TyKind};

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_last_error(msg: &str) {
    let clean = msg.replace('\0', " ");
    let cs = CString::new(clean).unwrap_or_default();
    LAST_ERROR.with(|slot| *slot.borrow_mut() = cs);
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = CString::default());
}

/// Status codes returned by the C embedding API (mirrors `NoevStatus` in `include/noev.h`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoevStatus {
    /// Success.
    Ok = 0,
    /// Could not load source files from disk.
    ErrLoad = 1,
    /// Parse, type, or ownership error.
    ErrCompile = 2,
    /// Missing host implementation for an `extern fn`.
    ErrMissingHostFn = 3,
    /// Registered host name is not an `extern fn`.
    ErrUnknownHostFn = 4,
    /// Called function does not exist.
    ErrUnknownFn = 5,
    /// Signature / arity mismatch.
    ErrSignature = 6,
    /// Type cannot cross the host boundary.
    ErrBoundaryType = 7,
    /// Value conversion or range error (e.g. `Int` outside `int64_t`).
    ErrConversion = 8,
    /// Runtime error inside Noev or a host callback.
    ErrRuntime = 9,
    /// Null pointer or invalid UTF-8 argument.
    ErrNull = 10,
}

/// Value tag matching `NoevValueKind` in `include/noev.h`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoevValueKind {
    /// `()`
    Unit = 0,
    /// `Int` (`int64_t`, checked)
    Int = 1,
    /// `Float` (`double`)
    Float = 2,
    /// `f32` (`float`)
    F32 = 3,
    /// `i8`
    I8 = 4,
    /// `i16`
    I16 = 5,
    /// `i32`
    I32 = 6,
    /// `i64`
    I64 = 7,
    /// `u8`
    U8 = 8,
    /// `u16`
    U16 = 9,
    /// `u32`
    U32 = 10,
    /// `u64`
    U64 = 11,
    /// `Bool`
    Bool = 12,
    /// `Bit` (`uint8_t`, 0 or 1)
    Bit = 13,
    /// `String` (`char*`, NUL-terminated UTF-8)
    String = 14,
    /// `Complex` (`{ double re, im }`)
    Complex = 15,
    /// `c32` (`{ float re, im }`)
    C32 = 16,
    /// Raw C-layout bytes for an `extern "C" struct` or `[T; N]`.
    Bytes = 17,
}

/// Complex64 payload for [`NoevValue`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NoevComplex64 {
    /// Real part.
    pub re: f64,
    /// Imaginary part.
    pub im: f64,
}

/// Complex32 payload for [`NoevValue`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NoevComplex32 {
    /// Real part.
    pub re: f32,
    /// Imaginary part.
    pub im: f32,
}

/// Byte buffer payload for [`NoevValue`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevBytes {
    /// Pointer to bytes.
    pub ptr: *mut u8,
    /// Byte length.
    pub len: usize,
}

/// Union payload for [`NoevValue`].
#[repr(C)]
#[derive(Clone, Copy)]
pub union NoevValuePayload {
    /// `Int` or `i64`.
    pub i64: i64,
    /// `Float`.
    pub f64: f64,
    /// `f32`.
    pub f32: f32,
    /// `i8`.
    pub i8: i8,
    /// `i16`.
    pub i16: i16,
    /// `i32`.
    pub i32: i32,
    /// `u8`.
    pub u8: u8,
    /// `u16`.
    pub u16: u16,
    /// `u32`.
    pub u32: u32,
    /// `u64`.
    pub u64: u64,
    /// `Bool`.
    pub b: bool,
    /// `Bit` (0 or 1).
    pub bit: u8,
    /// `String` (NUL-terminated UTF-8).
    pub str: *mut c_char,
    /// `Complex`.
    pub c64: NoevComplex64,
    /// `c32`.
    pub c32: NoevComplex32,
    /// Raw C-layout bytes (`extern "C" struct` or `[T; N]`).
    pub bytes: NoevBytes,
}

/// Tagged classical value crossing the C embedding boundary.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NoevValue {
    /// Discriminant.
    pub kind: NoevValueKind,
    /// Payload.
    pub payload: NoevValuePayload,
}

impl NoevValue {
    fn unit() -> Self {
        NoevValue {
            kind: NoevValueKind::Unit,
            payload: NoevValuePayload { u64: 0 },
        }
    }
}

/// C host function callback signature (matches `NoevHostFn` in `include/noev.h`).
pub type NoevHostFn = extern "C" fn(
    ctx: *mut NoevHostContext,
    args: *const NoevValue,
    argc: usize,
    out: *mut NoevValue,
    user_data: *mut c_void,
) -> NoevStatus;

/// Opaque compiled program handle.
pub struct NoevProgram {
    inner: noev::Program,
}

#[derive(Clone, Copy)]
enum CBackend {
    #[cfg(feature = "ketrs")]
    Ketrs,
    Export,
}

#[derive(Clone, Copy)]
struct RegisteredCHost {
    func: NoevHostFn,
    user_data: *mut c_void,
}

/// Opaque host runtime handle.
pub struct NoevHostRuntime {
    program: *const NoevProgram,
    backend: CBackend,
    hosts: HashMap<String, RegisteredCHost>,
    seed_state: u64,
}

/// Opaque callback context passed to [`NoevHostFn`].
pub struct NoevHostContext {
    program: *const NoevProgram,
    inner: *mut HostCallContext<'static>,
}

fn map_error(err: noev::Error) -> NoevStatus {
    set_last_error(&err.to_string());
    match err {
        noev::Error::Load(_) => NoevStatus::ErrLoad,
        noev::Error::Compile(_) => NoevStatus::ErrCompile,
        noev::Error::MissingHostFn(_) => NoevStatus::ErrMissingHostFn,
        noev::Error::UnknownHostFn(_) => NoevStatus::ErrUnknownHostFn,
        noev::Error::UnknownFunction(_) => NoevStatus::ErrUnknownFn,
        noev::Error::SignatureMismatch(_) => NoevStatus::ErrSignature,
        noev::Error::BoundaryType(_) => NoevStatus::ErrBoundaryType,
        noev::Error::Conversion(_) => NoevStatus::ErrConversion,
        noev::Error::Runtime(_) => NoevStatus::ErrRuntime,
    }
}

unsafe fn read_c_str<'a>(ptr: *const c_char, what: &str) -> Result<&'a str, NoevStatus> {
    if ptr.is_null() {
        set_last_error(&format!("{what} pointer is null"));
        return Err(NoevStatus::ErrNull);
    }
    unsafe { CStr::from_ptr(ptr) }.to_str().map_err(|_| {
        set_last_error(&format!("{what} is not valid UTF-8"));
        NoevStatus::ErrNull
    })
}

/// Returns the thread-local error message from the most recent failing C API call.
#[unsafe(no_mangle)]
pub extern "C" fn noev_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Compiles a `.nv` file or package directory from disk.
///
/// # Safety
/// `path` must be a valid NUL-terminated UTF-8 string and `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_program_compile_file(
    path: *const c_char,
    out: *mut *mut NoevProgram,
) -> NoevStatus {
    clear_last_error();
    if out.is_null() {
        set_last_error("out pointer is null");
        return NoevStatus::ErrNull;
    }
    let path = match unsafe { read_c_str(path, "path") } {
        Ok(s) => s,
        Err(st) => return st,
    };
    match noev::Program::compile_file(path) {
        Ok(program) => {
            unsafe { *out = Box::into_raw(Box::new(NoevProgram { inner: program })) };
            NoevStatus::Ok
        }
        Err(err) => map_error(err),
    }
}

/// Compiles a single-file Noev program from source text.
///
/// # Safety
/// `source` must be a valid NUL-terminated UTF-8 string and `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_program_compile_source(
    source: *const c_char,
    out: *mut *mut NoevProgram,
) -> NoevStatus {
    clear_last_error();
    if out.is_null() {
        set_last_error("out pointer is null");
        return NoevStatus::ErrNull;
    }
    let source = match unsafe { read_c_str(source, "source") } {
        Ok(s) => s,
        Err(st) => return st,
    };
    match noev::Program::compile_source(source) {
        Ok(program) => {
            unsafe { *out = Box::into_raw(Box::new(NoevProgram { inner: program })) };
            NoevStatus::Ok
        }
        Err(err) => map_error(err),
    }
}

/// Frees a [`NoevProgram`] created by `noev_program_compile_*`.
///
/// # Safety
/// `program` must be null or a valid pointer returned by `noev_program_compile_*` not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_program_free(program: *mut NoevProgram) {
    if !program.is_null() {
        drop(unsafe { Box::from_raw(program) });
    }
}

/// Creates a runtime for `program` using the default simulator (`ketrs` when enabled).
///
/// # Safety
/// `program` must outlive the returned runtime; `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_new(
    program: *const NoevProgram,
    out: *mut *mut NoevHostRuntime,
) -> NoevStatus {
    clear_last_error();
    if program.is_null() || out.is_null() {
        set_last_error("null pointer argument");
        return NoevStatus::ErrNull;
    }
    let rt = NoevHostRuntime {
        program,
        #[cfg(feature = "ketrs")]
        backend: CBackend::Ketrs,
        #[cfg(not(feature = "ketrs"))]
        backend: CBackend::Export,
        hosts: HashMap::new(),
        seed_state: 0,
    };
    unsafe { *out = Box::into_raw(Box::new(rt)) };
    NoevStatus::Ok
}

/// Creates a runtime for `program` using the circuit-export backend (works without `ketrs`).
///
/// # Safety
/// `program` must outlive the returned runtime; `out` must be non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_new_export(
    program: *const NoevProgram,
    out: *mut *mut NoevHostRuntime,
) -> NoevStatus {
    clear_last_error();
    if program.is_null() || out.is_null() {
        set_last_error("null pointer argument");
        return NoevStatus::ErrNull;
    }
    let rt = NoevHostRuntime {
        program,
        backend: CBackend::Export,
        hosts: HashMap::new(),
        seed_state: 0,
    };
    unsafe { *out = Box::into_raw(Box::new(rt)) };
    NoevStatus::Ok
}

/// Sets the PRNG seed on `rt`.
///
/// # Safety
/// `rt` must be a valid runtime pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_set_seed(rt: *mut NoevHostRuntime, seed: u64) {
    if !rt.is_null() {
        unsafe { (*rt).seed_state = seed };
    }
}

/// Registers a C host function for `extern fn <name>`.
///
/// # Safety
/// `rt` must be valid, `name` must be NUL-terminated UTF-8, and `user_data` must remain valid
/// for all subsequent calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_register_host_fn(
    rt: *mut NoevHostRuntime,
    name: *const c_char,
    func: NoevHostFn,
    user_data: *mut c_void,
) -> NoevStatus {
    clear_last_error();
    if rt.is_null() {
        set_last_error("runtime pointer is null");
        return NoevStatus::ErrNull;
    }
    let name = match unsafe { read_c_str(name, "name") } {
        Ok(s) => s,
        Err(st) => return st,
    };
    let rt = unsafe { &mut *rt };
    rt.hosts
        .insert(name.to_string(), RegisteredCHost { func, user_data });
    NoevStatus::Ok
}

/// Validates that every `extern fn` in `rt`'s program has a registered host implementation.
///
/// # Safety
/// `rt` must be a valid runtime pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_validate(rt: *const NoevHostRuntime) -> NoevStatus {
    clear_last_error();
    if rt.is_null() {
        set_last_error("runtime pointer is null");
        return NoevStatus::ErrNull;
    }
    let rt = unsafe { &*rt };
    validate_hosts(unsafe { &*rt.program }, &rt.hosts)
}

fn validate_hosts(program: &NoevProgram, hosts: &HashMap<String, RegisteredCHost>) -> NoevStatus {
    let ir = program.inner.ir();
    for name in hosts.keys() {
        let Some(id) = program.inner.function_id(name) else {
            set_last_error(&format!(
                "registered host function `{name}` is not declared as an `extern fn`"
            ));
            return NoevStatus::ErrUnknownHostFn;
        };
        if !ir.functions[id.index()].is_host_extern() {
            set_last_error(&format!("`{name}` is a Noev function, not an `extern fn`"));
            return NoevStatus::ErrUnknownHostFn;
        }
    }
    for f in &ir.functions {
        if f.is_host_extern() && !hosts.contains_key(&f.name) {
            set_last_error(&format!(
                "missing host implementation for `extern fn {}`",
                f.name
            ));
            return NoevStatus::ErrMissingHostFn;
        }
    }
    NoevStatus::Ok
}

/// Calls a Noev function named `name` with `args[0..argc]`, writing the result to `out`.
///
/// # Safety
/// `rt` must be valid, `name` must be NUL-terminated UTF-8, `args` must point to `argc` valid
/// [`NoevValue`]s (or be null when `argc == 0`), and `out` may be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_call(
    rt: *mut NoevHostRuntime,
    name: *const c_char,
    args: *const NoevValue,
    argc: usize,
    out: *mut NoevValue,
) -> NoevStatus {
    clear_last_error();
    if rt.is_null() {
        set_last_error("runtime pointer is null");
        return NoevStatus::ErrNull;
    }
    if argc > 0 && args.is_null() {
        set_last_error("args pointer is null with non-zero argc");
        return NoevStatus::ErrNull;
    }
    let name = match unsafe { read_c_str(name, "name") } {
        Ok(s) => s,
        Err(st) => return st,
    };

    let rt = unsafe { &mut *rt };
    let prog = unsafe { &*rt.program };
    let st = validate_hosts(prog, &rt.hosts);
    if st != NoevStatus::Ok {
        return st;
    }

    let (func_id, func) = match lookup_callable(&prog.inner, name) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let ir = prog.inner.ir();
    let raw_args = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(args, argc) }
    };
    let values = match convert_c_args(ir, func, raw_args) {
        Ok(v) => v,
        Err(st) => return st,
    };

    rt.seed_state = rt.seed_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = rt.seed_state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    let seed = z ^ (z >> 31);

    let exporter = Exporter::default();
    let backend: &dyn Backend = match rt.backend {
        #[cfg(feature = "ketrs")]
        CBackend::Ketrs => &noev::Ketrs,
        CBackend::Export => &exporter,
    };

    let host_table = build_c_host_table(rt.program, &rt.hosts);
    let mut machine = Machine::new(ir, backend, seed).with_hosts(Rc::new(host_table));
    let res = match machine.call(func_id, values) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(&err.message);
            return NoevStatus::ErrRuntime;
        }
    };

    if !out.is_null() {
        match noev_to_c(ir, &res, func.locals[0].ty) {
            Ok(cv) => unsafe { *out = cv },
            Err(st) => return st,
        }
    }
    NoevStatus::Ok
}

/// Re-entrantly calls a Noev function from inside a [`NoevHostFn`] callback.
///
/// # Safety
/// `ctx` must be the live context passed to the currently executing [`NoevHostFn`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_host_ctx_call(
    ctx: *mut NoevHostContext,
    name: *const c_char,
    args: *const NoevValue,
    argc: usize,
    out: *mut NoevValue,
) -> NoevStatus {
    clear_last_error();
    if ctx.is_null() {
        set_last_error("host context pointer is null");
        return NoevStatus::ErrNull;
    }
    if argc > 0 && args.is_null() {
        set_last_error("args pointer is null with non-zero argc");
        return NoevStatus::ErrNull;
    }
    let name = match unsafe { read_c_str(name, "name") } {
        Ok(s) => s,
        Err(st) => return st,
    };

    let ctx = unsafe { &mut *ctx };
    let prog = unsafe { &*ctx.program };
    let (func_id, func) = match lookup_callable(&prog.inner, name) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let ir = prog.inner.ir();
    let raw_args = if argc == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(args, argc) }
    };
    let values = match convert_c_args(ir, func, raw_args) {
        Ok(v) => v,
        Err(st) => return st,
    };

    let inner = unsafe { &mut *ctx.inner };
    let res = match inner.call(func_id, values) {
        Ok(v) => v,
        Err(err) => {
            set_last_error(&err.message);
            return NoevStatus::ErrRuntime;
        }
    };
    if !out.is_null() {
        match noev_to_c(ir, &res, func.locals[0].ty) {
            Ok(cv) => unsafe { *out = cv },
            Err(st) => return st,
        }
    }
    NoevStatus::Ok
}

/// Frees any heap-allocated string or byte buffer inside `val` and resets it to unit.
///
/// # Safety
/// `val` must be null or point to a [`NoevValue`] returned by `noev_runtime_call` or
/// `noev_host_ctx_call`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_value_free(val: *mut NoevValue) {
    if val.is_null() {
        return;
    }
    let v = unsafe { &mut *val };
    match v.kind {
        NoevValueKind::String => {
            let p = unsafe { v.payload.str };
            if !p.is_null() {
                drop(unsafe { CString::from_raw(p) });
            }
        }
        NoevValueKind::Bytes => {
            let b = unsafe { v.payload.bytes };
            if !b.ptr.is_null() && b.len > 0 {
                drop(unsafe { Vec::from_raw_parts(b.ptr, b.len, b.len) });
            }
        }
        _ => {}
    }
    *v = NoevValue::unit();
}

/// Frees a [`NoevHostRuntime`] returned by `noev_runtime_new*`.
///
/// # Safety
/// `rt` must be null or a valid pointer returned by `noev_runtime_new*` not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_runtime_free(rt: *mut NoevHostRuntime) {
    if !rt.is_null() {
        drop(unsafe { Box::from_raw(rt) });
    }
}

fn lookup_callable<'a>(
    program: &'a noev::Program,
    name: &str,
) -> Result<(FuncId, &'a Function), NoevStatus> {
    let Some(id) = program.function_id(name) else {
        set_last_error(&format!("no function named `{name}` in program"));
        return Err(NoevStatus::ErrUnknownFn);
    };
    let ir = program.ir();
    let func = &ir.functions[id.index()];
    for i in 0..func.params {
        let ty = func.locals[1 + i].ty;
        if let Err(err) = check_boundary_ty(ir, ty, &format!("parameter {}", i + 1), &func.name) {
            return Err(map_error(err));
        }
    }
    if let Err(err) = check_boundary_ty(ir, func.locals[0].ty, "return type", &func.name) {
        return Err(map_error(err));
    }
    Ok((id, func))
}

fn convert_c_args(
    ir: &IrProgram,
    func: &Function,
    args: &[NoevValue],
) -> Result<Vec<Value>, NoevStatus> {
    if args.len() != func.params {
        set_last_error(&format!(
            "`{}` expects {} argument(s), got {}",
            func.name,
            func.params,
            args.len()
        ));
        return Err(NoevStatus::ErrSignature);
    }
    let mut out = Vec::with_capacity(func.params);
    for (i, arg) in args.iter().enumerate() {
        out.push(c_to_noev(ir, arg, func.locals[1 + i].ty)?);
    }
    Ok(out)
}

fn build_c_host_table<'p>(
    program: *const NoevProgram,
    hosts: &HashMap<String, RegisteredCHost>,
) -> HostTable<'p> {
    let mut table = HostTable::new();
    for (name, &reg) in hosts {
        table.register(name.clone(), move |ctx, func_id, args| {
            let ir = ctx.program();
            let func = &ir.functions[func_id.index()];
            let mut c_args = Vec::with_capacity(args.len());
            for (i, v) in args.iter().enumerate() {
                match noev_to_c(ir, v, func.locals[1 + i].ty) {
                    Ok(cv) => c_args.push(cv),
                    Err(_) => {
                        for mut a in c_args {
                            unsafe { noev_value_free(&mut a) };
                        }
                        return Err(RuntimeError::new(last_error_string()));
                    }
                }
            }
            let mut c_out = NoevValue::unit();
            let mut c_ctx = NoevHostContext {
                program,
                inner: (ctx as *mut HostCallContext<'_>).cast::<HostCallContext<'static>>(),
            };
            let st = (reg.func)(
                &mut c_ctx,
                if c_args.is_empty() {
                    ptr::null()
                } else {
                    c_args.as_ptr()
                },
                c_args.len(),
                &mut c_out,
                reg.user_data,
            );
            for mut a in c_args {
                unsafe { noev_value_free(&mut a) };
            }
            if st != NoevStatus::Ok {
                let msg = last_error_string();
                return Err(RuntimeError::new(if msg.is_empty() {
                    format!("C host function `{}` failed with status {st:?}", func.name)
                } else {
                    msg
                }));
            }
            c_to_noev(ir, &c_out, func.locals[0].ty)
                .map_err(|_| RuntimeError::new(last_error_string()))
        });
    }
    table
}

fn last_error_string() -> String {
    LAST_ERROR.with(|slot| slot.borrow().to_string_lossy().into_owned())
}

fn c_to_noev(ir: &IrProgram, val: &NoevValue, ty: Ty) -> Result<Value, NoevStatus> {
    match (ir.types.kind(ty), val.kind) {
        (TyKind::Unit, NoevValueKind::Unit) => Ok(Value::Unit),
        (TyKind::Int, NoevValueKind::Int | NoevValueKind::I64) => {
            Ok(Value::Int(Int::Small(unsafe { val.payload.i64 })))
        }
        (TyKind::Float, NoevValueKind::Float) => Ok(Value::Float(unsafe { val.payload.f64 })),
        (TyKind::F32, NoevValueKind::F32) => Ok(Value::F32(unsafe { val.payload.f32 })),
        (TyKind::I8, NoevValueKind::I8) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.i8 }),
            Width {
                bits: 8,
                signed: true,
            },
        )),
        (TyKind::I16, NoevValueKind::I16) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.i16 }),
            Width {
                bits: 16,
                signed: true,
            },
        )),
        (TyKind::I32, NoevValueKind::I32) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.i32 }),
            Width {
                bits: 32,
                signed: true,
            },
        )),
        (TyKind::I64, NoevValueKind::I64 | NoevValueKind::Int) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.i64 }),
            Width {
                bits: 64,
                signed: true,
            },
        )),
        (TyKind::U8, NoevValueKind::U8) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.u8 }),
            Width {
                bits: 8,
                signed: false,
            },
        )),
        (TyKind::U16, NoevValueKind::U16) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.u16 }),
            Width {
                bits: 16,
                signed: false,
            },
        )),
        (TyKind::U32, NoevValueKind::U32) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.u32 }),
            Width {
                bits: 32,
                signed: false,
            },
        )),
        (TyKind::U64, NoevValueKind::U64) => Ok(Value::Sized(
            i128::from(unsafe { val.payload.u64 }),
            Width {
                bits: 64,
                signed: false,
            },
        )),
        (TyKind::Bool, NoevValueKind::Bool) => Ok(Value::Bool(unsafe { val.payload.b })),
        (TyKind::Bit, NoevValueKind::Bit) => match unsafe { val.payload.bit } {
            0 => Ok(Value::Bit(false)),
            1 => Ok(Value::Bit(true)),
            other => {
                set_last_error(&format!("Bit value must be 0 or 1, got {other}"));
                Err(NoevStatus::ErrConversion)
            }
        },
        (TyKind::String, NoevValueKind::String) => {
            let s = unsafe { read_c_str(val.payload.str, "string value") }?;
            Ok(Value::Str(Rc::from(s)))
        }
        (TyKind::Complex, NoevValueKind::Complex) => {
            let c = unsafe { val.payload.c64 };
            Ok(Value::Complex(c.re, c.im))
        }
        (TyKind::C32, NoevValueKind::C32) => {
            let c = unsafe { val.payload.c32 };
            Ok(Value::C32(c.re, c.im))
        }
        (TyKind::Array(..) | TyKind::Struct(_), NoevValueKind::Bytes) => {
            let b = unsafe { val.payload.bytes };
            let slice = if b.len == 0 {
                &[]
            } else if b.ptr.is_null() {
                set_last_error("bytes pointer is null with non-zero length");
                return Err(NoevStatus::ErrNull);
            } else {
                unsafe { std::slice::from_raw_parts(b.ptr, b.len) }
            };
            let (expected_size, _) = c_layout_size_align(ir, ty)?;
            if slice.len() != expected_size {
                set_last_error(&format!(
                    "expected {} bytes for `{}`, got {}",
                    expected_size,
                    show_ty(ir, ty),
                    slice.len()
                ));
                return Err(NoevStatus::ErrConversion);
            }
            let mut offset = 0usize;
            read_c_bytes(ir, slice, &mut offset, ty)
        }
        _ => {
            set_last_error(&format!(
                "NoevValue kind {:?} does not match Noev type `{}`",
                val.kind,
                show_ty(ir, ty)
            ));
            Err(NoevStatus::ErrSignature)
        }
    }
}

fn noev_to_c(ir: &IrProgram, val: &Value, ty: Ty) -> Result<NoevValue, NoevStatus> {
    match (ir.types.kind(ty), val) {
        (TyKind::Unit, Value::Unit) => Ok(NoevValue::unit()),
        (TyKind::Int, Value::Int(n)) => {
            let Some(v) = n.to_i64() else {
                set_last_error(&format!(
                    "Noev `Int` value `{n}` is outside the 64-bit signed integer range"
                ));
                return Err(NoevStatus::ErrConversion);
            };
            Ok(NoevValue {
                kind: NoevValueKind::Int,
                payload: NoevValuePayload { i64: v },
            })
        }
        (TyKind::Float, Value::Float(v)) => Ok(NoevValue {
            kind: NoevValueKind::Float,
            payload: NoevValuePayload { f64: *v },
        }),
        (TyKind::F32, Value::F32(v)) => Ok(NoevValue {
            kind: NoevValueKind::F32,
            payload: NoevValuePayload { f32: *v },
        }),
        (TyKind::I8, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::I8,
            payload: NoevValuePayload { i8: *v as i8 },
        }),
        (TyKind::I16, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::I16,
            payload: NoevValuePayload { i16: *v as i16 },
        }),
        (TyKind::I32, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::I32,
            payload: NoevValuePayload { i32: *v as i32 },
        }),
        (TyKind::I64, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::I64,
            payload: NoevValuePayload { i64: *v as i64 },
        }),
        (TyKind::U8, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::U8,
            payload: NoevValuePayload { u8: *v as u8 },
        }),
        (TyKind::U16, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::U16,
            payload: NoevValuePayload { u16: *v as u16 },
        }),
        (TyKind::U32, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::U32,
            payload: NoevValuePayload { u32: *v as u32 },
        }),
        (TyKind::U64, Value::Sized(v, _)) => Ok(NoevValue {
            kind: NoevValueKind::U64,
            payload: NoevValuePayload { u64: *v as u64 },
        }),
        (TyKind::Bool, Value::Bool(b)) => Ok(NoevValue {
            kind: NoevValueKind::Bool,
            payload: NoevValuePayload { b: *b },
        }),
        (TyKind::Bit, Value::Bit(b)) => Ok(NoevValue {
            kind: NoevValueKind::Bit,
            payload: NoevValuePayload { bit: u8::from(*b) },
        }),
        (TyKind::String, Value::Str(s)) => {
            let cs = CString::new(s.as_ref()).map_err(|_| {
                set_last_error("Noev string contains an interior NUL byte");
                NoevStatus::ErrConversion
            })?;
            Ok(NoevValue {
                kind: NoevValueKind::String,
                payload: NoevValuePayload { str: cs.into_raw() },
            })
        }
        (TyKind::Complex, Value::Complex(re, im)) => Ok(NoevValue {
            kind: NoevValueKind::Complex,
            payload: NoevValuePayload {
                c64: NoevComplex64 { re: *re, im: *im },
            },
        }),
        (TyKind::C32, Value::C32(re, im)) => Ok(NoevValue {
            kind: NoevValueKind::C32,
            payload: NoevValuePayload {
                c32: NoevComplex32 { re: *re, im: *im },
            },
        }),
        (TyKind::Array(..) | TyKind::Struct(_), Value::Aggregate(_)) => {
            let (size, _) = c_layout_size_align(ir, ty)?;
            let mut buf = vec![0u8; size];
            let mut offset = 0usize;
            write_c_bytes(ir, val, ty, &mut buf, &mut offset)?;
            let mut boxed = buf.into_boxed_slice();
            let ptr = boxed.as_mut_ptr();
            let len = boxed.len();
            std::mem::forget(boxed);
            Ok(NoevValue {
                kind: NoevValueKind::Bytes,
                payload: NoevValuePayload {
                    bytes: NoevBytes { ptr, len },
                },
            })
        }
        _ => {
            set_last_error(&format!(
                "Noev type `{}` cannot be converted to a C NoevValue",
                show_ty(ir, ty)
            ));
            Err(NoevStatus::ErrConversion)
        }
    }
}

fn align_up(offset: usize, align: usize) -> usize {
    (offset + align - 1) & !(align - 1)
}

fn c_layout_size_align(ir: &IrProgram, ty: Ty) -> Result<(usize, usize), NoevStatus> {
    match ir.types.kind(ty) {
        TyKind::Bool | TyKind::Bit | TyKind::I8 | TyKind::U8 => Ok((1, 1)),
        TyKind::I16 | TyKind::U16 => Ok((2, 2)),
        TyKind::I32 | TyKind::U32 | TyKind::F32 => Ok((4, 4)),
        TyKind::I64 | TyKind::U64 | TyKind::Int | TyKind::Float => Ok((8, 8)),
        TyKind::Array(elem, len) => {
            let (es, ea) = c_layout_size_align(ir, *elem)?;
            Ok((es * (*len as usize), ea))
        }
        TyKind::Struct(id) => {
            let def = &ir.types.structs[id.index()];
            if !def.is_extern_c || def.variants.is_some() {
                set_last_error(&format!(
                    "`{}` is not an `extern \"C\" struct` and has no defined C layout",
                    show_ty(ir, ty)
                ));
                return Err(NoevStatus::ErrBoundaryType);
            }
            let mut off = 0usize;
            let mut max_a = 1usize;
            for &(_, fty) in &def.fields {
                let (fs, fa) = c_layout_size_align(ir, fty)?;
                off = align_up(off, fa) + fs;
                max_a = max_a.max(fa);
            }
            Ok((align_up(off, max_a), max_a))
        }
        _ => {
            set_last_error(&format!(
                "`{}` cannot cross as raw C bytes",
                show_ty(ir, ty)
            ));
            Err(NoevStatus::ErrBoundaryType)
        }
    }
}

fn read_c_bytes(
    ir: &IrProgram,
    buf: &[u8],
    offset: &mut usize,
    ty: Ty,
) -> Result<Value, NoevStatus> {
    let (sz, al) = c_layout_size_align(ir, ty)?;
    *offset = align_up(*offset, al);
    let start = *offset;
    let slice = &buf[start..start + sz];
    let val = match ir.types.kind(ty) {
        TyKind::Bool => Value::Bool(slice[0] != 0),
        TyKind::Bit => Value::Bit(slice[0] != 0),
        TyKind::I8 => Value::Sized(
            i128::from(slice[0] as i8),
            Width {
                bits: 8,
                signed: true,
            },
        ),
        TyKind::U8 => Value::Sized(
            i128::from(slice[0]),
            Width {
                bits: 8,
                signed: false,
            },
        ),
        TyKind::I16 => Value::Sized(
            i128::from(i16::from_ne_bytes(slice.try_into().unwrap())),
            Width {
                bits: 16,
                signed: true,
            },
        ),
        TyKind::U16 => Value::Sized(
            i128::from(u16::from_ne_bytes(slice.try_into().unwrap())),
            Width {
                bits: 16,
                signed: false,
            },
        ),
        TyKind::I32 => Value::Sized(
            i128::from(i32::from_ne_bytes(slice.try_into().unwrap())),
            Width {
                bits: 32,
                signed: true,
            },
        ),
        TyKind::U32 => Value::Sized(
            i128::from(u32::from_ne_bytes(slice.try_into().unwrap())),
            Width {
                bits: 32,
                signed: false,
            },
        ),
        TyKind::I64 => Value::Sized(
            i128::from(i64::from_ne_bytes(slice.try_into().unwrap())),
            Width {
                bits: 64,
                signed: true,
            },
        ),
        TyKind::U64 => Value::Sized(
            i128::from(u64::from_ne_bytes(slice.try_into().unwrap())),
            Width {
                bits: 64,
                signed: false,
            },
        ),
        TyKind::Int => Value::Int(Int::Small(i64::from_ne_bytes(slice.try_into().unwrap()))),
        TyKind::F32 => Value::F32(f32::from_ne_bytes(slice.try_into().unwrap())),
        TyKind::Float => Value::Float(f64::from_ne_bytes(slice.try_into().unwrap())),
        &TyKind::Array(elem, len) => {
            let mut pos = start;
            let mut items = Vec::with_capacity(len as usize);
            for _ in 0..len {
                items.push(read_c_bytes(ir, buf, &mut pos, elem)?);
            }
            Value::Aggregate(items)
        }
        &TyKind::Struct(id) => {
            let def = &ir.types.structs[id.index()];
            let mut pos = start;
            let mut items = Vec::with_capacity(def.fields.len());
            for &(_, fty) in &def.fields {
                items.push(read_c_bytes(ir, buf, &mut pos, fty)?);
            }
            Value::Aggregate(items)
        }
        _ => return Err(NoevStatus::ErrBoundaryType),
    };
    *offset = start + sz;
    Ok(val)
}

fn write_c_bytes(
    ir: &IrProgram,
    val: &Value,
    ty: Ty,
    buf: &mut [u8],
    offset: &mut usize,
) -> Result<(), NoevStatus> {
    let (sz, al) = c_layout_size_align(ir, ty)?;
    *offset = align_up(*offset, al);
    let start = *offset;
    let dst = &mut buf[start..start + sz];
    match (ir.types.kind(ty), val) {
        (TyKind::Bool, Value::Bool(b)) => dst[0] = u8::from(*b),
        (TyKind::Bit, Value::Bit(b)) => dst[0] = u8::from(*b),
        (TyKind::I8, Value::Sized(v, _)) => dst[0] = (*v as i8) as u8,
        (TyKind::U8, Value::Sized(v, _)) => dst[0] = *v as u8,
        (TyKind::I16, Value::Sized(v, _)) => dst.copy_from_slice(&(*v as i16).to_ne_bytes()),
        (TyKind::U16, Value::Sized(v, _)) => dst.copy_from_slice(&(*v as u16).to_ne_bytes()),
        (TyKind::I32, Value::Sized(v, _)) => dst.copy_from_slice(&(*v as i32).to_ne_bytes()),
        (TyKind::U32, Value::Sized(v, _)) => dst.copy_from_slice(&(*v as u32).to_ne_bytes()),
        (TyKind::I64, Value::Sized(v, _)) => dst.copy_from_slice(&(*v as i64).to_ne_bytes()),
        (TyKind::U64, Value::Sized(v, _)) => dst.copy_from_slice(&(*v as u64).to_ne_bytes()),
        (TyKind::Int, Value::Int(n)) => {
            let Some(v) = n.to_i64() else {
                set_last_error(&format!("`Int` `{n}` outside 64-bit range"));
                return Err(NoevStatus::ErrConversion);
            };
            dst.copy_from_slice(&v.to_ne_bytes());
        }
        (TyKind::F32, Value::F32(v)) => dst.copy_from_slice(&v.to_ne_bytes()),
        (TyKind::Float, Value::Float(v)) => dst.copy_from_slice(&v.to_ne_bytes()),
        (&TyKind::Array(elem, _), Value::Aggregate(items)) => {
            let mut pos = start;
            for item in items {
                write_c_bytes(ir, item, elem, buf, &mut pos)?;
            }
        }
        (&TyKind::Struct(id), Value::Aggregate(items)) => {
            let def = &ir.types.structs[id.index()];
            let mut pos = start;
            for (item, &(_, fty)) in items.iter().zip(&def.fields) {
                write_c_bytes(ir, item, fty, buf, &mut pos)?;
            }
        }
        _ => return Err(NoevStatus::ErrConversion),
    }
    *offset = start + sz;
    Ok(())
}
