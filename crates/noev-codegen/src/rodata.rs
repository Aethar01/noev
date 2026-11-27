//! Static `.rodata` emission for UTF-8 strings and [`NoevTypeDesc`] tables.

use std::collections::HashMap;
use std::mem::{offset_of, size_of};

use cranelift_module::{DataDescription, DataId, Linkage, Module};
use cranelift_object::ObjectModule;
use noev_ir::Program;
use noev_runtime::cabi_compiled::{NoevFieldDesc, NoevTypeDesc, NoevVariantDesc, TypeTag};
use noev_sema::{Ty, TyKind};

use crate::CodegenError;
use crate::layout::LayoutCache;

/// Emits and caches read-only data objects (UTF-8 strings and type descriptors).
pub struct Rodata {
    bytes: HashMap<Vec<u8>, DataId>,
    descs: HashMap<Ty, DataId>,
    next_id: u32,
}

impl Rodata {
    /// Creates an empty `.rodata` emitter.
    pub fn new() -> Self {
        Self {
            bytes: HashMap::new(),
            descs: HashMap::new(),
            next_id: 0,
        }
    }

    fn fresh_name(&mut self, prefix: &str) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("__noev_{prefix}_{id}")
    }

    /// Interns a static byte slice in `.rodata` and returns its [`DataId`].
    pub fn intern_bytes(
        &mut self,
        module: &mut ObjectModule,
        data: &[u8],
    ) -> Result<DataId, CodegenError> {
        if let Some(&id) = self.bytes.get(data) {
            return Ok(id);
        }
        let name = self.fresh_name("str");
        let id = module.declare_data(&name, Linkage::Local, false, false)?;
        let mut desc = DataDescription::new();
        // Ensure non-empty payload so the symbol has a valid address even for empty slices.
        if data.is_empty() {
            desc.define(Box::new([0u8]));
        } else {
            desc.define(data.to_vec().into_boxed_slice());
        }
        desc.set_align(1);
        module.define_data(id, &desc)?;
        self.bytes.insert(data.to_vec(), id);
        Ok(id)
    }

    /// Emits (or returns cached) static [`NoevTypeDesc`] for `ty`.
    pub fn type_desc(
        &mut self,
        module: &mut ObjectModule,
        program: &Program,
        layouts: &mut LayoutCache<'_>,
        ty: Ty,
    ) -> Result<DataId, CodegenError> {
        if let Some(&id) = self.descs.get(&ty) {
            return Ok(id);
        }
        let name = self.fresh_name("ty");
        let id = module.declare_data(&name, Linkage::Local, false, false)?;
        // Insert early so self-referential structures (if any) don't recurse infinitely.
        self.descs.insert(ty, id);

        let layout = layouts.layout(ty);
        let kind = program.types.kind(ty).clone();

        let (
            tag,
            bare,
            name_id,
            name_len,
            elem_id,
            array_len,
            fields_id,
            n_fields,
            variants_id,
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
                let elem_desc = self.type_desc(module, program, layouts, elem)?;
                (
                    TypeTag::Vec,
                    0,
                    None,
                    0,
                    Some(elem_desc),
                    0,
                    None,
                    0,
                    None,
                    0,
                )
            }
            TyKind::IoStream => (TypeTag::IoStream, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Stdin => (TypeTag::Stdin, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::TcpStream => (TypeTag::TcpStream, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::TcpListener => (TypeTag::TcpListener, 0, None, 0, None, 0, None, 0, None, 0),
            TyKind::Ref { inner, .. } => {
                let elem = self.type_desc(module, program, layouts, inner)?;
                (TypeTag::Ref, 0, None, 0, Some(elem), 0, None, 0, None, 0)
            }
            TyKind::Array(elem, len) => {
                let elem_desc = self.type_desc(module, program, layouts, elem)?;
                (
                    TypeTag::Array,
                    0,
                    None,
                    0,
                    Some(elem_desc),
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
                let fid = if specs.is_empty() {
                    None
                } else {
                    Some(self.emit_fields(module, program, layouts, &specs)?)
                };
                (
                    TypeTag::Tuple,
                    0,
                    None,
                    0,
                    None,
                    0,
                    fid,
                    specs.len(),
                    None,
                    0,
                )
            }
            TyKind::Struct(sid) => {
                let sname = program.struct_names[sid.index()].as_bytes();
                let sname_id = self.intern_bytes(module, sname)?;
                let def = &program.types.structs[sid.index()];
                if let Some(variants) = def.variants.clone() {
                    let bare = u8::from(def.bare);
                    let vid = if variants.is_empty() {
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
                            let fid = if fspecs.is_empty() {
                                None
                            } else {
                                Some(self.emit_fields(module, program, layouts, &fspecs)?)
                            };
                            v_specs.push((vname.as_str(), vdef.named, fid, fspecs.len()));
                        }
                        Some(self.emit_variants(module, &v_specs)?)
                    };
                    (
                        TypeTag::Enum,
                        bare,
                        Some(sname_id),
                        sname.len(),
                        None,
                        0,
                        None,
                        0,
                        vid,
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
                    let fid = if fspecs.is_empty() {
                        None
                    } else {
                        Some(self.emit_fields(module, program, layouts, &fspecs)?)
                    };
                    (
                        TypeTag::Struct,
                        0,
                        Some(sname_id),
                        sname.len(),
                        None,
                        0,
                        fid,
                        fspecs.len(),
                        None,
                        0,
                    )
                }
            }
            _ => (TypeTag::Unit, 0, None, 0, None, 0, None, 0, None, 0),
        };

        let mut buf = vec![0u8; size_of::<NoevTypeDesc>()];
        buf[offset_of!(NoevTypeDesc, kind)] = tag as u8;
        buf[offset_of!(NoevTypeDesc, bare)] = bare;
        write_u32(&mut buf, offset_of!(NoevTypeDesc, size), layout.size);
        write_u32(&mut buf, offset_of!(NoevTypeDesc, align), layout.align);
        write_usize(&mut buf, offset_of!(NoevTypeDesc, name_len), name_len);
        write_u64(&mut buf, offset_of!(NoevTypeDesc, array_len), array_len);
        write_usize(&mut buf, offset_of!(NoevTypeDesc, n_fields), n_fields);
        write_usize(&mut buf, offset_of!(NoevTypeDesc, n_variants), n_variants);

        let mut desc = DataDescription::new();
        desc.define(buf.into_boxed_slice());
        desc.set_align(std::mem::align_of::<NoevTypeDesc>() as u64);
        if let Some(nid) = name_id {
            let gv = module.declare_data_in_data(nid, &mut desc);
            desc.write_data_addr(offset_of!(NoevTypeDesc, name) as u32, gv, 0);
        }
        if let Some(eid) = elem_id {
            let gv = module.declare_data_in_data(eid, &mut desc);
            desc.write_data_addr(offset_of!(NoevTypeDesc, elem) as u32, gv, 0);
        }
        if let Some(fid) = fields_id {
            let gv = module.declare_data_in_data(fid, &mut desc);
            desc.write_data_addr(offset_of!(NoevTypeDesc, fields) as u32, gv, 0);
        }
        if let Some(vid) = variants_id {
            let gv = module.declare_data_in_data(vid, &mut desc);
            desc.write_data_addr(offset_of!(NoevTypeDesc, variants) as u32, gv, 0);
        }
        module.define_data(id, &desc)?;
        Ok(id)
    }

    fn emit_fields(
        &mut self,
        module: &mut ObjectModule,
        program: &Program,
        layouts: &mut LayoutCache<'_>,
        specs: &[(Option<&str>, u32, Ty)],
    ) -> Result<DataId, CodegenError> {
        let stride = size_of::<NoevFieldDesc>();
        let mut buf = vec![0u8; stride * specs.len()];
        let mut relocs: Vec<(u32, DataId)> = Vec::new();
        for (i, &(fname, offset, fty)) in specs.iter().enumerate() {
            let base = i * stride;
            if let Some(n) = fname {
                let nid = self.intern_bytes(module, n.as_bytes())?;
                relocs.push(((base + offset_of!(NoevFieldDesc, name)) as u32, nid));
                write_usize(
                    &mut buf,
                    base + offset_of!(NoevFieldDesc, name_len),
                    n.len(),
                );
            }
            write_u32(&mut buf, base + offset_of!(NoevFieldDesc, offset), offset);
            let fty_id = self.type_desc(module, program, layouts, fty)?;
            relocs.push(((base + offset_of!(NoevFieldDesc, ty)) as u32, fty_id));
        }
        let name = self.fresh_name("fields");
        let id = module.declare_data(&name, Linkage::Local, false, false)?;
        let mut desc = DataDescription::new();
        desc.define(buf.into_boxed_slice());
        desc.set_align(std::mem::align_of::<NoevFieldDesc>() as u64);
        for (off, target) in relocs {
            let gv = module.declare_data_in_data(target, &mut desc);
            desc.write_data_addr(off, gv, 0);
        }
        module.define_data(id, &desc)?;
        Ok(id)
    }

    fn emit_variants(
        &mut self,
        module: &mut ObjectModule,
        specs: &[(&str, bool, Option<DataId>, usize)],
    ) -> Result<DataId, CodegenError> {
        let stride = size_of::<NoevVariantDesc>();
        let mut buf = vec![0u8; stride * specs.len()];
        let mut relocs: Vec<(u32, DataId)> = Vec::new();
        for (i, &(vname, named, fields_id, n_fields)) in specs.iter().enumerate() {
            let base = i * stride;
            let nid = self.intern_bytes(module, vname.as_bytes())?;
            relocs.push(((base + offset_of!(NoevVariantDesc, name)) as u32, nid));
            write_usize(
                &mut buf,
                base + offset_of!(NoevVariantDesc, name_len),
                vname.len(),
            );
            buf[base + offset_of!(NoevVariantDesc, named)] = u8::from(named);
            if let Some(fid) = fields_id {
                relocs.push(((base + offset_of!(NoevVariantDesc, fields)) as u32, fid));
            }
            write_usize(
                &mut buf,
                base + offset_of!(NoevVariantDesc, n_fields),
                n_fields,
            );
        }
        let name = self.fresh_name("variants");
        let id = module.declare_data(&name, Linkage::Local, false, false)?;
        let mut desc = DataDescription::new();
        desc.define(buf.into_boxed_slice());
        desc.set_align(std::mem::align_of::<NoevVariantDesc>() as u64);
        for (off, target) in relocs {
            let gv = module.declare_data_in_data(target, &mut desc);
            desc.write_data_addr(off, gv, 0);
        }
        module.define_data(id, &desc)?;
        Ok(id)
    }
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn write_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

fn write_usize(buf: &mut [u8], off: usize, v: usize) {
    let bytes = v.to_ne_bytes();
    buf[off..off + bytes.len()].copy_from_slice(&bytes);
}
