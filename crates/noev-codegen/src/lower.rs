//! Lowering [`noev_ir::Program`] into Cranelift IR and native object code.

use std::collections::{HashMap, HashSet, VecDeque};
use std::mem::{align_of, offset_of, size_of};

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    AbiParam, Block as ClifBlock, InstBuilder, MemFlagsData, Signature, StackSlot, StackSlotData,
    StackSlotKind, Type as ClifType, Value as ClifValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{DataId, FuncId as ClifFuncId, Linkage, Module};
use cranelift_object::ObjectModule;
use noev_ir::Program;
use noev_ir::ir::{
    Align, BinOp, Block, Const, FormatKind, FormatPart, FuncId, Function, Local, Operand, Place,
    Prep, Proj, Rvalue, Stmt, Terminator, UnOp,
};
use noev_runtime::cabi_compiled::{NoevFormatPart, NoevPrintArg};
use noev_sema::prelude::{DistMethod, FloatConst, Gate, Math, Pauli};
use noev_sema::{Ty, TyKind};

use crate::CodegenError;
use crate::layout::LayoutCache;
use crate::rodata::Rodata;
use crate::runtime::RuntimeImports;

/// Hashable key for a [`Place`] used in enum-variant dataflow analysis.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PlaceKey {
    local: Local,
    proj: Vec<ProjKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ProjKey {
    Deref,
    Field(u32),
    IndexConst(i64),
    IndexOther,
}

impl PlaceKey {
    fn from_place(p: &Place) -> Self {
        Self {
            local: p.local,
            proj: p
                .proj
                .iter()
                .map(|step| match step {
                    Proj::Deref => ProjKey::Deref,
                    Proj::Field(i) => ProjKey::Field(*i),
                    Proj::Index(Operand::Const(Const::Int(k))) => ProjKey::IndexConst(*k),
                    Proj::Index(_) => ProjKey::IndexOther,
                })
                .collect(),
        }
    }
}

const DEFAULT_PRECISION: f64 = 0.01;
const MIN_INLINE_INT: i64 = -(1i64 << 62);
const MAX_INLINE_INT: i64 = (1i64 << 62) - 1;

/// Compiles every function in `program`, any needed thunks, and the C `main` entry point.
pub fn compile_program(module: &mut ObjectModule, program: &Program) -> Result<(), CodegenError> {
    let ptr_ty = module.target_config().pointer_type();
    let ptr_bytes = u32::from(module.target_config().pointer_bytes());
    let mut layouts = LayoutCache::new(program, ptr_ty, ptr_bytes);
    let mut rodata = Rodata::new();
    let mut rt_imports = RuntimeImports::new(ptr_ty);

    // 1. Declare all IR functions.
    let mut func_ids: Vec<ClifFuncId> = Vec::with_capacity(program.functions.len());
    for (i, func) in program.functions.iter().enumerate() {
        let sig = fn_signature(module, &mut layouts, func);
        let name = format!("__noev_fn_{i}");
        let id = module.declare_function(&name, Linkage::Local, &sig)?;
        func_ids.push(id);
    }

    // 2. Collect which IR functions need `NoevCompiledFn` thunks (`sample`, `Expectation`,
    //    `Overlap`, `Undo`).
    let mut thunk_set: HashSet<FuncId> = HashSet::new();
    for func in &program.functions {
        for block in &func.blocks {
            for stmt in &block.stmts {
                match stmt {
                    Stmt::Sample { func: f, .. } => {
                        thunk_set.insert(*f);
                    }
                    Stmt::Expectation {
                        prep: Prep::Call(f, _),
                        ..
                    } => {
                        thunk_set.insert(*f);
                    }
                    Stmt::Overlap { bra, ket, .. } => {
                        if let Prep::Call(f, _) = bra {
                            thunk_set.insert(*f);
                        }
                        if let Prep::Call(f, _) = ket {
                            thunk_set.insert(*f);
                        }
                    }
                    _ => {}
                }
            }
            if let Terminator::Undo { func: f, .. } = &block.term {
                thunk_set.insert(*f);
            }
        }
    }
    let mut thunk_ids: HashMap<FuncId, ClifFuncId> = HashMap::new();
    let mut thunk_list: Vec<FuncId> = thunk_set.into_iter().collect();
    thunk_list.sort_unstable();
    for fid in &thunk_list {
        let mut sig = Signature::new(module.isa().default_call_conv());
        sig.params.push(AbiParam::new(ptr_ty)); // rt
        sig.params.push(AbiParam::new(ptr_ty)); // args_buf
        sig.params.push(AbiParam::new(ptr_ty)); // ret_buf
        sig.returns.push(AbiParam::new(types::I8)); // bool ok
        let name = format!("__noev_thunk_{}", fid.index());
        let cid = module.declare_function(&name, Linkage::Local, &sig)?;
        thunk_ids.insert(*fid, cid);
    }

    let mut ctx = module.make_context();
    let mut fb_ctx = FunctionBuilderContext::new();

    let target_config = module.target_config();

    // 3. Compile each IR function.
    for (i, func) in program.functions.iter().enumerate() {
        ctx.clear();
        ctx.func.signature = fn_signature(module, &mut layouts, func);
        let facts = analyze_enum_variants(func);
        {
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
            let mut lowerer = FnLowerer {
                module,
                program,
                func,
                layouts: &mut layouts,
                rodata: &mut rodata,
                rt_imports: &mut rt_imports,
                func_ids: &func_ids,
                thunk_ids: &thunk_ids,
                builder: &mut builder,
                ptr_ty,
                rt_val: ClifValue::from_u32(0),
                ctrl_depth: ClifValue::from_u32(0),
                ret_ptr: None,
                slots: Vec::new(),
                blocks: Vec::new(),
                err_block: ClifBlock::from_u32(0),
                facts: &facts,
                cur_block: Block(0),
            };
            lowerer.lower_function()?;
            builder.finalize(target_config);
        }
        module.define_function(func_ids[i], &mut ctx)?;
    }

    // 4. Compile thunks.
    for fid in thunk_list {
        compile_thunk(
            module,
            &mut ctx,
            &mut fb_ctx,
            &mut layouts,
            &program.functions[fid.index()],
            func_ids[fid.index()],
            thunk_ids[&fid],
            ptr_ty,
        )?;
    }

    // 5. Compile C `main(argc: i32, argv: *const *const c_char) -> i32`.
    if let Some(main_id) = program.main {
        compile_main(
            module,
            &mut ctx,
            &mut fb_ctx,
            &mut layouts,
            &mut rt_imports,
            &program.functions[main_id.index()],
            func_ids[main_id.index()],
            ptr_ty,
        )?;
    }

    Ok(())
}

/// Builds the Cranelift calling signature for an IR [`Function`]:
/// `(rt: ptr, non-ZST params..., [ret_ptr: ptr if size(ret) > 0]) -> ok: I8`.
fn fn_signature(
    module: &ObjectModule,
    layouts: &mut LayoutCache<'_>,
    func: &Function,
) -> Signature {
    let ptr_ty = module.target_config().pointer_type();
    let mut sig = Signature::new(module.isa().default_call_conv());
    sig.params.push(AbiParam::new(ptr_ty)); // rt
    for local in &func.locals[1..=func.params] {
        let l = layouts.layout(local.ty);
        if l.size == 0 {
            continue;
        }
        if let Some(clif) = l.scalar {
            sig.params.push(AbiParam::new(clif));
        } else {
            sig.params.push(AbiParam::new(ptr_ty));
        }
    }
    let ret_layout = layouts.layout(func.locals[0].ty);
    if ret_layout.size > 0 {
        sig.params.push(AbiParam::new(ptr_ty)); // ret_ptr
    }
    sig.returns.push(AbiParam::new(types::I8));
    sig
}

#[allow(clippy::too_many_arguments)]
fn compile_thunk(
    module: &mut ObjectModule,
    ctx: &mut Context,
    fb_ctx: &mut FunctionBuilderContext,
    layouts: &mut LayoutCache<'_>,
    func: &Function,
    callee_id: ClifFuncId,
    thunk_id: ClifFuncId,
    ptr_ty: ClifType,
) -> Result<(), CodegenError> {
    ctx.clear();
    ctx.func.signature = Signature::new(module.isa().default_call_conv());
    ctx.func.signature.params.push(AbiParam::new(ptr_ty));
    ctx.func.signature.params.push(AbiParam::new(ptr_ty));
    ctx.func.signature.params.push(AbiParam::new(ptr_ty));
    ctx.func.signature.returns.push(AbiParam::new(types::I8));

    let param_tys: Vec<Ty> = (1..=func.params).map(|i| func.locals[i].ty).collect();
    let (_, _, offsets) = layouts.pack_types(&param_tys);
    let ret_size = layouts.layout(func.locals[0].ty).size;

    let target_config = module.target_config();
    let callee_ref = module.declare_func_in_func(callee_id, &mut ctx.func);
    let mut b = FunctionBuilder::new(&mut ctx.func, fb_ctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);

    let rt = b.block_params(entry)[0];
    let args_buf = b.block_params(entry)[1];
    let ret_buf = b.block_params(entry)[2];

    let mut call_args = vec![rt];
    for (&ty, &off) in param_tys.iter().zip(&offsets) {
        let l = layouts.layout(ty);
        if l.size == 0 {
            continue;
        }
        let ptr = if off == 0 {
            args_buf
        } else {
            b.ins().iadd_imm_s(args_buf, i64::from(off))
        };
        if let Some(clif) = l.scalar {
            let val = b.ins().load(clif, MemFlagsData::new(), ptr, 0);
            call_args.push(val);
        } else {
            call_args.push(ptr);
        }
    }
    if ret_size > 0 {
        call_args.push(ret_buf);
    }
    let inst = b.ins().call(callee_ref, &call_args);
    let ok = b.inst_results(inst)[0];
    b.ins().return_(&[ok]);
    b.finalize(target_config);

    module.define_function(thunk_id, ctx)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_main(
    module: &mut ObjectModule,
    ctx: &mut Context,
    fb_ctx: &mut FunctionBuilderContext,
    layouts: &mut LayoutCache<'_>,
    rt_imports: &mut RuntimeImports,
    main_func: &Function,
    main_callee: ClifFuncId,
    ptr_ty: ClifType,
) -> Result<(), CodegenError> {
    let enter_id = rt_imports.get(
        module,
        "noev_rt_main_enter",
        &[types::I32, ptr_ty],
        Some(ptr_ty),
    )?;
    let exit_id = rt_imports.get(
        module,
        "noev_rt_main_exit",
        &[ptr_ty, types::I8],
        Some(types::I32),
    )?;

    let mut main_sig = Signature::new(module.isa().default_call_conv());
    main_sig.params.push(AbiParam::new(types::I32)); // argc
    main_sig.params.push(AbiParam::new(ptr_ty)); // argv
    main_sig.returns.push(AbiParam::new(types::I32)); // exit code
    let main_id = module.declare_function("main", Linkage::Export, &main_sig)?;

    ctx.clear();
    ctx.func.signature = main_sig;
    let target_config = module.target_config();
    let enter_ref = module.declare_func_in_func(enter_id, &mut ctx.func);
    let exit_ref = module.declare_func_in_func(exit_id, &mut ctx.func);
    let callee_ref = module.declare_func_in_func(main_callee, &mut ctx.func);

    let ret_layout = layouts.layout(main_func.locals[0].ty);

    let mut b = FunctionBuilder::new(&mut ctx.func, fb_ctx);
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    b.seal_block(entry);

    let argc = b.block_params(entry)[0];
    let argv = b.block_params(entry)[1];
    let rt_call = b.ins().call(enter_ref, &[argc, argv]);
    let rt = b.inst_results(rt_call)[0];

    let mut args = vec![rt];
    if ret_layout.size > 0 {
        let slot = b.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            ret_layout.size,
            LayoutCache::align_shift(ret_layout.align),
        ));
        args.push(b.ins().stack_addr(ptr_ty, slot, 0));
    }
    let run_call = b.ins().call(callee_ref, &args);
    let ok = b.inst_results(run_call)[0];
    let exit_call = b.ins().call(exit_ref, &[rt, ok]);
    let code = b.inst_results(exit_call)[0];
    b.ins().return_(&[code]);
    b.finalize(target_config);

    module.define_function(main_id, ctx)?;
    Ok(())
}

/// Checks whether `func` (or any function it calls) contains measurement-dependent control flow
/// (`Branch { control: Some(_) }` or a dynamic `Bit` label in `Prepare`), which requires
/// shot-by-shot execution rather than static circuit recording.
fn has_feedback(program: &Program, root: FuncId) -> bool {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(root);
    visited.insert(root);
    while let Some(fid) = queue.pop_front() {
        let func = &program.functions[fid.index()];
        for block in &func.blocks {
            for stmt in &block.stmts {
                if let Stmt::Prepare { label, .. } = stmt {
                    let is_const = matches!(label, Operand::Const(_));
                    if !is_const && operand_ty(program, func, label) == Ty::BIT {
                        return true;
                    }
                }
            }
            match &block.term {
                Terminator::Branch {
                    control: Some(_), ..
                } => return true,
                Terminator::Call { func: callee, .. } if visited.insert(*callee) => {
                    queue.push_back(*callee);
                }
                _ => {}
            }
        }
    }
    false
}

/// Forward dataflow pass per function: infers which enum variant a [`PlaceKey`] is known to hold
/// in each basic block (from `tag = Tag(place); cond = (tag == K); Branch(cond, then, else)`).
fn analyze_enum_variants(func: &Function) -> Vec<HashMap<PlaceKey, u32>> {
    let n = func.blocks.len();
    let mut in_facts: Vec<Option<HashMap<PlaceKey, u32>>> = vec![None; n];
    if n == 0 {
        return Vec::new();
    }
    in_facts[0] = Some(HashMap::new());
    let mut queue = VecDeque::new();
    queue.push_back(0usize);

    while let Some(bi) = queue.pop_front() {
        let mut facts = in_facts[bi].clone().unwrap_or_default();
        let mut tag_of: HashMap<Local, PlaceKey> = HashMap::new();
        let mut cond_fact: HashMap<Local, (PlaceKey, u32)> = HashMap::new();
        let block = &func.blocks[bi];

        for stmt in &block.stmts {
            if let Stmt::Assign(dest, rv) = stmt {
                // Any reassignment invalidates old facts about `dest`.
                facts.retain(|p, _| p.local != dest.local);
                if dest.proj.is_empty() {
                    tag_of.remove(&dest.local);
                    cond_fact.remove(&dest.local);
                    match rv {
                        Rvalue::Tag(Operand::Copy(p) | Operand::Move(p)) => {
                            tag_of.insert(dest.local, PlaceKey::from_place(p));
                        }
                        Rvalue::Binary(BinOp::Eq, a, b) => {
                            let matched = match (a, b) {
                                (
                                    Operand::Copy(p) | Operand::Move(p),
                                    Operand::Const(Const::Int(k)),
                                )
                                | (
                                    Operand::Const(Const::Int(k)),
                                    Operand::Copy(p) | Operand::Move(p),
                                ) if p.proj.is_empty() && *k >= 0 => {
                                    tag_of.get(&p.local).map(|src| (src.clone(), *k as u32))
                                }
                                _ => None,
                            };
                            if let Some(pair) = matched {
                                cond_fact.insert(dest.local, pair);
                            }
                        }
                        Rvalue::Variant(v, _) => {
                            facts.insert(PlaceKey::from_place(dest), *v);
                        }
                        _ => {}
                    }
                }
            }
        }

        let mut push_edge = |target: usize, edge_facts: HashMap<PlaceKey, u32>| {
            let changed = match &mut in_facts[target] {
                None => {
                    in_facts[target] = Some(edge_facts);
                    true
                }
                Some(existing) => {
                    let old_len = existing.len();
                    existing.retain(|k, v| edge_facts.get(k) == Some(v));
                    existing.len() != old_len
                }
            };
            if changed {
                queue.push_back(target);
            }
        };

        match &block.term {
            Terminator::Goto(next)
            | Terminator::Call { next, .. }
            | Terminator::Undo { next, .. }
            | Terminator::Assert { next, .. } => {
                push_edge(next.index(), facts);
            }
            Terminator::Branch {
                cond,
                then,
                otherwise,
                ..
            } => {
                let mut then_facts = facts.clone();
                if let Operand::Copy(p) | Operand::Move(p) = cond
                    && p.proj.is_empty()
                    && let Some((enum_place, v)) = cond_fact.get(&p.local)
                {
                    then_facts.insert(enum_place.clone(), *v);
                }
                push_edge(then.index(), then_facts);
                push_edge(otherwise.index(), facts);
            }
            Terminator::Return | Terminator::Unreachable => {}
        }
    }

    in_facts
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect()
}

/// Computes the type of an [`Operand`] in `func` (not needing enum variant resolution for
/// non-enum projections).
fn operand_ty(program: &Program, func: &Function, op: &Operand) -> Ty {
    match op {
        Operand::Copy(p) | Operand::Move(p) => {
            let mut ty = func.locals[p.local.index()].ty;
            for proj in &p.proj {
                ty = match proj {
                    Proj::Deref => match program.types.kind(ty) {
                        TyKind::Ref { inner, .. } => *inner,
                        _ => ty,
                    },
                    Proj::Field(i) => match program.types.kind(ty) {
                        TyKind::Tuple(elems) => elems[*i as usize],
                        TyKind::Struct(sid) => {
                            let def = &program.types.structs[sid.index()];
                            if let Some(variants) = &def.variants {
                                variants
                                    .iter()
                                    .find_map(|v| v.data.get(*i as usize).copied())
                                    .unwrap_or(Ty::UNIT)
                            } else {
                                def.fields[*i as usize].1
                            }
                        }
                        _ => Ty::UNIT,
                    },
                    Proj::Index(_) => match program.types.kind(ty) {
                        TyKind::Array(elem, _) => *elem,
                        _ => Ty::UNIT,
                    },
                };
            }
            ty
        }
        Operand::Const(c) => const_ty(*c),
    }
}

fn const_ty(c: Const) -> Ty {
    match c {
        Const::Unit => Ty::UNIT,
        Const::Int(_) => Ty::INT,
        Const::Sized(_, w) => match (w.bits, w.signed) {
            (8, true) => Ty::I8,
            (16, true) => Ty::I16,
            (32, true) => Ty::I32,
            (64, true) => Ty::I64,
            (8, false) => Ty::U8,
            (16, false) => Ty::U16,
            (32, false) => Ty::U32,
            _ => Ty::U64,
        },
        Const::Float(_) => Ty::FLOAT,
        Const::F32(_) => Ty::F32,
        Const::Complex(..) => Ty::COMPLEX,
        Const::C32(..) => Ty::C32,
        Const::Bool(_) => Ty::BOOL,
        Const::Bit(_) => Ty::BIT,
    }
}

struct FnLowerer<'a, 'b, 'p> {
    module: &'a mut ObjectModule,
    program: &'p Program,
    func: &'p Function,
    layouts: &'a mut LayoutCache<'p>,
    rodata: &'a mut Rodata,
    rt_imports: &'a mut RuntimeImports,
    func_ids: &'a [ClifFuncId],
    thunk_ids: &'a HashMap<FuncId, ClifFuncId>,
    builder: &'a mut FunctionBuilder<'b>,
    ptr_ty: ClifType,
    rt_val: ClifValue,
    ctrl_depth: ClifValue,
    ret_ptr: Option<ClifValue>,
    slots: Vec<Option<StackSlot>>,
    blocks: Vec<ClifBlock>,
    err_block: ClifBlock,
    facts: &'a [HashMap<PlaceKey, u32>],
    cur_block: Block,
}

impl<'a, 'b, 'p> FnLowerer<'a, 'b, 'p> {
    fn lower_function(&mut self) -> Result<(), CodegenError> {
        let entry = self.builder.create_block();
        self.builder.append_block_params_for_function_params(entry);
        self.err_block = self.builder.create_block();

        self.blocks = (0..self.func.blocks.len())
            .map(|_| self.builder.create_block())
            .collect();

        // Allocate a stack slot for every non-ZST local.
        self.slots = Vec::with_capacity(self.func.locals.len());
        for local in &self.func.locals {
            let l = self.layouts.layout(local.ty);
            if l.size == 0 {
                self.slots.push(None);
            } else {
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    l.size,
                    LayoutCache::align_shift(l.align),
                ));
                self.slots.push(Some(slot));
            }
        }

        // Entry block: save rt, copy incoming parameters into local stack slots, record ctrl depth.
        self.builder.switch_to_block(entry);
        let params: Vec<ClifValue> = self.builder.block_params(entry).to_vec();
        self.rt_val = params[0];
        let mut p_idx = 1usize;
        for i in 1..=self.func.params {
            let l = self.layouts.layout(self.func.locals[i].ty);
            if l.size == 0 {
                continue;
            }
            let slot = self.slots[i].expect("non-ZST param slot");
            let dst = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
            if l.scalar.is_some() {
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), params[p_idx], dst, 0);
            } else {
                self.emit_memcpy(dst, params[p_idx], l.size, l.align);
            }
            p_idx += 1;
        }
        let ret_layout = self.layouts.layout(self.func.locals[0].ty);
        if ret_layout.size > 0 {
            self.ret_ptr = Some(params[p_idx]);
        }

        // Record entry control stack depth so early `return` from `ctrl` restores it.
        let depth_id = self.rt_imports.get(
            self.module,
            "noev_rt_ctrl_depth",
            &[self.ptr_ty],
            Some(self.ptr_ty),
        )?;
        let depth_call = self.call_clif(depth_id, &[self.rt_val]);
        self.ctrl_depth = self.builder.inst_results(depth_call)[0];

        if let Some(&first_bb) = self.blocks.first() {
            self.builder.ins().jump(first_bb, &[]);
        } else {
            self.emit_return_ok()?;
        }

        // Error exit block: restore control depth and return `0`.
        self.builder.switch_to_block(self.err_block);
        let restore_id = self.rt_imports.get(
            self.module,
            "noev_rt_ctrl_restore",
            &[self.ptr_ty, self.ptr_ty],
            None,
        )?;
        self.call_clif(restore_id, &[self.rt_val, self.ctrl_depth]);
        let zero = self.builder.ins().iconst(types::I8, 0);
        self.builder.ins().return_(&[zero]);

        // Compile every IR basic block.
        for bi in 0..self.func.blocks.len() {
            self.cur_block = Block(bi as u32);
            self.builder.switch_to_block(self.blocks[bi]);
            let stmts = self.func.blocks[bi].stmts.clone();
            let term = self.func.blocks[bi].term.clone();
            for stmt in &stmts {
                self.lower_stmt(stmt)?;
            }
            self.lower_term(&term)?;
        }

        self.builder.seal_all_blocks();
        Ok(())
    }

    fn emit_return_ok(&mut self) -> Result<(), CodegenError> {
        let restore_id = self.rt_imports.get(
            self.module,
            "noev_rt_ctrl_restore",
            &[self.ptr_ty, self.ptr_ty],
            None,
        )?;
        self.call_clif(restore_id, &[self.rt_val, self.ctrl_depth]);

        let ret_layout = self.layouts.layout(self.func.locals[0].ty);
        if ret_layout.size > 0
            && let (Some(slot), Some(ret_ptr)) = (self.slots[0], self.ret_ptr)
        {
            let src = self.builder.ins().stack_addr(self.ptr_ty, slot, 0);
            self.emit_memcpy(ret_ptr, src, ret_layout.size, ret_layout.align);
        }
        let one = self.builder.ins().iconst(types::I8, 1);
        self.builder.ins().return_(&[one]);
        Ok(())
    }

    fn call_clif(&mut self, id: ClifFuncId, args: &[ClifValue]) -> cranelift_codegen::ir::Inst {
        let fref = self.module.declare_func_in_func(id, self.builder.func);
        self.builder.ins().call(fref, args)
    }

    fn data_addr(&mut self, id: DataId) -> ClifValue {
        let gv = self.module.declare_data_in_func(id, self.builder.func);
        self.builder.ins().symbol_value(self.ptr_ty, gv)
    }

    fn func_addr(&mut self, id: ClifFuncId) -> ClifValue {
        let fref = self.module.declare_func_in_func(id, self.builder.func);
        self.builder.ins().func_addr(self.ptr_ty, fref)
    }

    fn check_ok(&mut self, ok: ClifValue) {
        let cont = self.builder.create_block();
        self.builder.ins().brif(ok, cont, &[], self.err_block, &[]);
        self.builder.switch_to_block(cont);
    }

    fn alloc_temp_slot(&mut self, size: u32, align: u32) -> ClifValue {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size.max(1),
            LayoutCache::align_shift(align),
        ));
        self.builder.ins().stack_addr(self.ptr_ty, slot, 0)
    }

    fn emit_memcpy(&mut self, dst: ClifValue, src: ClifValue, size: u32, align: u32) {
        if size == 0 {
            return;
        }
        if size <= 64 {
            let mut off = 0u32;
            while off < size {
                let rem = size - off;
                let (ty, step) = if rem >= 8 {
                    (types::I64, 8)
                } else if rem >= 4 {
                    (types::I32, 4)
                } else if rem >= 2 {
                    (types::I16, 2)
                } else {
                    (types::I8, 1)
                };
                let v = self
                    .builder
                    .ins()
                    .load(ty, MemFlagsData::new(), src, off as i32);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), v, dst, off as i32);
                off += step;
            }
        } else {
            let align_u8 = align.min(16) as u8;
            let config = self.module.target_config();
            self.builder.emit_small_memory_copy(
                config,
                dst,
                src,
                u64::from(size),
                align_u8,
                align_u8,
                true,
                MemFlagsData::new(),
            );
        }
    }

    /// Resolves a [`Place`] to its native memory address (when non-ZST) and concrete [`Ty`].
    fn place_addr(&mut self, place: &Place) -> Result<(Option<ClifValue>, Ty), CodegenError> {
        let mut ty = self.func.locals[place.local.index()].ty;
        let mut addr = self.slots[place.local.index()]
            .map(|s| self.builder.ins().stack_addr(self.ptr_ty, s, 0));
        let mut prefix = Place::local(place.local);

        for proj in &place.proj {
            match proj {
                Proj::Deref => {
                    let inner = match self.program.types.kind(ty) {
                        TyKind::Ref { inner, .. } => *inner,
                        _ => ty,
                    };
                    if let Some(p) = addr {
                        let loaded =
                            self.builder
                                .ins()
                                .load(self.ptr_ty, MemFlagsData::new(), p, 0);
                        addr = Some(loaded);
                    }
                    ty = inner;
                }
                Proj::Field(fi) => {
                    let l = self.layouts.layout(ty);
                    match self.program.types.kind(ty).clone() {
                        TyKind::Tuple(elems) => {
                            let off = l.fields[*fi as usize];
                            if let Some(p) = addr
                                && off > 0
                            {
                                addr = Some(self.builder.ins().iadd_imm_s(p, i64::from(off)));
                            }
                            ty = elems[*fi as usize];
                        }
                        TyKind::Struct(sid) => {
                            let def = &self.program.types.structs[sid.index()];
                            if let Some(variants) = &def.variants {
                                // Enum variant payload field: look up statically-inferred variant.
                                let vi = self
                                    .facts
                                    .get(self.cur_block.index())
                                    .and_then(|m| m.get(&PlaceKey::from_place(&prefix)).copied())
                                    .or_else(|| {
                                        variants
                                            .iter()
                                            .position(|v| (*fi as usize) < v.data.len())
                                            .map(|i| i as u32)
                                    })
                                    .unwrap_or(0) as usize;
                                let off = l.enum_variants[vi][*fi as usize];
                                if let Some(p) = addr
                                    && off > 0
                                {
                                    addr = Some(self.builder.ins().iadd_imm_s(p, i64::from(off)));
                                }
                                ty = variants[vi].data[*fi as usize];
                            } else {
                                let off = l.fields[*fi as usize];
                                if let Some(p) = addr
                                    && off > 0
                                {
                                    addr = Some(self.builder.ins().iadd_imm_s(p, i64::from(off)));
                                }
                                ty = def.fields[*fi as usize].1;
                            }
                        }
                        _ => {}
                    }
                }
                Proj::Index(idx_op) => {
                    let (elem, len) = match self.program.types.kind(ty) {
                        &TyKind::Array(elem, len) => (elem, len),
                        _ => (Ty::UNIT, 0),
                    };
                    let idx_val = self.eval_scalar(idx_op)?;
                    let out_slot = self.alloc_temp_slot(8, 8);
                    let len_val = self.builder.ins().iconst(self.ptr_ty, len as i64);
                    let bc_id = self.rt_imports.get(
                        self.module,
                        "noev_rt_bounds_check",
                        &[self.ptr_ty, types::I64, self.ptr_ty, self.ptr_ty],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(bc_id, &[self.rt_val, idx_val, len_val, out_slot]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                    let checked_idx =
                        self.builder
                            .ins()
                            .load(self.ptr_ty, MemFlagsData::new(), out_slot, 0);
                    let elem_size = self.layouts.layout(elem).size;
                    if let Some(p) = addr {
                        let byte_off = if elem_size == 1 {
                            checked_idx
                        } else {
                            self.builder
                                .ins()
                                .imul_imm_s(checked_idx, i64::from(elem_size))
                        };
                        addr = Some(self.builder.ins().iadd(p, byte_off));
                    }
                    ty = elem;
                }
            }
            prefix.proj.push(proj.clone());
        }

        if self.layouts.layout(ty).size == 0 {
            addr = None;
        }
        Ok((addr, ty))
    }

    /// Evaluates a scalar [`Operand`] into a Cranelift SSA register value.
    fn eval_scalar(&mut self, op: &Operand) -> Result<ClifValue, CodegenError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let (addr, ty) = self.place_addr(p)?;
                let l = self.layouts.layout(ty);
                let clif = l.scalar.ok_or_else(|| {
                    CodegenError(format!("expected scalar operand, got {:?}", ty))
                })?;
                let ptr = addr.expect("scalar has address");
                Ok(self.builder.ins().load(clif, MemFlagsData::new(), ptr, 0))
            }
            Operand::Const(c) => self.eval_const_scalar(*c),
        }
    }

    fn eval_const_scalar(&mut self, c: Const) -> Result<ClifValue, CodegenError> {
        Ok(match c {
            Const::Int(n) => {
                if (MIN_INLINE_INT..=MAX_INLINE_INT).contains(&n) {
                    self.builder.ins().iconst(types::I64, n << 1)
                } else {
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_int_from_i64",
                        &[self.ptr_ty, types::I64],
                        Some(types::I64),
                    )?;
                    let v = self.builder.ins().iconst(types::I64, n);
                    let call = self.call_clif(id, &[self.rt_val, v]);
                    self.builder.inst_results(call)[0]
                }
            }
            Const::Sized(v, w) => {
                let ty = match w.bits {
                    8 => types::I8,
                    16 => types::I16,
                    32 => types::I32,
                    _ => types::I64,
                };
                self.builder.ins().iconst(ty, v as u64 as i64)
            }
            Const::Float(v) => self.builder.ins().f64const(v),
            Const::F32(v) => self.builder.ins().f32const(v),
            Const::Bool(b) | Const::Bit(b) => self.builder.ins().iconst(types::I8, i64::from(b)),
            Const::Unit | Const::Complex(..) | Const::C32(..) => {
                return Err(CodegenError(format!("non-scalar constant {c:?}")));
            }
        })
    }

    /// Returns a memory pointer to an operand's value (materializing constants into a temp slot)
    /// along with its concrete [`Ty`]. Returns `(None, ty)` for ZSTs.
    fn operand_addr(&mut self, op: &Operand) -> Result<(Option<ClifValue>, Ty), CodegenError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => self.place_addr(p),
            Operand::Const(c) => {
                let ty = const_ty(*c);
                let l = self.layouts.layout(ty);
                if l.size == 0 {
                    return Ok((None, ty));
                }
                let ptr = self.alloc_temp_slot(l.size, l.align);
                self.store_const(ptr, *c)?;
                Ok((Some(ptr), ty))
            }
        }
    }

    fn store_const(&mut self, dst: ClifValue, c: Const) -> Result<(), CodegenError> {
        match c {
            Const::Unit => {}
            Const::Complex(re, im) => {
                let r = self.builder.ins().f64const(re);
                let i = self.builder.ins().f64const(im);
                self.builder.ins().store(MemFlagsData::new(), r, dst, 0);
                self.builder.ins().store(MemFlagsData::new(), i, dst, 8);
            }
            Const::C32(re, im) => {
                let r = self.builder.ins().f32const(re);
                let i = self.builder.ins().f32const(im);
                self.builder.ins().store(MemFlagsData::new(), r, dst, 0);
                self.builder.ins().store(MemFlagsData::new(), i, dst, 4);
            }
            other => {
                let v = self.eval_const_scalar(other)?;
                self.builder.ins().store(MemFlagsData::new(), v, dst, 0);
            }
        }
        Ok(())
    }

    /// Copies an [`Operand`] into `dst` (for non-ZST types).
    fn store_operand(&mut self, dst: Option<ClifValue>, op: &Operand) -> Result<(), CodegenError> {
        let Some(dst) = dst else {
            return Ok(());
        };
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let (src, ty) = self.place_addr(p)?;
                let l = self.layouts.layout(ty);
                if let Some(src) = src {
                    self.emit_memcpy(dst, src, l.size, l.align);
                }
            }
            Operand::Const(c) => self.store_const(dst, *c)?,
        }
        Ok(())
    }

    /// Loads a `Qubit` handle from a `Place` (auto-dereferencing if `Place` has type `Ref(Qubit)`).
    fn load_qubit_place(&mut self, place: &Place) -> Result<ClifValue, CodegenError> {
        let (addr, ty) = self.place_addr(place)?;
        let mut ptr = addr.expect("qubit place has address");
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.program.types.kind(cur) {
            ptr = self
                .builder
                .ins()
                .load(self.ptr_ty, MemFlagsData::new(), ptr, 0);
            cur = *inner;
        }
        Ok(self
            .builder
            .ins()
            .load(self.ptr_ty, MemFlagsData::new(), ptr, 0))
    }

    /// Collects all `NoevQubit` handles from a slice of places (each possibly `Ref` to a qubit,
    /// array, tuple, or struct of qubits) into a temporary stack array, returning `(ptr, count)`.
    fn collect_qubit_handles(
        &mut self,
        places: &[Place],
    ) -> Result<(ClifValue, usize), CodegenError> {
        let mut loaded = Vec::new();
        for place in places {
            let (addr, ty) = self.place_addr(place)?;
            let Some(mut ptr) = addr else { continue };
            let mut cur = ty;
            while let TyKind::Ref { inner, .. } = self.program.types.kind(cur) {
                ptr = self
                    .builder
                    .ins()
                    .load(self.ptr_ty, MemFlagsData::new(), ptr, 0);
                cur = *inner;
            }
            let offsets = self.layouts.qubit_offsets(cur);
            for off in offsets {
                let q = self
                    .builder
                    .ins()
                    .load(self.ptr_ty, MemFlagsData::new(), ptr, off as i32);
                loaded.push(q);
            }
        }
        let n = loaded.len();
        if n == 0 {
            let null = self.builder.ins().iconst(self.ptr_ty, 0);
            return Ok((null, 0));
        }
        let stride = u32::from(self.module.target_config().pointer_bytes());
        let buf = self.alloc_temp_slot(stride * n as u32, stride);
        for (i, q) in loaded.into_iter().enumerate() {
            self.builder
                .ins()
                .store(MemFlagsData::new(), q, buf, (i as u32 * stride) as i32);
        }
        Ok((buf, n))
    }

    /// Packs a list of operands into a temporary buffer matching `pack_types`, returning a pointer.
    fn pack_args_buf(
        &mut self,
        args: &[Operand],
        param_tys: &[Ty],
    ) -> Result<ClifValue, CodegenError> {
        let (size, align, offsets) = self.layouts.pack_types(param_tys);
        if size == 0 {
            return Ok(self.builder.ins().iconst(self.ptr_ty, 0));
        }
        let buf = self.alloc_temp_slot(size, align);
        for (op, (&ty, &off)) in args.iter().zip(param_tys.iter().zip(&offsets)) {
            let l = self.layouts.layout(ty);
            if l.size == 0 {
                continue;
            }
            let dst = if off == 0 {
                buf
            } else {
                self.builder.ins().iadd_imm_s(buf, i64::from(off))
            };
            self.store_operand(Some(dst), op)?;
        }
        Ok(buf)
    }

    // -------------------------------------------------------------------------
    // Statements
    // -------------------------------------------------------------------------

    fn lower_stmt(&mut self, stmt: &Stmt) -> Result<(), CodegenError> {
        match stmt {
            Stmt::Assign(dest, rv) => self.lower_assign(dest, rv),
            Stmt::Alloc(place) => {
                let (addr, _) = self.place_addr(place)?;
                let dst = addr.expect("Qubit place");
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_qubit_allocate",
                    &[self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, dst]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Stmt::Prepare { qubit, label } => {
                let q = self.load_qubit_place(qubit)?;
                let (laddr, lty) = self.operand_addr(label)?;
                if lty == Ty::BIT {
                    let b = self.eval_scalar(label)?;
                    let do_x = self.builder.create_block();
                    let done = self.builder.create_block();
                    self.builder.ins().brif(b, do_x, &[], done, &[]);
                    self.builder.switch_to_block(do_x);
                    let x_id = self.rt_imports.rt_qubit(self.module, "noev_qis_x")?;
                    let call = self.call_clif(x_id, &[self.rt_val, q]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                    self.builder.ins().jump(done, &[]);
                    self.builder.switch_to_block(done);
                } else {
                    let tag = self.builder.ins().load(
                        types::I32,
                        MemFlagsData::new(),
                        laddr.expect("State enum"),
                        0,
                    );
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_prepare_state",
                        &[self.ptr_ty, self.ptr_ty, types::I32],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, q, tag]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                }
                Ok(())
            }
            Stmt::IntoZ { qubit, label } => {
                let (laddr, lty) = self.operand_addr(label)?;
                if lty != Ty::BIT {
                    let q = self.load_qubit_place(qubit)?;
                    let tag = self.builder.ins().load(
                        types::I32,
                        MemFlagsData::new(),
                        laddr.expect("State enum"),
                        0,
                    );
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_into_z_state",
                        &[self.ptr_ty, self.ptr_ty, types::I32],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, q, tag]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                }
                Ok(())
            }
            Stmt::Gate {
                gate,
                adjoint,
                qubits,
                params,
            } => self.lower_gate(*gate, *adjoint, qubits, params),
            Stmt::Measure { dest, qubit } => {
                let q = self.load_qubit_place(qubit)?;
                let (dst_addr, _) = self.place_addr(dest)?;
                let res_slot = self.alloc_temp_slot(8, 8);
                let mz_id = self.rt_imports.get(
                    self.module,
                    "noev_qis_mz",
                    &[self.ptr_ty, self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(mz_id, &[self.rt_val, q, res_slot]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                let res_val =
                    self.builder
                        .ins()
                        .load(self.ptr_ty, MemFlagsData::new(), res_slot, 0);
                let read_id = self.rt_imports.get(
                    self.module,
                    "noev_rt_result_read",
                    &[self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let rcall = self.call_clif(read_id, &[self.rt_val, res_val]);
                let bit = self.builder.inst_results(rcall)[0];
                if let Some(d) = dst_addr {
                    self.builder.ins().store(MemFlagsData::new(), bit, d, 0);
                }
                Ok(())
            }
            Stmt::Reset(place) => {
                let q = self.load_qubit_place(place)?;
                let id = self
                    .rt_imports
                    .rt_qubit(self.module, "noev_rt_qubit_reset")?;
                let call = self.call_clif(id, &[self.rt_val, q]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Stmt::Release(place) => {
                let q = self.load_qubit_place(place)?;
                let id = self
                    .rt_imports
                    .rt_qubit(self.module, "noev_rt_qubit_release")?;
                let call = self.call_clif(id, &[self.rt_val, q]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Stmt::CtrlPush(places) => {
                let (buf, n) = self.collect_qubit_handles(places)?;
                let n_val = self.builder.ins().iconst(self.ptr_ty, n as i64);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_ctrl_push",
                    &[self.ptr_ty, self.ptr_ty, self.ptr_ty],
                    None,
                )?;
                self.call_clif(id, &[self.rt_val, buf, n_val]);
                Ok(())
            }
            Stmt::CtrlPop => {
                let id = self.rt_imports.rt_void(self.module, "noev_rt_ctrl_pop")?;
                self.call_clif(id, &[self.rt_val]);
                Ok(())
            }
            Stmt::WithStart => {
                let id = self.rt_imports.rt_void(self.module, "noev_rt_with_start")?;
                self.call_clif(id, &[self.rt_val]);
                Ok(())
            }
            Stmt::WithDo => {
                let id = self.rt_imports.rt_void(self.module, "noev_rt_with_do")?;
                self.call_clif(id, &[self.rt_val]);
                Ok(())
            }
            Stmt::WithEnd => {
                let id = self.rt_imports.rt_bool(self.module, "noev_rt_with_end")?;
                let call = self.call_clif(id, &[self.rt_val]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Stmt::Print { stderr, args } => self.lower_print(*stderr, args),
            Stmt::Sample {
                dest,
                func,
                args,
                shots,
                elem,
            } => self.lower_sample(dest, *func, args, shots, *elem),
            Stmt::Expectation {
                dest,
                prep,
                observable,
                precision,
            } => self.lower_expectation(dest, prep, observable, precision.as_ref()),
            Stmt::Overlap {
                dest,
                bra,
                ket,
                observable,
                precision,
            } => self.lower_overlap(dest, bra, ket, observable.as_ref(), precision.as_ref()),
        }
    }

    fn lower_gate(
        &mut self,
        gate: Gate,
        adjoint: bool,
        qubits: &[Place],
        params: &[Operand],
    ) -> Result<(), CodegenError> {
        let ok = match gate {
            Gate::Unitary => {
                let (qs_ptr, n_q) = self.collect_qubit_handles(qubits)?;
                let (mat_addr, _) = self.operand_addr(&params[0])?;
                let mat_ptr = mat_addr.expect("unitary matrix");
                let dim = 1usize << n_q;
                let n_q_val = self.builder.ins().iconst(self.ptr_ty, n_q as i64);
                let dim_val = self.builder.ins().iconst(self.ptr_ty, dim as i64);
                let adj_val = self.builder.ins().iconst(types::I8, i64::from(adjoint));
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_unitary_matrix",
                    &[
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        types::I8,
                    ],
                    Some(types::I8),
                )?;
                let call = self.call_clif(
                    id,
                    &[self.rt_val, qs_ptr, n_q_val, mat_ptr, dim_val, adj_val],
                );
                self.builder.inst_results(call)[0]
            }
            Gate::Swap => {
                let q0 = self.load_qubit_place(&qubits[0])?;
                let q1 = self.load_qubit_place(&qubits[1])?;
                let id = self.rt_imports.get(
                    self.module,
                    "noev_qis_swap",
                    &[self.ptr_ty, self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, q0, q1]);
                self.builder.inst_results(call)[0]
            }
            Gate::X | Gate::Y | Gate::Z | Gate::H | Gate::S | Gate::T => {
                let q = self.load_qubit_place(&qubits[0])?;
                let name = match (gate, adjoint) {
                    (Gate::X, _) => "noev_qis_x",
                    (Gate::Y, _) => "noev_qis_y",
                    (Gate::Z, _) => "noev_qis_z",
                    (Gate::H, _) => "noev_qis_h",
                    (Gate::S, false) => "noev_qis_s",
                    (Gate::S, true) => "noev_qis_s_adj",
                    (Gate::T, false) => "noev_qis_t",
                    (Gate::T, true) => "noev_qis_t_adj",
                    _ => unreachable!(),
                };
                let id = self.rt_imports.rt_qubit(self.module, name)?;
                let call = self.call_clif(id, &[self.rt_val, q]);
                self.builder.inst_results(call)[0]
            }
            Gate::Rx | Gate::Ry | Gate::Rz | Gate::P => {
                let q = self.load_qubit_place(&qubits[0])?;
                let mut theta = self.eval_scalar(&params[0])?;
                if adjoint {
                    theta = self.builder.ins().fneg(theta);
                }
                let name = match gate {
                    Gate::Rx => "noev_qis_rx",
                    Gate::Ry => "noev_qis_ry",
                    Gate::Rz => "noev_qis_rz",
                    Gate::P => "noev_qis_p",
                    _ => unreachable!(),
                };
                let id = self.rt_imports.rt_rot(self.module, name)?;
                let call = self.call_clif(id, &[self.rt_val, theta, q]);
                self.builder.inst_results(call)[0]
            }
            Gate::U => {
                let q = self.load_qubit_place(&qubits[0])?;
                let theta = self.eval_scalar(&params[0])?;
                let phi = self.eval_scalar(&params[1])?;
                let lambda = self.eval_scalar(&params[2])?;
                let name = if adjoint {
                    "noev_qis_u_adj"
                } else {
                    "noev_qis_u"
                };
                let id = self.rt_imports.get(
                    self.module,
                    name,
                    &[self.ptr_ty, types::F64, types::F64, types::F64, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, theta, phi, lambda, q]);
                self.builder.inst_results(call)[0]
            }
        };
        self.check_ok(ok);
        Ok(())
    }

    fn lower_print(&mut self, stderr: bool, args: &[(Operand, Ty)]) -> Result<(), CodegenError> {
        let n = args.len();
        let stride = size_of::<NoevPrintArg>() as u32;
        let arr_ptr = if n == 0 {
            self.builder.ins().iconst(self.ptr_ty, 0)
        } else {
            let buf = self.alloc_temp_slot(stride * n as u32, align_of::<NoevPrintArg>() as u32);
            for (i, (op, ty)) in args.iter().enumerate() {
                let (val_addr, _) = self.operand_addr(op)?;
                let val_ptr = val_addr.unwrap_or_else(|| self.builder.ins().iconst(self.ptr_ty, 0));
                let desc_id =
                    self.rodata
                        .type_desc(self.module, self.program, self.layouts, *ty)?;
                let desc_ptr = self.data_addr(desc_id);
                let base = i as i32 * stride as i32;
                self.builder.ins().store(
                    MemFlagsData::new(),
                    val_ptr,
                    buf,
                    base + offset_of!(NoevPrintArg, ptr) as i32,
                );
                self.builder.ins().store(
                    MemFlagsData::new(),
                    desc_ptr,
                    buf,
                    base + offset_of!(NoevPrintArg, ty) as i32,
                );
            }
            buf
        };
        let stderr_val = self.builder.ins().iconst(types::I8, i64::from(stderr));
        let n_val = self.builder.ins().iconst(self.ptr_ty, n as i64);
        let id = self.rt_imports.get(
            self.module,
            "noev_rt_print",
            &[self.ptr_ty, types::I8, self.ptr_ty, self.ptr_ty],
            Some(types::I8),
        )?;
        let call = self.call_clif(id, &[self.rt_val, stderr_val, arr_ptr, n_val]);
        let ok = self.builder.inst_results(call)[0];
        self.check_ok(ok);
        Ok(())
    }

    fn lower_sample(
        &mut self,
        dest: &Place,
        func: FuncId,
        args: &[Operand],
        shots: &Operand,
        elem: Ty,
    ) -> Result<(), CodegenError> {
        let (dst_addr, _) = self.place_addr(dest)?;
        let dst = dst_addr.expect("Distribution dest");
        let shots_val = self.eval_scalar(shots)?;
        let callee_fn = &self.program.functions[func.index()];
        let use_circuit = callee_fn.sampled && !has_feedback(self.program, func);
        let use_circ_val = self.builder.ins().iconst(types::I8, i64::from(use_circuit));
        let thunk_ptr = self.func_addr(self.thunk_ids[&func]);
        let param_tys: Vec<Ty> = (1..=callee_fn.params)
            .map(|i| callee_fn.locals[i].ty)
            .collect();
        let args_buf = self.pack_args_buf(args, &param_tys)?;
        let ret_ty = callee_fn.locals[0].ty;
        let ret_desc_id = self
            .rodata
            .type_desc(self.module, self.program, self.layouts, ret_ty)?;
        let ret_desc_ptr = self.data_addr(ret_desc_id);
        let elem_desc_id = self
            .rodata
            .type_desc(self.module, self.program, self.layouts, elem)?;
        let elem_desc_ptr = self.data_addr(elem_desc_id);

        let id = self.rt_imports.get(
            self.module,
            "noev_rt_sample_compiled",
            &[
                self.ptr_ty,
                types::I64,
                types::I8,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
            ],
            Some(types::I8),
        )?;
        let call = self.call_clif(
            id,
            &[
                self.rt_val,
                shots_val,
                use_circ_val,
                thunk_ptr,
                args_buf,
                ret_desc_ptr,
                elem_desc_ptr,
                dst,
            ],
        );
        let ok = self.builder.inst_results(call)[0];
        self.check_ok(ok);
        Ok(())
    }

    fn lower_prep_args(
        &mut self,
        prep: &Prep,
    ) -> Result<(ClifValue, ClifValue, ClifValue, ClifValue, ClifValue), CodegenError> {
        let null = self.builder.ins().iconst(self.ptr_ty, 0);
        match prep {
            Prep::Ket(s) => {
                let bytes_id = self.rodata.intern_bytes(self.module, s.as_bytes())?;
                let ptr = self.data_addr(bytes_id);
                let len = self.builder.ins().iconst(self.ptr_ty, s.len() as i64);
                Ok((ptr, len, null, null, null))
            }
            Prep::Call(fid, args) => {
                let callee = &self.program.functions[fid.index()];
                let param_tys: Vec<Ty> = (1..=callee.params).map(|i| callee.locals[i].ty).collect();
                let args_buf = self.pack_args_buf(args, &param_tys)?;
                let thunk_ptr = self.func_addr(self.thunk_ids[fid]);
                let ret_desc_id = self.rodata.type_desc(
                    self.module,
                    self.program,
                    self.layouts,
                    callee.locals[0].ty,
                )?;
                let ret_desc_ptr = self.data_addr(ret_desc_id);
                Ok((null, null, thunk_ptr, args_buf, ret_desc_ptr))
            }
        }
    }

    fn lower_expectation(
        &mut self,
        dest: &Place,
        prep: &Prep,
        observable: &Operand,
        precision: Option<&Operand>,
    ) -> Result<(), CodegenError> {
        let (dst_addr, _) = self.place_addr(dest)?;
        let dst = dst_addr.expect("Float dest");
        let (ket_ptr, ket_len, body, args_buf, ret_desc) = self.lower_prep_args(prep)?;
        let obs = self.eval_scalar(observable)?;
        let prec = match precision {
            Some(p) => self.eval_scalar(p)?,
            None => self.builder.ins().f64const(DEFAULT_PRECISION),
        };
        let id = self.rt_imports.get(
            self.module,
            "noev_rt_expectation_compiled",
            &[
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                types::I64,
                types::F64,
                self.ptr_ty,
            ],
            Some(types::I8),
        )?;
        let call = self.call_clif(
            id,
            &[
                self.rt_val,
                ket_ptr,
                ket_len,
                body,
                args_buf,
                ret_desc,
                obs,
                prec,
                dst,
            ],
        );
        let ok = self.builder.inst_results(call)[0];
        self.check_ok(ok);
        Ok(())
    }

    fn lower_overlap(
        &mut self,
        dest: &Place,
        bra: &Prep,
        ket: &Prep,
        observable: Option<&Operand>,
        precision: Option<&Operand>,
    ) -> Result<(), CodegenError> {
        let (dst_addr, _) = self.place_addr(dest)?;
        let dst = dst_addr.expect("Complex dest");
        let (b_ptr, b_len, b_body, b_args, b_desc) = self.lower_prep_args(bra)?;
        let (k_ptr, k_len, k_body, k_args, k_desc) = self.lower_prep_args(ket)?;
        let (has_obs, obs_val) = match observable {
            Some(o) => (
                self.builder.ins().iconst(types::I8, 1),
                self.eval_scalar(o)?,
            ),
            None => (
                self.builder.ins().iconst(types::I8, 0),
                self.builder.ins().iconst(types::I64, 0),
            ),
        };
        let prec = match precision {
            Some(p) => self.eval_scalar(p)?,
            None => self.builder.ins().f64const(DEFAULT_PRECISION),
        };
        let id = self.rt_imports.get(
            self.module,
            "noev_rt_overlap_compiled",
            &[
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                self.ptr_ty,
                types::I8,
                types::I64,
                types::F64,
                self.ptr_ty,
            ],
            Some(types::I8),
        )?;
        let call = self.call_clif(
            id,
            &[
                self.rt_val,
                b_ptr,
                b_len,
                b_body,
                b_args,
                b_desc,
                k_ptr,
                k_len,
                k_body,
                k_args,
                k_desc,
                has_obs,
                obs_val,
                prec,
                dst,
            ],
        );
        let ok = self.builder.inst_results(call)[0];
        self.check_ok(ok);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Assignments & Rvalues
    // -------------------------------------------------------------------------

    fn lower_assign(&mut self, dest: &Place, rv: &Rvalue) -> Result<(), CodegenError> {
        let (dst_addr, dst_ty) = self.place_addr(dest)?;
        let dst_layout = self.layouts.layout(dst_ty);

        match rv {
            Rvalue::Use(op) => self.store_operand(dst_addr, op),
            Rvalue::Unary(op, x) => self.lower_unary(dst_addr, dst_ty, *op, x),
            Rvalue::Binary(op, a, b) => self.lower_binary(dst_addr, dst_ty, *op, a, b),
            Rvalue::Aggregate(ops) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                match self.program.types.kind(dst_ty).clone() {
                    TyKind::Array(elem, _) => {
                        let stride = self.layouts.layout(elem).size;
                        for (i, op) in ops.iter().enumerate() {
                            if stride == 0 {
                                continue;
                            }
                            let ptr = if i == 0 {
                                dst
                            } else {
                                self.builder
                                    .ins()
                                    .iadd_imm_s(dst, i64::from(i as u32 * stride))
                            };
                            self.store_operand(Some(ptr), op)?;
                        }
                    }
                    TyKind::Tuple(_) | TyKind::Struct(_) => {
                        for (i, op) in ops.iter().enumerate() {
                            let off = dst_layout.fields[i];
                            let ptr = if off == 0 {
                                dst
                            } else {
                                self.builder.ins().iadd_imm_s(dst, i64::from(off))
                            };
                            self.store_operand(Some(ptr), op)?;
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            Rvalue::Repeat(op, count) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (src_addr, elem_ty) = self.operand_addr(op)?;
                let el = self.layouts.layout(elem_ty);
                if el.size == 0 || *count == 0 {
                    return Ok(());
                }
                let src = src_addr.expect("non-ZST repeat element");
                if *count <= 16 {
                    for i in 0..*count {
                        let ptr = if i == 0 {
                            dst
                        } else {
                            self.builder
                                .ins()
                                .iadd_imm_s(dst, (i as u32 * el.size) as i64)
                        };
                        self.emit_memcpy(ptr, src, el.size, el.align);
                    }
                } else {
                    // Runtime loop for large repeats to keep code size tiny.
                    let loop_bb = self.builder.create_block();
                    let body_bb = self.builder.create_block();
                    let done_bb = self.builder.create_block();
                    self.builder.append_block_param(loop_bb, self.ptr_ty);
                    let zero = self.builder.ins().iconst(self.ptr_ty, 0);
                    let limit = self.builder.ins().iconst(self.ptr_ty, *count as i64);
                    self.builder.ins().jump(loop_bb, &[zero.into()]);

                    self.builder.switch_to_block(loop_bb);
                    let idx = self.builder.block_params(loop_bb)[0];
                    let cond = self.builder.ins().icmp(IntCC::UnsignedLessThan, idx, limit);
                    self.builder.ins().brif(cond, body_bb, &[], done_bb, &[]);

                    self.builder.switch_to_block(body_bb);
                    let off = if el.size == 1 {
                        idx
                    } else {
                        self.builder.ins().imul_imm_s(idx, i64::from(el.size))
                    };
                    let ptr = self.builder.ins().iadd(dst, off);
                    self.emit_memcpy(ptr, src, el.size, el.align);
                    let next = self.builder.ins().iadd_imm_s(idx, 1);
                    self.builder.ins().jump(loop_bb, &[next.into()]);

                    self.builder.switch_to_block(done_bb);
                }
                Ok(())
            }
            Rvalue::Ref { place, .. } => {
                let (target_addr, _) = self.place_addr(place)?;
                if let Some(dst) = dst_addr {
                    let p =
                        target_addr.unwrap_or_else(|| self.builder.ins().iconst(self.ptr_ty, 0));
                    self.builder.ins().store(MemFlagsData::new(), p, dst, 0);
                }
                Ok(())
            }
            Rvalue::Convert(target, op) => self.lower_convert(dst_addr, *target, op),
            Rvalue::TryConvert(target, op) => self.lower_try_convert(dst_addr, dst_ty, *target, op),
            Rvalue::Math(math, ops) => self.lower_math(dst_addr, dst_ty, *math, ops),
            Rvalue::FloatConst(c) => {
                let val = match c {
                    FloatConst::Pi => std::f64::consts::PI,
                    FloatConst::Tau => std::f64::consts::TAU,
                    FloatConst::E => std::f64::consts::E,
                };
                if let Some(dst) = dst_addr {
                    let f = self.builder.ins().f64const(val);
                    self.builder.ins().store(MemFlagsData::new(), f, dst, 0);
                }
                Ok(())
            }
            Rvalue::Amplitudes(reg) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (reg_addr, reg_ty) = self.operand_addr(reg)?;
                let mut ptr = reg_addr.expect("register operand");
                let mut cur = reg_ty;
                while let TyKind::Ref { inner, .. } = self.program.types.kind(cur) {
                    ptr = self
                        .builder
                        .ins()
                        .load(self.ptr_ty, MemFlagsData::new(), ptr, 0);
                    cur = *inner;
                }
                let offsets = self.layouts.qubit_offsets(cur);
                let n_q = offsets.len();
                let stride = u32::from(self.module.target_config().pointer_bytes());
                let qbuf = if n_q == 0 {
                    self.builder.ins().iconst(self.ptr_ty, 0)
                } else {
                    let b = self.alloc_temp_slot(stride * n_q as u32, stride);
                    for (i, off) in offsets.into_iter().enumerate() {
                        let q = self.builder.ins().load(
                            self.ptr_ty,
                            MemFlagsData::new(),
                            ptr,
                            off as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            q,
                            b,
                            (i as u32 * stride) as i32,
                        );
                    }
                    b
                };
                let n_q_val = self.builder.ins().iconst(self.ptr_ty, n_q as i64);
                let out_len = self
                    .builder
                    .ins()
                    .iconst(self.ptr_ty, (1usize << n_q) as i64);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_amplitudes",
                    &[
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                    ],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, qbuf, n_q_val, dst, out_len]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Rvalue::LabelBit(op) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (src, lty) = self.operand_addr(op)?;
                if lty == Ty::BIT {
                    let b = self.eval_scalar(op)?;
                    self.builder.ins().store(MemFlagsData::new(), b, dst, 0);
                } else {
                    let tag = self.builder.ins().load(
                        types::I32,
                        MemFlagsData::new(),
                        src.expect("State enum"),
                        0,
                    );
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_label_bit_state",
                        &[types::I32],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(id, &[tag]);
                    let bit = self.builder.inst_results(call)[0];
                    self.builder.ins().store(MemFlagsData::new(), bit, dst, 0);
                }
                Ok(())
            }
            Rvalue::BigInt(text) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let bid = self.rodata.intern_bytes(self.module, text.as_bytes())?;
                let ptr = self.data_addr(bid);
                let len = self.builder.ins().iconst(self.ptr_ty, text.len() as i64);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_int_parse",
                    &[self.ptr_ty, self.ptr_ty, self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, ptr, len, dst]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Rvalue::Pauli(p, idx) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let idx_val = self.eval_scalar(idx)?;
                let code = match p {
                    Pauli::X => 0i64,
                    Pauli::Y => 1,
                    Pauli::Z => 2,
                };
                let p_val = self.builder.ins().iconst(types::I8, code);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_obs_pauli",
                    &[self.ptr_ty, types::I8, types::I64, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, p_val, idx_val, dst]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Rvalue::Identity => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_obs_identity",
                    &[self.ptr_ty],
                    Some(types::I64),
                )?;
                let call = self.call_clif(id, &[self.rt_val]);
                let handle = self.builder.inst_results(call)[0];
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), handle, dst, 0);
                Ok(())
            }
            Rvalue::Projector {
                x_basis,
                index,
                bit,
            } => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let idx_val = self.eval_scalar(index)?;
                let bit_val = self.eval_scalar(bit)?;
                let xb = self.builder.ins().iconst(types::I8, i64::from(*x_basis));
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_obs_projector",
                    &[self.ptr_ty, types::I8, types::I64, types::I8, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, xb, idx_val, bit_val, dst]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                Ok(())
            }
            Rvalue::Format(parts) => self.lower_format(dst_addr, parts),
            Rvalue::Dist(method, ops, elem) => self.lower_dist(dst_addr, *method, ops, *elem),
            Rvalue::Variant(tag, ops) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let t = self.builder.ins().iconst(types::I32, i64::from(*tag));
                self.builder.ins().store(MemFlagsData::new(), t, dst, 0);
                let v_offsets = &dst_layout.enum_variants[*tag as usize];
                for (i, op) in ops.iter().enumerate() {
                    let off = v_offsets[i];
                    let ptr = self.builder.ins().iadd_imm_s(dst, i64::from(off));
                    self.store_operand(Some(ptr), op)?;
                }
                Ok(())
            }
            Rvalue::Tag(op) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (src, _) = self.operand_addr(op)?;
                let tag32 = self.builder.ins().load(
                    types::I32,
                    MemFlagsData::new(),
                    src.expect("enum value"),
                    0,
                );
                let tag64 = self.builder.ins().uextend(types::I64, tag32);
                // Inline tagged Int: tag << 1 (always fits in 63 bits).
                let tagged = self.builder.ins().ishl_imm_u(tag64, 1);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), tagged, dst, 0);
                Ok(())
            }
        }
    }

    fn lower_unary(
        &mut self,
        dst_addr: Option<ClifValue>,
        dst_ty: Ty,
        op: UnOp,
        x: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        match self.program.types.kind(dst_ty) {
            TyKind::Int => {
                let v = self.eval_scalar(x)?;
                let code = match op {
                    UnOp::Neg => 0i64,
                    UnOp::BitNot => 1,
                    UnOp::Not => 1,
                };
                let op_val = self.builder.ins().iconst(types::I8, code);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_int_unary",
                    &[self.ptr_ty, types::I8, types::I64],
                    Some(types::I64),
                )?;
                let call = self.call_clif(id, &[self.rt_val, op_val, v]);
                let res = self.builder.inst_results(call)[0];
                self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
            }
            TyKind::Float | TyKind::F32 => {
                let v = self.eval_scalar(x)?;
                let res = self.builder.ins().fneg(v);
                self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
            }
            TyKind::Complex => {
                let (src, _) = self.operand_addr(x)?;
                let src = src.expect("Complex");
                let re = self
                    .builder
                    .ins()
                    .load(types::F64, MemFlagsData::new(), src, 0);
                let im = self
                    .builder
                    .ins()
                    .load(types::F64, MemFlagsData::new(), src, 8);
                let nre = self.builder.ins().fneg(re);
                let nim = self.builder.ins().fneg(im);
                self.builder.ins().store(MemFlagsData::new(), nre, dst, 0);
                self.builder.ins().store(MemFlagsData::new(), nim, dst, 8);
            }
            TyKind::C32 => {
                let (src, _) = self.operand_addr(x)?;
                let src = src.expect("c32");
                let re = self
                    .builder
                    .ins()
                    .load(types::F32, MemFlagsData::new(), src, 0);
                let im = self
                    .builder
                    .ins()
                    .load(types::F32, MemFlagsData::new(), src, 4);
                let nre = self.builder.ins().fneg(re);
                let nim = self.builder.ins().fneg(im);
                self.builder.ins().store(MemFlagsData::new(), nre, dst, 0);
                self.builder.ins().store(MemFlagsData::new(), nim, dst, 4);
            }
            TyKind::Bool | TyKind::Bit => {
                let v = self.eval_scalar(x)?;
                let res = self.builder.ins().bxor_imm_u(v, 1);
                self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
            }
            TyKind::Observable => {
                let v = self.eval_scalar(x)?;
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_obs_neg",
                    &[self.ptr_ty, types::I64],
                    Some(types::I64),
                )?;
                let call = self.call_clif(id, &[self.rt_val, v]);
                let res = self.builder.inst_results(call)[0];
                self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
            }
            _ if dst_ty.width().is_some() => {
                let w = dst_ty.width().expect("sized int");
                let v = self.eval_scalar(x)?;
                match op {
                    UnOp::BitNot => {
                        let res = self.builder.ins().bnot(v);
                        self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                    }
                    UnOp::Neg => {
                        if let Operand::Const(Const::Sized(cv, cw)) = *x
                            && let Some(neg) = cv.checked_neg()
                            && cw.holds(neg)
                        {
                            let clif_ty = match cw.bits {
                                8 => types::I8,
                                16 => types::I16,
                                32 => types::I32,
                                _ => types::I64,
                            };
                            let cval = self.builder.ins().iconst(clif_ty, neg as u64 as i64);
                            self.builder.ins().store(MemFlagsData::new(), cval, dst, 0);
                            return Ok(());
                        }
                        let wide = self.widen_sized_to_u64(v, w.bits, w.signed);
                        let out_slot = self.alloc_temp_slot(8, 8);
                        let bits_v = self.builder.ins().iconst(types::I32, i64::from(w.bits));
                        let sgn_v = self.builder.ins().iconst(types::I8, i64::from(w.signed));
                        let id = self.rt_imports.get(
                            self.module,
                            "noev_rt_sized_neg",
                            &[self.ptr_ty, types::I64, types::I32, types::I8, self.ptr_ty],
                            Some(types::I8),
                        )?;
                        let call =
                            self.call_clif(id, &[self.rt_val, wide, bits_v, sgn_v, out_slot]);
                        let ok = self.builder.inst_results(call)[0];
                        self.check_ok(ok);
                        let raw =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlagsData::new(), out_slot, 0);
                        let narrowed = self.narrow_u64_to_sized(raw, w.bits);
                        self.builder
                            .ins()
                            .store(MemFlagsData::new(), narrowed, dst, 0);
                    }
                    UnOp::Not => unreachable!(),
                }
            }
            _ => {
                return Err(CodegenError(format!(
                    "unsupported unary {op:?} on {:?}",
                    dst_ty
                )));
            }
        }
        Ok(())
    }

    fn widen_sized_to_u64(&mut self, v: ClifValue, bits: u32, signed: bool) -> ClifValue {
        if bits == 64 {
            v
        } else if signed {
            self.builder.ins().sextend(types::I64, v)
        } else {
            self.builder.ins().uextend(types::I64, v)
        }
    }

    fn narrow_u64_to_sized(&mut self, raw: ClifValue, bits: u32) -> ClifValue {
        match bits {
            8 => self.builder.ins().ireduce(types::I8, raw),
            16 => self.builder.ins().ireduce(types::I16, raw),
            32 => self.builder.ins().ireduce(types::I32, raw),
            _ => raw,
        }
    }

    fn lower_binary(
        &mut self,
        dst_addr: Option<ClifValue>,
        _dst_ty: Ty,
        op: BinOp,
        a: &Operand,
        b: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let (a_addr, a_ty) = self.operand_addr(a)?;
        let (b_addr, b_ty) = self.operand_addr(b)?;

        // Observable operations (including Float * Observable and Observable * Float).
        if a_ty == Ty::OBSERVABLE || b_ty == Ty::OBSERVABLE {
            let (code, obs_a, obs_b, scale) = if a_ty == Ty::OBSERVABLE && b_ty == Ty::OBSERVABLE {
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                let zero = self.builder.ins().f64const(0.0);
                let c = match op {
                    BinOp::Add => 0i64,
                    BinOp::Sub => 1,
                    BinOp::Mul => 2,
                    _ => return Err(CodegenError(format!("unsupported obs op {op:?}"))),
                };
                (c, va, vb, zero)
            } else if a_ty == Ty::FLOAT && b_ty == Ty::OBSERVABLE {
                let sc = self.eval_scalar(a)?;
                let obs = self.eval_scalar(b)?;
                let zero = self.builder.ins().iconst(types::I64, 0);
                (3i64, obs, zero, sc)
            } else {
                let obs = self.eval_scalar(a)?;
                let sc = self.eval_scalar(b)?;
                let zero = self.builder.ins().iconst(types::I64, 0);
                (3i64, obs, zero, sc)
            };
            let op_v = self.builder.ins().iconst(types::I8, code);
            let id = self.rt_imports.get(
                self.module,
                "noev_rt_obs_binary",
                &[
                    self.ptr_ty,
                    types::I8,
                    types::I64,
                    types::I64,
                    types::F64,
                    self.ptr_ty,
                ],
                Some(types::I8),
            )?;
            let call = self.call_clif(id, &[self.rt_val, op_v, obs_a, obs_b, scale, dst]);
            let ok = self.builder.inst_results(call)[0];
            self.check_ok(ok);
            return Ok(());
        }

        match self.program.types.kind(a_ty).clone() {
            TyKind::Int => {
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                match op {
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cmp_id = self.rt_imports.get(
                            self.module,
                            "noev_rt_int_cmp",
                            &[self.ptr_ty, types::I64, types::I64],
                            Some(types::I8),
                        )?;
                        let call = self.call_clif(cmp_id, &[self.rt_val, va, vb]);
                        let ord = self.builder.inst_results(call)[0];
                        let cc = match op {
                            BinOp::Eq => IntCC::Equal,
                            BinOp::NotEq => IntCC::NotEqual,
                            BinOp::Lt => IntCC::SignedLessThan,
                            BinOp::Le => IntCC::SignedLessThanOrEqual,
                            BinOp::Gt => IntCC::SignedGreaterThan,
                            BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                            _ => unreachable!(),
                        };
                        let res = self.builder.ins().icmp_imm_s(cc, ord, 0);
                        self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                    }
                    _ => {
                        let code = match op {
                            BinOp::Add => 0i64,
                            BinOp::Sub => 1,
                            BinOp::Mul => 2,
                            BinOp::Div => 3,
                            BinOp::Rem => 4,
                            BinOp::BitAnd => 5,
                            BinOp::BitOr => 6,
                            BinOp::BitXor => 7,
                            BinOp::Shl => 8,
                            BinOp::Shr => 9,
                            _ => return Err(CodegenError(format!("unsupported Int op {op:?}"))),
                        };
                        let op_v = self.builder.ins().iconst(types::I8, code);
                        let id = self.rt_imports.get(
                            self.module,
                            "noev_rt_int_binary",
                            &[self.ptr_ty, types::I8, types::I64, types::I64, self.ptr_ty],
                            Some(types::I8),
                        )?;
                        let call = self.call_clif(id, &[self.rt_val, op_v, va, vb, dst]);
                        let ok = self.builder.inst_results(call)[0];
                        self.check_ok(ok);
                    }
                }
            }
            TyKind::Float | TyKind::F32 => {
                let is_f32 = a_ty == Ty::F32;
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                let res = match op {
                    BinOp::Add => self.builder.ins().fadd(va, vb),
                    BinOp::Sub => self.builder.ins().fsub(va, vb),
                    BinOp::Mul => self.builder.ins().fmul(va, vb),
                    BinOp::Div => self.builder.ins().fdiv(va, vb),
                    BinOp::Rem => {
                        let name = if is_f32 {
                            "noev_rt_f32_math"
                        } else {
                            "noev_rt_f64_math"
                        };
                        let ft = if is_f32 { types::F32 } else { types::F64 };
                        let id = self.rt_imports.get(
                            self.module,
                            name,
                            &[types::I8, ft, ft],
                            Some(ft),
                        )?;
                        let op_v = self.builder.ins().iconst(types::I8, 18);
                        let call = self.call_clif(id, &[op_v, va, vb]);
                        self.builder.inst_results(call)[0]
                    }
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cc = match op {
                            BinOp::Eq => FloatCC::Equal,
                            BinOp::NotEq => FloatCC::NotEqual,
                            BinOp::Lt => FloatCC::LessThan,
                            BinOp::Le => FloatCC::LessThanOrEqual,
                            BinOp::Gt => FloatCC::GreaterThan,
                            BinOp::Ge => FloatCC::GreaterThanOrEqual,
                            _ => unreachable!(),
                        };
                        self.builder.ins().fcmp(cc, va, vb)
                    }
                    _ => return Err(CodegenError(format!("unsupported float op {op:?}"))),
                };
                self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
            }
            TyKind::Complex => {
                let pa = a_addr.expect("Complex a");
                let pb = b_addr.expect("Complex b");
                let ar = self
                    .builder
                    .ins()
                    .load(types::F64, MemFlagsData::new(), pa, 0);
                let ai = self
                    .builder
                    .ins()
                    .load(types::F64, MemFlagsData::new(), pa, 8);
                let br = self
                    .builder
                    .ins()
                    .load(types::F64, MemFlagsData::new(), pb, 0);
                let bi = self
                    .builder
                    .ins()
                    .load(types::F64, MemFlagsData::new(), pb, 8);
                match op {
                    BinOp::Add => {
                        let rr = self.builder.ins().fadd(ar, br);
                        let ri = self.builder.ins().fadd(ai, bi);
                        self.builder.ins().store(MemFlagsData::new(), rr, dst, 0);
                        self.builder.ins().store(MemFlagsData::new(), ri, dst, 8);
                    }
                    BinOp::Sub => {
                        let rr = self.builder.ins().fsub(ar, br);
                        let ri = self.builder.ins().fsub(ai, bi);
                        self.builder.ins().store(MemFlagsData::new(), rr, dst, 0);
                        self.builder.ins().store(MemFlagsData::new(), ri, dst, 8);
                    }
                    BinOp::Mul => {
                        let t1 = self.builder.ins().fmul(ar, br);
                        let t2 = self.builder.ins().fmul(ai, bi);
                        let rr = self.builder.ins().fsub(t1, t2);
                        let t3 = self.builder.ins().fmul(ar, bi);
                        let t4 = self.builder.ins().fmul(ai, br);
                        let ri = self.builder.ins().fadd(t3, t4);
                        self.builder.ins().store(MemFlagsData::new(), rr, dst, 0);
                        self.builder.ins().store(MemFlagsData::new(), ri, dst, 8);
                    }
                    BinOp::Div => {
                        let op_v = self.builder.ins().iconst(types::I8, 0);
                        let id = self.rt_imports.get(
                            self.module,
                            "noev_rt_complex_op",
                            &[
                                types::I8,
                                types::F64,
                                types::F64,
                                types::F64,
                                types::F64,
                                self.ptr_ty,
                            ],
                            None,
                        )?;
                        self.call_clif(id, &[op_v, ar, ai, br, bi, dst]);
                    }
                    BinOp::Eq | BinOp::NotEq => {
                        let er = self.builder.ins().fcmp(FloatCC::Equal, ar, br);
                        let ei = self.builder.ins().fcmp(FloatCC::Equal, ai, bi);
                        let mut eq = self.builder.ins().band(er, ei);
                        if op == BinOp::NotEq {
                            eq = self.builder.ins().bxor_imm_u(eq, 1);
                        }
                        self.builder.ins().store(MemFlagsData::new(), eq, dst, 0);
                    }
                    _ => return Err(CodegenError(format!("unsupported Complex op {op:?}"))),
                }
            }
            TyKind::C32 => {
                let pa = a_addr.expect("c32 a");
                let pb = b_addr.expect("c32 b");
                let ar = self
                    .builder
                    .ins()
                    .load(types::F32, MemFlagsData::new(), pa, 0);
                let ai = self
                    .builder
                    .ins()
                    .load(types::F32, MemFlagsData::new(), pa, 4);
                let br = self
                    .builder
                    .ins()
                    .load(types::F32, MemFlagsData::new(), pb, 0);
                let bi = self
                    .builder
                    .ins()
                    .load(types::F32, MemFlagsData::new(), pb, 4);
                match op {
                    BinOp::Add => {
                        let rr = self.builder.ins().fadd(ar, br);
                        let ri = self.builder.ins().fadd(ai, bi);
                        self.builder.ins().store(MemFlagsData::new(), rr, dst, 0);
                        self.builder.ins().store(MemFlagsData::new(), ri, dst, 4);
                    }
                    BinOp::Sub => {
                        let rr = self.builder.ins().fsub(ar, br);
                        let ri = self.builder.ins().fsub(ai, bi);
                        self.builder.ins().store(MemFlagsData::new(), rr, dst, 0);
                        self.builder.ins().store(MemFlagsData::new(), ri, dst, 4);
                    }
                    BinOp::Mul => {
                        let t1 = self.builder.ins().fmul(ar, br);
                        let t2 = self.builder.ins().fmul(ai, bi);
                        let rr = self.builder.ins().fsub(t1, t2);
                        let t3 = self.builder.ins().fmul(ar, bi);
                        let t4 = self.builder.ins().fmul(ai, br);
                        let ri = self.builder.ins().fadd(t3, t4);
                        self.builder.ins().store(MemFlagsData::new(), rr, dst, 0);
                        self.builder.ins().store(MemFlagsData::new(), ri, dst, 4);
                    }
                    BinOp::Div => {
                        let op_v = self.builder.ins().iconst(types::I8, 0);
                        let id = self.rt_imports.get(
                            self.module,
                            "noev_rt_c32_op",
                            &[
                                types::I8,
                                types::F32,
                                types::F32,
                                types::F32,
                                types::F32,
                                self.ptr_ty,
                            ],
                            None,
                        )?;
                        self.call_clif(id, &[op_v, ar, ai, br, bi, dst]);
                    }
                    BinOp::Eq | BinOp::NotEq => {
                        let er = self.builder.ins().fcmp(FloatCC::Equal, ar, br);
                        let ei = self.builder.ins().fcmp(FloatCC::Equal, ai, bi);
                        let mut eq = self.builder.ins().band(er, ei);
                        if op == BinOp::NotEq {
                            eq = self.builder.ins().bxor_imm_u(eq, 1);
                        }
                        self.builder.ins().store(MemFlagsData::new(), eq, dst, 0);
                    }
                    _ => return Err(CodegenError(format!("unsupported c32 op {op:?}"))),
                }
            }
            TyKind::Bool | TyKind::Bit => {
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                let res = match op {
                    BinOp::And | BinOp::BitAnd => self.builder.ins().band(va, vb),
                    BinOp::Or | BinOp::BitOr => self.builder.ins().bor(va, vb),
                    BinOp::BitXor => self.builder.ins().bxor(va, vb),
                    BinOp::Eq => self.builder.ins().icmp(IntCC::Equal, va, vb),
                    BinOp::NotEq => self.builder.ins().icmp(IntCC::NotEqual, va, vb),
                    _ => return Err(CodegenError(format!("unsupported Bool/Bit op {op:?}"))),
                };
                self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
            }
            _ if a_ty.width().is_some() => {
                let w = a_ty.width().expect("sized int");
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                match op {
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cc = match (op, w.signed) {
                            (BinOp::Eq, _) => IntCC::Equal,
                            (BinOp::NotEq, _) => IntCC::NotEqual,
                            (BinOp::Lt, true) => IntCC::SignedLessThan,
                            (BinOp::Le, true) => IntCC::SignedLessThanOrEqual,
                            (BinOp::Gt, true) => IntCC::SignedGreaterThan,
                            (BinOp::Ge, true) => IntCC::SignedGreaterThanOrEqual,
                            (BinOp::Lt, false) => IntCC::UnsignedLessThan,
                            (BinOp::Le, false) => IntCC::UnsignedLessThanOrEqual,
                            (BinOp::Gt, false) => IntCC::UnsignedGreaterThan,
                            (BinOp::Ge, false) => IntCC::UnsignedGreaterThanOrEqual,
                            _ => unreachable!(),
                        };
                        let res = self.builder.ins().icmp(cc, va, vb);
                        self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                    }
                    BinOp::BitAnd => {
                        let res = self.builder.ins().band(va, vb);
                        self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                    }
                    BinOp::BitOr => {
                        let res = self.builder.ins().bor(va, vb);
                        self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                    }
                    BinOp::BitXor => {
                        let res = self.builder.ins().bxor(va, vb);
                        self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                    }
                    _ => {
                        let code = match op {
                            BinOp::Add => 0i64,
                            BinOp::Sub => 1,
                            BinOp::Mul => 2,
                            BinOp::Div => 3,
                            BinOp::Rem => 4,
                            BinOp::Shl => 8,
                            BinOp::Shr => 9,
                            _ => return Err(CodegenError(format!("unsupported sized op {op:?}"))),
                        };
                        let wa = self.widen_sized_to_u64(va, w.bits, w.signed);
                        let b_is_int = b_ty == Ty::INT;
                        let wb = if b_is_int {
                            vb
                        } else {
                            self.widen_sized_to_u64(vb, w.bits, w.signed)
                        };
                        let out_slot = self.alloc_temp_slot(8, 8);
                        let op_v = self.builder.ins().iconst(types::I8, code);
                        let bits_v = self.builder.ins().iconst(types::I32, i64::from(w.bits));
                        let sgn_v = self.builder.ins().iconst(types::I8, i64::from(w.signed));
                        let bi_v = self.builder.ins().iconst(types::I8, i64::from(b_is_int));
                        let id = self.rt_imports.get(
                            self.module,
                            "noev_rt_sized_binary",
                            &[
                                self.ptr_ty,
                                types::I8,
                                types::I64,
                                types::I64,
                                types::I32,
                                types::I8,
                                types::I8,
                                self.ptr_ty,
                            ],
                            Some(types::I8),
                        )?;
                        let call = self.call_clif(
                            id,
                            &[self.rt_val, op_v, wa, wb, bits_v, sgn_v, bi_v, out_slot],
                        );
                        let ok = self.builder.inst_results(call)[0];
                        self.check_ok(ok);
                        let raw =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlagsData::new(), out_slot, 0);
                        let narrowed = self.narrow_u64_to_sized(raw, w.bits);
                        self.builder
                            .ins()
                            .store(MemFlagsData::new(), narrowed, dst, 0);
                    }
                }
            }
            // Deep equality on String, Tuple, Array, Struct, Enum, Unit.
            _ if matches!(op, BinOp::Eq | BinOp::NotEq) => {
                if self.layouts.layout(a_ty).size == 0 {
                    let res = self
                        .builder
                        .ins()
                        .iconst(types::I8, i64::from(op == BinOp::Eq));
                    self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                } else {
                    let pa = a_addr.expect("lhs");
                    let pb = b_addr.expect("rhs");
                    let desc_id =
                        self.rodata
                            .type_desc(self.module, self.program, self.layouts, a_ty)?;
                    let desc_ptr = self.data_addr(desc_id);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_val_eq",
                        &[self.ptr_ty, self.ptr_ty, self.ptr_ty, self.ptr_ty],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, pa, pb, desc_ptr]);
                    let mut eq = self.builder.inst_results(call)[0];
                    if op == BinOp::NotEq {
                        eq = self.builder.ins().bxor_imm_u(eq, 1);
                    }
                    self.builder.ins().store(MemFlagsData::new(), eq, dst, 0);
                }
            }
            _ => {
                return Err(CodegenError(format!(
                    "unsupported binary {op:?} on {:?} and {:?}",
                    a_ty, b_ty
                )));
            }
        }
        Ok(())
    }

    fn convert_to_f64(&mut self, op: &Operand, src_ty: Ty) -> Result<ClifValue, CodegenError> {
        match self.program.types.kind(src_ty) {
            TyKind::Float => self.eval_scalar(op),
            TyKind::F32 => {
                let v = self.eval_scalar(op)?;
                Ok(self.builder.ins().fpromote(types::F64, v))
            }
            TyKind::Int => {
                let v = self.eval_scalar(op)?;
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_int_to_f64",
                    &[self.ptr_ty, types::I64],
                    Some(types::F64),
                )?;
                let call = self.call_clif(id, &[self.rt_val, v]);
                Ok(self.builder.inst_results(call)[0])
            }
            _ if src_ty.width().is_some() => {
                let w = src_ty.width().expect("sized");
                let v = self.eval_scalar(op)?;
                let wide = self.widen_sized_to_u64(v, w.bits, w.signed);
                Ok(if w.signed {
                    self.builder.ins().fcvt_from_sint(types::F64, wide)
                } else {
                    self.builder.ins().fcvt_from_uint(types::F64, wide)
                })
            }
            _ => Err(CodegenError(format!(
                "cannot convert {:?} to Float",
                src_ty
            ))),
        }
    }

    fn lower_convert(
        &mut self,
        dst_addr: Option<ClifValue>,
        target: Ty,
        op: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let (src_addr, src_ty) = self.operand_addr(op)?;
        if src_ty == target {
            return self.store_operand(Some(dst), op);
        }
        match target {
            Ty::FLOAT => {
                let f = self.convert_to_f64(op, src_ty)?;
                self.builder.ins().store(MemFlagsData::new(), f, dst, 0);
            }
            Ty::F32 => {
                let f = if let Some(w) = src_ty.width() {
                    let v = self.eval_scalar(op)?;
                    let wide = self.widen_sized_to_u64(v, w.bits, w.signed);
                    if w.signed {
                        self.builder.ins().fcvt_from_sint(types::F32, wide)
                    } else {
                        self.builder.ins().fcvt_from_uint(types::F32, wide)
                    }
                } else {
                    let f64v = self.convert_to_f64(op, src_ty)?;
                    self.builder.ins().fdemote(types::F32, f64v)
                };
                self.builder.ins().store(MemFlagsData::new(), f, dst, 0);
            }
            Ty::COMPLEX => {
                if src_ty == Ty::C32 {
                    let s = src_addr.expect("c32");
                    let r = self
                        .builder
                        .ins()
                        .load(types::F32, MemFlagsData::new(), s, 0);
                    let i = self
                        .builder
                        .ins()
                        .load(types::F32, MemFlagsData::new(), s, 4);
                    let r64 = self.builder.ins().fpromote(types::F64, r);
                    let i64 = self.builder.ins().fpromote(types::F64, i);
                    self.builder.ins().store(MemFlagsData::new(), r64, dst, 0);
                    self.builder.ins().store(MemFlagsData::new(), i64, dst, 8);
                } else {
                    let re = self.convert_to_f64(op, src_ty)?;
                    let im = self.builder.ins().f64const(0.0);
                    self.builder.ins().store(MemFlagsData::new(), re, dst, 0);
                    self.builder.ins().store(MemFlagsData::new(), im, dst, 8);
                }
            }
            Ty::C32 => {
                if src_ty == Ty::COMPLEX {
                    let s = src_addr.expect("Complex");
                    let r = self
                        .builder
                        .ins()
                        .load(types::F64, MemFlagsData::new(), s, 0);
                    let i = self
                        .builder
                        .ins()
                        .load(types::F64, MemFlagsData::new(), s, 8);
                    let r32 = self.builder.ins().fdemote(types::F32, r);
                    let i32 = self.builder.ins().fdemote(types::F32, i);
                    self.builder.ins().store(MemFlagsData::new(), r32, dst, 0);
                    self.builder.ins().store(MemFlagsData::new(), i32, dst, 4);
                } else {
                    let re = if let Some(w) = src_ty.width() {
                        let v = self.eval_scalar(op)?;
                        let wide = self.widen_sized_to_u64(v, w.bits, w.signed);
                        if w.signed {
                            self.builder.ins().fcvt_from_sint(types::F32, wide)
                        } else {
                            self.builder.ins().fcvt_from_uint(types::F32, wide)
                        }
                    } else {
                        let f64v = self.convert_to_f64(op, src_ty)?;
                        self.builder.ins().fdemote(types::F32, f64v)
                    };
                    let im = self.builder.ins().f32const(0.0);
                    self.builder.ins().store(MemFlagsData::new(), re, dst, 0);
                    self.builder.ins().store(MemFlagsData::new(), im, dst, 4);
                }
            }
            Ty::INT => {
                if matches!(src_ty, Ty::BIT | Ty::BOOL) {
                    let b = self.eval_scalar(op)?;
                    let e = self.builder.ins().uextend(types::I64, b);
                    let tagged = self.builder.ins().ishl_imm_u(e, 1);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), tagged, dst, 0);
                } else if let Some(w) = src_ty.width() {
                    let v = self.eval_scalar(op)?;
                    let wide = self.widen_sized_to_u64(v, w.bits, w.signed);
                    let name = if w.signed {
                        "noev_rt_int_from_i64"
                    } else {
                        "noev_rt_int_from_u64"
                    };
                    let id = self.rt_imports.get(
                        self.module,
                        name,
                        &[self.ptr_ty, types::I64],
                        Some(types::I64),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, wide]);
                    let tagged = self.builder.inst_results(call)[0];
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), tagged, dst, 0);
                } else {
                    let f = self.convert_to_f64(op, src_ty)?;
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_int_from_f64",
                        &[self.ptr_ty, types::F64, self.ptr_ty],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, f, dst]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                }
            }
            Ty::BIT | Ty::BOOL => {
                let v = self.eval_scalar(op)?;
                self.builder.ins().store(MemFlagsData::new(), v, dst, 0);
            }
            _ if target.width().is_some() => {
                let dst_w = target.width().expect("sized target");
                if matches!(src_ty, Ty::BIT | Ty::BOOL) {
                    let b = self.eval_scalar(op)?;
                    let wide = self.builder.ins().uextend(types::I64, b);
                    let narrowed = self.narrow_u64_to_sized(wide, dst_w.bits);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), narrowed, dst, 0);
                } else if matches!(self.program.types.kind(src_ty), TyKind::Array(Ty::BIT, _)) {
                    let src = src_addr.expect("[Bit; N]");
                    let bits_v = self.builder.ins().iconst(types::I32, i64::from(dst_w.bits));
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_sized_from_bits",
                        &[self.ptr_ty, types::I32],
                        Some(types::I64),
                    )?;
                    let call = self.call_clif(id, &[src, bits_v]);
                    let raw = self.builder.inst_results(call)[0];
                    let narrowed = self.narrow_u64_to_sized(raw, dst_w.bits);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), narrowed, dst, 0);
                } else {
                    let (src_kind, raw, src_bits, src_signed) = if src_ty == Ty::INT {
                        (0i64, self.eval_scalar(op)?, 64i64, 1i64)
                    } else if let Some(sw) = src_ty.width() {
                        let v = self.eval_scalar(op)?;
                        (
                            1,
                            self.widen_sized_to_u64(v, sw.bits, sw.signed),
                            i64::from(sw.bits),
                            i64::from(sw.signed),
                        )
                    } else if src_ty == Ty::FLOAT {
                        let f = self.eval_scalar(op)?;
                        (
                            2,
                            self.builder
                                .ins()
                                .bitcast(types::I64, MemFlagsData::new(), f),
                            64,
                            1,
                        )
                    } else {
                        let f = self.eval_scalar(op)?;
                        let u32v = self
                            .builder
                            .ins()
                            .bitcast(types::I32, MemFlagsData::new(), f);
                        (3, self.builder.ins().uextend(types::I64, u32v), 32, 1)
                    };
                    let out_slot = self.alloc_temp_slot(8, 8);
                    let sk = self.builder.ins().iconst(types::I8, src_kind);
                    let sb = self.builder.ins().iconst(types::I32, src_bits);
                    let ss = self.builder.ins().iconst(types::I8, src_signed);
                    let db = self.builder.ins().iconst(types::I32, i64::from(dst_w.bits));
                    let ds = self
                        .builder
                        .ins()
                        .iconst(types::I8, i64::from(dst_w.signed));
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_sized_convert",
                        &[
                            self.ptr_ty,
                            types::I8,
                            types::I64,
                            types::I32,
                            types::I8,
                            types::I32,
                            types::I8,
                            self.ptr_ty,
                        ],
                        Some(types::I8),
                    )?;
                    let call =
                        self.call_clif(id, &[self.rt_val, sk, raw, sb, ss, db, ds, out_slot]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                    let res_u64 =
                        self.builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), out_slot, 0);
                    let narrowed = self.narrow_u64_to_sized(res_u64, dst_w.bits);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), narrowed, dst, 0);
                }
            }
            _ => {
                self.store_operand(Some(dst), op)?;
            }
        }
        Ok(())
    }

    fn lower_try_convert(
        &mut self,
        dst_addr: Option<ClifValue>,
        result_ty: Ty,
        target: Ty,
        op: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let dst_w = target.width().expect("try_from target is sized integer");
        let (_, src_ty) = self.operand_addr(op)?;
        let (src_kind, raw, src_bits, src_signed) = if src_ty == Ty::INT {
            (0i64, self.eval_scalar(op)?, 64i64, 1i64)
        } else {
            let sw = src_ty.width().expect("try_from source is int or sized");
            let v = self.eval_scalar(op)?;
            (
                1,
                self.widen_sized_to_u64(v, sw.bits, sw.signed),
                i64::from(sw.bits),
                i64::from(sw.signed),
            )
        };
        let desc_id = self
            .rodata
            .type_desc(self.module, self.program, self.layouts, result_ty)?;
        let desc_ptr = self.data_addr(desc_id);
        let sk = self.builder.ins().iconst(types::I8, src_kind);
        let sb = self.builder.ins().iconst(types::I32, src_bits);
        let ss = self.builder.ins().iconst(types::I8, src_signed);
        let db = self.builder.ins().iconst(types::I32, i64::from(dst_w.bits));
        let ds = self
            .builder
            .ins()
            .iconst(types::I8, i64::from(dst_w.signed));
        let id = self.rt_imports.get(
            self.module,
            "noev_rt_sized_try_from",
            &[
                self.ptr_ty,
                types::I8,
                types::I64,
                types::I32,
                types::I8,
                types::I32,
                types::I8,
                self.ptr_ty,
                self.ptr_ty,
            ],
            Some(types::I8),
        )?;
        let call = self.call_clif(id, &[self.rt_val, sk, raw, sb, ss, db, ds, dst, desc_ptr]);
        let ok = self.builder.inst_results(call)[0];
        self.check_ok(ok);
        Ok(())
    }

    fn lower_math(
        &mut self,
        dst_addr: Option<ClifValue>,
        dst_ty: Ty,
        math: Math,
        ops: &[Operand],
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let (a0_addr, ty0) = self.operand_addr(&ops[0])?;

        // FFT / IFFT on [Complex; N] or [c32; N].
        if matches!(math, Math::Fft | Math::Ifft) {
            let (elem, len) = match self.program.types.kind(ty0) {
                &TyKind::Array(e, n) => (e, n),
                _ => return Err(CodegenError("fft requires an array".into())),
            };
            let src = a0_addr.expect("fft input");
            let len_v = self.builder.ins().iconst(self.ptr_ty, len as i64);
            let inv_v = self
                .builder
                .ins()
                .iconst(types::I8, i64::from(math == Math::Ifft));
            let c32_v = self
                .builder
                .ins()
                .iconst(types::I8, i64::from(elem == Ty::C32));
            let id = self.rt_imports.get(
                self.module,
                "noev_rt_fft",
                &[self.ptr_ty, self.ptr_ty, self.ptr_ty, types::I8, types::I8],
                None,
            )?;
            self.call_clif(id, &[src, dst, len_v, inv_v, c32_v]);
            return Ok(());
        }

        // Polar: Complex::polar(r, theta) or c32::polar(r, theta).
        if math == Math::Polar {
            if dst_ty == Ty::C32 {
                let r = self.eval_scalar(&ops[0])?;
                let th = self.eval_scalar(&ops[1])?;
                let zero = self.builder.ins().f32const(0.0);
                let op_v = self.builder.ins().iconst(types::I8, 3);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_c32_op",
                    &[
                        types::I8,
                        types::F32,
                        types::F32,
                        types::F32,
                        types::F32,
                        self.ptr_ty,
                    ],
                    None,
                )?;
                self.call_clif(id, &[op_v, r, th, zero, zero, dst]);
            } else {
                let r = self.eval_scalar(&ops[0])?;
                let th = self.eval_scalar(&ops[1])?;
                let zero = self.builder.ins().f64const(0.0);
                let op_v = self.builder.ins().iconst(types::I8, 3);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_complex_op",
                    &[
                        types::I8,
                        types::F64,
                        types::F64,
                        types::F64,
                        types::F64,
                        self.ptr_ty,
                    ],
                    None,
                )?;
                self.call_clif(id, &[op_v, r, th, zero, zero, dst]);
            }
            return Ok(());
        }

        // Complex methods.
        if ty0 == Ty::COMPLEX {
            let src = a0_addr.expect("Complex");
            let re = self
                .builder
                .ins()
                .load(types::F64, MemFlagsData::new(), src, 0);
            let im = self
                .builder
                .ins()
                .load(types::F64, MemFlagsData::new(), src, 8);
            match math {
                Math::Re => {
                    self.builder.ins().store(MemFlagsData::new(), re, dst, 0);
                }
                Math::Im => {
                    self.builder.ins().store(MemFlagsData::new(), im, dst, 0);
                }
                Math::Conj => {
                    let nim = self.builder.ins().fneg(im);
                    self.builder.ins().store(MemFlagsData::new(), re, dst, 0);
                    self.builder.ins().store(MemFlagsData::new(), nim, dst, 8);
                }
                Math::Abs | Math::Arg => {
                    let code = if math == Math::Abs { 0i64 } else { 1 };
                    let op_v = self.builder.ins().iconst(types::I8, code);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_complex_scalar",
                        &[types::I8, types::F64, types::F64],
                        Some(types::F64),
                    )?;
                    let call = self.call_clif(id, &[op_v, re, im]);
                    let res = self.builder.inst_results(call)[0];
                    self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                }
                Math::Exp | Math::Sqrt => {
                    let code = if math == Math::Exp { 1i64 } else { 2 };
                    let op_v = self.builder.ins().iconst(types::I8, code);
                    let zero = self.builder.ins().f64const(0.0);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_complex_op",
                        &[
                            types::I8,
                            types::F64,
                            types::F64,
                            types::F64,
                            types::F64,
                            self.ptr_ty,
                        ],
                        None,
                    )?;
                    self.call_clif(id, &[op_v, re, im, zero, zero, dst]);
                }
                _ => return Err(CodegenError(format!("unsupported Complex math {math:?}"))),
            }
            return Ok(());
        }

        // c32 methods.
        if ty0 == Ty::C32 {
            let src = a0_addr.expect("c32");
            let re = self
                .builder
                .ins()
                .load(types::F32, MemFlagsData::new(), src, 0);
            let im = self
                .builder
                .ins()
                .load(types::F32, MemFlagsData::new(), src, 4);
            match math {
                Math::Re => {
                    self.builder.ins().store(MemFlagsData::new(), re, dst, 0);
                }
                Math::Im => {
                    self.builder.ins().store(MemFlagsData::new(), im, dst, 0);
                }
                Math::Conj => {
                    let nim = self.builder.ins().fneg(im);
                    self.builder.ins().store(MemFlagsData::new(), re, dst, 0);
                    self.builder.ins().store(MemFlagsData::new(), nim, dst, 4);
                }
                Math::Abs | Math::Arg => {
                    let re64 = self.builder.ins().fpromote(types::F64, re);
                    let im64 = self.builder.ins().fpromote(types::F64, im);
                    let code = if math == Math::Abs { 0i64 } else { 1 };
                    let op_v = self.builder.ins().iconst(types::I8, code);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_complex_scalar",
                        &[types::I8, types::F64, types::F64],
                        Some(types::F64),
                    )?;
                    let call = self.call_clif(id, &[op_v, re64, im64]);
                    let res64 = self.builder.inst_results(call)[0];
                    let res32 = self.builder.ins().fdemote(types::F32, res64);
                    self.builder.ins().store(MemFlagsData::new(), res32, dst, 0);
                }
                Math::Exp | Math::Sqrt => {
                    let code = if math == Math::Exp { 1i64 } else { 2 };
                    let op_v = self.builder.ins().iconst(types::I8, code);
                    let zero = self.builder.ins().f32const(0.0);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_c32_op",
                        &[
                            types::I8,
                            types::F32,
                            types::F32,
                            types::F32,
                            types::F32,
                            self.ptr_ty,
                        ],
                        None,
                    )?;
                    self.call_clif(id, &[op_v, re, im, zero, zero, dst]);
                }
                _ => return Err(CodegenError(format!("unsupported c32 math {math:?}"))),
            }
            return Ok(());
        }

        // Sized integer methods: Abs, Min, Max, Bits.
        if let Some(w) = ty0.width() {
            let va = self.eval_scalar(&ops[0])?;
            match math {
                Math::Bits => {
                    let wide = self.widen_sized_to_u64(va, w.bits, false);
                    let bits_v = self.builder.ins().iconst(types::I32, i64::from(w.bits));
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_sized_bits",
                        &[types::I64, types::I32, self.ptr_ty],
                        None,
                    )?;
                    self.call_clif(id, &[wide, bits_v, dst]);
                }
                Math::Abs => {
                    if !w.signed {
                        self.builder.ins().store(MemFlagsData::new(), va, dst, 0);
                    } else {
                        let is_neg = self.builder.ins().icmp_imm_s(IntCC::SignedLessThan, va, 0);
                        let neg_bb = self.builder.create_block();
                        let pos_bb = self.builder.create_block();
                        let done_bb = self.builder.create_block();
                        self.builder.ins().brif(is_neg, neg_bb, &[], pos_bb, &[]);

                        self.builder.switch_to_block(neg_bb);
                        let wide = self.widen_sized_to_u64(va, w.bits, true);
                        let out_slot = self.alloc_temp_slot(8, 8);
                        let bits_v = self.builder.ins().iconst(types::I32, i64::from(w.bits));
                        let sgn_v = self.builder.ins().iconst(types::I8, 1);
                        let id = self.rt_imports.get(
                            self.module,
                            "noev_rt_sized_neg",
                            &[self.ptr_ty, types::I64, types::I32, types::I8, self.ptr_ty],
                            Some(types::I8),
                        )?;
                        let call =
                            self.call_clif(id, &[self.rt_val, wide, bits_v, sgn_v, out_slot]);
                        let ok = self.builder.inst_results(call)[0];
                        self.check_ok(ok);
                        let raw =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlagsData::new(), out_slot, 0);
                        let narrowed = self.narrow_u64_to_sized(raw, w.bits);
                        self.builder
                            .ins()
                            .store(MemFlagsData::new(), narrowed, dst, 0);
                        self.builder.ins().jump(done_bb, &[]);

                        self.builder.switch_to_block(pos_bb);
                        self.builder.ins().store(MemFlagsData::new(), va, dst, 0);
                        self.builder.ins().jump(done_bb, &[]);

                        self.builder.switch_to_block(done_bb);
                    }
                }
                Math::Min | Math::Max => {
                    let vb = self.eval_scalar(&ops[1])?;
                    let cc = match (math, w.signed) {
                        (Math::Min, true) => IntCC::SignedLessThan,
                        (Math::Min, false) => IntCC::UnsignedLessThan,
                        (Math::Max, true) => IntCC::SignedGreaterThan,
                        (Math::Max, false) => IntCC::UnsignedGreaterThan,
                        _ => unreachable!(),
                    };
                    let take_a = self.builder.ins().icmp(cc, va, vb);
                    let res = self.builder.ins().select(take_a, va, vb);
                    self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                }
                _ => return Err(CodegenError(format!("unsupported sized math {math:?}"))),
            }
            return Ok(());
        }

        // Arbitrary-precision Int methods: Abs, Min, Max.
        if ty0 == Ty::INT {
            let va = self.eval_scalar(&ops[0])?;
            match math {
                Math::Abs => {
                    let op_v = self.builder.ins().iconst(types::I8, 2);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_int_unary",
                        &[self.ptr_ty, types::I8, types::I64],
                        Some(types::I64),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, op_v, va]);
                    let res = self.builder.inst_results(call)[0];
                    self.builder.ins().store(MemFlagsData::new(), res, dst, 0);
                }
                Math::Min | Math::Max => {
                    let vb = self.eval_scalar(&ops[1])?;
                    let code = if math == Math::Min { 11i64 } else { 12 };
                    let op_v = self.builder.ins().iconst(types::I8, code);
                    let id = self.rt_imports.get(
                        self.module,
                        "noev_rt_int_binary",
                        &[self.ptr_ty, types::I8, types::I64, types::I64, self.ptr_ty],
                        Some(types::I8),
                    )?;
                    let call = self.call_clif(id, &[self.rt_val, op_v, va, vb, dst]);
                    let ok = self.builder.inst_results(call)[0];
                    self.check_ok(ok);
                }
                _ => return Err(CodegenError(format!("unsupported Int math {math:?}"))),
            }
            return Ok(());
        }

        // Float / f32 methods.
        let op_code = match math {
            Math::Sin => 0i64,
            Math::Cos => 1,
            Math::Tan => 2,
            Math::Asin => 3,
            Math::Acos => 4,
            Math::Atan => 5,
            Math::Atan2 => 6,
            Math::Sqrt => 7,
            Math::Exp => 8,
            Math::Ln => 9,
            Math::Abs => 10,
            Math::Min => 11,
            Math::Max => 12,
            Math::Trunc => 13,
            Math::Round => 14,
            Math::Floor => 15,
            Math::Ceil => 16,
            _ => return Err(CodegenError(format!("unsupported float math {math:?}"))),
        };
        let is_f32 = ty0 == Ty::F32;
        let va_raw = self.eval_scalar(&ops[0])?;
        let va64 = if is_f32 {
            self.builder.ins().fpromote(types::F64, va_raw)
        } else {
            va_raw
        };
        let vb64 = if ops.len() > 1 {
            let v = self.eval_scalar(&ops[1])?;
            if is_f32 {
                self.builder.ins().fpromote(types::F64, v)
            } else {
                v
            }
        } else {
            self.builder.ins().f64const(0.0)
        };
        let op_v = self.builder.ins().iconst(types::I8, op_code);
        let id = self.rt_imports.get(
            self.module,
            "noev_rt_f64_math",
            &[types::I8, types::F64, types::F64],
            Some(types::F64),
        )?;
        let call = self.call_clif(id, &[op_v, va64, vb64]);
        let res64 = self.builder.inst_results(call)[0];

        if dst_ty == Ty::INT {
            let conv_id = self.rt_imports.get(
                self.module,
                "noev_rt_int_from_f64",
                &[self.ptr_ty, types::F64, self.ptr_ty],
                Some(types::I8),
            )?;
            let ccall = self.call_clif(conv_id, &[self.rt_val, res64, dst]);
            let ok = self.builder.inst_results(ccall)[0];
            self.check_ok(ok);
        } else if dst_ty == Ty::F32 {
            let res32 = self.builder.ins().fdemote(types::F32, res64);
            self.builder.ins().store(MemFlagsData::new(), res32, dst, 0);
        } else {
            self.builder.ins().store(MemFlagsData::new(), res64, dst, 0);
        }
        Ok(())
    }

    fn lower_format(
        &mut self,
        dst_addr: Option<ClifValue>,
        parts: &[FormatPart],
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        // Fast path: single literal string.
        if let [FormatPart::Text(s)] = parts {
            let bid = self.rodata.intern_bytes(self.module, s.as_bytes())?;
            let ptr = self.data_addr(bid);
            let len = self.builder.ins().iconst(self.ptr_ty, s.len() as i64);
            let id = self.rt_imports.get(
                self.module,
                "noev_rt_str_from_bytes",
                &[self.ptr_ty, self.ptr_ty, self.ptr_ty],
                Some(types::I64),
            )?;
            let call = self.call_clif(id, &[self.rt_val, ptr, len]);
            let h = self.builder.inst_results(call)[0];
            self.builder.ins().store(MemFlagsData::new(), h, dst, 0);
            return Ok(());
        }

        let n = parts.len();
        let stride = size_of::<NoevFormatPart>() as u32;
        let buf = if n == 0 {
            self.builder.ins().iconst(self.ptr_ty, 0)
        } else {
            let b = self.alloc_temp_slot(stride * n as u32, align_of::<NoevFormatPart>() as u32);
            for (i, part) in parts.iter().enumerate() {
                let base = i as i32 * stride as i32;
                let zero8 = self.builder.ins().iconst(types::I8, 0);
                let zero32 = self.builder.ins().iconst(types::I32, 0);
                let zero_ptr = self.builder.ins().iconst(self.ptr_ty, 0);
                match part {
                    FormatPart::Text(s) => {
                        let bid = self.rodata.intern_bytes(self.module, s.as_bytes())?;
                        let ptr = self.data_addr(bid);
                        let len = self.builder.ins().iconst(self.ptr_ty, s.len() as i64);
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            zero8,
                            b,
                            base + offset_of!(NoevFormatPart, is_value) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            zero8,
                            b,
                            base + offset_of!(NoevFormatPart, has_spec) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            ptr,
                            b,
                            base + offset_of!(NoevFormatPart, ptr) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            len,
                            b,
                            base + offset_of!(NoevFormatPart, len) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            zero_ptr,
                            b,
                            base + offset_of!(NoevFormatPart, ty) as i32,
                        );
                    }
                    FormatPart::Value(op, ty, spec) => {
                        let (vaddr, _) = self.operand_addr(op)?;
                        let vptr = vaddr.unwrap_or(zero_ptr);
                        let desc_id =
                            self.rodata
                                .type_desc(self.module, self.program, self.layouts, *ty)?;
                        let dptr = self.data_addr(desc_id);
                        let one8 = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            one8,
                            b,
                            base + offset_of!(NoevFormatPart, is_value) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            vptr,
                            b,
                            base + offset_of!(NoevFormatPart, ptr) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            zero_ptr,
                            b,
                            base + offset_of!(NoevFormatPart, len) as i32,
                        );
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            dptr,
                            b,
                            base + offset_of!(NoevFormatPart, ty) as i32,
                        );
                        if let Some(sp) = spec {
                            let align_code = match sp.align {
                                Some(Align::Left) => 1i64,
                                Some(Align::Right) => 2,
                                Some(Align::Center) => 3,
                                None => 0,
                            };
                            let kind_code = match sp.kind {
                                Some(FormatKind::Binary) => 1i64,
                                Some(FormatKind::Octal) => 2,
                                Some(FormatKind::Hex) => 3,
                                Some(FormatKind::HexUpper) => 4,
                                Some(FormatKind::Exponent) => 5,
                                None => 0,
                            };
                            let mut flags = 0i64;
                            if sp.plus {
                                flags |= 1;
                            }
                            if sp.zero {
                                flags |= 2;
                            }
                            if sp.width.is_some() {
                                flags |= 4;
                            }
                            if sp.precision.is_some() {
                                flags |= 8;
                            }
                            let a_v = self.builder.ins().iconst(types::I8, align_code);
                            let k_v = self.builder.ins().iconst(types::I8, kind_code);
                            let f_v = self.builder.ins().iconst(types::I8, flags);
                            let fill_v = self
                                .builder
                                .ins()
                                .iconst(types::I32, i64::from(sp.fill as u32));
                            let w_v = self
                                .builder
                                .ins()
                                .iconst(types::I32, i64::from(sp.width.unwrap_or(0)));
                            let p_v = self
                                .builder
                                .ins()
                                .iconst(types::I32, i64::from(sp.precision.unwrap_or(0)));
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                one8,
                                b,
                                base + offset_of!(NoevFormatPart, has_spec) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                a_v,
                                b,
                                base + offset_of!(NoevFormatPart, align) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                k_v,
                                b,
                                base + offset_of!(NoevFormatPart, kind) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                f_v,
                                b,
                                base + offset_of!(NoevFormatPart, flags) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                fill_v,
                                b,
                                base + offset_of!(NoevFormatPart, fill) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                w_v,
                                b,
                                base + offset_of!(NoevFormatPart, width) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                p_v,
                                b,
                                base + offset_of!(NoevFormatPart, precision) as i32,
                            );
                        } else {
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                zero8,
                                b,
                                base + offset_of!(NoevFormatPart, has_spec) as i32,
                            );
                            self.builder.ins().store(
                                MemFlagsData::new(),
                                zero32,
                                b,
                                base + offset_of!(NoevFormatPart, fill) as i32,
                            );
                        }
                    }
                }
            }
            b
        };
        let n_val = self.builder.ins().iconst(self.ptr_ty, n as i64);
        let id = self.rt_imports.get(
            self.module,
            "noev_rt_format",
            &[self.ptr_ty, self.ptr_ty, self.ptr_ty],
            Some(types::I64),
        )?;
        let call = self.call_clif(id, &[self.rt_val, buf, n_val]);
        let h = self.builder.inst_results(call)[0];
        self.builder.ins().store(MemFlagsData::new(), h, dst, 0);
        Ok(())
    }

    fn lower_dist(
        &mut self,
        dst_addr: Option<ClifValue>,
        method: DistMethod,
        ops: &[Operand],
        elem: Ty,
    ) -> Result<(), CodegenError> {
        let dist = self.eval_scalar(&ops[0])?;
        match method {
            DistMethod::Shots => {
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_dist_shots",
                    &[self.ptr_ty, types::I64],
                    Some(types::I64),
                )?;
                let call = self.call_clif(id, &[self.rt_val, dist]);
                let shots = self.builder.inst_results(call)[0];
                if let Some(dst) = dst_addr {
                    self.builder.ins().store(MemFlagsData::new(), shots, dst, 0);
                }
            }
            DistMethod::Count => {
                let (vaddr, _) = self.operand_addr(&ops[1])?;
                let vptr = vaddr.unwrap_or_else(|| self.builder.ins().iconst(self.ptr_ty, 0));
                let desc_id =
                    self.rodata
                        .type_desc(self.module, self.program, self.layouts, elem)?;
                let dptr = self.data_addr(desc_id);
                let dst = dst_addr.expect("Int dest");
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_dist_count",
                    &[
                        self.ptr_ty,
                        types::I64,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                    ],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, dist, vptr, dptr, dst]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
            }
            DistMethod::Probability => {
                let (vaddr, _) = self.operand_addr(&ops[1])?;
                let vptr = vaddr.unwrap_or_else(|| self.builder.ins().iconst(self.ptr_ty, 0));
                let desc_id =
                    self.rodata
                        .type_desc(self.module, self.program, self.layouts, elem)?;
                let dptr = self.data_addr(desc_id);
                let dst = dst_addr.expect("Float dest");
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_dist_probability",
                    &[
                        self.ptr_ty,
                        types::I64,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                    ],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, dist, vptr, dptr, dst]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
            }
            DistMethod::MostLikely => {
                let desc_id =
                    self.rodata
                        .type_desc(self.module, self.program, self.layouts, elem)?;
                let dptr = self.data_addr(desc_id);
                let out_ptr = dst_addr.unwrap_or_else(|| self.alloc_temp_slot(1, 1));
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_dist_most_likely",
                    &[self.ptr_ty, types::I64, self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, dist, out_ptr, dptr]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Terminators
    // -------------------------------------------------------------------------

    fn lower_term(&mut self, term: &Terminator) -> Result<(), CodegenError> {
        match term {
            Terminator::Goto(next) => {
                self.builder.ins().jump(self.blocks[next.index()], &[]);
            }
            Terminator::Branch {
                cond,
                then,
                otherwise,
                ..
            } => {
                let c = self.eval_scalar(cond)?;
                self.builder.ins().brif(
                    c,
                    self.blocks[then.index()],
                    &[],
                    self.blocks[otherwise.index()],
                    &[],
                );
            }
            Terminator::Call {
                func,
                args,
                dest,
                adjoint,
                next,
            } => {
                let callee = &self.program.functions[func.index()];
                let mut call_args = vec![self.rt_val];
                for (i, arg) in args.iter().enumerate() {
                    let pty = callee.locals[i + 1].ty;
                    let l = self.layouts.layout(pty);
                    if l.size == 0 {
                        continue;
                    }
                    if l.scalar.is_some() {
                        call_args.push(self.eval_scalar(arg)?);
                    } else {
                        let (addr, _) = self.operand_addr(arg)?;
                        // Pass a fresh copy if the arg could alias dest? Callee copies params
                        // into its own stack slots on entry before writing ret_ptr on exit, so
                        // passing `addr` directly is always alias-safe!
                        call_args.push(addr.expect("aggregate arg"));
                    }
                }
                let ret_layout = self.layouts.layout(callee.locals[0].ty);
                let (dst_addr, _) = self.place_addr(dest)?;
                if ret_layout.size > 0 {
                    let ret_ptr = dst_addr
                        .unwrap_or_else(|| self.alloc_temp_slot(ret_layout.size, ret_layout.align));
                    call_args.push(ret_ptr);
                }

                if *adjoint {
                    let start_id = self.rt_imports.rt_void(self.module, "noev_rt_adj_start")?;
                    self.call_clif(start_id, &[self.rt_val]);
                }

                let call = self.call_clif(self.func_ids[func.index()], &call_args);
                let ok = self.builder.inst_results(call)[0];

                if *adjoint {
                    let ok_bb = self.builder.create_block();
                    let abort_bb = self.builder.create_block();
                    self.builder.ins().brif(ok, ok_bb, &[], abort_bb, &[]);

                    self.builder.switch_to_block(abort_bb);
                    let abort_id = self.rt_imports.rt_void(self.module, "noev_rt_adj_abort")?;
                    self.call_clif(abort_id, &[self.rt_val]);
                    self.builder.ins().jump(self.err_block, &[]);

                    self.builder.switch_to_block(ok_bb);
                    let end_id = self.rt_imports.rt_bool(self.module, "noev_rt_adj_end")?;
                    let ecall = self.call_clif(end_id, &[self.rt_val]);
                    let eok = self.builder.inst_results(ecall)[0];
                    self.check_ok(eok);
                } else {
                    self.check_ok(ok);
                }

                self.builder.ins().jump(self.blocks[next.index()], &[]);
            }
            Terminator::Undo {
                func,
                args,
                target,
                next,
            } => {
                let callee = &self.program.functions[func.index()];
                let param_tys: Vec<Ty> = (1..=callee.params).map(|i| callee.locals[i].ty).collect();
                let args_buf = self.pack_args_buf(args, &param_tys)?;
                let thunk_ptr = self.func_addr(self.thunk_ids[func]);
                let ret_desc_id = self.rodata.type_desc(
                    self.module,
                    self.program,
                    self.layouts,
                    callee.locals[0].ty,
                )?;
                let ret_desc_ptr = self.data_addr(ret_desc_id);
                let (targets_ptr, n_targets) =
                    self.collect_qubit_handles(std::slice::from_ref(target))?;
                let n_val = self.builder.ins().iconst(self.ptr_ty, n_targets as i64);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_undo_compiled",
                    &[
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                        self.ptr_ty,
                    ],
                    Some(types::I8),
                )?;
                let call = self.call_clif(
                    id,
                    &[
                        self.rt_val,
                        thunk_ptr,
                        args_buf,
                        ret_desc_ptr,
                        targets_ptr,
                        n_val,
                    ],
                );
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                self.builder.ins().jump(self.blocks[next.index()], &[]);
            }
            Terminator::Assert { cond, next, .. } => {
                let ok_cond = self.eval_scalar(cond)?;
                let msg = b"assertion failed";
                let mid = self.rodata.intern_bytes(self.module, msg)?;
                let mptr = self.data_addr(mid);
                let mlen = self.builder.ins().iconst(self.ptr_ty, msg.len() as i64);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_assert",
                    &[self.ptr_ty, types::I8, self.ptr_ty, self.ptr_ty],
                    Some(types::I8),
                )?;
                let call = self.call_clif(id, &[self.rt_val, ok_cond, mptr, mlen]);
                let ok = self.builder.inst_results(call)[0];
                self.check_ok(ok);
                self.builder.ins().jump(self.blocks[next.index()], &[]);
            }
            Terminator::Return => {
                self.emit_return_ok()?;
            }
            Terminator::Unreachable => {
                let msg = format!("reached unreachable code in `{}`", self.func.name);
                let mid = self.rodata.intern_bytes(self.module, msg.as_bytes())?;
                let mptr = self.data_addr(mid);
                let mlen = self.builder.ins().iconst(self.ptr_ty, msg.len() as i64);
                let id = self.rt_imports.get(
                    self.module,
                    "noev_rt_set_error",
                    &[self.ptr_ty, self.ptr_ty, self.ptr_ty],
                    None,
                )?;
                self.call_clif(id, &[self.rt_val, mptr, mlen]);
                self.builder.ins().jump(self.err_block, &[]);
            }
        }
        Ok(())
    }
}
