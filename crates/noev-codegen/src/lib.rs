//! Cranelift native code generator for Noev (`spec/runtime.md` §2).
//!
//! Compiles a lowered [`noev_ir::Program`] into a native object file (`emit_object`) or a
//! standalone executable (`emit_binary`) linked against `libnoev_runtime.a`.

mod layout;
mod lower;
mod rodata;
mod runtime;

use std::fmt;
use std::path::Path;
use std::process::Command;

use cranelift_codegen::settings::{self, Configurable};
use cranelift_module::ModuleError;
use cranelift_object::{ObjectBuilder, ObjectModule};
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

impl From<ModuleError> for CodegenError {
    fn from(e: ModuleError) -> Self {
        CodegenError(e.to_string())
    }
}

/// Compiles `program` to a native object file (ELF on Linux, Mach-O on macOS, COFF on Windows)
/// targeting the host machine.
pub fn emit_object(program: &Program) -> Result<Vec<u8>, CodegenError> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CodegenError(e.to_string()))?;
    // Position-independent code so the object links cleanly into PIE executables on Linux.
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
    lower::compile_program(&mut module, program)?;
    let product = module.finish();
    product.emit().map_err(|e| CodegenError(e.to_string()))
}

/// Compiles `program` to a standalone native executable at `output`, linking against the
/// `libnoev_runtime.a` static library at `runtime_lib`.
pub fn emit_binary(
    program: &Program,
    output: &Path,
    runtime_lib: &Path,
) -> Result<(), CodegenError> {
    let obj_bytes = emit_object(program)?;
    let obj_path = output.with_extension("noev.tmp.o");
    std::fs::write(&obj_path, &obj_bytes).map_err(|e| {
        CodegenError(format!(
            "cannot write temporary object {}: {e}",
            obj_path.display()
        ))
    })?;

    let mut cmd = Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()));
    cmd.arg(&obj_path).arg(runtime_lib).arg("-o").arg(output);
    #[cfg(target_os = "linux")]
    cmd.args(["-lm", "-lpthread", "-ldl"]);
    #[cfg(target_os = "macos")]
    cmd.args(["-lm", "-lpthread"]);

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
