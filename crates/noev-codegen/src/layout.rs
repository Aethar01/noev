//! Memory layout for concrete Noev types in native compiled code.

use std::collections::HashMap;

use cranelift_codegen::ir::Type as ClifType;
use cranelift_codegen::ir::types;
use noev_ir::Program;
use noev_sema::{Ty, TyKind};

/// Computed memory layout for one concrete [`Ty`].
#[derive(Clone, Debug)]
pub struct TypeLayout {
    /// Total byte size (rounded up to `align`; `0` for ZSTs like `()`).
    pub size: u32,
    /// Byte alignment (power of two, >= 1).
    pub align: u32,
    /// Cranelift SSA register type when this value fits in a single register (`None` for
    /// aggregates like `Complex`, `c32`, tuples, arrays, structs, enums, and ZSTs).
    pub scalar: Option<ClifType>,
    /// For `Tuple` and `Struct` (non-enum): byte offset of each field.
    pub fields: Vec<u32>,
    /// For `Enum`: for each variant, byte offsets of its payload fields (from enum base).
    pub enum_variants: Vec<Vec<u32>>,
}

/// Cache of [`TypeLayout`]s for a [`Program`].
pub struct LayoutCache<'p> {
    program: &'p Program,
    ptr_ty: ClifType,
    ptr_bytes: u32,
    cache: HashMap<Ty, TypeLayout>,
}

impl<'p> LayoutCache<'p> {
    /// Creates a new layout cache for `program` targeting pointers of `ptr_ty` (`ptr_bytes` wide).
    pub fn new(program: &'p Program, ptr_ty: ClifType, ptr_bytes: u32) -> Self {
        Self {
            program,
            ptr_ty,
            ptr_bytes,
            cache: HashMap::new(),
        }
    }

    /// Returns the layout of `ty`, computing and caching it if needed.
    pub fn layout(&mut self, ty: Ty) -> TypeLayout {
        if let Some(l) = self.cache.get(&ty) {
            return l.clone();
        }
        let computed = self.compute(ty);
        self.cache.insert(ty, computed.clone());
        computed
    }

    /// Returns the alignment shift (`log2(align)`) for Cranelift `StackSlotData`.
    pub fn align_shift(align: u32) -> u8 {
        align.max(1).trailing_zeros() as u8
    }

    /// Packs a list of argument types into a contiguous struct-like buffer (for thunks),
    /// returning `(total_size, max_align, field_offsets)`.
    pub fn pack_types(&mut self, tys: &[Ty]) -> (u32, u32, Vec<u32>) {
        let mut offset = 0u32;
        let mut max_align = 1u32;
        let mut offsets = Vec::with_capacity(tys.len());
        for &ty in tys {
            let l = self.layout(ty);
            offset = align_to(offset, l.align);
            offsets.push(offset);
            offset = offset.checked_add(l.size).expect("layout size overflow");
            max_align = max_align.max(l.align);
        }
        (align_to(offset, max_align), max_align, offsets)
    }

    /// Collects the byte offset of every `Qubit` leaf inside `ty`, in declaration/element order.
    pub fn qubit_offsets(&mut self, ty: Ty) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect_qubits(ty, 0, &mut out);
        out
    }

    fn collect_qubits(&mut self, ty: Ty, base: u32, out: &mut Vec<u32>) {
        match self.program.types.kind(ty).clone() {
            TyKind::Qubit => out.push(base),
            TyKind::Tuple(elems) => {
                let l = self.layout(ty);
                for (i, &elem) in elems.iter().enumerate() {
                    self.collect_qubits(elem, base + l.fields[i], out);
                }
            }
            TyKind::Array(elem, len) => {
                let elem_size = self.layout(elem).size;
                for i in 0..len {
                    self.collect_qubits(elem, base + (i as u32) * elem_size, out);
                }
            }
            TyKind::Struct(id) if self.program.types.structs[id.index()].variants.is_none() => {
                let l = self.layout(ty);
                let fields: Vec<Ty> = self.program.types.structs[id.index()]
                    .fields
                    .iter()
                    .map(|&(_, t)| t)
                    .collect();
                for (i, fty) in fields.into_iter().enumerate() {
                    self.collect_qubits(fty, base + l.fields[i], out);
                }
            }
            _ => {}
        }
    }

    fn compute(&mut self, ty: Ty) -> TypeLayout {
        let ptr_b = self.ptr_bytes;
        let ptr_t = self.ptr_ty;
        let scalar = |size: u32, align: u32, clif: ClifType| TypeLayout {
            size,
            align,
            scalar: Some(clif),
            fields: Vec::new(),
            enum_variants: Vec::new(),
        };
        match self.program.types.kind(ty).clone() {
            TyKind::Unit | TyKind::Never | TyKind::Error => TypeLayout {
                size: 0,
                align: 1,
                scalar: None,
                fields: Vec::new(),
                enum_variants: Vec::new(),
            },
            TyKind::Bool | TyKind::Bit | TyKind::I8 | TyKind::U8 => scalar(1, 1, types::I8),
            TyKind::I16 | TyKind::U16 => scalar(2, 2, types::I16),
            TyKind::I32 | TyKind::U32 => scalar(4, 4, types::I32),
            TyKind::I64
            | TyKind::U64
            | TyKind::Int
            | TyKind::String
            | TyKind::Observable
            | TyKind::Distribution(_) => scalar(8, 8, types::I64),
            TyKind::Qubit | TyKind::Ref { .. } => scalar(ptr_b, ptr_b, ptr_t),
            TyKind::F32 => scalar(4, 4, types::F32),
            TyKind::Float => scalar(8, 8, types::F64),
            TyKind::Complex => TypeLayout {
                size: 16,
                align: 8,
                scalar: None,
                fields: vec![0, 8],
                enum_variants: Vec::new(),
            },
            TyKind::C32 => TypeLayout {
                size: 8,
                align: 4,
                scalar: None,
                fields: vec![0, 4],
                enum_variants: Vec::new(),
            },
            TyKind::Tuple(elems) => {
                let (size, align, fields) = self.pack_types(&elems);
                TypeLayout {
                    size,
                    align,
                    scalar: None,
                    fields,
                    enum_variants: Vec::new(),
                }
            }
            TyKind::Array(elem, len) => {
                let el = self.layout(elem);
                let size = el
                    .size
                    .checked_mul(u32::try_from(len).expect("array fits in u32"))
                    .expect("array byte size overflow");
                TypeLayout {
                    size,
                    align: el.align,
                    scalar: None,
                    fields: Vec::new(),
                    enum_variants: Vec::new(),
                }
            }
            TyKind::Struct(id) => {
                let def = self.program.types.structs[id.index()].clone();
                if let Some(variants) = def.variants {
                    let mut max_align = 4u32;
                    let mut max_size = 4u32;
                    let mut enum_variants = Vec::with_capacity(variants.len());
                    for v in &variants {
                        let mut off = 4u32;
                        let mut v_offsets = Vec::with_capacity(v.data.len());
                        for &fty in &v.data {
                            let fl = self.layout(fty);
                            off = align_to(off, fl.align);
                            v_offsets.push(off);
                            off = off.checked_add(fl.size).expect("enum variant overflow");
                            max_align = max_align.max(fl.align);
                        }
                        max_size = max_size.max(off);
                        enum_variants.push(v_offsets);
                    }
                    TypeLayout {
                        size: align_to(max_size, max_align),
                        align: max_align,
                        scalar: None,
                        fields: Vec::new(),
                        enum_variants,
                    }
                } else {
                    let field_tys: Vec<Ty> = def.fields.iter().map(|&(_, t)| t).collect();
                    let (size, align, fields) = self.pack_types(&field_tys);
                    TypeLayout {
                        size,
                        align,
                        scalar: None,
                        fields,
                        enum_variants: Vec::new(),
                    }
                }
            }
            TyKind::Param(_)
            | TyKind::Assoc { .. }
            | TyKind::SizedArray(..)
            | TyKind::Size(_)
            | TyKind::SizeSum { .. } => {
                panic!("unmonomorphised type reached codegen: {:?}", ty)
            }
        }
    }
}

/// Rounds `offset` up to the next multiple of `align` (a power of two).
pub fn align_to(offset: u32, align: u32) -> u32 {
    let a = align.max(1);
    (offset + a - 1) & !(a - 1)
}
