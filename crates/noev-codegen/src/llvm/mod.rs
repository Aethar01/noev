//! LLVM native code generator (`llvm` feature) (`spec/runtime.md` §2, §4).
//!
//! Lowers [`noev_ir::Program`] to modern opaque-pointer LLVM IR (targeting either Noev's
//! Runtime C ABI or QIR's `__quantum__qis__*` / `__quantum__rt__*` runtime symbols), then
//! verifies, optimizes (`-O2`), and compiles to native object code through the host's
//! `libLLVM` C API (with `llc` CLI fallback).

mod emit;
mod sys;

use noev_ir::Program;

use crate::{CodeGenerator, CodegenError, CodegenOptions};

/// The LLVM native code generator (`llvm` feature).
#[derive(Clone, Copy, Debug, Default)]
pub struct LlvmCodegen;

impl LlvmCodegen {
    /// Emits the LLVM IR module text (`.ll`) for `program` without running LLVM's backend.
    pub fn emit_llvm_ir(
        &self,
        program: &Program,
        opts: CodegenOptions,
    ) -> Result<String, CodegenError> {
        emit::emit_module(program, opts)
    }

    /// Compiles an LLVM IR text module (`.ll`) to native object bytes (`.o`).
    pub fn compile_ir_to_object(
        &self,
        ir: &str,
        opts: CodegenOptions,
    ) -> Result<Vec<u8>, CodegenError> {
        sys::compile_ir_to_object(ir, opts.effective_llvm_opt())
    }
}

impl CodeGenerator for LlvmCodegen {
    fn name(&self) -> &'static str {
        "llvm"
    }

    fn emit_object(
        &self,
        program: &Program,
        opts: CodegenOptions,
    ) -> Result<Vec<u8>, CodegenError> {
        let ir = self.emit_llvm_ir(program, opts)?;
        self.compile_ir_to_object(&ir, opts)
    }
}
