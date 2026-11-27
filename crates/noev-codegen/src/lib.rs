//! Native code generators for Noev (`spec/runtime.md` §2).
//!
//! Both Cranelift (`cranelift` feature) and LLVM (`llvm` feature) sit behind the same
//! [`CodeGenerator`] interface, lowering a checked [`noev_ir::Program`] into a native object
//! file or standalone executable linked against `libnoev_runtime.a` (or, with LLVM and
//! [`RuntimeAbi::Qir`], calling QIR runtime functions directly).

#[cfg(any(feature = "cranelift", feature = "llvm"))]
pub(crate) mod analysis;
#[cfg(any(feature = "cranelift", feature = "llvm"))]
pub(crate) mod layout;

#[cfg(feature = "cranelift")]
mod lower;
#[cfg(feature = "cranelift")]
mod rodata;
#[cfg(feature = "cranelift")]
mod runtime;

#[cfg(feature = "llvm")]
pub mod llvm;

use std::fmt;
use std::path::Path;
use std::process::Command;

use noev_ir::Program;

/// A failure while generating native code or linking an executable.
#[derive(Debug)]
pub struct CodegenError(pub String);

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodegenError {}

#[cfg(feature = "cranelift")]
impl From<cranelift_module::ModuleError> for CodegenError {
    fn from(e: cranelift_module::ModuleError) -> Self {
        CodegenError(e.to_string())
    }
}

/// Which runtime symbol set quantum operations call (`spec/runtime.md` §4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RuntimeAbi {
    /// Noev's own Runtime C ABI (`noev_qis_*`, `noev_rt_*`), linking against `libnoev_runtime.a`.
    #[default]
    Noev,
    /// QIR's standard runtime symbols (`__quantum__qis__*`, `__quantum__rt__*`), for linking
    /// against an external QIR runtime.
    Qir,
}

/// Optimization level requested from a code generator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OptLevel {
    /// Fast compilation, minimal optimization.
    None,
    /// Standard speed optimizations (`-O2`).
    #[default]
    Speed,
}

/// Options shared by every [`CodeGenerator`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodegenOptions {
    /// Optimization level.
    pub opt: OptLevel,
    /// Which runtime ABI quantum operations target.
    pub runtime_abi: RuntimeAbi,
    /// Emit a native test runner `main` executing `program.tests` instead of `program.main`.
    pub test_harness: bool,
}

/// Common interface implemented by every native code generator (`spec/runtime.md` §2).
pub trait CodeGenerator: Sync {
    /// Short backend name (`"cranelift"`, `"llvm"`).
    fn name(&self) -> &'static str;

    /// Compiles `program` to a native relocatable object file (`.o`) for the host machine.
    fn emit_object(&self, program: &Program, opts: CodegenOptions)
    -> Result<Vec<u8>, CodegenError>;

    /// Compiles `program` to a standalone native executable at `output`, linking `runtime_lib`.
    fn emit_binary(
        &self,
        program: &Program,
        output: &Path,
        runtime_lib: &Path,
        opts: CodegenOptions,
    ) -> Result<(), CodegenError> {
        let obj_bytes = self.emit_object(program, opts)?;
        link_object(&obj_bytes, output, runtime_lib)
    }
}

/// Links native object bytes with `runtime_lib` into a standalone executable at `output`.
pub fn link_object(
    obj_bytes: &[u8],
    output: &Path,
    runtime_lib: &Path,
) -> Result<(), CodegenError> {
    link_object_with_args(obj_bytes, output, runtime_lib, &[])
}

/// Links native object bytes with `runtime_lib` and extra C objects, archives, or `-l`/`-L`
/// flags into a standalone executable at `output`.
pub fn link_object_with_args(
    obj_bytes: &[u8],
    output: &Path,
    runtime_lib: &Path,
    extra_args: &[String],
) -> Result<(), CodegenError> {
    let obj_path = output.with_extension("noev.tmp.o");
    std::fs::write(&obj_path, obj_bytes).map_err(|e| {
        CodegenError(format!(
            "cannot write temporary object {}: {e}",
            obj_path.display()
        ))
    })?;

    let mut cmd = if cfg!(target_os = "windows") && std::env::var_os("CC").is_none() {
        let mut c = Command::new("clang");
        c.arg(&obj_path);
        c
    } else {
        let mut c = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()));
        c.arg(&obj_path);
        c
    };
    for arg in extra_args {
        if Path::new(arg).exists() || arg.starts_with('-') {
            cmd.arg(arg);
        } else {
            cmd.arg(format!("-l{arg}"));
        }
    }
    cmd.arg(runtime_lib).arg("-o").arg(output);
    #[cfg(target_os = "linux")]
    cmd.args(["-lm", "-lpthread", "-ldl"]);
    #[cfg(target_os = "macos")]
    cmd.args(["-lm", "-lpthread"]);
    #[cfg(target_os = "windows")]
    cmd.args(["-lws2_32", "-lbcrypt", "-luserenv", "-lntdll"]);

    let status = cmd.output();
    let _ = std::fs::remove_file(&obj_path);

    let out = status.map_err(|e| CodegenError(format!("failed to invoke C linker `cc`: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(CodegenError(format!(
            "linker failed ({}):\n{}",
            out.status,
            stderr.trim()
        )));
    }
    Ok(())
}

/// The Cranelift native code generator (`cranelift` feature).
#[cfg(feature = "cranelift")]
#[derive(Clone, Copy, Debug, Default)]
pub struct CraneliftCodegen;

#[cfg(feature = "cranelift")]
impl CodeGenerator for CraneliftCodegen {
    fn name(&self) -> &'static str {
        "cranelift"
    }

    fn emit_object(
        &self,
        program: &Program,
        opts: CodegenOptions,
    ) -> Result<Vec<u8>, CodegenError> {
        use cranelift_codegen::settings::{self, Configurable};
        use cranelift_object::{ObjectBuilder, ObjectModule};

        if opts.runtime_abi == RuntimeAbi::Qir {
            return Err(CodegenError(
                "QIR runtime linking (`--qir-runtime`) requires the LLVM code generator (`--codegen=llvm`)"
                    .into(),
            ));
        }

        let mut flag_builder = settings::builder();
        let opt = match opts.opt {
            OptLevel::None => "none",
            OptLevel::Speed => "speed",
        };
        flag_builder
            .set("opt_level", opt)
            .map_err(|e| CodegenError(e.to_string()))?;
        flag_builder
            .set("is_pic", "true")
            .map_err(|e| CodegenError(e.to_string()))?;
        let isa_builder = cranelift_native::builder().map_err(|e| CodegenError(e.to_string()))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| CodegenError(e.to_string()))?;

        let builder = ObjectBuilder::new(
            isa,
            "noev_program",
            cranelift_module::default_libcall_names(),
        )?;
        let mut module = ObjectModule::new(builder);
        lower::compile_program(&mut module, program, opts.test_harness)?;
        let product = module.finish();
        product.emit().map_err(|e| CodegenError(e.to_string()))
    }
}

/// Convenience wrapper compiling `program` with [`CraneliftCodegen`] using default options.
#[cfg(feature = "cranelift")]
pub fn emit_object(program: &Program) -> Result<Vec<u8>, CodegenError> {
    CraneliftCodegen.emit_object(program, CodegenOptions::default())
}

/// Convenience wrapper compiling and linking `program` with [`CraneliftCodegen`].
#[cfg(feature = "cranelift")]
pub fn emit_binary(
    program: &Program,
    output: &Path,
    runtime_lib: &Path,
) -> Result<(), CodegenError> {
    CraneliftCodegen.emit_binary(program, output, runtime_lib, CodegenOptions::default())
}
