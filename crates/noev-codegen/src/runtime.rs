//! Imported Runtime C ABI (`noev_rt_*` / `noev_qis_*`) function declarations for Cranelift.

use std::collections::HashMap;

use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{AbiParam, Signature, Type as ClifType};
use cranelift_module::{FuncId as ClifFuncId, Linkage, Module};
use cranelift_object::ObjectModule;

use crate::CodegenError;

/// Caches imported `extern "C"` runtime symbols declared in the [`ObjectModule`].
pub struct RuntimeImports {
    ptr_ty: ClifType,
    funcs: HashMap<&'static str, ClifFuncId>,
}

impl RuntimeImports {
    /// Creates a new import table for pointer type `ptr_ty`.
    pub fn new(ptr_ty: ClifType) -> Self {
        Self {
            ptr_ty,
            funcs: HashMap::new(),
        }
    }

    /// Declares (or retrieves cached) `extern "C"` symbol `name` with `params` and optional `ret`.
    pub fn get(
        &mut self,
        module: &mut ObjectModule,
        name: &'static str,
        params: &[ClifType],
        ret: Option<ClifType>,
    ) -> Result<ClifFuncId, CodegenError> {
        if let Some(&id) = self.funcs.get(name) {
            return Ok(id);
        }
        let mut sig = Signature::new(module.isa().default_call_conv());
        for &p in params {
            sig.params.push(AbiParam::new(p));
        }
        if let Some(r) = ret {
            sig.returns.push(AbiParam::new(r));
        }
        let id = module.declare_function(name, Linkage::Import, &sig)?;
        self.funcs.insert(name, id);
        Ok(id)
    }

    /// Convenience: `(rt: ptr) -> ()`.
    pub fn rt_void(
        &mut self,
        module: &mut ObjectModule,
        name: &'static str,
    ) -> Result<ClifFuncId, CodegenError> {
        let p = self.ptr_ty;
        self.get(module, name, &[p], None)
    }

    /// Convenience: `(rt: ptr) -> bool(I8)`.
    pub fn rt_bool(
        &mut self,
        module: &mut ObjectModule,
        name: &'static str,
    ) -> Result<ClifFuncId, CodegenError> {
        let p = self.ptr_ty;
        self.get(module, name, &[p], Some(types::I8))
    }

    /// Convenience: `(rt: ptr, q: ptr) -> bool(I8)`.
    pub fn rt_qubit(
        &mut self,
        module: &mut ObjectModule,
        name: &'static str,
    ) -> Result<ClifFuncId, CodegenError> {
        let p = self.ptr_ty;
        self.get(module, name, &[p, p], Some(types::I8))
    }

    /// Convenience: `(rt: ptr, theta: f64, q: ptr) -> bool(I8)`.
    pub fn rt_rot(
        &mut self,
        module: &mut ObjectModule,
        name: &'static str,
    ) -> Result<ClifFuncId, CodegenError> {
        let p = self.ptr_ty;
        self.get(module, name, &[p, types::F64, p], Some(types::I8))
    }
}
