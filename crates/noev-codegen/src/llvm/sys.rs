//! Minimal dynamic loader for the stable LLVM C API (`libLLVM.so` / `libLLVM.dylib`), with
//! fallback to the `llc` CLI when no shared library is available.

#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::process::Command;
use std::ptr;
use std::sync::OnceLock;

use crate::{CodegenError, LlvmOpt};

type LlvmContextRef = *mut c_void;
type LlvmModuleRef = *mut c_void;
type LlvmMemoryBufferRef = *mut c_void;
type LlvmTargetRef = *mut c_void;
type LlvmTargetMachineRef = *mut c_void;
type LlvmPassBuilderOptionsRef = *mut c_void;
type LlvmErrorRef = *mut c_void;
type LlvmBool = c_int;

#[cfg(unix)]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

struct LlvmApi {
    context_create: unsafe extern "C" fn() -> LlvmContextRef,
    context_dispose: unsafe extern "C" fn(LlvmContextRef),
    create_mem_buf_copy:
        unsafe extern "C" fn(*const c_char, usize, *const c_char) -> LlvmMemoryBufferRef,
    get_buf_start: unsafe extern "C" fn(LlvmMemoryBufferRef) -> *const u8,
    get_buf_size: unsafe extern "C" fn(LlvmMemoryBufferRef) -> usize,
    dispose_mem_buf: unsafe extern "C" fn(LlvmMemoryBufferRef),
    parse_ir_in_context: unsafe extern "C" fn(
        LlvmContextRef,
        LlvmMemoryBufferRef,
        *mut LlvmModuleRef,
        *mut *mut c_char,
    ) -> LlvmBool,
    verify_module: unsafe extern "C" fn(LlvmModuleRef, c_int, *mut *mut c_char) -> LlvmBool,
    dispose_module: unsafe extern "C" fn(LlvmModuleRef),
    get_default_triple: unsafe extern "C" fn() -> *mut c_char,
    set_target: Option<unsafe extern "C" fn(LlvmModuleRef, *const c_char)>,
    get_target_from_triple:
        unsafe extern "C" fn(*const c_char, *mut LlvmTargetRef, *mut *mut c_char) -> LlvmBool,
    create_target_machine: unsafe extern "C" fn(
        LlvmTargetRef,
        *const c_char,
        *const c_char,
        *const c_char,
        c_int,
        c_int,
        c_int,
    ) -> LlvmTargetMachineRef,
    dispose_target_machine: unsafe extern "C" fn(LlvmTargetMachineRef),
    emit_to_mem_buf: unsafe extern "C" fn(
        LlvmTargetMachineRef,
        LlvmModuleRef,
        c_int,
        *mut *mut c_char,
        *mut LlvmMemoryBufferRef,
    ) -> LlvmBool,
    create_pass_opts: Option<unsafe extern "C" fn() -> LlvmPassBuilderOptionsRef>,
    dispose_pass_opts: Option<unsafe extern "C" fn(LlvmPassBuilderOptionsRef)>,
    run_passes: Option<
        unsafe extern "C" fn(
            LlvmModuleRef,
            *const c_char,
            LlvmTargetMachineRef,
            LlvmPassBuilderOptionsRef,
        ) -> LlvmErrorRef,
    >,
    get_error_message: Option<unsafe extern "C" fn(LlvmErrorRef) -> *mut c_char>,
    dispose_error_message: Option<unsafe extern "C" fn(*mut c_char)>,
    dispose_message: unsafe extern "C" fn(*mut c_char),
}

// Function pointers loaded from a process-lifetime shared library are thread-safe to read.
unsafe impl Send for LlvmApi {}
unsafe impl Sync for LlvmApi {}

static LLVM_API: OnceLock<Option<LlvmApi>> = OnceLock::new();

#[cfg(unix)]
unsafe fn load_sym<T>(handle: *mut c_void, name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let ptr = unsafe { dlsym(handle, c.as_ptr()) };
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute_copy(&ptr) })
    }
}

#[cfg(unix)]
fn try_load_llvm() -> Option<LlvmApi> {
    const RTLD_NOW: c_int = 0x2;
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(env) = std::env::var("NOEV_LLVM_LIB") {
        candidates.push(env);
    }
    #[cfg(target_os = "linux")]
    {
        candidates.push("libLLVM.so".into());
        for ver in (15..=24).rev() {
            candidates.push(format!("libLLVM-{ver}.so"));
            candidates.push(format!("libLLVM.so.{ver}"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push("libLLVM.dylib".into());
        candidates.push("/opt/homebrew/opt/llvm/lib/libLLVM.dylib".into());
        candidates.push("/usr/local/opt/llvm/lib/libLLVM.dylib".into());
    }

    let mut handle = ptr::null_mut();
    for path in &candidates {
        let Ok(cpath) = CString::new(path.as_str()) else {
            continue;
        };
        let h = unsafe { dlopen(cpath.as_ptr(), RTLD_NOW) };
        if !h.is_null() {
            handle = h;
            break;
        }
    }
    if handle.is_null() {
        return None;
    }

    // Initialize native target once.
    #[cfg(target_arch = "x86_64")]
    const TARGET_INITS: &[&str] = &[
        "LLVMInitializeX86TargetInfo",
        "LLVMInitializeX86Target",
        "LLVMInitializeX86TargetMC",
        "LLVMInitializeX86AsmPrinter",
    ];
    #[cfg(target_arch = "aarch64")]
    const TARGET_INITS: &[&str] = &[
        "LLVMInitializeAArch64TargetInfo",
        "LLVMInitializeAArch64Target",
        "LLVMInitializeAArch64TargetMC",
        "LLVMInitializeAArch64AsmPrinter",
    ];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const TARGET_INITS: &[&str] = &[];

    for &sym in TARGET_INITS {
        let init: unsafe extern "C" fn() = unsafe { load_sym(handle, sym)? };
        unsafe { init() };
    }

    unsafe {
        Some(LlvmApi {
            context_create: load_sym(handle, "LLVMContextCreate")?,
            context_dispose: load_sym(handle, "LLVMContextDispose")?,
            create_mem_buf_copy: load_sym(handle, "LLVMCreateMemoryBufferWithMemoryRangeCopy")?,
            get_buf_start: load_sym(handle, "LLVMGetBufferStart")?,
            get_buf_size: load_sym(handle, "LLVMGetBufferSize")?,
            dispose_mem_buf: load_sym(handle, "LLVMDisposeMemoryBuffer")?,
            parse_ir_in_context: load_sym(handle, "LLVMParseIRInContext")?,
            verify_module: load_sym(handle, "LLVMVerifyModule")?,
            dispose_module: load_sym(handle, "LLVMDisposeModule")?,
            get_default_triple: load_sym(handle, "LLVMGetDefaultTargetTriple")?,
            set_target: load_sym(handle, "LLVMSetTarget"),
            get_target_from_triple: load_sym(handle, "LLVMGetTargetFromTriple")?,
            create_target_machine: load_sym(handle, "LLVMCreateTargetMachine")?,
            dispose_target_machine: load_sym(handle, "LLVMDisposeTargetMachine")?,
            emit_to_mem_buf: load_sym(handle, "LLVMTargetMachineEmitToMemoryBuffer")?,
            create_pass_opts: load_sym(handle, "LLVMCreatePassBuilderOptions"),
            dispose_pass_opts: load_sym(handle, "LLVMDisposePassBuilderOptions"),
            run_passes: load_sym(handle, "LLVMRunPasses"),
            get_error_message: load_sym(handle, "LLVMGetErrorMessage"),
            dispose_error_message: load_sym(handle, "LLVMDisposeErrorMessage"),
            dispose_message: load_sym(handle, "LLVMDisposeMessage")?,
        })
    }
}

#[cfg(not(unix))]
fn try_load_llvm() -> Option<LlvmApi> {
    None
}

fn llvm_api() -> Option<&'static LlvmApi> {
    LLVM_API.get_or_init(try_load_llvm).as_ref()
}

/// Compiles LLVM IR text (`.ll`) into native object bytes (`.o`), using `libLLVM` if present
/// and falling back to `llc`.
pub fn compile_ir_to_object(ir: &str, opt: LlvmOpt) -> Result<Vec<u8>, CodegenError> {
    if let Some(api) = llvm_api() {
        return compile_with_libllvm(api, ir, opt);
    }
    compile_with_llc(ir, opt)
}

fn take_llvm_msg(api: &LlvmApi, ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return "unknown LLVM error".into();
    }
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { (api.dispose_message)(ptr) };
    s
}

fn compile_with_libllvm(api: &LlvmApi, ir: &str, opt: LlvmOpt) -> Result<Vec<u8>, CodegenError> {
    unsafe {
        let ctx = (api.context_create)();
        let buf_name = c"noev_module";
        let mem = (api.create_mem_buf_copy)(ir.as_ptr().cast(), ir.len(), buf_name.as_ptr());

        let mut module: LlvmModuleRef = ptr::null_mut();
        let mut err_msg: *mut c_char = ptr::null_mut();
        // LLVMParseIRInContext takes ownership of `mem`.
        let parse_err = (api.parse_ir_in_context)(ctx, mem, &mut module, &mut err_msg);
        if parse_err != 0 || module.is_null() {
            let msg = take_llvm_msg(api, err_msg);
            (api.context_dispose)(ctx);
            return Err(CodegenError(format!("LLVM IR parse failed: {msg}")));
        }

        // Verify module (LLVMReturnStatusAction = 2).
        let mut verify_msg: *mut c_char = ptr::null_mut();
        if (api.verify_module)(module, 2, &mut verify_msg) != 0 {
            let msg = take_llvm_msg(api, verify_msg);
            (api.dispose_module)(module);
            (api.context_dispose)(ctx);
            return Err(CodegenError(format!("LLVM IR verification failed: {msg}")));
        }
        if !verify_msg.is_null() {
            (api.dispose_message)(verify_msg);
        }

        let triple = (api.get_default_triple)();
        if let Some(set_target) = api.set_target {
            set_target(module, triple);
        }

        let mut target: LlvmTargetRef = ptr::null_mut();
        let mut target_err: *mut c_char = ptr::null_mut();
        if (api.get_target_from_triple)(triple, &mut target, &mut target_err) != 0 {
            let msg = take_llvm_msg(api, target_err);
            (api.dispose_message)(triple);
            (api.dispose_module)(module);
            (api.context_dispose)(ctx);
            return Err(CodegenError(format!("LLVM target lookup failed: {msg}")));
        }

        let opt_level = match opt {
            LlvmOpt::O0 => 0,                             // LLVMCodeGenLevelNone
            LlvmOpt::O1 => 1,                             // LLVMCodeGenLevelLess (-O1)
            LlvmOpt::O2 | LlvmOpt::Os | LlvmOpt::Oz => 2, // LLVMCodeGenLevelDefault
            LlvmOpt::O3 => 3,                             // LLVMCodeGenLevelAggressive (-O3)
        };
        let cpu = c"generic";
        let features = c"";
        // Reloc = 2 (PIC), CodeModel = 0 (Default).
        let tm = (api.create_target_machine)(
            target,
            triple,
            cpu.as_ptr(),
            features.as_ptr(),
            opt_level,
            2,
            0,
        );
        (api.dispose_message)(triple);
        if tm.is_null() {
            (api.dispose_module)(module);
            (api.context_dispose)(ctx);
            return Err(CodegenError("failed to create LLVM target machine".into()));
        }

        // Run LLVM new-pass-manager optimization pipeline for -O1..-O3/-Os/-Oz.
        // Modern LLVM implements -Os/-Oz via `default<O2>` combined with `optsize`/`minsize`
        // function attributes (set by `emit.rs`).
        let passes = match opt {
            LlvmOpt::O0 => None,
            LlvmOpt::O1 => Some(c"default<O1>"),
            LlvmOpt::O2 | LlvmOpt::Os | LlvmOpt::Oz => Some(c"default<O2>"),
            LlvmOpt::O3 => Some(c"default<O3>"),
        };
        if let Some(passes) = passes
            && let (Some(create_opts), Some(dispose_opts), Some(run_passes)) =
                (api.create_pass_opts, api.dispose_pass_opts, api.run_passes)
        {
            let pb_opts = create_opts();
            let err = run_passes(module, passes.as_ptr(), tm, pb_opts);
            dispose_opts(pb_opts);
            if !err.is_null() {
                let msg = if let (Some(get_msg), Some(disp_msg)) =
                    (api.get_error_message, api.dispose_error_message)
                {
                    let p = get_msg(err);
                    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                    disp_msg(p);
                    s
                } else {
                    "LLVM optimization pass failed".into()
                };
                (api.dispose_target_machine)(tm);
                (api.dispose_module)(module);
                (api.context_dispose)(ctx);
                return Err(CodegenError(format!("LLVM optimization failed: {msg}")));
            }
        }

        // Emit native object file (LLVMObjectFile = 1).
        let mut out_buf: LlvmMemoryBufferRef = ptr::null_mut();
        let mut emit_err: *mut c_char = ptr::null_mut();
        let failed = (api.emit_to_mem_buf)(tm, module, 1, &mut emit_err, &mut out_buf);
        if failed != 0 || out_buf.is_null() {
            let msg = take_llvm_msg(api, emit_err);
            (api.dispose_target_machine)(tm);
            (api.dispose_module)(module);
            (api.context_dispose)(ctx);
            return Err(CodegenError(format!("LLVM object emission failed: {msg}")));
        }

        let start = (api.get_buf_start)(out_buf);
        let len = (api.get_buf_size)(out_buf);
        let bytes = std::slice::from_raw_parts(start, len).to_vec();

        (api.dispose_mem_buf)(out_buf);
        (api.dispose_target_machine)(tm);
        (api.dispose_module)(module);
        (api.context_dispose)(ctx);
        Ok(bytes)
    }
}

fn compile_with_llc(ir: &str, opt: LlvmOpt) -> Result<Vec<u8>, CodegenError> {
    let dir = std::env::temp_dir();
    let tag = format!(
        "noev-llvm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let ll_path = dir.join(format!("{tag}.ll"));
    let obj_path = dir.join(format!("{tag}.o"));
    std::fs::write(&ll_path, ir)
        .map_err(|e| CodegenError(format!("cannot write temp LLVM IR: {e}")))?;

    let mut cmd = Command::new(std::env::var("LLC").unwrap_or_else(|_| "llc".into()));
    cmd.args(["-filetype=obj", "-relocation-model=pic"]);
    cmd.arg(match opt {
        LlvmOpt::O0 => "-O0",
        LlvmOpt::O1 => "-O1",
        LlvmOpt::O2 | LlvmOpt::Os | LlvmOpt::Oz => "-O2",
        LlvmOpt::O3 => "-O3",
    });
    cmd.arg("-o").arg(&obj_path).arg(&ll_path);

    let out = cmd.output();
    let _ = std::fs::remove_file(&ll_path);
    let out = out.map_err(|e| {
        CodegenError(format!(
            "LLVM backend unavailable: neither `libLLVM` nor `llc` could be loaded ({e}); \
             install LLVM or set `NOEV_LLVM_LIB` / `LLC`"
        ))
    })?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&obj_path);
        return Err(CodegenError(format!(
            "llc failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let bytes = std::fs::read(&obj_path)
        .map_err(|e| CodegenError(format!("cannot read llc output: {e}")))?;
    let _ = std::fs::remove_file(&obj_path);
    Ok(bytes)
}
