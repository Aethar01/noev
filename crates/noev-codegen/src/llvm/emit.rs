//! Lowering [`noev_ir::Program`] to modern opaque-pointer LLVM IR text (`.ll`).
//!
//! Supports both [`RuntimeAbi::Noev`] (linking against `libnoev_runtime.a`) and
//! [`RuntimeAbi::Qir`] (emitting calls to standard QIR `__quantum__qis__*` and
//! `__quantum__rt__*` functions so compiled programs link directly to QIR runtimes).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::mem::{align_of, offset_of, size_of};

use noev_ir::Program;
use noev_ir::ir::{
    Align, BinOp, Block, Const, ExternKind, FormatKind, FormatPart, FuncId, Function, Operand,
    Place, Prep, Proj, QasmPart, Rvalue, Stmt, Terminator, UnOp,
};
use noev_runtime::cabi_compiled::{NoevFormatPart, NoevPrintArg, TypeTag};
use noev_sema::prelude::{DistMethod, FloatConst, Gate, Math, Pauli};
use noev_sema::{Ty, TyKind};

use crate::analysis::{
    PlaceKey, analyze_enum_variants, collect_thunks, const_ty, has_feedback, operand_ty,
};
use crate::layout::{CAbiArg, LayoutCache, ScalarKind};
use crate::{CodegenError, CodegenOptions, RuntimeAbi};

const DEFAULT_PRECISION: f64 = 0.01;
const MIN_INLINE_INT: i64 = -(1i64 << 62);
const MAX_INLINE_INT: i64 = (1i64 << 62) - 1;

fn scalar_llvm(kind: ScalarKind) -> &'static str {
    match kind {
        ScalarKind::I8 => "i8",
        ScalarKind::I16 => "i16",
        ScalarKind::I32 => "i32",
        ScalarKind::I64 => "i64",
        ScalarKind::Ptr => "ptr",
        ScalarKind::F32 => "float",
        ScalarKind::F64 => "double",
    }
}

fn llvm_f64(x: f64) -> String {
    format!("0x{:016X}", x.to_bits())
}

fn llvm_f32(x: f32) -> String {
    format!("0x{:016X}", f64::from(x).to_bits())
}

fn llvm_bytes_literal(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() + 4);
    s.push_str("c\"");
    for &b in bytes {
        if (0x20..=0x7E).contains(&b) && b != b'"' && b != b'\\' {
            s.push(b as char);
        } else {
            let _ = write!(s, "\\{b:02X}");
        }
    }
    s.push('"');
    s
}

/// Emits a complete LLVM IR module (`.ll`) for `program`.
pub fn emit_module(program: &Program, opts: CodegenOptions) -> Result<String, CodegenError> {
    let ptr_bytes = size_of::<usize>() as u32;
    let mut layouts = LayoutCache::new(program, ptr_bytes);
    let mut globals = GlobalPool::new();
    let mut decls: BTreeMap<String, String> = BTreeMap::new();
    let mut bodies = String::new();

    // 1. Emit every IR function (plus C import declarations and C export wrappers).
    for (i, func) in program.functions.iter().enumerate() {
        match func.extern_kind {
            ExternKind::Host => {
                return Err(CodegenError(format!(
                    "`extern fn {}` is a host function and requires an embedding host runtime",
                    func.name
                )));
            }
            ExternKind::CImport => {
                let param_tys: Vec<Ty> =
                    func.locals[1..=func.params].iter().map(|l| l.ty).collect();
                let decl = llvm_c_decl(&mut layouts, &func.name, &param_tys, func.locals[0].ty);
                decls.entry(func.name.clone()).or_insert(decl);
                continue;
            }
            ExternKind::None | ExternKind::CExport => {}
        }
        let facts = analyze_enum_variants(func);
        let mut emitter = FnEmitter {
            program,
            func,
            func_index: i,
            layouts: &mut layouts,
            globals: &mut globals,
            decls: &mut decls,
            opts,
            facts: &facts,
            cur_block: Block(0),
            allocas: String::new(),
            body: String::new(),
            next_ssa: 0,
            next_lbl: 0,
            has_ret_ptr: false,
            ctrl_depth_var: String::new(),
        };
        emitter.emit_function(&mut bodies)?;
        if func.is_c_export() {
            emit_c_export_wrapper(&mut bodies, &mut layouts, &mut decls, func, i);
        }
    }

    // 2. Emit thunks for `sample`, `Expectation`, `Overlap`, `Undo`.
    for fid in collect_thunks(program) {
        emit_thunk(
            &mut bodies,
            &mut layouts,
            &program.functions[fid.index()],
            fid.index(),
        );
    }

    // 3. Emit C `main(i32 %argc, ptr %argv) -> i32`.
    if let Some(main_id) = program.main {
        emit_main(
            &mut bodies,
            &mut layouts,
            &mut decls,
            &program.functions[main_id.index()],
            main_id.index(),
            opts.runtime_abi,
        );
    }

    // Assemble final module.
    let mut out = String::new();
    out.push_str("; ModuleID = 'noev_program'\n");
    out.push_str("%NoevFieldDesc = type { ptr, i64, i32, ptr }\n");
    out.push_str("%NoevVariantDesc = type { ptr, i64, i8, ptr, i64 }\n");
    out.push_str(
        "%NoevTypeDesc = type { i8, i8, i32, i32, ptr, i64, ptr, i64, ptr, i64, ptr, i64 }\n\n",
    );
    out.push_str(&globals.defs);
    if !globals.defs.is_empty() {
        out.push('\n');
    }
    for decl in decls.values() {
        out.push_str(decl);
        out.push('\n');
    }
    if !decls.is_empty() {
        out.push('\n');
    }
    out.push_str(&bodies);
    Ok(out)
}

fn emit_fn_sig_params(
    layouts: &mut LayoutCache<'_>,
    func: &Function,
) -> Vec<(&'static str, String)> {
    let mut params = vec![("ptr", "%rt".to_string())];
    for (idx, local) in func.locals[1..=func.params].iter().enumerate() {
        let l = layouts.layout(local.ty);
        if l.size == 0 {
            continue;
        }
        let ty = l.scalar.map_or("ptr", scalar_llvm);
        params.push((ty, format!("%p{}", idx + 1)));
    }
    if layouts.layout(func.locals[0].ty).size > 0 {
        params.push(("ptr", "%ret_ptr".to_string()));
    }
    params
}

fn emit_thunk(out: &mut String, layouts: &mut LayoutCache<'_>, func: &Function, idx: usize) {
    let param_tys: Vec<Ty> = (1..=func.params).map(|i| func.locals[i].ty).collect();
    let (_, _, offsets) = layouts.pack_types(&param_tys);
    let ret_size = layouts.layout(func.locals[0].ty).size;

    let _ = writeln!(
        out,
        "\ndefine internal i8 @__noev_thunk_{idx}(ptr %rt, ptr %args_buf, ptr %ret_buf) {{"
    );
    out.push_str("entry:\n");
    let mut call_args = vec!["ptr %rt".to_string()];
    let mut n = 0usize;
    for (&ty, &off) in param_tys.iter().zip(&offsets) {
        let l = layouts.layout(ty);
        if l.size == 0 {
            continue;
        }
        let ptr_name = if off == 0 {
            "%args_buf".to_string()
        } else {
            let p = format!("%ap{n}");
            n += 1;
            let _ = writeln!(
                out,
                "  {p} = getelementptr inbounds i8, ptr %args_buf, i64 {off}"
            );
            p
        };
        if let Some(sk) = l.scalar {
            let sty = scalar_llvm(sk);
            let v = format!("%av{n}");
            n += 1;
            let _ = writeln!(out, "  {v} = load {sty}, ptr {ptr_name}, align {}", l.align);
            call_args.push(format!("{sty} {v}"));
        } else {
            call_args.push(format!("ptr {ptr_name}"));
        }
    }
    if ret_size > 0 {
        call_args.push("ptr %ret_buf".to_string());
    }
    let _ = writeln!(
        out,
        "  %ok = call i8 @__noev_fn_{idx}({})",
        call_args.join(", ")
    );
    out.push_str("  ret i8 %ok\n}\n");
}

fn emit_main(
    out: &mut String,
    layouts: &mut LayoutCache<'_>,
    decls: &mut BTreeMap<String, String>,
    main_func: &Function,
    main_idx: usize,
    abi: RuntimeAbi,
) {
    let ret_layout = layouts.layout(main_func.locals[0].ty);
    out.push_str("\ndefine i32 @main(i32 %argc, ptr %argv) {\nentry:\n");
    if abi == RuntimeAbi::Qir {
        decls
            .entry("__quantum__rt__initialize".into())
            .or_insert_with(|| "declare void @__quantum__rt__initialize(ptr)".into());
        out.push_str("  call void @__quantum__rt__initialize(ptr null)\n");
        let mut args = vec!["ptr null".to_string()];
        if ret_layout.size > 0 {
            let _ = writeln!(
                out,
                "  %ret = alloca [{} x i8], align {}",
                ret_layout.size, ret_layout.align
            );
            args.push("ptr %ret".into());
        }
        let _ = writeln!(
            out,
            "  %ok = call i8 @__noev_fn_{main_idx}({})",
            args.join(", ")
        );
        out.push_str("  %is_ok = icmp ne i8 %ok, 0\n");
        out.push_str("  %code = select i1 %is_ok, i32 0, i32 1\n");
        out.push_str("  ret i32 %code\n}\n");
        return;
    }

    decls
        .entry("noev_rt_main_enter".into())
        .or_insert_with(|| "declare ptr @noev_rt_main_enter(i32, ptr)".into());
    decls
        .entry("noev_rt_main_exit".into())
        .or_insert_with(|| "declare i32 @noev_rt_main_exit(ptr, i8)".into());

    out.push_str("  %rt = call ptr @noev_rt_main_enter(i32 %argc, ptr %argv)\n");
    let mut args = vec!["ptr %rt".to_string()];
    if ret_layout.size > 0 {
        let _ = writeln!(
            out,
            "  %ret = alloca [{} x i8], align {}",
            ret_layout.size, ret_layout.align
        );
        args.push("ptr %ret".into());
    }
    let _ = writeln!(
        out,
        "  %ok = call i8 @__noev_fn_{main_idx}({})",
        args.join(", ")
    );
    out.push_str("  %code = call i32 @noev_rt_main_exit(ptr %rt, i8 %ok)\n");
    out.push_str("  ret i32 %code\n}\n");
}

// -------------------------------------------------------------------------
// Read-only global pool (UTF-8 strings & NoevTypeDesc tables)
// -------------------------------------------------------------------------

struct GlobalPool {
    defs: String,
    bytes: HashMap<Vec<u8>, String>,
    descs: HashMap<Ty, String>,
    next_id: u32,
}

impl GlobalPool {
    fn new() -> Self {
        Self {
            defs: String::new(),
            bytes: HashMap::new(),
            descs: HashMap::new(),
            next_id: 0,
        }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("@__noev_{prefix}_{id}")
    }

    fn intern_bytes(&mut self, data: &[u8]) -> String {
        if let Some(name) = self.bytes.get(data) {
            return name.clone();
        }
        let name = self.fresh("str");
        let payload = if data.is_empty() { &[0u8][..] } else { data };
        let lit = llvm_bytes_literal(payload);
        let _ = writeln!(
            self.defs,
            "{name} = private unnamed_addr constant [{} x i8] {lit}, align 1",
            payload.len()
        );
        self.bytes.insert(data.to_vec(), name.clone());
        name
    }

    fn type_desc(&mut self, program: &Program, layouts: &mut LayoutCache<'_>, ty: Ty) -> String {
        if let Some(name) = self.descs.get(&ty) {
            return name.clone();
        }
        let name = self.fresh("ty");
        self.descs.insert(ty, name.clone());

        let layout = layouts.layout(ty);
        let kind = program.types.kind(ty).clone();

        let (
            tag,
            bare,
            name_sym,
            name_len,
            elem_sym,
            array_len,
            fields_sym,
            n_fields,
            variants_sym,
            n_variants,
        ) = match kind {
            TyKind::Unit | TyKind::Never | TyKind::Error => {
                (TypeTag::Unit, 0, None, 0, None, 0, None, 0, None, 0)
            }
            TyKind::Int => (TypeTag::Int, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::I8 => (TypeTag::I8, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::I16 => (TypeTag::I16, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::I32 => (TypeTag::I32, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::I64 => (TypeTag::I64, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::U8 => (TypeTag::U8, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::U16 => (TypeTag::U16, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::U32 => (TypeTag::U32, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::U64 => (TypeTag::U64, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::F32 => (TypeTag::F32, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Float => (TypeTag::Float, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Complex => (TypeTag::Complex, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::C32 => (TypeTag::C32, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Bool => (TypeTag::Bool, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Bit => (TypeTag::Bit, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::String => (TypeTag::Str, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Qubit => (TypeTag::Qubit, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Observable => (TypeTag::Observable, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Distribution(_) => {
                (TypeTag::Distribution, 0, None, 0, None, 0, None, 0, None, 0)
            }
            TyKind::Vec(elem) => {
                let elem_d = self.type_desc(program, layouts, elem);
                (TypeTag::Vec, 0, None, 0, Some(elem_d), 0, None, 0, None, 0)
            }
            TyKind::IoStream => (TypeTag::IoStream, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Stdin => (TypeTag::Stdin, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::TcpStream => (TypeTag::TcpStream, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::TcpListener => (TypeTag::TcpListener, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Ref { inner, .. } => {
                let elem = self.type_desc(program, layouts, inner);
                (TypeTag::Ref, 0, None, 0, Some(elem), 0, None, 0, None, 0)
            }
            TyKind::Array(elem, len) => {
                let elem_d = self.type_desc(program, layouts, elem);
                (
                    TypeTag::Array,
                    0,
                    None,
                    0,
                    Some(elem_d),
                    len,
                    None,
                    0,
                    None,
                    0,
                )
            }
            TyKind::Tuple(elems) => {
                let specs: Vec<(Option<&str>, u32, Ty)> = elems
                    .iter()
                    .enumerate()
                    .map(|(i, &t)| (None, layout.fields[i], t))
                    .collect();
                let fsym = if specs.is_empty() {
                    None
                } else {
                    Some(self.emit_fields(program, layouts, &specs))
                };
                (
                    TypeTag::Tuple,
                    0,
                    None,
                    0,
                    None,
                    0,
                    fsym,
                    specs.len(),
                    None,
                    0,
                )
            }
            TyKind::Struct(sid) => {
                let sname = program.struct_names[sid.index()].as_bytes();
                let sname_sym = self.intern_bytes(sname);
                let def = &program.types.structs[sid.index()];
                if let Some(variants) = def.variants.clone() {
                    let bare = u8::from(def.bare);
                    let vsym = if variants.is_empty() {
                        None
                    } else {
                        let mut v_specs = Vec::with_capacity(variants.len());
                        for (vi, vdef) in variants.iter().enumerate() {
                            let vname = &program.variant_names[sid.index()][vi];
                            let fnames = &program.variant_fields[sid.index()][vi];
                            let fspecs: Vec<(Option<&str>, u32, Ty)> = vdef
                                .data
                                .iter()
                                .enumerate()
                                .map(|(fi, &fty)| {
                                    let fname = fnames.get(fi).map(String::as_str);
                                    (fname, layout.enum_variants[vi][fi], fty)
                                })
                                .collect();
                            let fsym = if fspecs.is_empty() {
                                None
                            } else {
                                Some(self.emit_fields(program, layouts, &fspecs))
                            };
                            v_specs.push((vname.as_str(), vdef.named, fsym, fspecs.len()));
                        }
                        Some(self.emit_variants(&v_specs))
                    };
                    (
                        TypeTag::Enum,
                        bare,
                        Some(sname_sym),
                        sname.len(),
                        None,
                        0,
                        None,
                        0,
                        vsym,
                        variants.len(),
                    )
                } else {
                    let fnames = &program.struct_fields[sid.index()];
                    let fspecs: Vec<(Option<&str>, u32, Ty)> = def
                        .fields
                        .iter()
                        .enumerate()
                        .map(|(fi, &(_, fty))| {
                            let fname = fnames.get(fi).map(String::as_str);
                            (fname, layout.fields[fi], fty)
                        })
                        .collect();
                    let fsym = if fspecs.is_empty() {
                        None
                    } else {
                        Some(self.emit_fields(program, layouts, &fspecs))
                    };
                    (
                        TypeTag::Struct,
                        0,
                        Some(sname_sym),
                        sname.len(),
                        None,
                        0,
                        fsym,
                        fspecs.len(),
                        None,
                        0,
                    )
                }
            }
            _ => (TypeTag::Unit, 0, None, 0, None, 0, None, 0, None, 0),
        };

        let p = |s: Option<String>| s.unwrap_or_else(|| "null".into());
        let _ = writeln!(
            self.defs,
            "{name} = private unnamed_addr constant %NoevTypeDesc {{ \
             i8 {}, i8 {}, i32 {}, i32 {}, ptr {}, i64 {}, ptr {}, i64 {}, \
             ptr {}, i64 {}, ptr {}, i64 {} }}, align 8",
            tag as u8,
            bare,
            layout.size,
            layout.align,
            p(name_sym),
            name_len,
            p(elem_sym),
            array_len,
            p(fields_sym),
            n_fields,
            p(variants_sym),
            n_variants
        );
        name
    }

    fn emit_fields(
        &mut self,
        program: &Program,
        layouts: &mut LayoutCache<'_>,
        specs: &[(Option<&str>, u32, Ty)],
    ) -> String {
        let mut items = Vec::with_capacity(specs.len());
        for &(fname, offset, fty) in specs {
            let (nsym, nlen) = match fname {
                Some(n) => (self.intern_bytes(n.as_bytes()), n.len()),
                None => ("null".into(), 0),
            };
            let tsym = self.type_desc(program, layouts, fty);
            items.push(format!(
                "%NoevFieldDesc {{ ptr {nsym}, i64 {nlen}, i32 {offset}, ptr {tsym} }}"
            ));
        }
        let sym = self.fresh("fields");
        let _ = writeln!(
            self.defs,
            "{sym} = private unnamed_addr constant [{} x %NoevFieldDesc] [ {} ], align 8",
            items.len(),
            items.join(", ")
        );
        sym
    }

    fn emit_variants(&mut self, specs: &[(&str, bool, Option<String>, usize)]) -> String {
        let mut items = Vec::with_capacity(specs.len());
        for &(vname, named, ref fsym, n_fields) in specs {
            let nsym = self.intern_bytes(vname.as_bytes());
            let fptr = fsym.as_deref().unwrap_or("null");
            items.push(format!(
                "%NoevVariantDesc {{ ptr {nsym}, i64 {}, i8 {}, ptr {fptr}, i64 {n_fields} }}",
                vname.len(),
                u8::from(named)
            ));
        }
        let sym = self.fresh("variants");
        let _ = writeln!(
            self.defs,
            "{sym} = private unnamed_addr constant [{} x %NoevVariantDesc] [ {} ], align 8",
            items.len(),
            items.join(", ")
        );
        sym
    }
}

// -------------------------------------------------------------------------
// Per-function LLVM IR emitter
// -------------------------------------------------------------------------

struct FnEmitter<'a, 'p> {
    program: &'p Program,
    func: &'p Function,
    func_index: usize,
    layouts: &'a mut LayoutCache<'p>,
    globals: &'a mut GlobalPool,
    decls: &'a mut BTreeMap<String, String>,
    opts: CodegenOptions,
    facts: &'a [HashMap<PlaceKey, u32>],
    cur_block: Block,
    allocas: String,
    body: String,
    next_ssa: usize,
    next_lbl: usize,
    has_ret_ptr: bool,
    ctrl_depth_var: String,
}

impl<'a, 'p> FnEmitter<'a, 'p> {
    fn fresh(&mut self) -> String {
        let id = self.next_ssa;
        self.next_ssa += 1;
        format!("%v{id}")
    }

    fn fresh_lbl(&mut self, prefix: &str) -> String {
        let id = self.next_lbl;
        self.next_lbl += 1;
        format!("{prefix}.{id}")
    }

    fn declare(&mut self, name: &str, sig: &str) {
        self.decls
            .entry(name.to_string())
            .or_insert_with(|| sig.to_string());
    }

    /// Allocates a stack buffer in the entry block so `mem2reg` promotes it and loops never grow
    /// the stack.
    fn alloca_bytes(&mut self, size: u32, align: u32) -> String {
        let v = self.fresh();
        let _ = writeln!(
            self.allocas,
            "  {v} = alloca [{} x i8], align {}",
            size.max(1),
            align.max(1)
        );
        v
    }

    fn check_ok(&mut self, ok_i8: &str) {
        let cond = self.fresh();
        let cont = self.fresh_lbl("ok");
        let _ = writeln!(self.body, "  {cond} = icmp ne i8 {ok_i8}, 0");
        let _ = writeln!(self.body, "  br i1 {cond}, label %{cont}, label %err_exit");
        let _ = writeln!(self.body, "{cont}:");
    }

    fn emit_memcpy(&mut self, dst: &str, src: &str, size: u32) {
        if size == 0 {
            return;
        }
        self.declare(
            "llvm.memcpy.p0.p0.i64",
            "declare void @llvm.memcpy.p0.p0.i64(ptr noalias writeonly, ptr noalias readonly, i64, i1 immarg)",
        );
        let _ = writeln!(
            self.body,
            "  call void @llvm.memcpy.p0.p0.i64(ptr {dst}, ptr {src}, i64 {size}, i1 false)"
        );
    }

    fn emit_function(&mut self, out: &mut String) -> Result<(), CodegenError> {
        let params = emit_fn_sig_params(self.layouts, self.func);
        let param_list: Vec<String> = params.iter().map(|(ty, n)| format!("{ty} {n}")).collect();
        self.has_ret_ptr = self.layouts.layout(self.func.locals[0].ty).size > 0;

        // Allocate stack slots for all non-ZST locals in entry block.
        for (i, local) in self.func.locals.iter().enumerate() {
            let l = self.layouts.layout(local.ty);
            if l.size > 0 {
                let _ = writeln!(
                    self.allocas,
                    "  %l_{i} = alloca [{} x i8], align {}",
                    l.size, l.align
                );
            }
        }

        // Copy incoming arguments into local slots.
        for i in 1..=self.func.params {
            let l = self.layouts.layout(self.func.locals[i].ty);
            if l.size == 0 {
                continue;
            }
            if let Some(sk) = l.scalar {
                let sty = scalar_llvm(sk);
                let _ = writeln!(
                    self.body,
                    "  store {sty} %p{i}, ptr %l_{i}, align {}",
                    l.align
                );
            } else {
                self.emit_memcpy(&format!("%l_{i}"), &format!("%p{i}"), l.size);
            }
        }

        // Record entry control depth when targeting Noev's runtime ABI.
        if self.opts.runtime_abi == RuntimeAbi::Noev {
            self.declare("noev_rt_ctrl_depth", "declare i64 @noev_rt_ctrl_depth(ptr)");
            self.declare(
                "noev_rt_ctrl_restore",
                "declare void @noev_rt_ctrl_restore(ptr, i64)",
            );
            let d = self.fresh();
            let _ = writeln!(self.body, "  {d} = call i64 @noev_rt_ctrl_depth(ptr %rt)");
            self.ctrl_depth_var = d;
        }

        if !self.func.blocks.is_empty() {
            self.body.push_str("  br label %bb0\n");
        } else {
            self.emit_return_ok();
        }

        // Error exit block.
        self.body.push_str("err_exit:\n");
        if self.opts.runtime_abi == RuntimeAbi::Noev {
            let _ = writeln!(
                self.body,
                "  call void @noev_rt_ctrl_restore(ptr %rt, i64 {})",
                self.ctrl_depth_var
            );
        }
        self.body.push_str("  ret i8 0\n");

        // Emit each IR basic block.
        for bi in 0..self.func.blocks.len() {
            self.cur_block = Block(bi as u32);
            let _ = writeln!(self.body, "bb{bi}:");
            let stmts = self.func.blocks[bi].stmts.clone();
            let term = self.func.blocks[bi].term.clone();
            for stmt in &stmts {
                self.emit_stmt(stmt)?;
            }
            self.emit_term(&term)?;
        }

        let _ = writeln!(
            out,
            "\ndefine internal i8 @__noev_fn_{}({}) {{",
            self.func_index,
            param_list.join(", ")
        );
        out.push_str("entry:\n");
        out.push_str(&self.allocas);
        out.push_str(&self.body);
        out.push_str("}\n");
        Ok(())
    }

    fn emit_return_ok(&mut self) {
        if self.opts.runtime_abi == RuntimeAbi::Noev {
            let _ = writeln!(
                self.body,
                "  call void @noev_rt_ctrl_restore(ptr %rt, i64 {})",
                self.ctrl_depth_var
            );
        }
        if self.has_ret_ptr {
            let ret_size = self.layouts.layout(self.func.locals[0].ty).size;
            self.emit_memcpy("%ret_ptr", "%l_0", ret_size);
        }
        self.body.push_str("  ret i8 1\n");
    }

    fn place_addr(&mut self, place: &Place) -> Result<(Option<String>, Ty), CodegenError> {
        let mut ty = self.func.locals[place.local.index()].ty;
        let mut addr =
            (self.layouts.layout(ty).size > 0).then(|| format!("%l_{}", place.local.index()));
        let mut prefix = Place::local(place.local);

        for proj in &place.proj {
            match proj {
                Proj::Deref => {
                    let inner = match self.program.types.kind(ty) {
                        TyKind::Ref { inner, .. } => *inner,
                        _ => ty,
                    };
                    if let Some(p) = addr {
                        let loaded = self.fresh();
                        let _ = writeln!(self.body, "  {loaded} = load ptr, ptr {p}, align 8");
                        addr = Some(loaded);
                    }
                    ty = inner;
                }
                Proj::Field(fi) => {
                    let l = self.layouts.layout(ty);
                    match self.program.types.kind(ty).clone() {
                        TyKind::Tuple(elems) => {
                            let off = l.fields[*fi as usize];
                            if let Some(p) = addr.as_ref()
                                && off > 0
                            {
                                let np = self.fresh();
                                let _ = writeln!(
                                    self.body,
                                    "  {np} = getelementptr inbounds i8, ptr {p}, i64 {off}"
                                );
                                addr = Some(np);
                            }
                            ty = elems[*fi as usize];
                        }
                        TyKind::Struct(sid) => {
                            let def = &self.program.types.structs[sid.index()];
                            if let Some(variants) = &def.variants {
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
                                if let Some(p) = addr.as_ref()
                                    && off > 0
                                {
                                    let np = self.fresh();
                                    let _ = writeln!(
                                        self.body,
                                        "  {np} = getelementptr inbounds i8, ptr {p}, i64 {off}"
                                    );
                                    addr = Some(np);
                                }
                                ty = variants[vi].data[*fi as usize];
                            } else {
                                let off = l.fields[*fi as usize];
                                if let Some(p) = addr.as_ref()
                                    && off > 0
                                {
                                    let np = self.fresh();
                                    let _ = writeln!(
                                        self.body,
                                        "  {np} = getelementptr inbounds i8, ptr {p}, i64 {off}"
                                    );
                                    addr = Some(np);
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
                    let checked = if self.opts.runtime_abi == RuntimeAbi::Qir {
                        // Inline untag for fast QIR-mode array indexing.
                        let raw = self.fresh();
                        let _ = writeln!(self.body, "  {raw} = ashr i64 {idx_val}, 1");
                        raw
                    } else {
                        self.declare(
                            "noev_rt_bounds_check",
                            "declare i8 @noev_rt_bounds_check(ptr, i64, i64, ptr)",
                        );
                        let slot = self.alloca_bytes(8, 8);
                        let ok = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {ok} = call i8 @noev_rt_bounds_check(ptr %rt, i64 {idx_val}, i64 {len}, ptr {slot})"
                        );
                        self.check_ok(&ok);
                        let c = self.fresh();
                        let _ = writeln!(self.body, "  {c} = load i64, ptr {slot}, align 8");
                        c
                    };
                    let elem_size = self.layouts.layout(elem).size;
                    if let Some(p) = addr {
                        let off = if elem_size == 1 {
                            checked
                        } else {
                            let o = self.fresh();
                            let _ = writeln!(self.body, "  {o} = mul i64 {checked}, {elem_size}");
                            o
                        };
                        let np = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {np} = getelementptr inbounds i8, ptr {p}, i64 {off}"
                        );
                        addr = Some(np);
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

    fn eval_scalar(&mut self, op: &Operand) -> Result<String, CodegenError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let (addr, ty) = self.place_addr(p)?;
                let l = self.layouts.layout(ty);
                let sk = l.scalar.ok_or_else(|| {
                    CodegenError(format!("expected scalar operand, got {:?}", ty))
                })?;
                let ptr = addr.expect("scalar has address");
                let v = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {v} = load {}, ptr {ptr}, align {}",
                    scalar_llvm(sk),
                    l.align
                );
                Ok(v)
            }
            Operand::Const(c) => self.eval_const_scalar(*c),
        }
    }

    fn eval_const_scalar(&mut self, c: Const) -> Result<String, CodegenError> {
        Ok(match c {
            Const::Int(n) => {
                if (MIN_INLINE_INT..=MAX_INLINE_INT).contains(&n) {
                    format!("{}", n << 1)
                } else {
                    self.declare(
                        "noev_rt_int_from_i64",
                        "declare i64 @noev_rt_int_from_i64(ptr, i64)",
                    );
                    let v = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {v} = call i64 @noev_rt_int_from_i64(ptr %rt, i64 {n})"
                    );
                    v
                }
            }
            Const::Sized(v, w) => match w.bits {
                8 => format!("{}", v as u8 as i8),
                16 => format!("{}", v as u16 as i16),
                32 => format!("{}", v as u32 as i32),
                _ => format!("{}", v as u64 as i64),
            },
            Const::Float(v) => llvm_f64(v),
            Const::F32(v) => llvm_f32(v),
            Const::Bool(b) | Const::Bit(b) => format!("{}", u8::from(b)),
            Const::Unit | Const::Complex(..) | Const::C32(..) => {
                return Err(CodegenError(format!("non-scalar constant {c:?}")));
            }
        })
    }

    fn operand_addr(&mut self, op: &Operand) -> Result<(Option<String>, Ty), CodegenError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => self.place_addr(p),
            Operand::Const(c) => {
                let ty = const_ty(*c);
                let l = self.layouts.layout(ty);
                if l.size == 0 {
                    return Ok((None, ty));
                }
                let ptr = self.alloca_bytes(l.size, l.align);
                self.store_const(&ptr, *c)?;
                Ok((Some(ptr), ty))
            }
        }
    }

    fn store_const(&mut self, dst: &str, c: Const) -> Result<(), CodegenError> {
        match c {
            Const::Unit => {}
            Const::Complex(re, im) => {
                let _ = writeln!(
                    self.body,
                    "  store double {}, ptr {dst}, align 8",
                    llvm_f64(re)
                );
                let p1 = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {p1} = getelementptr inbounds i8, ptr {dst}, i64 8"
                );
                let _ = writeln!(
                    self.body,
                    "  store double {}, ptr {p1}, align 8",
                    llvm_f64(im)
                );
            }
            Const::C32(re, im) => {
                let _ = writeln!(
                    self.body,
                    "  store float {}, ptr {dst}, align 4",
                    llvm_f32(re)
                );
                let p1 = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {p1} = getelementptr inbounds i8, ptr {dst}, i64 4"
                );
                let _ = writeln!(
                    self.body,
                    "  store float {}, ptr {p1}, align 4",
                    llvm_f32(im)
                );
            }
            other => {
                let ty = const_ty(other);
                let l = self.layouts.layout(ty);
                let sty = scalar_llvm(l.scalar.expect("scalar const"));
                let v = self.eval_const_scalar(other)?;
                let _ = writeln!(self.body, "  store {sty} {v}, ptr {dst}, align {}", l.align);
            }
        }
        Ok(())
    }

    fn store_operand(&mut self, dst: Option<&str>, op: &Operand) -> Result<(), CodegenError> {
        let Some(dst) = dst else {
            return Ok(());
        };
        match op {
            Operand::Copy(p) | Operand::Move(p) => {
                let (src, ty) = self.place_addr(p)?;
                let l = self.layouts.layout(ty);
                if let Some(src) = src {
                    self.emit_memcpy(dst, &src, l.size);
                }
            }
            Operand::Const(c) => self.store_const(dst, *c)?,
        }
        Ok(())
    }

    fn load_qubit_place(&mut self, place: &Place) -> Result<String, CodegenError> {
        let (addr, ty) = self.place_addr(place)?;
        let mut ptr = addr.expect("qubit place");
        let mut cur = ty;
        while let TyKind::Ref { inner, .. } = self.program.types.kind(cur) {
            let np = self.fresh();
            let _ = writeln!(self.body, "  {np} = load ptr, ptr {ptr}, align 8");
            ptr = np;
            cur = *inner;
        }
        let q = self.fresh();
        let _ = writeln!(self.body, "  {q} = load ptr, ptr {ptr}, align 8");
        Ok(q)
    }

    fn collect_qubit_handles(&mut self, places: &[Place]) -> Result<(String, usize), CodegenError> {
        let mut loaded = Vec::new();
        for place in places {
            let (addr, ty) = self.place_addr(place)?;
            let Some(mut ptr) = addr else { continue };
            let mut cur = ty;
            while let TyKind::Ref { inner, .. } = self.program.types.kind(cur) {
                let np = self.fresh();
                let _ = writeln!(self.body, "  {np} = load ptr, ptr {ptr}, align 8");
                ptr = np;
                cur = *inner;
            }
            for off in self.layouts.qubit_offsets(cur) {
                let qp = if off == 0 {
                    ptr.clone()
                } else {
                    let np = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {np} = getelementptr inbounds i8, ptr {ptr}, i64 {off}"
                    );
                    np
                };
                let q = self.fresh();
                let _ = writeln!(self.body, "  {q} = load ptr, ptr {qp}, align 8");
                loaded.push(q);
            }
        }
        let n = loaded.len();
        if n == 0 {
            return Ok(("null".into(), 0));
        }
        let stride = size_of::<usize>() as u32;
        let buf = self.alloca_bytes(stride * n as u32, stride);
        for (i, q) in loaded.into_iter().enumerate() {
            let dst = if i == 0 {
                buf.clone()
            } else {
                let p = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {p} = getelementptr inbounds i8, ptr {buf}, i64 {}",
                    i as u32 * stride
                );
                p
            };
            let _ = writeln!(self.body, "  store ptr {q}, ptr {dst}, align {stride}");
        }
        Ok((buf, n))
    }

    fn pack_args_buf(
        &mut self,
        args: &[Operand],
        param_tys: &[Ty],
    ) -> Result<String, CodegenError> {
        let (size, align, offsets) = self.layouts.pack_types(param_tys);
        if size == 0 {
            return Ok("null".into());
        }
        let buf = self.alloca_bytes(size, align);
        for (op, (&ty, &off)) in args.iter().zip(param_tys.iter().zip(&offsets)) {
            if self.layouts.layout(ty).size == 0 {
                continue;
            }
            let dst = if off == 0 {
                buf.clone()
            } else {
                let p = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {p} = getelementptr inbounds i8, ptr {buf}, i64 {off}"
                );
                p
            };
            self.store_operand(Some(&dst), op)?;
        }
        Ok(buf)
    }

    fn widen_sized_to_i64(&mut self, v: &str, bits: u32, signed: bool) -> String {
        if bits == 64 {
            return v.to_string();
        }
        let src_ty = match bits {
            8 => "i8",
            16 => "i16",
            32 => "i32",
            _ => "i64",
        };
        let op = if signed { "sext" } else { "zext" };
        let res = self.fresh();
        let _ = writeln!(self.body, "  {res} = {op} {src_ty} {v} to i64");
        res
    }

    fn narrow_i64_to_sized(&mut self, v: &str, bits: u32) -> (String, &'static str) {
        match bits {
            8 => {
                let r = self.fresh();
                let _ = writeln!(self.body, "  {r} = trunc i64 {v} to i8");
                (r, "i8")
            }
            16 => {
                let r = self.fresh();
                let _ = writeln!(self.body, "  {r} = trunc i64 {v} to i16");
                (r, "i16")
            }
            32 => {
                let r = self.fresh();
                let _ = writeln!(self.body, "  {r} = trunc i64 {v} to i32");
                (r, "i32")
            }
            _ => (v.to_string(), "i64"),
        }
    }

    // -------------------------------------------------------------------------
    // Statements
    // -------------------------------------------------------------------------

    fn emit_stmt(&mut self, stmt: &Stmt) -> Result<(), CodegenError> {
        let qir_abi = self.opts.runtime_abi == RuntimeAbi::Qir;
        match stmt {
            Stmt::Assign(dest, rv) => self.emit_assign(dest, rv),
            Stmt::Alloc(place) => {
                let (addr, _) = self.place_addr(place)?;
                let dst = addr.expect("Qubit place");
                if qir_abi {
                    self.declare(
                        "__quantum__rt__qubit_allocate",
                        "declare ptr @__quantum__rt__qubit_allocate()",
                    );
                    let q = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {q} = call ptr @__quantum__rt__qubit_allocate()"
                    );
                    let _ = writeln!(self.body, "  store ptr {q}, ptr {dst}, align 8");
                } else {
                    self.declare(
                        "noev_rt_qubit_allocate",
                        "declare i8 @noev_rt_qubit_allocate(ptr, ptr)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_qubit_allocate(ptr %rt, ptr {dst})"
                    );
                    self.check_ok(&ok);
                }
                Ok(())
            }
            Stmt::Prepare { qubit, label } => {
                let q = self.load_qubit_place(qubit)?;
                let (laddr, lty) = self.operand_addr(label)?;
                if lty == Ty::BIT {
                    let b = self.eval_scalar(label)?;
                    let cond = self.fresh();
                    let do_x = self.fresh_lbl("prep.x");
                    let done = self.fresh_lbl("prep.done");
                    let _ = writeln!(self.body, "  {cond} = icmp ne i8 {b}, 0");
                    let _ = writeln!(self.body, "  br i1 {cond}, label %{do_x}, label %{done}");
                    let _ = writeln!(self.body, "{do_x}:");
                    self.emit_1q_gate(Gate::X, false, &q);
                    let _ = writeln!(self.body, "  br label %{done}");
                    let _ = writeln!(self.body, "{done}:");
                } else {
                    let tag = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {tag} = load i32, ptr {}, align 4",
                        laddr.expect("State")
                    );
                    self.declare(
                        "noev_rt_prepare_state",
                        "declare i8 @noev_rt_prepare_state(ptr, ptr, i32)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_prepare_state(ptr %rt, ptr {q}, i32 {tag})"
                    );
                    self.check_ok(&ok);
                }
                Ok(())
            }
            Stmt::IntoZ { qubit, label } => {
                let (laddr, lty) = self.operand_addr(label)?;
                if lty != Ty::BIT {
                    let q = self.load_qubit_place(qubit)?;
                    let tag = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {tag} = load i32, ptr {}, align 4",
                        laddr.expect("State")
                    );
                    self.declare(
                        "noev_rt_into_z_state",
                        "declare i8 @noev_rt_into_z_state(ptr, ptr, i32)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_into_z_state(ptr %rt, ptr {q}, i32 {tag})"
                    );
                    self.check_ok(&ok);
                }
                Ok(())
            }
            Stmt::Gate {
                gate,
                adjoint,
                qubits,
                params,
            } => self.emit_gate(*gate, *adjoint, qubits, params),
            Stmt::Measure { dest, qubit } => {
                let q = self.load_qubit_place(qubit)?;
                let (dst_addr, _) = self.place_addr(dest)?;
                if qir_abi {
                    self.declare(
                        "__quantum__qis__m__body",
                        "declare ptr @__quantum__qis__m__body(ptr)",
                    );
                    self.declare(
                        "__quantum__rt__read_result",
                        "declare i1 @__quantum__rt__read_result(ptr)",
                    );
                    let res = self.fresh();
                    let bit1 = self.fresh();
                    let bit8 = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {res} = call ptr @__quantum__qis__m__body(ptr {q})"
                    );
                    let _ = writeln!(
                        self.body,
                        "  {bit1} = call i1 @__quantum__rt__read_result(ptr {res})"
                    );
                    let _ = writeln!(self.body, "  {bit8} = zext i1 {bit1} to i8");
                    if let Some(d) = dst_addr {
                        let _ = writeln!(self.body, "  store i8 {bit8}, ptr {d}, align 1");
                    }
                } else {
                    self.declare("noev_qis_mz", "declare i8 @noev_qis_mz(ptr, ptr, ptr)");
                    self.declare(
                        "noev_rt_result_read",
                        "declare i8 @noev_rt_result_read(ptr, i64)",
                    );
                    let slot = self.alloca_bytes(8, 8);
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_qis_mz(ptr %rt, ptr {q}, ptr {slot})"
                    );
                    self.check_ok(&ok);
                    let res = self.fresh();
                    let _ = writeln!(self.body, "  {res} = load i64, ptr {slot}, align 8");
                    let bit = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {bit} = call i8 @noev_rt_result_read(ptr %rt, i64 {res})"
                    );
                    if let Some(d) = dst_addr {
                        let _ = writeln!(self.body, "  store i8 {bit}, ptr {d}, align 1");
                    }
                }
                Ok(())
            }
            Stmt::Reset(place) => {
                let q = self.load_qubit_place(place)?;
                if qir_abi {
                    self.declare(
                        "__quantum__qis__reset__body",
                        "declare void @__quantum__qis__reset__body(ptr)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @__quantum__qis__reset__body(ptr {q})"
                    );
                } else {
                    self.declare(
                        "noev_rt_qubit_reset",
                        "declare i8 @noev_rt_qubit_reset(ptr, ptr)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_qubit_reset(ptr %rt, ptr {q})"
                    );
                    self.check_ok(&ok);
                }
                Ok(())
            }
            Stmt::Release(place) => {
                let q = self.load_qubit_place(place)?;
                if qir_abi {
                    self.declare(
                        "__quantum__rt__qubit_release",
                        "declare void @__quantum__rt__qubit_release(ptr)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @__quantum__rt__qubit_release(ptr {q})"
                    );
                } else {
                    self.declare(
                        "noev_rt_qubit_release",
                        "declare i8 @noev_rt_qubit_release(ptr, ptr)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_qubit_release(ptr %rt, ptr {q})"
                    );
                    self.check_ok(&ok);
                }
                Ok(())
            }
            Stmt::CtrlPush(places) => {
                let (buf, n) = self.collect_qubit_handles(places)?;
                if qir_abi {
                    self.declare(
                        "__quantum__rt__ctrl_push",
                        "declare void @__quantum__rt__ctrl_push(ptr, i64)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @__quantum__rt__ctrl_push(ptr {buf}, i64 {n})"
                    );
                } else {
                    self.declare(
                        "noev_rt_ctrl_push",
                        "declare void @noev_rt_ctrl_push(ptr, ptr, i64)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @noev_rt_ctrl_push(ptr %rt, ptr {buf}, i64 {n})"
                    );
                }
                Ok(())
            }
            Stmt::CtrlPop => {
                if qir_abi {
                    self.declare(
                        "__quantum__rt__ctrl_pop",
                        "declare void @__quantum__rt__ctrl_pop()",
                    );
                    self.body
                        .push_str("  call void @__quantum__rt__ctrl_pop()\n");
                } else {
                    self.declare("noev_rt_ctrl_pop", "declare void @noev_rt_ctrl_pop(ptr)");
                    self.body
                        .push_str("  call void @noev_rt_ctrl_pop(ptr %rt)\n");
                }
                Ok(())
            }
            Stmt::WithStart => {
                self.declare(
                    "noev_rt_with_start",
                    "declare void @noev_rt_with_start(ptr)",
                );
                self.body
                    .push_str("  call void @noev_rt_with_start(ptr %rt)\n");
                Ok(())
            }
            Stmt::WithDo => {
                self.declare("noev_rt_with_do", "declare void @noev_rt_with_do(ptr)");
                self.body
                    .push_str("  call void @noev_rt_with_do(ptr %rt)\n");
                Ok(())
            }
            Stmt::WithEnd => {
                self.declare("noev_rt_with_end", "declare i8 @noev_rt_with_end(ptr)");
                let ok = self.fresh();
                let _ = writeln!(self.body, "  {ok} = call i8 @noev_rt_with_end(ptr %rt)");
                self.check_ok(&ok);
                Ok(())
            }
            Stmt::Print { stderr, args } => self.emit_print(*stderr, args),
            Stmt::Qasm { parts, qubits } => self.emit_qasm(parts, qubits),
            Stmt::StdCall { op, args, dest } => self.emit_std_call(*op, args, dest.as_ref()),
            Stmt::Sample {
                dest,
                func,
                args,
                shots,
                elem,
            } => self.emit_sample(dest, *func, args, shots, *elem),
            Stmt::Expectation {
                dest,
                prep,
                observable,
                precision,
            } => self.emit_expectation(dest, prep, observable, precision.as_ref()),
            Stmt::Overlap {
                dest,
                bra,
                ket,
                observable,
                precision,
            } => self.emit_overlap(dest, bra, ket, observable.as_ref(), precision.as_ref()),
        }
    }

    fn emit_1q_gate(&mut self, gate: Gate, adjoint: bool, q: &str) {
        if self.opts.runtime_abi == RuntimeAbi::Qir {
            let name = match (gate, adjoint) {
                (Gate::X, _) => "__quantum__qis__x__body",
                (Gate::Y, _) => "__quantum__qis__y__body",
                (Gate::Z, _) => "__quantum__qis__z__body",
                (Gate::H, _) => "__quantum__qis__h__body",
                (Gate::S, false) => "__quantum__qis__s__body",
                (Gate::S, true) => "__quantum__qis__s__adj",
                (Gate::T, false) => "__quantum__qis__t__body",
                (Gate::T, true) => "__quantum__qis__t__adj",
                _ => unreachable!(),
            };
            self.declare(name, &format!("declare void @{name}(ptr)"));
            let _ = writeln!(self.body, "  call void @{name}(ptr {q})");
        } else {
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
            self.declare(name, &format!("declare i8 @{name}(ptr, ptr)"));
            let ok = self.fresh();
            let _ = writeln!(self.body, "  {ok} = call i8 @{name}(ptr %rt, ptr {q})");
            self.check_ok(&ok);
        }
    }

    fn emit_rot_gate(&mut self, gate: Gate, theta: &str, q: &str) {
        if self.opts.runtime_abi == RuntimeAbi::Qir {
            let name = match gate {
                Gate::Rx => "__quantum__qis__rx__body",
                Gate::Ry => "__quantum__qis__ry__body",
                Gate::Rz => "__quantum__qis__rz__body",
                Gate::P => "__quantum__qis__r1__body",
                _ => unreachable!(),
            };
            self.declare(name, &format!("declare void @{name}(double, ptr)"));
            let _ = writeln!(self.body, "  call void @{name}(double {theta}, ptr {q})");
        } else {
            let name = match gate {
                Gate::Rx => "noev_qis_rx",
                Gate::Ry => "noev_qis_ry",
                Gate::Rz => "noev_qis_rz",
                Gate::P => "noev_qis_p",
                _ => unreachable!(),
            };
            self.declare(name, &format!("declare i8 @{name}(ptr, double, ptr)"));
            let ok = self.fresh();
            let _ = writeln!(
                self.body,
                "  {ok} = call i8 @{name}(ptr %rt, double {theta}, ptr {q})"
            );
            self.check_ok(&ok);
        }
    }

    fn emit_gate(
        &mut self,
        gate: Gate,
        adjoint: bool,
        qubits: &[Place],
        params: &[Operand],
    ) -> Result<(), CodegenError> {
        let qir_abi = self.opts.runtime_abi == RuntimeAbi::Qir;
        match gate {
            Gate::Unitary => {
                let (qs_ptr, n_q) = self.collect_qubit_handles(qubits)?;
                let (mat_addr, _) = self.operand_addr(&params[0])?;
                let mat_ptr = mat_addr.expect("unitary matrix");
                let dim = 1usize << n_q;
                self.declare(
                    "noev_rt_unitary_matrix",
                    "declare i8 @noev_rt_unitary_matrix(ptr, ptr, i64, ptr, i64, i8)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_unitary_matrix(ptr %rt, ptr {qs_ptr}, i64 {n_q}, ptr {mat_ptr}, i64 {dim}, i8 {})",
                    u8::from(adjoint)
                );
                self.check_ok(&ok);
            }
            Gate::Swap => {
                let q0 = self.load_qubit_place(&qubits[0])?;
                let q1 = self.load_qubit_place(&qubits[1])?;
                if qir_abi {
                    self.declare(
                        "__quantum__qis__swap__body",
                        "declare void @__quantum__qis__swap__body(ptr, ptr)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @__quantum__qis__swap__body(ptr {q0}, ptr {q1})"
                    );
                } else {
                    self.declare("noev_qis_swap", "declare i8 @noev_qis_swap(ptr, ptr, ptr)");
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_qis_swap(ptr %rt, ptr {q0}, ptr {q1})"
                    );
                    self.check_ok(&ok);
                }
            }
            Gate::X | Gate::Y | Gate::Z | Gate::H | Gate::S | Gate::T => {
                let q = self.load_qubit_place(&qubits[0])?;
                self.emit_1q_gate(gate, adjoint, &q);
            }
            Gate::Rx | Gate::Ry | Gate::Rz | Gate::P => {
                let q = self.load_qubit_place(&qubits[0])?;
                let mut theta = self.eval_scalar(&params[0])?;
                if adjoint {
                    let neg = self.fresh();
                    let _ = writeln!(self.body, "  {neg} = fneg double {theta}");
                    theta = neg;
                }
                self.emit_rot_gate(gate, &theta, &q);
            }
            Gate::U => {
                let q = self.load_qubit_place(&qubits[0])?;
                let theta = self.eval_scalar(&params[0])?;
                let phi = self.eval_scalar(&params[1])?;
                let lambda = self.eval_scalar(&params[2])?;
                if qir_abi {
                    let (t, p, l) = if adjoint {
                        let nt = self.fresh();
                        let np = self.fresh();
                        let nl = self.fresh();
                        let _ = writeln!(self.body, "  {nt} = fneg double {theta}");
                        let _ = writeln!(self.body, "  {np} = fneg double {lambda}");
                        let _ = writeln!(self.body, "  {nl} = fneg double {phi}");
                        (nt, np, nl)
                    } else {
                        (theta, phi, lambda)
                    };
                    self.emit_rot_gate(Gate::Rz, &l, &q);
                    self.emit_rot_gate(Gate::Ry, &t, &q);
                    self.emit_rot_gate(Gate::Rz, &p, &q);
                } else {
                    let name = if adjoint {
                        "noev_qis_u_adj"
                    } else {
                        "noev_qis_u"
                    };
                    self.declare(
                        name,
                        &format!("declare i8 @{name}(ptr, double, double, double, ptr)"),
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @{name}(ptr %rt, double {theta}, double {phi}, double {lambda}, ptr {q})"
                    );
                    self.check_ok(&ok);
                }
            }
        }
        Ok(())
    }

    fn emit_print(&mut self, stderr: bool, args: &[(Operand, Ty)]) -> Result<(), CodegenError> {
        let n = args.len();
        let stride = size_of::<NoevPrintArg>() as u32;
        let arr_ptr = if n == 0 {
            "null".to_string()
        } else {
            let buf = self.alloca_bytes(stride * n as u32, align_of::<NoevPrintArg>() as u32);
            for (i, (op, ty)) in args.iter().enumerate() {
                let (vaddr, _) = self.operand_addr(op)?;
                let vptr = vaddr.unwrap_or_else(|| "null".into());
                let desc = self.globals.type_desc(self.program, self.layouts, *ty);
                let base = i * stride as usize;
                let p0 = self.fresh();
                let p1 = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {p0} = getelementptr inbounds i8, ptr {buf}, i64 {}",
                    base + offset_of!(NoevPrintArg, ptr)
                );
                let _ = writeln!(self.body, "  store ptr {vptr}, ptr {p0}, align 8");
                let _ = writeln!(
                    self.body,
                    "  {p1} = getelementptr inbounds i8, ptr {buf}, i64 {}",
                    base + offset_of!(NoevPrintArg, ty)
                );
                let _ = writeln!(self.body, "  store ptr {desc}, ptr {p1}, align 8");
            }
            buf
        };
        self.declare(
            "noev_rt_print",
            "declare i8 @noev_rt_print(ptr, i8, ptr, i64)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_rt_print(ptr %rt, i8 {}, ptr {arr_ptr}, i64 {n})",
            u8::from(stderr)
        );
        self.check_ok(&ok);
        Ok(())
    }

    fn emit_qasm(&mut self, parts: &[QasmPart], qubits: &[Place]) -> Result<(), CodegenError> {
        if self.opts.runtime_abi == RuntimeAbi::Qir {
            return Err(CodegenError(
                "inline OpenQASM (`qasm(\"…\")`) is not supported with `--qir-runtime`".into(),
            ));
        }
        let fmt_parts: Vec<FormatPart> = parts
            .iter()
            .map(|p| match p {
                QasmPart::Text(t) => FormatPart::Text(t.clone()),
                QasmPart::Qubit(idx) => FormatPart::Text(format!("\x01{idx}\x01")),
                QasmPart::Value(op, ty) => FormatPart::Value(op.clone(), *ty, None),
            })
            .collect();
        let str_slot = self.alloca_bytes(8, 8);
        self.emit_format(Some(&str_slot), &fmt_parts)?;
        let str_handle = self.fresh();
        let _ = writeln!(
            self.body,
            "  {str_handle} = load i64, ptr {str_slot}, align 8"
        );
        let (q_ptr, q_count) = self.collect_qubit_handles(qubits)?;
        self.declare(
            "noev_qis_qasm_str",
            "declare i8 @noev_qis_qasm_str(ptr, i64, ptr, i64)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_qis_qasm_str(ptr %rt, i64 {str_handle}, ptr {q_ptr}, i64 {q_count})"
        );
        self.check_ok(&ok);
        Ok(())
    }

    fn emit_std_call(
        &mut self,
        op: noev_sema::prelude::StdOp,
        args: &[(Operand, Ty)],
        dest: Option<&(Place, Ty)>,
    ) -> Result<(), CodegenError> {
        let n = args.len();
        let stride = size_of::<NoevPrintArg>() as u32;
        let arr_ptr = if n == 0 {
            "null".to_string()
        } else {
            let buf = self.alloca_bytes(stride * n as u32, align_of::<NoevPrintArg>() as u32);
            for (i, (arg_op, ty)) in args.iter().enumerate() {
                let (vaddr, _) = self.operand_addr(arg_op)?;
                let vptr = vaddr.unwrap_or_else(|| "null".into());
                let desc = self.globals.type_desc(self.program, self.layouts, *ty);
                let base = i * stride as usize;
                let p0 = self.fresh();
                let p1 = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {p0} = getelementptr inbounds i8, ptr {buf}, i64 {}",
                    base + offset_of!(NoevPrintArg, ptr)
                );
                let _ = writeln!(self.body, "  store ptr {vptr}, ptr {p0}, align 8");
                let _ = writeln!(
                    self.body,
                    "  {p1} = getelementptr inbounds i8, ptr {buf}, i64 {}",
                    base + offset_of!(NoevPrintArg, ty)
                );
                let _ = writeln!(self.body, "  store ptr {desc}, ptr {p1}, align 8");
            }
            buf
        };
        let (out_ptr, out_ty_ptr) = if let Some((place, ty)) = dest {
            let (addr, _) = self.place_addr(place)?;
            let ptr = addr.unwrap_or_else(|| "null".into());
            let desc = self.globals.type_desc(self.program, self.layouts, *ty);
            (ptr, desc)
        } else {
            ("null".to_string(), "null".to_string())
        };
        self.declare(
            "noev_rt_std_call",
            "declare i8 @noev_rt_std_call(ptr, i32, ptr, i64, ptr, ptr)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_rt_std_call(ptr %rt, i32 {}, ptr {arr_ptr}, i64 {n}, ptr {out_ptr}, ptr {out_ty_ptr})",
            op as u32
        );
        self.check_ok(&ok);
        Ok(())
    }

    fn emit_sample(
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
        let callee = &self.program.functions[func.index()];
        let use_circuit = callee.sampled && !has_feedback(self.program, func);
        let param_tys: Vec<Ty> = (1..=callee.params).map(|i| callee.locals[i].ty).collect();
        let args_buf = self.pack_args_buf(args, &param_tys)?;
        let ret_desc = self
            .globals
            .type_desc(self.program, self.layouts, callee.locals[0].ty);
        let elem_desc = self.globals.type_desc(self.program, self.layouts, elem);

        self.declare(
            "noev_rt_sample_compiled",
            "declare i8 @noev_rt_sample_compiled(ptr, i64, i8, ptr, ptr, ptr, ptr, ptr)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_rt_sample_compiled(ptr %rt, i64 {shots_val}, i8 {}, \
             ptr @__noev_thunk_{}, ptr {args_buf}, ptr {ret_desc}, ptr {elem_desc}, ptr {dst})",
            u8::from(use_circuit),
            func.index()
        );
        self.check_ok(&ok);
        Ok(())
    }

    fn emit_prep_args(
        &mut self,
        prep: &Prep,
    ) -> Result<(String, usize, String, String, String), CodegenError> {
        match prep {
            Prep::Ket(s) => {
                let sym = self.globals.intern_bytes(s.as_bytes());
                Ok((sym, s.len(), "null".into(), "null".into(), "null".into()))
            }
            Prep::Call(fid, args) => {
                let callee = &self.program.functions[fid.index()];
                let param_tys: Vec<Ty> = (1..=callee.params).map(|i| callee.locals[i].ty).collect();
                let args_buf = self.pack_args_buf(args, &param_tys)?;
                let ret_desc =
                    self.globals
                        .type_desc(self.program, self.layouts, callee.locals[0].ty);
                Ok((
                    "null".into(),
                    0,
                    format!("@__noev_thunk_{}", fid.index()),
                    args_buf,
                    ret_desc,
                ))
            }
        }
    }

    fn emit_expectation(
        &mut self,
        dest: &Place,
        prep: &Prep,
        observable: &Operand,
        precision: Option<&Operand>,
    ) -> Result<(), CodegenError> {
        let (dst_addr, _) = self.place_addr(dest)?;
        let dst = dst_addr.expect("Float dest");
        let (kptr, klen, body, args_buf, rdesc) = self.emit_prep_args(prep)?;
        let obs = self.eval_scalar(observable)?;
        let prec = match precision {
            Some(p) => self.eval_scalar(p)?,
            None => llvm_f64(DEFAULT_PRECISION),
        };
        self.declare(
            "noev_rt_expectation_compiled",
            "declare i8 @noev_rt_expectation_compiled(ptr, ptr, i64, ptr, ptr, ptr, i64, double, ptr)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_rt_expectation_compiled(ptr %rt, ptr {kptr}, i64 {klen}, \
             ptr {body}, ptr {args_buf}, ptr {rdesc}, i64 {obs}, double {prec}, ptr {dst})"
        );
        self.check_ok(&ok);
        Ok(())
    }

    fn emit_overlap(
        &mut self,
        dest: &Place,
        bra: &Prep,
        ket: &Prep,
        observable: Option<&Operand>,
        precision: Option<&Operand>,
    ) -> Result<(), CodegenError> {
        let (dst_addr, _) = self.place_addr(dest)?;
        let dst = dst_addr.expect("Complex dest");
        let (bp, bl, bb, ba, bd) = self.emit_prep_args(bra)?;
        let (kp, kl, kb, ka, kd) = self.emit_prep_args(ket)?;
        let (has_obs, obs_v) = match observable {
            Some(o) => (1u8, self.eval_scalar(o)?),
            None => (0u8, "0".into()),
        };
        let prec = match precision {
            Some(p) => self.eval_scalar(p)?,
            None => llvm_f64(DEFAULT_PRECISION),
        };
        self.declare(
            "noev_rt_overlap_compiled",
            "declare i8 @noev_rt_overlap_compiled(ptr, ptr, i64, ptr, ptr, ptr, ptr, i64, ptr, ptr, ptr, i8, i64, double, ptr)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_rt_overlap_compiled(ptr %rt, ptr {bp}, i64 {bl}, ptr {bb}, \
             ptr {ba}, ptr {bd}, ptr {kp}, i64 {kl}, ptr {kb}, ptr {ka}, ptr {kd}, i8 {has_obs}, \
             i64 {obs_v}, double {prec}, ptr {dst})"
        );
        self.check_ok(&ok);
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Assignments & Rvalues
    // -------------------------------------------------------------------------

    fn emit_assign(&mut self, dest: &Place, rv: &Rvalue) -> Result<(), CodegenError> {
        let (dst_addr, dst_ty) = self.place_addr(dest)?;
        let dst_layout = self.layouts.layout(dst_ty);

        match rv {
            Rvalue::Use(op) => self.store_operand(dst_addr.as_deref(), op),
            Rvalue::Unary(op, x) => self.emit_unary(dst_addr.as_deref(), dst_ty, *op, x),
            Rvalue::Binary(op, a, b) => self.emit_binary(dst_addr.as_deref(), dst_ty, *op, a, b),
            Rvalue::Aggregate(ops) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                match self.program.types.kind(dst_ty).clone() {
                    TyKind::Array(elem, _) => {
                        let stride = self.layouts.layout(elem).size;
                        for (i, op) in ops.iter().enumerate() {
                            if stride == 0 {
                                continue;
                            }
                            let p = if i == 0 {
                                dst.clone()
                            } else {
                                let np = self.fresh();
                                let _ = writeln!(
                                    self.body,
                                    "  {np} = getelementptr inbounds i8, ptr {dst}, i64 {}",
                                    i as u32 * stride
                                );
                                np
                            };
                            self.store_operand(Some(&p), op)?;
                        }
                    }
                    TyKind::Tuple(_) | TyKind::Struct(_) => {
                        for (i, op) in ops.iter().enumerate() {
                            let off = dst_layout.fields[i];
                            let p = if off == 0 {
                                dst.clone()
                            } else {
                                let np = self.fresh();
                                let _ = writeln!(
                                    self.body,
                                    "  {np} = getelementptr inbounds i8, ptr {dst}, i64 {off}"
                                );
                                np
                            };
                            self.store_operand(Some(&p), op)?;
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
                        let p = if i == 0 {
                            dst.clone()
                        } else {
                            let np = self.fresh();
                            let _ = writeln!(
                                self.body,
                                "  {np} = getelementptr inbounds i8, ptr {dst}, i64 {}",
                                i * u64::from(el.size)
                            );
                            np
                        };
                        self.emit_memcpy(&p, &src, el.size);
                    }
                } else {
                    let idx_slot = self.alloca_bytes(8, 8);
                    let _ = writeln!(self.body, "  store i64 0, ptr {idx_slot}, align 8");
                    let cond_lbl = self.fresh_lbl("rep.cond");
                    let body_lbl = self.fresh_lbl("rep.body");
                    let done_lbl = self.fresh_lbl("rep.done");
                    let _ = writeln!(self.body, "  br label %{cond_lbl}");
                    let _ = writeln!(self.body, "{cond_lbl}:");
                    let idx = self.fresh();
                    let cmp = self.fresh();
                    let _ = writeln!(self.body, "  {idx} = load i64, ptr {idx_slot}, align 8");
                    let _ = writeln!(self.body, "  {cmp} = icmp ult i64 {idx}, {count}");
                    let _ = writeln!(
                        self.body,
                        "  br i1 {cmp}, label %{body_lbl}, label %{done_lbl}"
                    );
                    let _ = writeln!(self.body, "{body_lbl}:");
                    let off = self.fresh();
                    let ptr = self.fresh();
                    let _ = writeln!(self.body, "  {off} = mul i64 {idx}, {}", el.size);
                    let _ = writeln!(
                        self.body,
                        "  {ptr} = getelementptr inbounds i8, ptr {dst}, i64 {off}"
                    );
                    self.emit_memcpy(&ptr, &src, el.size);
                    let next = self.fresh();
                    let _ = writeln!(self.body, "  {next} = add i64 {idx}, 1");
                    let _ = writeln!(self.body, "  store i64 {next}, ptr {idx_slot}, align 8");
                    let _ = writeln!(self.body, "  br label %{cond_lbl}");
                    let _ = writeln!(self.body, "{done_lbl}:");
                }
                Ok(())
            }
            Rvalue::Ref { place, .. } => {
                let (target_addr, _) = self.place_addr(place)?;
                if let Some(dst) = dst_addr {
                    let p = target_addr.unwrap_or_else(|| "null".into());
                    let _ = writeln!(self.body, "  store ptr {p}, ptr {dst}, align 8");
                }
                Ok(())
            }
            Rvalue::Convert(target, op) => self.emit_convert(dst_addr.as_deref(), *target, op),
            Rvalue::TryConvert(target, op) => {
                self.emit_try_convert(dst_addr.as_deref(), dst_ty, *target, op)
            }
            Rvalue::Math(math, ops) => self.emit_math(dst_addr.as_deref(), dst_ty, *math, ops),
            Rvalue::FloatConst(c) => {
                let val = match c {
                    FloatConst::Pi => std::f64::consts::PI,
                    FloatConst::Tau => std::f64::consts::TAU,
                    FloatConst::E => std::f64::consts::E,
                };
                if let Some(dst) = dst_addr {
                    let _ = writeln!(
                        self.body,
                        "  store double {}, ptr {dst}, align 8",
                        llvm_f64(val)
                    );
                }
                Ok(())
            }
            Rvalue::Amplitudes(reg) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (reg_addr, reg_ty) = self.operand_addr(reg)?;
                let mut ptr = reg_addr.expect("register operand");
                let mut cur = reg_ty;
                while let TyKind::Ref { inner, .. } = self.program.types.kind(cur) {
                    let np = self.fresh();
                    let _ = writeln!(self.body, "  {np} = load ptr, ptr {ptr}, align 8");
                    ptr = np;
                    cur = *inner;
                }
                let offsets = self.layouts.qubit_offsets(cur);
                let n_q = offsets.len();
                let stride = size_of::<usize>() as u32;
                let qbuf = if n_q == 0 {
                    "null".into()
                } else {
                    let b = self.alloca_bytes(stride * n_q as u32, stride);
                    for (i, off) in offsets.into_iter().enumerate() {
                        let qp = if off == 0 {
                            ptr.clone()
                        } else {
                            let np = self.fresh();
                            let _ = writeln!(
                                self.body,
                                "  {np} = getelementptr inbounds i8, ptr {ptr}, i64 {off}"
                            );
                            np
                        };
                        let q = self.fresh();
                        let _ = writeln!(self.body, "  {q} = load ptr, ptr {qp}, align 8");
                        let bp = if i == 0 {
                            b.clone()
                        } else {
                            let np = self.fresh();
                            let _ = writeln!(
                                self.body,
                                "  {np} = getelementptr inbounds i8, ptr {b}, i64 {}",
                                i as u32 * stride
                            );
                            np
                        };
                        let _ = writeln!(self.body, "  store ptr {q}, ptr {bp}, align {stride}");
                    }
                    b
                };
                self.declare(
                    "noev_rt_amplitudes",
                    "declare i8 @noev_rt_amplitudes(ptr, ptr, i64, ptr, i64)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_amplitudes(ptr %rt, ptr {qbuf}, i64 {n_q}, ptr {dst}, i64 {})",
                    1usize << n_q
                );
                self.check_ok(&ok);
                Ok(())
            }
            Rvalue::LabelBit(op) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (src, lty) = self.operand_addr(op)?;
                if lty == Ty::BIT {
                    let b = self.eval_scalar(op)?;
                    let _ = writeln!(self.body, "  store i8 {b}, ptr {dst}, align 1");
                } else {
                    let tag = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {tag} = load i32, ptr {}, align 4",
                        src.expect("State")
                    );
                    self.declare(
                        "noev_rt_label_bit_state",
                        "declare i8 @noev_rt_label_bit_state(i32)",
                    );
                    let bit = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {bit} = call i8 @noev_rt_label_bit_state(i32 {tag})"
                    );
                    let _ = writeln!(self.body, "  store i8 {bit}, ptr {dst}, align 1");
                }
                Ok(())
            }
            Rvalue::BigInt(text) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let sym = self.globals.intern_bytes(text.as_bytes());
                self.declare(
                    "noev_rt_int_parse",
                    "declare i8 @noev_rt_int_parse(ptr, ptr, i64, ptr)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_int_parse(ptr %rt, ptr {sym}, i64 {}, ptr {dst})",
                    text.len()
                );
                self.check_ok(&ok);
                Ok(())
            }
            Rvalue::Pauli(p, idx) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let idx_val = self.eval_scalar(idx)?;
                let code = match p {
                    Pauli::X => 0u8,
                    Pauli::Y => 1,
                    Pauli::Z => 2,
                };
                self.declare(
                    "noev_rt_obs_pauli",
                    "declare i8 @noev_rt_obs_pauli(ptr, i8, i64, ptr)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_obs_pauli(ptr %rt, i8 {code}, i64 {idx_val}, ptr {dst})"
                );
                self.check_ok(&ok);
                Ok(())
            }
            Rvalue::Identity => {
                let Some(dst) = dst_addr else { return Ok(()) };
                self.declare(
                    "noev_rt_obs_identity",
                    "declare i64 @noev_rt_obs_identity(ptr)",
                );
                let h = self.fresh();
                let _ = writeln!(self.body, "  {h} = call i64 @noev_rt_obs_identity(ptr %rt)");
                let _ = writeln!(self.body, "  store i64 {h}, ptr {dst}, align 8");
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
                self.declare(
                    "noev_rt_obs_projector",
                    "declare i8 @noev_rt_obs_projector(ptr, i8, i64, i8, ptr)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_obs_projector(ptr %rt, i8 {}, i64 {idx_val}, i8 {bit_val}, ptr {dst})",
                    u8::from(*x_basis)
                );
                self.check_ok(&ok);
                Ok(())
            }
            Rvalue::Format(parts) => self.emit_format(dst_addr.as_deref(), parts),
            Rvalue::Dist(method, ops, elem) => {
                self.emit_dist(dst_addr.as_deref(), *method, ops, *elem)
            }
            Rvalue::Variant(tag, ops) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let _ = writeln!(self.body, "  store i32 {tag}, ptr {dst}, align 4");
                let v_offsets = &dst_layout.enum_variants[*tag as usize];
                for (i, op) in ops.iter().enumerate() {
                    let off = v_offsets[i];
                    let p = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {p} = getelementptr inbounds i8, ptr {dst}, i64 {off}"
                    );
                    self.store_operand(Some(&p), op)?;
                }
                Ok(())
            }
            Rvalue::Tag(op) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let (src, _) = self.operand_addr(op)?;
                let t32 = self.fresh();
                let t64 = self.fresh();
                let tagged = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {t32} = load i32, ptr {}, align 4",
                    src.expect("enum value")
                );
                let _ = writeln!(self.body, "  {t64} = zext i32 {t32} to i64");
                let _ = writeln!(self.body, "  {tagged} = shl i64 {t64}, 1");
                let _ = writeln!(self.body, "  store i64 {tagged}, ptr {dst}, align 8");
                Ok(())
            }
            Rvalue::FnPtr(fid) => {
                let Some(dst) = dst_addr else { return Ok(()) };
                let target = &self.program.functions[fid.index()];
                let _ = writeln!(
                    self.body,
                    "  store ptr @{}, ptr {dst}, align 8",
                    target.name
                );
                Ok(())
            }
        }
    }

    fn emit_unary(
        &mut self,
        dst_addr: Option<&str>,
        dst_ty: Ty,
        op: UnOp,
        x: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        match self.program.types.kind(dst_ty) {
            TyKind::Int => {
                let v = self.eval_scalar(x)?;
                let code = match op {
                    UnOp::Neg => 0u8,
                    UnOp::BitNot | UnOp::Not => 1,
                };
                self.declare(
                    "noev_rt_int_unary",
                    "declare i64 @noev_rt_int_unary(ptr, i8, i64)",
                );
                let res = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {res} = call i64 @noev_rt_int_unary(ptr %rt, i8 {code}, i64 {v})"
                );
                let _ = writeln!(self.body, "  store i64 {res}, ptr {dst}, align 8");
            }
            TyKind::Float | TyKind::F32 => {
                let (fty, align) = if dst_ty == Ty::F32 {
                    ("float", 4)
                } else {
                    ("double", 8)
                };
                let v = self.eval_scalar(x)?;
                let res = self.fresh();
                let _ = writeln!(self.body, "  {res} = fneg {fty} {v}");
                let _ = writeln!(self.body, "  store {fty} {res}, ptr {dst}, align {align}");
            }
            TyKind::Complex => {
                let (src, _) = self.operand_addr(x)?;
                let src = src.expect("Complex");
                let s1 = self.fresh();
                let d1 = self.fresh();
                let re = self.fresh();
                let im = self.fresh();
                let nre = self.fresh();
                let nim = self.fresh();
                let _ = writeln!(self.body, "  {re} = load double, ptr {src}, align 8");
                let _ = writeln!(
                    self.body,
                    "  {s1} = getelementptr inbounds i8, ptr {src}, i64 8"
                );
                let _ = writeln!(self.body, "  {im} = load double, ptr {s1}, align 8");
                let _ = writeln!(self.body, "  {nre} = fneg double {re}");
                let _ = writeln!(self.body, "  {nim} = fneg double {im}");
                let _ = writeln!(self.body, "  store double {nre}, ptr {dst}, align 8");
                let _ = writeln!(
                    self.body,
                    "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 8"
                );
                let _ = writeln!(self.body, "  store double {nim}, ptr {d1}, align 8");
            }
            TyKind::C32 => {
                let (src, _) = self.operand_addr(x)?;
                let src = src.expect("c32");
                let s1 = self.fresh();
                let d1 = self.fresh();
                let re = self.fresh();
                let im = self.fresh();
                let nre = self.fresh();
                let nim = self.fresh();
                let _ = writeln!(self.body, "  {re} = load float, ptr {src}, align 4");
                let _ = writeln!(
                    self.body,
                    "  {s1} = getelementptr inbounds i8, ptr {src}, i64 4"
                );
                let _ = writeln!(self.body, "  {im} = load float, ptr {s1}, align 4");
                let _ = writeln!(self.body, "  {nre} = fneg float {re}");
                let _ = writeln!(self.body, "  {nim} = fneg float {im}");
                let _ = writeln!(self.body, "  store float {nre}, ptr {dst}, align 4");
                let _ = writeln!(
                    self.body,
                    "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 4"
                );
                let _ = writeln!(self.body, "  store float {nim}, ptr {d1}, align 4");
            }
            TyKind::Bool | TyKind::Bit => {
                let v = self.eval_scalar(x)?;
                let res = self.fresh();
                let _ = writeln!(self.body, "  {res} = xor i8 {v}, 1");
                let _ = writeln!(self.body, "  store i8 {res}, ptr {dst}, align 1");
            }
            TyKind::Observable => {
                let v = self.eval_scalar(x)?;
                self.declare("noev_rt_obs_neg", "declare i64 @noev_rt_obs_neg(ptr, i64)");
                let res = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {res} = call i64 @noev_rt_obs_neg(ptr %rt, i64 {v})"
                );
                let _ = writeln!(self.body, "  store i64 {res}, ptr {dst}, align 8");
            }
            _ if dst_ty.width().is_some() => {
                let w = dst_ty.width().expect("sized");
                let ity = match w.bits {
                    8 => "i8",
                    16 => "i16",
                    32 => "i32",
                    _ => "i64",
                };
                let align = w.bits / 8;
                match op {
                    UnOp::BitNot => {
                        let v = self.eval_scalar(x)?;
                        let res = self.fresh();
                        let _ = writeln!(self.body, "  {res} = xor {ity} {v}, -1");
                        let _ =
                            writeln!(self.body, "  store {ity} {res}, ptr {dst}, align {align}");
                    }
                    UnOp::Neg => {
                        if let Operand::Const(Const::Sized(cv, cw)) = *x
                            && let Some(neg) = cv.checked_neg()
                            && cw.holds(neg)
                        {
                            let lit = match cw.bits {
                                8 => format!("{}", neg as u8 as i8),
                                16 => format!("{}", neg as u16 as i16),
                                32 => format!("{}", neg as u32 as i32),
                                _ => format!("{}", neg as u64 as i64),
                            };
                            let _ = writeln!(
                                self.body,
                                "  store {ity} {lit}, ptr {dst}, align {align}"
                            );
                            return Ok(());
                        }
                        let v = self.eval_scalar(x)?;
                        let wide = self.widen_sized_to_i64(&v, w.bits, w.signed);
                        let slot = self.alloca_bytes(8, 8);
                        self.declare(
                            "noev_rt_sized_neg",
                            "declare i8 @noev_rt_sized_neg(ptr, i64, i32, i8, ptr)",
                        );
                        let ok = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {ok} = call i8 @noev_rt_sized_neg(ptr %rt, i64 {wide}, i32 {}, i8 {}, ptr {slot})",
                            w.bits,
                            u8::from(w.signed)
                        );
                        self.check_ok(&ok);
                        let raw = self.fresh();
                        let _ = writeln!(self.body, "  {raw} = load i64, ptr {slot}, align 8");
                        let (narrow, nty) = self.narrow_i64_to_sized(&raw, w.bits);
                        let _ = writeln!(
                            self.body,
                            "  store {nty} {narrow}, ptr {dst}, align {align}"
                        );
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

    fn emit_binary(
        &mut self,
        dst_addr: Option<&str>,
        _dst_ty: Ty,
        op: BinOp,
        a: &Operand,
        b: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let (a_addr, a_ty) = self.operand_addr(a)?;
        let (b_addr, b_ty) = self.operand_addr(b)?;

        if a_ty == Ty::OBSERVABLE || b_ty == Ty::OBSERVABLE {
            let (code, obs_a, obs_b, scale) = if a_ty == Ty::OBSERVABLE && b_ty == Ty::OBSERVABLE {
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                let c = match op {
                    BinOp::Add => 0u8,
                    BinOp::Sub => 1,
                    BinOp::Mul => 2,
                    _ => return Err(CodegenError(format!("unsupported obs op {op:?}"))),
                };
                (c, va, vb, llvm_f64(0.0))
            } else if a_ty == Ty::FLOAT && b_ty == Ty::OBSERVABLE {
                (3u8, self.eval_scalar(b)?, "0".into(), self.eval_scalar(a)?)
            } else {
                (3u8, self.eval_scalar(a)?, "0".into(), self.eval_scalar(b)?)
            };
            self.declare(
                "noev_rt_obs_binary",
                "declare i8 @noev_rt_obs_binary(ptr, i8, i64, i64, double, ptr)",
            );
            let ok = self.fresh();
            let _ = writeln!(
                self.body,
                "  {ok} = call i8 @noev_rt_obs_binary(ptr %rt, i8 {code}, i64 {obs_a}, i64 {obs_b}, double {scale}, ptr {dst})"
            );
            self.check_ok(&ok);
            return Ok(());
        }

        match self.program.types.kind(a_ty).clone() {
            TyKind::Int => {
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                match op {
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cmp1 = if self.opts.runtime_abi == RuntimeAbi::Qir {
                            let cc = match op {
                                BinOp::Eq => "eq",
                                BinOp::NotEq => "ne",
                                BinOp::Lt => "slt",
                                BinOp::Le => "sle",
                                BinOp::Gt => "sgt",
                                BinOp::Ge => "sge",
                                _ => unreachable!(),
                            };
                            let c = self.fresh();
                            let _ = writeln!(self.body, "  {c} = icmp {cc} i64 {va}, {vb}");
                            c
                        } else {
                            self.declare(
                                "noev_rt_int_cmp",
                                "declare i8 @noev_rt_int_cmp(ptr, i64, i64)",
                            );
                            let ord = self.fresh();
                            let _ = writeln!(
                                self.body,
                                "  {ord} = call i8 @noev_rt_int_cmp(ptr %rt, i64 {va}, i64 {vb})"
                            );
                            let cc = match op {
                                BinOp::Eq => "eq",
                                BinOp::NotEq => "ne",
                                BinOp::Lt => "slt",
                                BinOp::Le => "sle",
                                BinOp::Gt => "sgt",
                                BinOp::Ge => "sge",
                                _ => unreachable!(),
                            };
                            let c = self.fresh();
                            let _ = writeln!(self.body, "  {c} = icmp {cc} i8 {ord}, 0");
                            c
                        };
                        let res = self.fresh();
                        let _ = writeln!(self.body, "  {res} = zext i1 {cmp1} to i8");
                        let _ = writeln!(self.body, "  store i8 {res}, ptr {dst}, align 1");
                    }
                    _ => {
                        if self.opts.runtime_abi == RuntimeAbi::Qir
                            && matches!(op, BinOp::Add | BinOp::Sub)
                        {
                            let inst = if op == BinOp::Add { "add" } else { "sub" };
                            let res = self.fresh();
                            let _ = writeln!(self.body, "  {res} = {inst} i64 {va}, {vb}");
                            let _ = writeln!(self.body, "  store i64 {res}, ptr {dst}, align 8");
                            return Ok(());
                        }
                        let code = match op {
                            BinOp::Add => 0u8,
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
                        self.declare(
                            "noev_rt_int_binary",
                            "declare i8 @noev_rt_int_binary(ptr, i8, i64, i64, ptr)",
                        );
                        let ok = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {ok} = call i8 @noev_rt_int_binary(ptr %rt, i8 {code}, i64 {va}, i64 {vb}, ptr {dst})"
                        );
                        self.check_ok(&ok);
                    }
                }
            }
            TyKind::Float | TyKind::F32 => {
                let (fty, align) = if a_ty == Ty::F32 {
                    ("float", 4)
                } else {
                    ("double", 8)
                };
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                        let inst = match op {
                            BinOp::Add => "fadd",
                            BinOp::Sub => "fsub",
                            BinOp::Mul => "fmul",
                            BinOp::Div => "fdiv",
                            BinOp::Rem => "frem",
                            _ => unreachable!(),
                        };
                        let res = self.fresh();
                        let _ = writeln!(self.body, "  {res} = {inst} {fty} {va}, {vb}");
                        let _ =
                            writeln!(self.body, "  store {fty} {res}, ptr {dst}, align {align}");
                    }
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cc = match op {
                            BinOp::Eq => "oeq",
                            BinOp::NotEq => "une",
                            BinOp::Lt => "olt",
                            BinOp::Le => "ole",
                            BinOp::Gt => "ogt",
                            BinOp::Ge => "oge",
                            _ => unreachable!(),
                        };
                        let cmp1 = self.fresh();
                        let res = self.fresh();
                        let _ = writeln!(self.body, "  {cmp1} = fcmp {cc} {fty} {va}, {vb}");
                        let _ = writeln!(self.body, "  {res} = zext i1 {cmp1} to i8");
                        let _ = writeln!(self.body, "  store i8 {res}, ptr {dst}, align 1");
                    }
                    _ => return Err(CodegenError(format!("unsupported float op {op:?}"))),
                }
            }
            TyKind::Complex | TyKind::C32 => {
                let is_c32 = a_ty == Ty::C32;
                let (fty, step, align) = if is_c32 {
                    ("float", 4, 4)
                } else {
                    ("double", 8, 8)
                };
                let pa = a_addr.expect("complex a");
                let pb = b_addr.expect("complex b");
                let pa1 = self.fresh();
                let pb1 = self.fresh();
                let ar = self.fresh();
                let ai = self.fresh();
                let br = self.fresh();
                let bi = self.fresh();
                let _ = writeln!(self.body, "  {ar} = load {fty}, ptr {pa}, align {align}");
                let _ = writeln!(
                    self.body,
                    "  {pa1} = getelementptr inbounds i8, ptr {pa}, i64 {step}"
                );
                let _ = writeln!(self.body, "  {ai} = load {fty}, ptr {pa1}, align {align}");
                let _ = writeln!(self.body, "  {br} = load {fty}, ptr {pb}, align {align}");
                let _ = writeln!(
                    self.body,
                    "  {pb1} = getelementptr inbounds i8, ptr {pb}, i64 {step}"
                );
                let _ = writeln!(self.body, "  {bi} = load {fty}, ptr {pb1}, align {align}");
                match op {
                    BinOp::Add | BinOp::Sub => {
                        let inst = if op == BinOp::Add { "fadd" } else { "fsub" };
                        let rr = self.fresh();
                        let ri = self.fresh();
                        let d1 = self.fresh();
                        let _ = writeln!(self.body, "  {rr} = {inst} {fty} {ar}, {br}");
                        let _ = writeln!(self.body, "  {ri} = {inst} {fty} {ai}, {bi}");
                        let _ = writeln!(self.body, "  store {fty} {rr}, ptr {dst}, align {align}");
                        let _ = writeln!(
                            self.body,
                            "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 {step}"
                        );
                        let _ = writeln!(self.body, "  store {fty} {ri}, ptr {d1}, align {align}");
                    }
                    BinOp::Mul => {
                        let t1 = self.fresh();
                        let t2 = self.fresh();
                        let rr = self.fresh();
                        let t3 = self.fresh();
                        let t4 = self.fresh();
                        let ri = self.fresh();
                        let d1 = self.fresh();
                        let _ = writeln!(self.body, "  {t1} = fmul {fty} {ar}, {br}");
                        let _ = writeln!(self.body, "  {t2} = fmul {fty} {ai}, {bi}");
                        let _ = writeln!(self.body, "  {rr} = fsub {fty} {t1}, {t2}");
                        let _ = writeln!(self.body, "  {t3} = fmul {fty} {ar}, {bi}");
                        let _ = writeln!(self.body, "  {t4} = fmul {fty} {ai}, {br}");
                        let _ = writeln!(self.body, "  {ri} = fadd {fty} {t3}, {t4}");
                        let _ = writeln!(self.body, "  store {fty} {rr}, ptr {dst}, align {align}");
                        let _ = writeln!(
                            self.body,
                            "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 {step}"
                        );
                        let _ = writeln!(self.body, "  store {fty} {ri}, ptr {d1}, align {align}");
                    }
                    BinOp::Div => {
                        if is_c32 {
                            self.declare(
                                "noev_rt_c32_op",
                                "declare void @noev_rt_c32_op(i8, float, float, float, float, ptr)",
                            );
                            let _ = writeln!(
                                self.body,
                                "  call void @noev_rt_c32_op(i8 0, float {ar}, float {ai}, float {br}, float {bi}, ptr {dst})"
                            );
                        } else {
                            self.declare(
                                "noev_rt_complex_op",
                                "declare void @noev_rt_complex_op(i8, double, double, double, double, ptr)",
                            );
                            let _ = writeln!(
                                self.body,
                                "  call void @noev_rt_complex_op(i8 0, double {ar}, double {ai}, double {br}, double {bi}, ptr {dst})"
                            );
                        }
                    }
                    BinOp::Eq | BinOp::NotEq => {
                        let er = self.fresh();
                        let ei = self.fresh();
                        let eq = self.fresh();
                        let mut b8 = self.fresh();
                        let _ = writeln!(self.body, "  {er} = fcmp oeq {fty} {ar}, {br}");
                        let _ = writeln!(self.body, "  {ei} = fcmp oeq {fty} {ai}, {bi}");
                        let _ = writeln!(self.body, "  {eq} = and i1 {er}, {ei}");
                        let _ = writeln!(self.body, "  {b8} = zext i1 {eq} to i8");
                        if op == BinOp::NotEq {
                            let inv = self.fresh();
                            let _ = writeln!(self.body, "  {inv} = xor i8 {b8}, 1");
                            b8 = inv;
                        }
                        let _ = writeln!(self.body, "  store i8 {b8}, ptr {dst}, align 1");
                    }
                    _ => return Err(CodegenError(format!("unsupported complex op {op:?}"))),
                }
            }
            TyKind::Bool | TyKind::Bit => {
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                let res = match op {
                    BinOp::And | BinOp::BitAnd => {
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {r} = and i8 {va}, {vb}");
                        r
                    }
                    BinOp::Or | BinOp::BitOr => {
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {r} = or i8 {va}, {vb}");
                        r
                    }
                    BinOp::BitXor => {
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {r} = xor i8 {va}, {vb}");
                        r
                    }
                    BinOp::Eq | BinOp::NotEq => {
                        let cc = if op == BinOp::Eq { "eq" } else { "ne" };
                        let c = self.fresh();
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {c} = icmp {cc} i8 {va}, {vb}");
                        let _ = writeln!(self.body, "  {r} = zext i1 {c} to i8");
                        r
                    }
                    _ => return Err(CodegenError(format!("unsupported Bool/Bit op {op:?}"))),
                };
                let _ = writeln!(self.body, "  store i8 {res}, ptr {dst}, align 1");
            }
            _ if a_ty.width().is_some() => {
                let w = a_ty.width().expect("sized");
                let ity = match w.bits {
                    8 => "i8",
                    16 => "i16",
                    32 => "i32",
                    _ => "i64",
                };
                let align = w.bits / 8;
                let va = self.eval_scalar(a)?;
                let vb = self.eval_scalar(b)?;
                match op {
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        let cc = match (op, w.signed) {
                            (BinOp::Eq, _) => "eq",
                            (BinOp::NotEq, _) => "ne",
                            (BinOp::Lt, true) => "slt",
                            (BinOp::Le, true) => "sle",
                            (BinOp::Gt, true) => "sgt",
                            (BinOp::Ge, true) => "sge",
                            (BinOp::Lt, false) => "ult",
                            (BinOp::Le, false) => "ule",
                            (BinOp::Gt, false) => "ugt",
                            (BinOp::Ge, false) => "uge",
                            _ => unreachable!(),
                        };
                        let c = self.fresh();
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {c} = icmp {cc} {ity} {va}, {vb}");
                        let _ = writeln!(self.body, "  {r} = zext i1 {c} to i8");
                        let _ = writeln!(self.body, "  store i8 {r}, ptr {dst}, align 1");
                    }
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                        let inst = match op {
                            BinOp::BitAnd => "and",
                            BinOp::BitOr => "or",
                            BinOp::BitXor => "xor",
                            _ => unreachable!(),
                        };
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {r} = {inst} {ity} {va}, {vb}");
                        let _ = writeln!(self.body, "  store {ity} {r}, ptr {dst}, align {align}");
                    }
                    _ => {
                        let code = match op {
                            BinOp::Add => 0u8,
                            BinOp::Sub => 1,
                            BinOp::Mul => 2,
                            BinOp::Div => 3,
                            BinOp::Rem => 4,
                            BinOp::Shl => 8,
                            BinOp::Shr => 9,
                            _ => return Err(CodegenError(format!("unsupported sized op {op:?}"))),
                        };
                        let wa = self.widen_sized_to_i64(&va, w.bits, w.signed);
                        let b_is_int = b_ty == Ty::INT;
                        let wb = if b_is_int {
                            vb
                        } else {
                            self.widen_sized_to_i64(&vb, w.bits, w.signed)
                        };
                        let slot = self.alloca_bytes(8, 8);
                        self.declare(
                            "noev_rt_sized_binary",
                            "declare i8 @noev_rt_sized_binary(ptr, i8, i64, i64, i32, i8, i8, ptr)",
                        );
                        let ok = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {ok} = call i8 @noev_rt_sized_binary(ptr %rt, i8 {code}, i64 {wa}, \
                             i64 {wb}, i32 {}, i8 {}, i8 {}, ptr {slot})",
                            w.bits,
                            u8::from(w.signed),
                            u8::from(b_is_int)
                        );
                        self.check_ok(&ok);
                        let raw = self.fresh();
                        let _ = writeln!(self.body, "  {raw} = load i64, ptr {slot}, align 8");
                        let (narrow, nty) = self.narrow_i64_to_sized(&raw, w.bits);
                        let _ = writeln!(
                            self.body,
                            "  store {nty} {narrow}, ptr {dst}, align {align}"
                        );
                    }
                }
            }
            _ if matches!(op, BinOp::Eq | BinOp::NotEq) => {
                if self.layouts.layout(a_ty).size == 0 {
                    let _ = writeln!(
                        self.body,
                        "  store i8 {}, ptr {dst}, align 1",
                        u8::from(op == BinOp::Eq)
                    );
                } else {
                    let pa = a_addr.expect("lhs");
                    let pb = b_addr.expect("rhs");
                    let desc = self.globals.type_desc(self.program, self.layouts, a_ty);
                    self.declare(
                        "noev_rt_val_eq",
                        "declare i8 @noev_rt_val_eq(ptr, ptr, ptr, ptr)",
                    );
                    let mut eq = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {eq} = call i8 @noev_rt_val_eq(ptr %rt, ptr {pa}, ptr {pb}, ptr {desc})"
                    );
                    if op == BinOp::NotEq {
                        let inv = self.fresh();
                        let _ = writeln!(self.body, "  {inv} = xor i8 {eq}, 1");
                        eq = inv;
                    }
                    let _ = writeln!(self.body, "  store i8 {eq}, ptr {dst}, align 1");
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

    fn convert_to_f64(&mut self, op: &Operand, src_ty: Ty) -> Result<String, CodegenError> {
        match self.program.types.kind(src_ty) {
            TyKind::Float => self.eval_scalar(op),
            TyKind::F32 => {
                let v = self.eval_scalar(op)?;
                let r = self.fresh();
                let _ = writeln!(self.body, "  {r} = fpext float {v} to double");
                Ok(r)
            }
            TyKind::Int => {
                let v = self.eval_scalar(op)?;
                self.declare(
                    "noev_rt_int_to_f64",
                    "declare double @noev_rt_int_to_f64(ptr, i64)",
                );
                let r = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {r} = call double @noev_rt_int_to_f64(ptr %rt, i64 {v})"
                );
                Ok(r)
            }
            _ if src_ty.width().is_some() => {
                let w = src_ty.width().expect("sized");
                let v = self.eval_scalar(op)?;
                let wide = self.widen_sized_to_i64(&v, w.bits, w.signed);
                let inst = if w.signed { "sitofp" } else { "uitofp" };
                let r = self.fresh();
                let _ = writeln!(self.body, "  {r} = {inst} i64 {wide} to double");
                Ok(r)
            }
            _ => Err(CodegenError(format!(
                "cannot convert {:?} to Float",
                src_ty
            ))),
        }
    }

    fn emit_convert(
        &mut self,
        dst_addr: Option<&str>,
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
                let _ = writeln!(self.body, "  store double {f}, ptr {dst}, align 8");
            }
            Ty::F32 => {
                let f = if let Some(w) = src_ty.width() {
                    let v = self.eval_scalar(op)?;
                    let wide = self.widen_sized_to_i64(&v, w.bits, w.signed);
                    let inst = if w.signed { "sitofp" } else { "uitofp" };
                    let r = self.fresh();
                    let _ = writeln!(self.body, "  {r} = {inst} i64 {wide} to float");
                    r
                } else {
                    let f64v = self.convert_to_f64(op, src_ty)?;
                    let r = self.fresh();
                    let _ = writeln!(self.body, "  {r} = fptrunc double {f64v} to float");
                    r
                };
                let _ = writeln!(self.body, "  store float {f}, ptr {dst}, align 4");
            }
            Ty::COMPLEX => {
                if src_ty == Ty::C32 {
                    let s = src_addr.expect("c32");
                    let s1 = self.fresh();
                    let d1 = self.fresh();
                    let r32 = self.fresh();
                    let i32v = self.fresh();
                    let r64 = self.fresh();
                    let i64v = self.fresh();
                    let _ = writeln!(self.body, "  {r32} = load float, ptr {s}, align 4");
                    let _ = writeln!(
                        self.body,
                        "  {s1} = getelementptr inbounds i8, ptr {s}, i64 4"
                    );
                    let _ = writeln!(self.body, "  {i32v} = load float, ptr {s1}, align 4");
                    let _ = writeln!(self.body, "  {r64} = fpext float {r32} to double");
                    let _ = writeln!(self.body, "  {i64v} = fpext float {i32v} to double");
                    let _ = writeln!(self.body, "  store double {r64}, ptr {dst}, align 8");
                    let _ = writeln!(
                        self.body,
                        "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 8"
                    );
                    let _ = writeln!(self.body, "  store double {i64v}, ptr {d1}, align 8");
                } else {
                    let re = self.convert_to_f64(op, src_ty)?;
                    let d1 = self.fresh();
                    let _ = writeln!(self.body, "  store double {re}, ptr {dst}, align 8");
                    let _ = writeln!(
                        self.body,
                        "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 8"
                    );
                    let _ = writeln!(
                        self.body,
                        "  store double {}, ptr {d1}, align 8",
                        llvm_f64(0.0)
                    );
                }
            }
            Ty::C32 => {
                if src_ty == Ty::COMPLEX {
                    let s = src_addr.expect("Complex");
                    let s1 = self.fresh();
                    let d1 = self.fresh();
                    let r64 = self.fresh();
                    let i64v = self.fresh();
                    let r32 = self.fresh();
                    let i32v = self.fresh();
                    let _ = writeln!(self.body, "  {r64} = load double, ptr {s}, align 8");
                    let _ = writeln!(
                        self.body,
                        "  {s1} = getelementptr inbounds i8, ptr {s}, i64 8"
                    );
                    let _ = writeln!(self.body, "  {i64v} = load double, ptr {s1}, align 8");
                    let _ = writeln!(self.body, "  {r32} = fptrunc double {r64} to float");
                    let _ = writeln!(self.body, "  {i32v} = fptrunc double {i64v} to float");
                    let _ = writeln!(self.body, "  store float {r32}, ptr {dst}, align 4");
                    let _ = writeln!(
                        self.body,
                        "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 4"
                    );
                    let _ = writeln!(self.body, "  store float {i32v}, ptr {d1}, align 4");
                } else {
                    let re = if let Some(w) = src_ty.width() {
                        let v = self.eval_scalar(op)?;
                        let wide = self.widen_sized_to_i64(&v, w.bits, w.signed);
                        let inst = if w.signed { "sitofp" } else { "uitofp" };
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {r} = {inst} i64 {wide} to float");
                        r
                    } else {
                        let f64v = self.convert_to_f64(op, src_ty)?;
                        let r = self.fresh();
                        let _ = writeln!(self.body, "  {r} = fptrunc double {f64v} to float");
                        r
                    };
                    let d1 = self.fresh();
                    let _ = writeln!(self.body, "  store float {re}, ptr {dst}, align 4");
                    let _ = writeln!(
                        self.body,
                        "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 4"
                    );
                    let _ = writeln!(
                        self.body,
                        "  store float {}, ptr {d1}, align 4",
                        llvm_f32(0.0)
                    );
                }
            }
            Ty::INT => {
                if matches!(src_ty, Ty::BIT | Ty::BOOL) {
                    let b = self.eval_scalar(op)?;
                    let e = self.fresh();
                    let tagged = self.fresh();
                    let _ = writeln!(self.body, "  {e} = zext i8 {b} to i64");
                    let _ = writeln!(self.body, "  {tagged} = shl i64 {e}, 1");
                    let _ = writeln!(self.body, "  store i64 {tagged}, ptr {dst}, align 8");
                } else if let Some(w) = src_ty.width() {
                    let v = self.eval_scalar(op)?;
                    let wide = self.widen_sized_to_i64(&v, w.bits, w.signed);
                    let name = if w.signed {
                        "noev_rt_int_from_i64"
                    } else {
                        "noev_rt_int_from_u64"
                    };
                    self.declare(name, &format!("declare i64 @{name}(ptr, i64)"));
                    let tagged = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {tagged} = call i64 @{name}(ptr %rt, i64 {wide})"
                    );
                    let _ = writeln!(self.body, "  store i64 {tagged}, ptr {dst}, align 8");
                } else {
                    let f = self.convert_to_f64(op, src_ty)?;
                    self.declare(
                        "noev_rt_int_from_f64",
                        "declare i8 @noev_rt_int_from_f64(ptr, double, ptr)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_int_from_f64(ptr %rt, double {f}, ptr {dst})"
                    );
                    self.check_ok(&ok);
                }
            }
            Ty::BIT | Ty::BOOL => {
                let v = self.eval_scalar(op)?;
                let _ = writeln!(self.body, "  store i8 {v}, ptr {dst}, align 1");
            }
            _ if target.width().is_some() => {
                let dst_w = target.width().expect("sized target");
                let align = dst_w.bits / 8;
                if matches!(src_ty, Ty::BIT | Ty::BOOL) {
                    let b = self.eval_scalar(op)?;
                    let wide = self.fresh();
                    let _ = writeln!(self.body, "  {wide} = zext i8 {b} to i64");
                    let (narrow, nty) = self.narrow_i64_to_sized(&wide, dst_w.bits);
                    let _ = writeln!(
                        self.body,
                        "  store {nty} {narrow}, ptr {dst}, align {align}"
                    );
                } else if matches!(self.program.types.kind(src_ty), TyKind::Array(Ty::BIT, _)) {
                    let src = src_addr.expect("[Bit; N]");
                    self.declare(
                        "noev_rt_sized_from_bits",
                        "declare i64 @noev_rt_sized_from_bits(ptr, i32)",
                    );
                    let raw = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {raw} = call i64 @noev_rt_sized_from_bits(ptr {src}, i32 {})",
                        dst_w.bits
                    );
                    let (narrow, nty) = self.narrow_i64_to_sized(&raw, dst_w.bits);
                    let _ = writeln!(
                        self.body,
                        "  store {nty} {narrow}, ptr {dst}, align {align}"
                    );
                } else {
                    let (src_kind, raw, src_bits, src_signed) = if src_ty == Ty::INT {
                        (0u8, self.eval_scalar(op)?, 64u32, 1u8)
                    } else if let Some(sw) = src_ty.width() {
                        let v = self.eval_scalar(op)?;
                        (
                            1,
                            self.widen_sized_to_i64(&v, sw.bits, sw.signed),
                            sw.bits,
                            u8::from(sw.signed),
                        )
                    } else if src_ty == Ty::FLOAT {
                        let f = self.eval_scalar(op)?;
                        let b = self.fresh();
                        let _ = writeln!(self.body, "  {b} = bitcast double {f} to i64");
                        (2, b, 64, 1)
                    } else {
                        let f = self.eval_scalar(op)?;
                        let b32 = self.fresh();
                        let b64 = self.fresh();
                        let _ = writeln!(self.body, "  {b32} = bitcast float {f} to i32");
                        let _ = writeln!(self.body, "  {b64} = zext i32 {b32} to i64");
                        (3, b64, 32, 1)
                    };
                    let slot = self.alloca_bytes(8, 8);
                    self.declare(
                        "noev_rt_sized_convert",
                        "declare i8 @noev_rt_sized_convert(ptr, i8, i64, i32, i8, i32, i8, ptr)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_sized_convert(ptr %rt, i8 {src_kind}, i64 {raw}, \
                         i32 {src_bits}, i8 {src_signed}, i32 {}, i8 {}, ptr {slot})",
                        dst_w.bits,
                        u8::from(dst_w.signed)
                    );
                    self.check_ok(&ok);
                    let res = self.fresh();
                    let _ = writeln!(self.body, "  {res} = load i64, ptr {slot}, align 8");
                    let (narrow, nty) = self.narrow_i64_to_sized(&res, dst_w.bits);
                    let _ = writeln!(
                        self.body,
                        "  store {nty} {narrow}, ptr {dst}, align {align}"
                    );
                }
            }
            _ => self.store_operand(Some(dst), op)?,
        }
        Ok(())
    }

    fn emit_try_convert(
        &mut self,
        dst_addr: Option<&str>,
        result_ty: Ty,
        target: Ty,
        op: &Operand,
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let dst_w = target.width().expect("try_from target");
        let (_, src_ty) = self.operand_addr(op)?;
        let (src_kind, raw, src_bits, src_signed) = if src_ty == Ty::INT {
            (0u8, self.eval_scalar(op)?, 64u32, 1u8)
        } else {
            let sw = src_ty.width().expect("try_from source");
            let v = self.eval_scalar(op)?;
            (
                1,
                self.widen_sized_to_i64(&v, sw.bits, sw.signed),
                sw.bits,
                u8::from(sw.signed),
            )
        };
        let desc = self
            .globals
            .type_desc(self.program, self.layouts, result_ty);
        self.declare(
            "noev_rt_sized_try_from",
            "declare i8 @noev_rt_sized_try_from(ptr, i8, i64, i32, i8, i32, i8, ptr, ptr)",
        );
        let ok = self.fresh();
        let _ = writeln!(
            self.body,
            "  {ok} = call i8 @noev_rt_sized_try_from(ptr %rt, i8 {src_kind}, i64 {raw}, \
             i32 {src_bits}, i8 {src_signed}, i32 {}, i8 {}, ptr {dst}, ptr {desc})",
            dst_w.bits,
            u8::from(dst_w.signed)
        );
        self.check_ok(&ok);
        Ok(())
    }

    fn emit_math(
        &mut self,
        dst_addr: Option<&str>,
        dst_ty: Ty,
        math: Math,
        ops: &[Operand],
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        let (a0_addr, ty0) = self.operand_addr(&ops[0])?;

        if matches!(math, Math::Fft | Math::Ifft) {
            let (elem, len) = match self.program.types.kind(ty0) {
                &TyKind::Array(e, n) => (e, n),
                _ => return Err(CodegenError("fft requires an array".into())),
            };
            let src = a0_addr.expect("fft input");
            self.declare(
                "noev_rt_fft",
                "declare void @noev_rt_fft(ptr, ptr, i64, i8, i8)",
            );
            let _ = writeln!(
                self.body,
                "  call void @noev_rt_fft(ptr {src}, ptr {dst}, i64 {len}, i8 {}, i8 {})",
                u8::from(math == Math::Ifft),
                u8::from(elem == Ty::C32)
            );
            return Ok(());
        }

        if math == Math::Polar {
            let r = self.eval_scalar(&ops[0])?;
            let th = self.eval_scalar(&ops[1])?;
            if dst_ty == Ty::C32 {
                self.declare(
                    "noev_rt_c32_op",
                    "declare void @noev_rt_c32_op(i8, float, float, float, float, ptr)",
                );
                let _ = writeln!(
                    self.body,
                    "  call void @noev_rt_c32_op(i8 3, float {r}, float {th}, float {}, float {}, ptr {dst})",
                    llvm_f32(0.0),
                    llvm_f32(0.0)
                );
            } else {
                self.declare(
                    "noev_rt_complex_op",
                    "declare void @noev_rt_complex_op(i8, double, double, double, double, ptr)",
                );
                let _ = writeln!(
                    self.body,
                    "  call void @noev_rt_complex_op(i8 3, double {r}, double {th}, double {}, double {}, ptr {dst})",
                    llvm_f64(0.0),
                    llvm_f64(0.0)
                );
            }
            return Ok(());
        }

        if ty0 == Ty::COMPLEX || ty0 == Ty::C32 {
            let is_c32 = ty0 == Ty::C32;
            let (fty, step, align) = if is_c32 {
                ("float", 4, 4)
            } else {
                ("double", 8, 8)
            };
            let src = a0_addr.expect("complex");
            let s1 = self.fresh();
            let re = self.fresh();
            let im = self.fresh();
            let _ = writeln!(self.body, "  {re} = load {fty}, ptr {src}, align {align}");
            let _ = writeln!(
                self.body,
                "  {s1} = getelementptr inbounds i8, ptr {src}, i64 {step}"
            );
            let _ = writeln!(self.body, "  {im} = load {fty}, ptr {s1}, align {align}");
            match math {
                Math::Re => {
                    let _ = writeln!(self.body, "  store {fty} {re}, ptr {dst}, align {align}");
                }
                Math::Im => {
                    let _ = writeln!(self.body, "  store {fty} {im}, ptr {dst}, align {align}");
                }
                Math::Conj => {
                    let nim = self.fresh();
                    let d1 = self.fresh();
                    let _ = writeln!(self.body, "  {nim} = fneg {fty} {im}");
                    let _ = writeln!(self.body, "  store {fty} {re}, ptr {dst}, align {align}");
                    let _ = writeln!(
                        self.body,
                        "  {d1} = getelementptr inbounds i8, ptr {dst}, i64 {step}"
                    );
                    let _ = writeln!(self.body, "  store {fty} {nim}, ptr {d1}, align {align}");
                }
                Math::Abs | Math::Arg => {
                    let code = if math == Math::Abs { 0u8 } else { 1 };
                    let (re64, im64) = if is_c32 {
                        let r = self.fresh();
                        let i = self.fresh();
                        let _ = writeln!(self.body, "  {r} = fpext float {re} to double");
                        let _ = writeln!(self.body, "  {i} = fpext float {im} to double");
                        (r, i)
                    } else {
                        (re, im)
                    };
                    self.declare(
                        "noev_rt_complex_scalar",
                        "declare double @noev_rt_complex_scalar(i8, double, double)",
                    );
                    let res64 = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {res64} = call double @noev_rt_complex_scalar(i8 {code}, double {re64}, double {im64})"
                    );
                    if is_c32 {
                        let r32 = self.fresh();
                        let _ = writeln!(self.body, "  {r32} = fptrunc double {res64} to float");
                        let _ = writeln!(self.body, "  store float {r32}, ptr {dst}, align 4");
                    } else {
                        let _ = writeln!(self.body, "  store double {res64}, ptr {dst}, align 8");
                    }
                }
                Math::Exp | Math::Sqrt => {
                    let code = if math == Math::Exp { 1u8 } else { 2 };
                    if is_c32 {
                        self.declare(
                            "noev_rt_c32_op",
                            "declare void @noev_rt_c32_op(i8, float, float, float, float, ptr)",
                        );
                        let _ = writeln!(
                            self.body,
                            "  call void @noev_rt_c32_op(i8 {code}, float {re}, float {im}, float {}, float {}, ptr {dst})",
                            llvm_f32(0.0),
                            llvm_f32(0.0)
                        );
                    } else {
                        self.declare(
                            "noev_rt_complex_op",
                            "declare void @noev_rt_complex_op(i8, double, double, double, double, ptr)",
                        );
                        let _ = writeln!(
                            self.body,
                            "  call void @noev_rt_complex_op(i8 {code}, double {re}, double {im}, double {}, double {}, ptr {dst})",
                            llvm_f64(0.0),
                            llvm_f64(0.0)
                        );
                    }
                }
                _ => return Err(CodegenError(format!("unsupported complex math {math:?}"))),
            }
            return Ok(());
        }

        if let Some(w) = ty0.width() {
            let ity = match w.bits {
                8 => "i8",
                16 => "i16",
                32 => "i32",
                _ => "i64",
            };
            let align = w.bits / 8;
            let va = self.eval_scalar(&ops[0])?;
            match math {
                Math::Bits => {
                    let wide = self.widen_sized_to_i64(&va, w.bits, false);
                    self.declare(
                        "noev_rt_sized_bits",
                        "declare void @noev_rt_sized_bits(i64, i32, ptr)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @noev_rt_sized_bits(i64 {wide}, i32 {}, ptr {dst})",
                        w.bits
                    );
                }
                Math::Abs => {
                    if !w.signed {
                        let _ = writeln!(self.body, "  store {ity} {va}, ptr {dst}, align {align}");
                    } else {
                        let is_neg = self.fresh();
                        let neg_lbl = self.fresh_lbl("abs.neg");
                        let pos_lbl = self.fresh_lbl("abs.pos");
                        let done_lbl = self.fresh_lbl("abs.done");
                        let _ = writeln!(self.body, "  {is_neg} = icmp slt {ity} {va}, 0");
                        let _ = writeln!(
                            self.body,
                            "  br i1 {is_neg}, label %{neg_lbl}, label %{pos_lbl}"
                        );
                        let _ = writeln!(self.body, "{neg_lbl}:");
                        let wide = self.widen_sized_to_i64(&va, w.bits, true);
                        let slot = self.alloca_bytes(8, 8);
                        self.declare(
                            "noev_rt_sized_neg",
                            "declare i8 @noev_rt_sized_neg(ptr, i64, i32, i8, ptr)",
                        );
                        let ok = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {ok} = call i8 @noev_rt_sized_neg(ptr %rt, i64 {wide}, i32 {}, i8 1, ptr {slot})",
                            w.bits
                        );
                        self.check_ok(&ok);
                        let raw = self.fresh();
                        let _ = writeln!(self.body, "  {raw} = load i64, ptr {slot}, align 8");
                        let (narrow, nty) = self.narrow_i64_to_sized(&raw, w.bits);
                        let _ = writeln!(
                            self.body,
                            "  store {nty} {narrow}, ptr {dst}, align {align}"
                        );
                        let _ = writeln!(self.body, "  br label %{done_lbl}");
                        let _ = writeln!(self.body, "{pos_lbl}:");
                        let _ = writeln!(self.body, "  store {ity} {va}, ptr {dst}, align {align}");
                        let _ = writeln!(self.body, "  br label %{done_lbl}");
                        let _ = writeln!(self.body, "{done_lbl}:");
                    }
                }
                Math::Min | Math::Max => {
                    let vb = self.eval_scalar(&ops[1])?;
                    let cc = match (math, w.signed) {
                        (Math::Min, true) => "slt",
                        (Math::Min, false) => "ult",
                        (Math::Max, true) => "sgt",
                        (Math::Max, false) => "ugt",
                        _ => unreachable!(),
                    };
                    let cmp = self.fresh();
                    let res = self.fresh();
                    let _ = writeln!(self.body, "  {cmp} = icmp {cc} {ity} {va}, {vb}");
                    let _ = writeln!(
                        self.body,
                        "  {res} = select i1 {cmp}, {ity} {va}, {ity} {vb}"
                    );
                    let _ = writeln!(self.body, "  store {ity} {res}, ptr {dst}, align {align}");
                }
                _ => return Err(CodegenError(format!("unsupported sized math {math:?}"))),
            }
            return Ok(());
        }

        if ty0 == Ty::INT {
            let va = self.eval_scalar(&ops[0])?;
            match math {
                Math::Abs => {
                    self.declare(
                        "noev_rt_int_unary",
                        "declare i64 @noev_rt_int_unary(ptr, i8, i64)",
                    );
                    let res = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {res} = call i64 @noev_rt_int_unary(ptr %rt, i8 2, i64 {va})"
                    );
                    let _ = writeln!(self.body, "  store i64 {res}, ptr {dst}, align 8");
                }
                Math::Min | Math::Max => {
                    let vb = self.eval_scalar(&ops[1])?;
                    let code = if math == Math::Min { 11u8 } else { 12 };
                    self.declare(
                        "noev_rt_int_binary",
                        "declare i8 @noev_rt_int_binary(ptr, i8, i64, i64, ptr)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_int_binary(ptr %rt, i8 {code}, i64 {va}, i64 {vb}, ptr {dst})"
                    );
                    self.check_ok(&ok);
                }
                _ => return Err(CodegenError(format!("unsupported Int math {math:?}"))),
            }
            return Ok(());
        }

        // Float / f32 math.
        let op_code = match math {
            Math::Sin => 0u8,
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
            let r = self.fresh();
            let _ = writeln!(self.body, "  {r} = fpext float {va_raw} to double");
            r
        } else {
            va_raw
        };
        let vb64 = if ops.len() > 1 {
            let v = self.eval_scalar(&ops[1])?;
            if is_f32 {
                let r = self.fresh();
                let _ = writeln!(self.body, "  {r} = fpext float {v} to double");
                r
            } else {
                v
            }
        } else {
            llvm_f64(0.0)
        };
        self.declare(
            "noev_rt_f64_math",
            "declare double @noev_rt_f64_math(i8, double, double)",
        );
        let res64 = self.fresh();
        let _ = writeln!(
            self.body,
            "  {res64} = call double @noev_rt_f64_math(i8 {op_code}, double {va64}, double {vb64})"
        );
        if dst_ty == Ty::INT {
            self.declare(
                "noev_rt_int_from_f64",
                "declare i8 @noev_rt_int_from_f64(ptr, double, ptr)",
            );
            let ok = self.fresh();
            let _ = writeln!(
                self.body,
                "  {ok} = call i8 @noev_rt_int_from_f64(ptr %rt, double {res64}, ptr {dst})"
            );
            self.check_ok(&ok);
        } else if dst_ty == Ty::F32 {
            let r32 = self.fresh();
            let _ = writeln!(self.body, "  {r32} = fptrunc double {res64} to float");
            let _ = writeln!(self.body, "  store float {r32}, ptr {dst}, align 4");
        } else {
            let _ = writeln!(self.body, "  store double {res64}, ptr {dst}, align 8");
        }
        Ok(())
    }

    fn emit_format(
        &mut self,
        dst_addr: Option<&str>,
        parts: &[FormatPart],
    ) -> Result<(), CodegenError> {
        let Some(dst) = dst_addr else { return Ok(()) };
        if let [FormatPart::Text(s)] = parts {
            let sym = self.globals.intern_bytes(s.as_bytes());
            self.declare(
                "noev_rt_str_from_bytes",
                "declare i64 @noev_rt_str_from_bytes(ptr, ptr, i64)",
            );
            let h = self.fresh();
            let _ = writeln!(
                self.body,
                "  {h} = call i64 @noev_rt_str_from_bytes(ptr %rt, ptr {sym}, i64 {})",
                s.len()
            );
            let _ = writeln!(self.body, "  store i64 {h}, ptr {dst}, align 8");
            return Ok(());
        }

        let n = parts.len();
        let stride = size_of::<NoevFormatPart>() as u32;
        let buf = if n == 0 {
            "null".to_string()
        } else {
            let b = self.alloca_bytes(stride * n as u32, align_of::<NoevFormatPart>() as u32);
            for (i, part) in parts.iter().enumerate() {
                let base = i * stride as usize;
                let store_field = |body: &mut String,
                                   next_ssa: &mut usize,
                                   off: usize,
                                   ty: &str,
                                   val: &str,
                                   align: u32| {
                    let p = format!("%v{}", *next_ssa);
                    *next_ssa += 1;
                    let _ = writeln!(
                        body,
                        "  {p} = getelementptr inbounds i8, ptr {b}, i64 {}",
                        base + off
                    );
                    let _ = writeln!(body, "  store {ty} {val}, ptr {p}, align {align}");
                };
                match part {
                    FormatPart::Text(s) => {
                        let sym = self.globals.intern_bytes(s.as_bytes());
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, is_value),
                            "i8",
                            "0",
                            1,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, has_spec),
                            "i8",
                            "0",
                            1,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, ptr),
                            "ptr",
                            &sym,
                            8,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, len),
                            "i64",
                            &s.len().to_string(),
                            8,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, ty),
                            "ptr",
                            "null",
                            8,
                        );
                    }
                    FormatPart::Value(op, ty, spec) => {
                        let (vaddr, _) = self.operand_addr(op)?;
                        let vptr = vaddr.unwrap_or_else(|| "null".into());
                        let desc = self.globals.type_desc(self.program, self.layouts, *ty);
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, is_value),
                            "i8",
                            "1",
                            1,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, ptr),
                            "ptr",
                            &vptr,
                            8,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, len),
                            "i64",
                            "0",
                            8,
                        );
                        store_field(
                            &mut self.body,
                            &mut self.next_ssa,
                            offset_of!(NoevFormatPart, ty),
                            "ptr",
                            &desc,
                            8,
                        );
                        if let Some(sp) = spec {
                            let align_code = match sp.align {
                                Some(Align::Left) => 1u8,
                                Some(Align::Right) => 2,
                                Some(Align::Center) => 3,
                                None => 0,
                            };
                            let kind_code = match sp.kind {
                                Some(FormatKind::Binary) => 1u8,
                                Some(FormatKind::Octal) => 2,
                                Some(FormatKind::Hex) => 3,
                                Some(FormatKind::HexUpper) => 4,
                                Some(FormatKind::Exponent) => 5,
                                None => 0,
                            };
                            let mut flags = 0u8;
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
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, has_spec),
                                "i8",
                                "1",
                                1,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, align),
                                "i8",
                                &align_code.to_string(),
                                1,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, kind),
                                "i8",
                                &kind_code.to_string(),
                                1,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, flags),
                                "i8",
                                &flags.to_string(),
                                1,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, fill),
                                "i32",
                                &(sp.fill as u32).to_string(),
                                4,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, width),
                                "i32",
                                &sp.width.unwrap_or(0).to_string(),
                                4,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, precision),
                                "i32",
                                &sp.precision.unwrap_or(0).to_string(),
                                4,
                            );
                        } else {
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, has_spec),
                                "i8",
                                "0",
                                1,
                            );
                            store_field(
                                &mut self.body,
                                &mut self.next_ssa,
                                offset_of!(NoevFormatPart, fill),
                                "i32",
                                "0",
                                4,
                            );
                        }
                    }
                }
            }
            b
        };
        self.declare(
            "noev_rt_format",
            "declare i64 @noev_rt_format(ptr, ptr, i64)",
        );
        let h = self.fresh();
        let _ = writeln!(
            self.body,
            "  {h} = call i64 @noev_rt_format(ptr %rt, ptr {buf}, i64 {n})"
        );
        let _ = writeln!(self.body, "  store i64 {h}, ptr {dst}, align 8");
        Ok(())
    }

    fn emit_dist(
        &mut self,
        dst_addr: Option<&str>,
        method: DistMethod,
        ops: &[Operand],
        elem: Ty,
    ) -> Result<(), CodegenError> {
        let dist = self.eval_scalar(&ops[0])?;
        match method {
            DistMethod::Shots => {
                self.declare(
                    "noev_rt_dist_shots",
                    "declare i64 @noev_rt_dist_shots(ptr, i64)",
                );
                let s = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {s} = call i64 @noev_rt_dist_shots(ptr %rt, i64 {dist})"
                );
                if let Some(dst) = dst_addr {
                    let _ = writeln!(self.body, "  store i64 {s}, ptr {dst}, align 8");
                }
            }
            DistMethod::Count => {
                let (vaddr, _) = self.operand_addr(&ops[1])?;
                let vptr = vaddr.unwrap_or_else(|| "null".into());
                let desc = self.globals.type_desc(self.program, self.layouts, elem);
                let dst = dst_addr.expect("Int dest");
                self.declare(
                    "noev_rt_dist_count",
                    "declare i8 @noev_rt_dist_count(ptr, i64, ptr, ptr, ptr)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_dist_count(ptr %rt, i64 {dist}, ptr {vptr}, ptr {desc}, ptr {dst})"
                );
                self.check_ok(&ok);
            }
            DistMethod::Probability => {
                let (vaddr, _) = self.operand_addr(&ops[1])?;
                let vptr = vaddr.unwrap_or_else(|| "null".into());
                let desc = self.globals.type_desc(self.program, self.layouts, elem);
                let dst = dst_addr.expect("Float dest");
                self.declare(
                    "noev_rt_dist_probability",
                    "declare i8 @noev_rt_dist_probability(ptr, i64, ptr, ptr, ptr)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_dist_probability(ptr %rt, i64 {dist}, ptr {vptr}, ptr {desc}, ptr {dst})"
                );
                self.check_ok(&ok);
            }
            DistMethod::MostLikely => {
                let desc = self.globals.type_desc(self.program, self.layouts, elem);
                let out_ptr = match dst_addr {
                    Some(d) => d.to_string(),
                    None => self.alloca_bytes(1, 1),
                };
                self.declare(
                    "noev_rt_dist_most_likely",
                    "declare i8 @noev_rt_dist_most_likely(ptr, i64, ptr, ptr)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_dist_most_likely(ptr %rt, i64 {dist}, ptr {out_ptr}, ptr {desc})"
                );
                self.check_ok(&ok);
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Terminators
    // -------------------------------------------------------------------------

    fn emit_term(&mut self, term: &Terminator) -> Result<(), CodegenError> {
        match term {
            Terminator::Goto(next) => {
                let _ = writeln!(self.body, "  br label %bb{}", next.index());
            }
            Terminator::Branch {
                cond,
                then,
                otherwise,
                ..
            } => {
                let c8 = self.eval_scalar(cond)?;
                let c1 = self.fresh();
                let _ = writeln!(self.body, "  {c1} = icmp ne i8 {c8}, 0");
                let _ = writeln!(
                    self.body,
                    "  br i1 {c1}, label %bb{}, label %bb{}",
                    then.index(),
                    otherwise.index()
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
                if callee.is_c_import() {
                    let param_tys: Vec<Ty> = callee.locals[1..=callee.params]
                        .iter()
                        .map(|l| l.ty)
                        .collect();
                    let ret_ty = callee.locals[0].ty;
                    let callee_sym = format!("@{}", callee.name);
                    self.emit_c_call(&callee_sym, &param_tys, ret_ty, args, dest, *next)?;
                    return Ok(());
                }
                let mut call_args = vec!["ptr %rt".to_string()];
                for (i, arg) in args.iter().enumerate() {
                    let pty = callee.locals[i + 1].ty;
                    let l = self.layouts.layout(pty);
                    if l.size == 0 {
                        continue;
                    }
                    if let Some(sk) = l.scalar {
                        let v = self.eval_scalar(arg)?;
                        call_args.push(format!("{} {v}", scalar_llvm(sk)));
                    } else {
                        let (addr, _) = self.operand_addr(arg)?;
                        call_args.push(format!("ptr {}", addr.expect("aggregate arg")));
                    }
                }
                let ret_layout = self.layouts.layout(callee.locals[0].ty);
                let (dst_addr, _) = self.place_addr(dest)?;
                if ret_layout.size > 0 {
                    let rp = dst_addr
                        .unwrap_or_else(|| self.alloca_bytes(ret_layout.size, ret_layout.align));
                    call_args.push(format!("ptr {rp}"));
                }

                if *adjoint {
                    self.declare("noev_rt_adj_start", "declare void @noev_rt_adj_start(ptr)");
                    self.body
                        .push_str("  call void @noev_rt_adj_start(ptr %rt)\n");
                }
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @__noev_fn_{}({})",
                    func.index(),
                    call_args.join(", ")
                );
                if *adjoint {
                    let cond = self.fresh();
                    let ok_lbl = self.fresh_lbl("adj.ok");
                    let ab_lbl = self.fresh_lbl("adj.abort");
                    let _ = writeln!(self.body, "  {cond} = icmp ne i8 {ok}, 0");
                    let _ = writeln!(
                        self.body,
                        "  br i1 {cond}, label %{ok_lbl}, label %{ab_lbl}"
                    );
                    let _ = writeln!(self.body, "{ab_lbl}:");
                    self.declare("noev_rt_adj_abort", "declare void @noev_rt_adj_abort(ptr)");
                    self.body.push_str(
                        "  call void @noev_rt_adj_abort(ptr %rt)\n  br label %err_exit\n",
                    );
                    let _ = writeln!(self.body, "{ok_lbl}:");
                    self.declare("noev_rt_adj_end", "declare i8 @noev_rt_adj_end(ptr)");
                    let eok = self.fresh();
                    let _ = writeln!(self.body, "  {eok} = call i8 @noev_rt_adj_end(ptr %rt)");
                    self.check_ok(&eok);
                } else {
                    self.check_ok(&ok);
                }
                let _ = writeln!(self.body, "  br label %bb{}", next.index());
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
                let ret_desc =
                    self.globals
                        .type_desc(self.program, self.layouts, callee.locals[0].ty);
                let (targets_ptr, n_targets) =
                    self.collect_qubit_handles(std::slice::from_ref(target))?;
                self.declare(
                    "noev_rt_undo_compiled",
                    "declare i8 @noev_rt_undo_compiled(ptr, ptr, ptr, ptr, ptr, i64)",
                );
                let ok = self.fresh();
                let _ = writeln!(
                    self.body,
                    "  {ok} = call i8 @noev_rt_undo_compiled(ptr %rt, ptr @__noev_thunk_{}, \
                     ptr {args_buf}, ptr {ret_desc}, ptr {targets_ptr}, i64 {n_targets})",
                    func.index()
                );
                self.check_ok(&ok);
                let _ = writeln!(self.body, "  br label %bb{}", next.index());
            }
            Terminator::Assert { cond, next, .. } => {
                let ok_cond = self.eval_scalar(cond)?;
                if self.opts.runtime_abi == RuntimeAbi::Qir {
                    self.check_ok(&ok_cond);
                } else {
                    let msg = b"assertion failed";
                    let sym = self.globals.intern_bytes(msg);
                    self.declare(
                        "noev_rt_assert",
                        "declare i8 @noev_rt_assert(ptr, i8, ptr, i64)",
                    );
                    let ok = self.fresh();
                    let _ = writeln!(
                        self.body,
                        "  {ok} = call i8 @noev_rt_assert(ptr %rt, i8 {ok_cond}, ptr {sym}, i64 {})",
                        msg.len()
                    );
                    self.check_ok(&ok);
                }
                let _ = writeln!(self.body, "  br label %bb{}", next.index());
            }
            Terminator::CallIndirect {
                callee,
                args,
                dest,
                next,
            } => {
                let fptr_ty = operand_ty(self.program, self.func, callee);
                let TyKind::ExternCFn { params, ret } = self.program.types.kind(fptr_ty).clone()
                else {
                    return Err(CodegenError(
                        "indirect call target does not have `extern \"C\" fn` type".into(),
                    ));
                };
                let fp_val = self.eval_scalar(callee)?;
                self.emit_c_call(&fp_val, &params, ret, args, dest, *next)?;
            }
            Terminator::Return => {
                self.emit_return_ok();
            }
            Terminator::Unreachable => {
                if self.opts.runtime_abi == RuntimeAbi::Noev {
                    let msg = format!("reached unreachable code in `{}`", self.func.name);
                    let sym = self.globals.intern_bytes(msg.as_bytes());
                    self.declare(
                        "noev_rt_set_error",
                        "declare void @noev_rt_set_error(ptr, ptr, i64)",
                    );
                    let _ = writeln!(
                        self.body,
                        "  call void @noev_rt_set_error(ptr %rt, ptr {sym}, i64 {})",
                        msg.len()
                    );
                }
                self.body.push_str("  br label %err_exit\n");
            }
        }
        Ok(())
    }

    fn emit_c_call(
        &mut self,
        callee_expr: &str,
        param_tys: &[Ty],
        ret_ty: Ty,
        args: &[Operand],
        dest: &Place,
        next: Block,
    ) -> Result<(), CodegenError> {
        let ret_abi = self.layouts.c_abi_arg(ret_ty);
        let ret_layout = self.layouts.layout(ret_ty);
        let (dst_addr, _) = self.place_addr(dest)?;

        let mut call_args = Vec::new();
        let ret_llvm = llvm_c_ret_ty(&ret_abi);
        if let CAbiArg::Indirect { size, align } = ret_abi {
            let rp = dst_addr
                .clone()
                .unwrap_or_else(|| self.alloca_bytes(ret_layout.size, ret_layout.align));
            call_args.push(format!("ptr sret([{size} x i8]) align {align} {rp}"));
        }

        for (arg, &pty) in args.iter().zip(param_tys.iter()) {
            let l = self.layouts.layout(pty);
            match self.layouts.c_abi_arg(pty) {
                CAbiArg::Void => {}
                CAbiArg::Scalar(sk) => {
                    let v = self.eval_scalar(arg)?;
                    if pty == Ty::INT {
                        let out = self.alloca_bytes(8, 8);
                        self.declare(
                            "noev_rt_int_to_c_i64",
                            "declare i8 @noev_rt_int_to_c_i64(ptr, i64, ptr)",
                        );
                        let ok = self.fresh();
                        let _ = writeln!(
                            self.body,
                            "  {ok} = call i8 @noev_rt_int_to_c_i64(ptr %rt, i64 {v}, ptr {out})"
                        );
                        self.check_ok(&ok);
                        let raw = self.fresh();
                        let _ = writeln!(self.body, "  {raw} = load i64, ptr {out}, align 8");
                        call_args.push(format!("i64 {raw}"));
                    } else {
                        call_args.push(format!("{} {v}", scalar_llvm(sk)));
                    }
                }
                CAbiArg::DirectChunks(chunks) => {
                    let (src_opt, _) = self.operand_addr(arg)?;
                    let src = src_opt.expect("struct arg");
                    let tmp = self.alloca_bytes(l.size.max(16), l.align.max(8));
                    self.emit_memcpy(&tmp, &src, l.size);
                    for (ci, sk) in chunks.into_iter().enumerate() {
                        let p = if ci == 0 {
                            tmp.clone()
                        } else {
                            let gep = self.fresh();
                            let _ = writeln!(
                                self.body,
                                "  {gep} = getelementptr inbounds i8, ptr {tmp}, i64 {}",
                                ci * 8
                            );
                            gep
                        };
                        let v = self.fresh();
                        let lt = scalar_llvm(sk);
                        let _ = writeln!(self.body, "  {v} = load {lt}, ptr {p}, align 1");
                        call_args.push(format!("{lt} {v}"));
                    }
                }
                CAbiArg::Indirect { size, align } => {
                    let (src_opt, _) = self.operand_addr(arg)?;
                    let src = src_opt.expect("struct arg");
                    call_args.push(format!("ptr byval([{size} x i8]) align {align} {src}"));
                }
            }
        }

        if ret_llvm == "void" {
            let _ = writeln!(
                self.body,
                "  call void {callee_expr}({})",
                call_args.join(", ")
            );
        } else {
            let res = self.fresh();
            let _ = writeln!(
                self.body,
                "  {res} = call {ret_llvm} {callee_expr}({})",
                call_args.join(", ")
            );
            if let Some(dst) = dst_addr {
                match ret_abi {
                    CAbiArg::Void | CAbiArg::Indirect { .. } => {}
                    CAbiArg::Scalar(sk) => {
                        let val = if ret_ty == Ty::INT {
                            self.declare(
                                "noev_rt_int_from_c_i64",
                                "declare i64 @noev_rt_int_from_c_i64(ptr, i64)",
                            );
                            let tagged = self.fresh();
                            let _ = writeln!(
                                self.body,
                                "  {tagged} = call i64 @noev_rt_int_from_c_i64(ptr %rt, i64 {res})"
                            );
                            tagged
                        } else {
                            res
                        };
                        let _ = writeln!(
                            self.body,
                            "  store {} {val}, ptr {dst}, align {}",
                            scalar_llvm(sk),
                            ret_layout.align
                        );
                    }
                    CAbiArg::DirectChunks(chunks) => {
                        let tmp =
                            self.alloca_bytes(ret_layout.size.max(16), ret_layout.align.max(8));
                        if chunks.len() == 1 {
                            let _ = writeln!(
                                self.body,
                                "  store {} {res}, ptr {tmp}, align 1",
                                scalar_llvm(chunks[0])
                            );
                        } else {
                            for (ci, sk) in chunks.iter().enumerate() {
                                let part = self.fresh();
                                let _ = writeln!(
                                    self.body,
                                    "  {part} = extractvalue {ret_llvm} {res}, {ci}"
                                );
                                let p = if ci == 0 {
                                    tmp.clone()
                                } else {
                                    let gep = self.fresh();
                                    let _ = writeln!(
                                        self.body,
                                        "  {gep} = getelementptr inbounds i8, ptr {tmp}, i64 {}",
                                        ci * 8
                                    );
                                    gep
                                };
                                let _ = writeln!(
                                    self.body,
                                    "  store {} {part}, ptr {p}, align 1",
                                    scalar_llvm(*sk)
                                );
                            }
                        }
                        self.emit_memcpy(&dst, &tmp, ret_layout.size);
                    }
                }
            }
        }

        let _ = writeln!(self.body, "  br label %bb{}", next.index());
        Ok(())
    }
}

fn llvm_c_ret_ty(ret_abi: &CAbiArg) -> String {
    match ret_abi {
        CAbiArg::Void | CAbiArg::Indirect { .. } => "void".into(),
        CAbiArg::Scalar(sk) => scalar_llvm(*sk).into(),
        CAbiArg::DirectChunks(chunks) => {
            if chunks.len() == 1 {
                scalar_llvm(chunks[0]).into()
            } else {
                let parts: Vec<&str> = chunks.iter().map(|&sk| scalar_llvm(sk)).collect();
                format!("{{ {} }}", parts.join(", "))
            }
        }
    }
}

fn llvm_c_decl(layouts: &mut LayoutCache<'_>, name: &str, param_tys: &[Ty], ret_ty: Ty) -> String {
    let ret_abi = layouts.c_abi_arg(ret_ty);
    let ret_llvm = llvm_c_ret_ty(&ret_abi);
    let mut params = Vec::new();
    if let CAbiArg::Indirect { size, align } = ret_abi {
        params.push(format!("ptr sret([{size} x i8]) align {align}"));
    }
    for &pty in param_tys {
        match layouts.c_abi_arg(pty) {
            CAbiArg::Void => {}
            CAbiArg::Scalar(sk) => params.push(scalar_llvm(sk).into()),
            CAbiArg::DirectChunks(chunks) => {
                for sk in chunks {
                    params.push(scalar_llvm(sk).into());
                }
            }
            CAbiArg::Indirect { size, align } => {
                params.push(format!("ptr byval([{size} x i8]) align {align}"));
            }
        }
    }
    format!("declare {ret_llvm} @{name}({})", params.join(", "))
}

/// Emits the exported C-ABI wrapper `define <c_ret> @<name>(...)` for an `extern "C" fn`
/// (`spec/interop.md` §3).
fn emit_c_export_wrapper(
    out: &mut String,
    layouts: &mut LayoutCache<'_>,
    decls: &mut BTreeMap<String, String>,
    func: &Function,
    func_index: usize,
) {
    decls
        .entry("noev_rt_callback_enter".into())
        .or_insert_with(|| "declare ptr @noev_rt_callback_enter()".into());
    decls
        .entry("noev_rt_callback_exit".into())
        .or_insert_with(|| "declare void @noev_rt_callback_exit(ptr, i8)".into());

    let param_tys: Vec<Ty> = func.locals[1..=func.params].iter().map(|l| l.ty).collect();
    let ret_ty = func.locals[0].ty;
    let ret_abi = layouts.c_abi_arg(ret_ty);
    let ret_layout = layouts.layout(ret_ty);
    let ret_llvm = llvm_c_ret_ty(&ret_abi);

    let mut sig_params = Vec::new();
    let mut p_idx = 0usize;
    let sret_param = if let CAbiArg::Indirect { size, align } = ret_abi {
        sig_params.push(format!("ptr sret([{size} x i8]) align {align} %c_sret"));
        Some("%c_sret".to_string())
    } else {
        None
    };

    let mut param_abis = Vec::with_capacity(param_tys.len());
    for &pty in &param_tys {
        let abi = layouts.c_abi_arg(pty);
        match &abi {
            CAbiArg::Void => {}
            CAbiArg::Scalar(sk) => {
                sig_params.push(format!("{} %cp{p_idx}", scalar_llvm(*sk)));
                p_idx += 1;
            }
            CAbiArg::DirectChunks(chunks) => {
                for &sk in chunks {
                    sig_params.push(format!("{} %cp{p_idx}", scalar_llvm(sk)));
                    p_idx += 1;
                }
            }
            CAbiArg::Indirect { size, align } => {
                sig_params.push(format!("ptr byval([{size} x i8]) align {align} %cp{p_idx}"));
                p_idx += 1;
            }
        }
        param_abis.push((pty, abi));
    }

    let _ = writeln!(
        out,
        "\ndefine {ret_llvm} @{}({}) {{\nentry:",
        func.name,
        sig_params.join(", ")
    );
    out.push_str("  %rt = call ptr @noev_rt_callback_enter()\n");

    let mut inner_args = vec!["ptr %rt".to_string()];
    let mut c_arg = 0usize;
    let mut tmp_idx = 0usize;
    for (pty, abi) in param_abis {
        let l = layouts.layout(pty);
        match abi {
            CAbiArg::Void => {}
            CAbiArg::Scalar(sk) => {
                let name = format!("%cp{c_arg}");
                c_arg += 1;
                if pty == Ty::INT {
                    decls
                        .entry("noev_rt_int_from_c_i64".into())
                        .or_insert_with(|| "declare i64 @noev_rt_int_from_c_i64(ptr, i64)".into());
                    let t = format!("%t{tmp_idx}");
                    tmp_idx += 1;
                    let _ = writeln!(
                        out,
                        "  {t} = call i64 @noev_rt_int_from_c_i64(ptr %rt, i64 {name})"
                    );
                    inner_args.push(format!("i64 {t}"));
                } else {
                    inner_args.push(format!("{} {name}", scalar_llvm(sk)));
                }
            }
            CAbiArg::DirectChunks(chunks) => {
                let slot = format!("%slot{tmp_idx}");
                tmp_idx += 1;
                let sz = l.size.max(16);
                let al = l.align.max(8);
                let _ = writeln!(out, "  {slot} = alloca [{sz} x i8], align {al}");
                for (ci, sk) in chunks.into_iter().enumerate() {
                    let pname = format!("%cp{c_arg}");
                    c_arg += 1;
                    let p = if ci == 0 {
                        slot.clone()
                    } else {
                        let g = format!("%gep{tmp_idx}");
                        tmp_idx += 1;
                        let _ = writeln!(
                            out,
                            "  {g} = getelementptr inbounds i8, ptr {slot}, i64 {}",
                            ci * 8
                        );
                        g
                    };
                    let _ = writeln!(out, "  store {} {pname}, ptr {p}, align 1", scalar_llvm(sk));
                }
                inner_args.push(format!("ptr {slot}"));
            }
            CAbiArg::Indirect { .. } => {
                let pname = format!("%cp{c_arg}");
                c_arg += 1;
                inner_args.push(format!("ptr {pname}"));
            }
        }
    }

    let ret_slot = if ret_layout.size > 0 {
        if let Some(sp) = sret_param {
            inner_args.push(format!("ptr {sp}"));
            Some(sp)
        } else {
            let sz = ret_layout.size.max(16);
            let al = ret_layout.align.max(8);
            let _ = writeln!(out, "  %ret_slot = alloca [{sz} x i8], align {al}");
            inner_args.push("ptr %ret_slot".into());
            Some("%ret_slot".into())
        }
    } else {
        None
    };

    let _ = writeln!(
        out,
        "  %ok0 = call i8 @__noev_fn_{func_index}({})",
        inner_args.join(", ")
    );

    let mut ok_var = "%ok0".to_string();
    let ret_val_expr: Option<String> = match &ret_abi {
        CAbiArg::Void | CAbiArg::Indirect { .. } => None,
        CAbiArg::Scalar(sk) => {
            let rs = ret_slot.as_deref().expect("ret slot");
            let _ = writeln!(
                out,
                "  %raw_ret = load {}, ptr {rs}, align {}",
                scalar_llvm(*sk),
                ret_layout.align
            );
            if ret_ty == Ty::INT {
                decls
                    .entry("noev_rt_int_to_c_i64".into())
                    .or_insert_with(|| "declare i8 @noev_rt_int_to_c_i64(ptr, i64, ptr)".into());
                out.push_str("  %c_i64_slot = alloca i64, align 8\n");
                out.push_str(
                    "  %ok_conv = call i8 @noev_rt_int_to_c_i64(ptr %rt, i64 %raw_ret, ptr %c_i64_slot)\n",
                );
                out.push_str("  %ok1 = and i8 %ok0, %ok_conv\n");
                ok_var = "%ok1".into();
                out.push_str("  %c_ret = load i64, ptr %c_i64_slot, align 8\n");
                Some("%c_ret".into())
            } else {
                Some("%raw_ret".into())
            }
        }
        CAbiArg::DirectChunks(chunks) => {
            let rs = ret_slot.as_deref().expect("ret slot");
            if chunks.len() == 1 {
                let _ = writeln!(
                    out,
                    "  %c_ret = load {}, ptr {rs}, align 1",
                    scalar_llvm(chunks[0])
                );
                Some("%c_ret".into())
            } else {
                let mut agg = "undef".to_string();
                for (ci, sk) in chunks.iter().enumerate() {
                    let p = if ci == 0 {
                        rs.to_string()
                    } else {
                        let g = format!("%rgep{ci}");
                        let _ = writeln!(
                            out,
                            "  {g} = getelementptr inbounds i8, ptr {rs}, i64 {}",
                            ci * 8
                        );
                        g
                    };
                    let v = format!("%rc{ci}");
                    let lt = scalar_llvm(*sk);
                    let _ = writeln!(out, "  {v} = load {lt}, ptr {p}, align 1");
                    let next_agg = format!("%ragg{ci}");
                    let _ = writeln!(
                        out,
                        "  {next_agg} = insertvalue {ret_llvm} {agg}, {lt} {v}, {ci}"
                    );
                    agg = next_agg;
                }
                Some(agg)
            }
        }
    };

    let _ = writeln!(
        out,
        "  call void @noev_rt_callback_exit(ptr %rt, i8 {ok_var})"
    );
    if let Some(rv) = ret_val_expr {
        let _ = writeln!(out, "  ret {ret_llvm} {rv}\n}}");
    } else {
        out.push_str("  ret void\n}\n");
    }
}
