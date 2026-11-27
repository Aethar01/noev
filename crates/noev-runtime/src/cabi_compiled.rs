//! Extended Runtime C ABI helpers for compiled native code (Cranelift & LLVM).
//!
//! Native code runs scalars, aggregates, references, control flow and loops directly in CPU
//! registers and stack slots, calling into this module for arbitrary-precision `Int`,
//! checked sized arithmetic, formatting/printing, observables, distributions, FFT, matrix
//! validation, and compiled `sample` / expectation / overlap / undo runs.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char};
use std::fmt::Write as _;
use std::ptr;
use std::rc::Rc;

use noev_ir::ir::{Align, BinOp, FormatKind, FormatSpec};
use noev_sema::prelude::Gate;
use noev_sema::ty::Width;
use num_bigint::BigInt;
use num_traits::ToPrimitive;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::backend::BackendError;
#[cfg(not(feature = "ketrs"))]
use crate::cabi::Output;
use crate::cabi::{
    self, Mode, NoevComplex, NoevPauliFactor, NoevPauliTerm, NoevQubit, NoevRuntime, Recording,
    RuntimeError,
};
use crate::int::Int;
use crate::value::{Distribution, HandleKind, Observable, Outcome, Pauli, Value};

// ---------------------------------------------------------------------------------------
// Type descriptors emitted in `.rodata` by code generators
// ---------------------------------------------------------------------------------------

/// Tag identifying a Noev type in [`NoevTypeDesc::kind`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeTag {
    /// `()`
    Unit = 0,
    /// Arbitrary-precision `Int` (tagged `i64`: even = `n << 1`, odd = `(bigint_idx << 1) | 1`).
    Int = 1,
    /// `i8`
    I8 = 2,
    /// `i16`
    I16 = 3,
    /// `i32`
    I32 = 4,
    /// `i64`
    I64 = 5,
    /// `u8`
    U8 = 6,
    /// `u16`
    U16 = 7,
    /// `u32`
    U32 = 8,
    /// `u64`
    U64 = 9,
    /// `f32`
    F32 = 10,
    /// `Float` (`f64`)
    Float = 11,
    /// `Complex` (`{ re: f64, im: f64 }`)
    Complex = 12,
    /// `c32` (`{ re: f32, im: f32 }`)
    C32 = 13,
    /// `Bool` (`u8`: 0 or 1)
    Bool = 14,
    /// `Bit` (`u8`: 0 or 1)
    Bit = 15,
    /// `String` (`u64` handle into `rt.strings`)
    Str = 16,
    /// `Qubit` (`NoevQubit` = `usize`)
    Qubit = 17,
    /// `Observable` (`u64` handle into `rt.observables`)
    Observable = 18,
    /// `Distribution<T>` (`u64` handle into `rt.distributions`)
    Distribution = 19,
    /// `ref T` or `mut T` (pointer to `T`)
    Ref = 20,
    /// `(T0, T1, …)`
    Tuple = 21,
    /// `[T; N]`
    Array = 22,
    /// Struct
    Struct = 23,
    /// Enum (`{ tag: u32, payload: … }`)
    Enum = 24,
    /// `Vec<T>` (`u64` handle into `rt.vecs`, with `elem` descriptor)
    Vec = 25,
    /// `std::io::IoStream` (`u64` handle: 0 = stdout, 1 = stderr)
    IoStream = 26,
    /// `std::io::Stdin` (`u64` handle)
    Stdin = 27,
    /// `std::net::TcpStream` (`u64` handle into `rt.tcp_streams`)
    TcpStream = 28,
    /// `std::net::TcpListener` (`u64` handle into `rt.tcp_listeners`)
    TcpListener = 29,
}

/// Field descriptor inside a tuple, struct, or enum variant payload.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevFieldDesc {
    /// UTF-8 field name pointer (null for tuple/positional fields).
    pub name: *const u8,
    /// Byte length of `name`.
    pub name_len: usize,
    /// Byte offset from the start of the enclosing aggregate (or enum value).
    pub offset: u32,
    /// Field's type descriptor.
    pub ty: *const NoevTypeDesc,
}

/// Variant descriptor inside an enum's [`NoevTypeDesc`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevVariantDesc {
    /// UTF-8 variant name pointer.
    pub name: *const u8,
    /// Byte length of `name`.
    pub name_len: usize,
    /// Non-zero for struct-like variants (`Retry { attempts: Int }`).
    pub named: u8,
    /// Fields of this variant, in declaration order.
    pub fields: *const NoevFieldDesc,
    /// Number of fields.
    pub n_fields: usize,
}

/// Static `.rodata` descriptor for any concrete Noev type, used by `print`, `format`,
/// aggregate `==`, `sample`, `Expectation`, `Overlap`, and `Undo`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevTypeDesc {
    /// [`TypeTag`] discriminant.
    pub kind: u8,
    /// For enums: non-zero when variants print bare (`Some(1)` instead of `Option::Some(1)`).
    pub bare: u8,
    /// Byte size in memory.
    pub size: u32,
    /// Byte alignment in memory.
    pub align: u32,
    /// Struct/enum UTF-8 name pointer (null otherwise).
    pub name: *const u8,
    /// Byte length of `name`.
    pub name_len: usize,
    /// For `Array` / `Ref`: element or inner type descriptor.
    pub elem: *const NoevTypeDesc,
    /// For `Array`: element count.
    pub array_len: u64,
    /// For `Tuple` / `Struct`: fields array pointer.
    pub fields: *const NoevFieldDesc,
    /// For `Tuple` / `Struct`: field count.
    pub n_fields: usize,
    /// For `Enum`: variants array pointer.
    pub variants: *const NoevVariantDesc,
    /// For `Enum`: variant count.
    pub n_variants: usize,
}

/// One piece of a format string passed to [`noev_rt_format`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevFormatPart {
    /// `0` = literal text (`ptr` = UTF-8 bytes, `len` = byte length),
    /// `1` = value (`ptr` = value pointer, `ty` = type descriptor).
    pub is_value: u8,
    /// Format spec present (`0` = none, `1` = use `spec_*` fields).
    pub has_spec: u8,
    /// Alignment: `0` = default, `1` = Left, `2` = Right, `3` = Center.
    pub align: u8,
    /// Kind: `0` = none, `1` = Binary, `2` = Octal, `3` = Hex, `4` = HexUpper, `5` = Exponent.
    pub kind: u8,
    /// Flags: bit 0 = plus, bit 1 = zero, bit 2 = has_width, bit 3 = has_precision.
    pub flags: u8,
    /// Fill Unicode scalar value.
    pub fill: u32,
    /// Width (when bit 2 of `flags` is set).
    pub width: u32,
    /// Precision (when bit 3 of `flags` is set).
    pub precision: u32,
    /// Pointer to literal bytes or native value.
    pub ptr: *const u8,
    /// Literal byte length (unused for values).
    pub len: usize,
    /// Type descriptor (for values).
    pub ty: *const NoevTypeDesc,
}

/// One argument to [`noev_rt_print`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NoevPrintArg {
    /// Pointer to the value in native memory.
    pub ptr: *const u8,
    /// Its type descriptor.
    pub ty: *const NoevTypeDesc,
}

/// Compiled thunk signature for `sample`, `Expectation`, `Overlap`, and `Undo`:
/// takes `(rt, args_buf, ret_buf)` and returns `true` on success.
pub type NoevCompiledFn =
    extern "C" fn(rt: *mut NoevRuntime, args: *const u8, ret: *mut u8) -> bool;

// ---------------------------------------------------------------------------------------
// Tagged `Int` helpers (even = `n << 1`, odd = `(bigint_idx << 1) | 1`)
// ---------------------------------------------------------------------------------------

const MIN_INLINE_INT: i64 = -(1i64 << 62);
const MAX_INLINE_INT: i64 = (1i64 << 62) - 1;

/// Encodes an [`Int`] into a tagged 64-bit value, storing big values in `rt.bigints`.
pub fn encode_int(rt: &mut NoevRuntime, v: Int) -> i64 {
    match v {
        Int::Small(n) if (MIN_INLINE_INT..=MAX_INLINE_INT).contains(&n) => n << 1,
        Int::Small(n) => {
            let idx = rt.bigints.len();
            rt.bigints.push(BigInt::from(n));
            ((idx as u64) << 1 | 1) as i64
        }
        Int::Big(b) => {
            if let Some(n) = b.to_i64()
                && (MIN_INLINE_INT..=MAX_INLINE_INT).contains(&n)
            {
                return n << 1;
            }
            let idx = rt.bigints.len();
            rt.bigints.push((*b).clone());
            ((idx as u64) << 1 | 1) as i64
        }
    }
}

/// Decodes a tagged 64-bit `Int` back into [`Int`].
pub fn decode_int(rt: &NoevRuntime, raw: i64) -> Int {
    if (raw & 1) == 0 {
        Int::Small(raw >> 1)
    } else {
        let idx = ((raw as u64) >> 1) as usize;
        rt.bigints
            .get(idx)
            .cloned()
            .map(Int::from)
            .unwrap_or(Int::Small(0))
    }
}

fn store_str(rt: &mut NoevRuntime, s: String) -> u64 {
    let idx = rt.strings.len();
    rt.strings.push(s);
    idx as u64
}

fn read_str(rt: &NoevRuntime, handle: u64) -> &str {
    rt.strings.get(handle as usize).map_or("", String::as_str)
}

fn store_obs(rt: &mut NoevRuntime, o: Observable) -> u64 {
    let idx = rt.observables.len();
    rt.observables.push(o);
    idx as u64
}

fn read_obs(rt: &NoevRuntime, handle: u64) -> &Observable {
    rt.observables
        .get(handle as usize)
        .unwrap_or(&rt.observables[0])
}

fn store_dist(rt: &mut NoevRuntime, d: Distribution) -> u64 {
    let idx = rt.distributions.len();
    rt.distributions.push(Rc::new(d));
    idx as u64
}

// ---------------------------------------------------------------------------------------
// Reading / writing native values via `NoevTypeDesc`
// ---------------------------------------------------------------------------------------

unsafe fn desc_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    if ptr.is_null() || len == 0 {
        ""
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        std::str::from_utf8(bytes).unwrap_or("")
    }
}

/// Reads a native value at `ptr` (described by `desc`) into a runtime [`Value`], following
/// references when `deref_refs` is true (for printing/formatting).
unsafe fn read_native(rt: &NoevRuntime, ptr: *const u8, desc: &NoevTypeDesc) -> Value {
    match desc.kind {
        0 => Value::Unit,
        1 => Value::Int(decode_int(rt, unsafe {
            ptr.cast::<i64>().read_unaligned()
        })),
        2 => Value::Sized(
            i128::from(unsafe { ptr.cast::<i8>().read_unaligned() }),
            Width {
                bits: 8,
                signed: true,
            },
        ),
        3 => Value::Sized(
            i128::from(unsafe { ptr.cast::<i16>().read_unaligned() }),
            Width {
                bits: 16,
                signed: true,
            },
        ),
        4 => Value::Sized(
            i128::from(unsafe { ptr.cast::<i32>().read_unaligned() }),
            Width {
                bits: 32,
                signed: true,
            },
        ),
        5 => Value::Sized(
            i128::from(unsafe { ptr.cast::<i64>().read_unaligned() }),
            Width {
                bits: 64,
                signed: true,
            },
        ),
        6 => Value::Sized(
            i128::from(unsafe { ptr.cast::<u8>().read_unaligned() }),
            Width {
                bits: 8,
                signed: false,
            },
        ),
        7 => Value::Sized(
            i128::from(unsafe { ptr.cast::<u16>().read_unaligned() }),
            Width {
                bits: 16,
                signed: false,
            },
        ),
        8 => Value::Sized(
            i128::from(unsafe { ptr.cast::<u32>().read_unaligned() }),
            Width {
                bits: 32,
                signed: false,
            },
        ),
        9 => Value::Sized(
            i128::from(unsafe { ptr.cast::<u64>().read_unaligned() }),
            Width {
                bits: 64,
                signed: false,
            },
        ),
        10 => Value::F32(unsafe { ptr.cast::<f32>().read_unaligned() }),
        11 => Value::Float(unsafe { ptr.cast::<f64>().read_unaligned() }),
        12 => {
            let re = unsafe { ptr.cast::<f64>().read_unaligned() };
            let im = unsafe { ptr.add(8).cast::<f64>().read_unaligned() };
            Value::Complex(re, im)
        }
        13 => {
            let re = unsafe { ptr.cast::<f32>().read_unaligned() };
            let im = unsafe { ptr.add(4).cast::<f32>().read_unaligned() };
            Value::C32(re, im)
        }
        14 => Value::Bool(unsafe { ptr.read() } != 0),
        15 => Value::Bit(unsafe { ptr.read() } != 0),
        16 => {
            let handle = unsafe { ptr.cast::<u64>().read_unaligned() };
            Value::Str(read_str(rt, handle).into())
        }
        17 => Value::Qubit(unsafe { ptr.cast::<usize>().read_unaligned() }),
        18 => {
            let handle = unsafe { ptr.cast::<u64>().read_unaligned() };
            Value::Observable(read_obs(rt, handle).clone())
        }
        19 => {
            let handle = unsafe { ptr.cast::<u64>().read_unaligned() };
            match rt.distributions.get(handle as usize) {
                Some(d) => Value::Distribution(d.clone()),
                None => Value::Unit,
            }
        }
        20 => {
            let target = unsafe { ptr.cast::<*const u8>().read_unaligned() };
            if target.is_null() || desc.elem.is_null() {
                Value::Unit
            } else {
                unsafe { read_native(rt, target, &*desc.elem) }
            }
        }
        21 | 23 => {
            let fields = if desc.n_fields == 0 || desc.fields.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(desc.fields, desc.n_fields) }
            };
            let items = fields
                .iter()
                .map(|f| unsafe { read_native(rt, ptr.add(f.offset as usize), &*f.ty) })
                .collect();
            Value::Aggregate(items)
        }
        22 => {
            let elem = unsafe { &*desc.elem };
            let stride = elem.size as usize;
            let items = (0..desc.array_len as usize)
                .map(|i| unsafe { read_native(rt, ptr.add(i * stride), elem) })
                .collect();
            Value::Aggregate(items)
        }
        24 => {
            let tag = unsafe { ptr.cast::<u32>().read_unaligned() };
            let variants = if desc.n_variants == 0 || desc.variants.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(desc.variants, desc.n_variants) }
            };
            let data = if let Some(v) = variants.get(tag as usize) {
                let fields = if v.n_fields == 0 || v.fields.is_null() {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(v.fields, v.n_fields) }
                };
                fields
                    .iter()
                    .map(|f| unsafe { read_native(rt, ptr.add(f.offset as usize), &*f.ty) })
                    .collect()
            } else {
                Vec::new()
            };
            Value::Variant(tag, data)
        }
        25 => {
            let handle = unsafe { ptr.cast::<u64>().read_unaligned() };
            match rt.vecs.get(handle as usize) {
                Some(v) => Value::Vec(v.clone()),
                None => Value::Vec(Rc::new(RefCell::new(Vec::new()))),
            }
        }
        26 => Value::Handle(HandleKind::IoStream, unsafe {
            ptr.cast::<u64>().read_unaligned()
        }),
        27 => Value::Handle(HandleKind::Stdin, unsafe {
            ptr.cast::<u64>().read_unaligned()
        }),
        28 => Value::Handle(HandleKind::TcpStream, unsafe {
            ptr.cast::<u64>().read_unaligned()
        }),
        29 => Value::Handle(HandleKind::TcpListener, unsafe {
            ptr.cast::<u64>().read_unaligned()
        }),
        _ => Value::Unit,
    }
}

/// Writes a runtime [`Value`] into native memory at `ptr` described by `desc`.
unsafe fn write_native(rt: &mut NoevRuntime, val: &Value, ptr: *mut u8, desc: &NoevTypeDesc) {
    match (val, desc.kind) {
        (_, 0) => {}
        (Value::Int(v), 1) => {
            let raw = encode_int(rt, v.clone());
            unsafe { ptr.cast::<i64>().write_unaligned(raw) };
        }
        (Value::Sized(v, _), 2) => unsafe { ptr.cast::<i8>().write_unaligned(*v as i8) },
        (Value::Sized(v, _), 3) => unsafe { ptr.cast::<i16>().write_unaligned(*v as i16) },
        (Value::Sized(v, _), 4) => unsafe { ptr.cast::<i32>().write_unaligned(*v as i32) },
        (Value::Sized(v, _), 5) => unsafe { ptr.cast::<i64>().write_unaligned(*v as i64) },
        (Value::Sized(v, _), 6) => unsafe { ptr.cast::<u8>().write_unaligned(*v as u8) },
        (Value::Sized(v, _), 7) => unsafe { ptr.cast::<u16>().write_unaligned(*v as u16) },
        (Value::Sized(v, _), 8) => unsafe { ptr.cast::<u32>().write_unaligned(*v as u32) },
        (Value::Sized(v, _), 9) => unsafe { ptr.cast::<u64>().write_unaligned(*v as u64) },
        (Value::F32(v), 10) => unsafe { ptr.cast::<f32>().write_unaligned(*v) },
        (Value::Float(v), 11) => unsafe { ptr.cast::<f64>().write_unaligned(*v) },
        (Value::Complex(re, im), 12) => unsafe {
            ptr.cast::<f64>().write_unaligned(*re);
            ptr.add(8).cast::<f64>().write_unaligned(*im);
        },
        (Value::C32(re, im), 13) => unsafe {
            ptr.cast::<f32>().write_unaligned(*re);
            ptr.add(4).cast::<f32>().write_unaligned(*im);
        },
        (Value::Bool(b), 14) | (Value::Bit(b), 15) => unsafe { ptr.write(u8::from(*b)) },
        (Value::Str(s), 16) => {
            let h = store_str(rt, s.to_string());
            unsafe { ptr.cast::<u64>().write_unaligned(h) };
        }
        (Value::Qubit(q), 17) => unsafe { ptr.cast::<usize>().write_unaligned(*q) },
        (Value::Observable(o), 18) => {
            let h = store_obs(rt, o.clone());
            unsafe { ptr.cast::<u64>().write_unaligned(h) };
        }
        (Value::Distribution(d), 19) => {
            let idx = rt.distributions.len();
            rt.distributions.push(d.clone());
            unsafe { ptr.cast::<u64>().write_unaligned(idx as u64) };
        }
        (Value::Vec(v), 25) => {
            let idx = rt.vecs.len();
            rt.vecs.push(v.clone());
            unsafe { ptr.cast::<u64>().write_unaligned(idx as u64) };
        }
        (Value::Handle(_, h), 26..=29) => {
            unsafe { ptr.cast::<u64>().write_unaligned(*h) };
        }
        (Value::Aggregate(items), 21 | 23) => {
            let fields = if desc.n_fields == 0 || desc.fields.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(desc.fields, desc.n_fields) }
            };
            for (item, f) in items.iter().zip(fields) {
                unsafe { write_native(rt, item, ptr.add(f.offset as usize), &*f.ty) };
            }
        }
        (Value::Aggregate(items), 22) => {
            let elem = unsafe { &*desc.elem };
            let stride = elem.size as usize;
            for (i, item) in items.iter().enumerate() {
                unsafe { write_native(rt, item, ptr.add(i * stride), elem) };
            }
        }
        (Value::Variant(tag, data), 24) => {
            unsafe { ptr.cast::<u32>().write_unaligned(*tag) };
            let variants = if desc.n_variants == 0 || desc.variants.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(desc.variants, desc.n_variants) }
            };
            if let Some(v) = variants.get(*tag as usize) {
                let fields = if v.n_fields == 0 || v.fields.is_null() {
                    &[]
                } else {
                    unsafe { std::slice::from_raw_parts(v.fields, v.n_fields) }
                };
                for (item, f) in data.iter().zip(fields) {
                    unsafe { write_native(rt, item, ptr.add(f.offset as usize), &*f.ty) };
                }
            }
        }
        _ => {}
    }
}

/// Formats a [`Value`] described by [`NoevTypeDesc`] identically to [`crate::format::display`].
fn display_desc(val: &Value, desc: &NoevTypeDesc, out: &mut String) {
    // Follow reference descriptors to the underlying printed type.
    let mut desc = desc;
    while desc.kind == TypeTag::Ref as u8 && !desc.elem.is_null() {
        desc = unsafe { &*desc.elem };
    }
    match val {
        Value::Unit => out.push_str("()"),
        Value::Int(v) => {
            let _ = write!(out, "{v}");
        }
        Value::Sized(v, _) => {
            let _ = write!(out, "{v}");
        }
        Value::F32(v) => {
            let _ = write!(out, "{v:?}");
        }
        Value::Complex(re, im) => out.push_str(&crate::format::complex(*re, *im, None)),
        Value::C32(re, im) => out.push_str(&crate::format::complex32(*re, *im, None)),
        Value::Float(v) => {
            let _ = write!(out, "{v:?}");
        }
        Value::Bool(v) => {
            let _ = write!(out, "{v}");
        }
        Value::Bit(v) => out.push(if *v { '1' } else { '0' }),
        Value::Str(s) => out.push_str(s),
        Value::Observable(o) => crate::format::write_observable(o, out),
        Value::Distribution(d) => {
            let _ = write!(out, "{d}");
        }
        Value::Vec(v) => {
            out.push('[');
            let elem = if desc.elem.is_null() {
                None
            } else {
                Some(unsafe { &*desc.elem })
            };
            for (i, item) in v.borrow().iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if let Some(elem_desc) = elem {
                    display_desc(item, elem_desc, out);
                } else {
                    let _ = write!(out, "{item:?}");
                }
            }
            out.push(']');
        }
        Value::Handle(kind, _) => {
            let _ = write!(out, "<{kind:?}>");
        }
        Value::Aggregate(items) => {
            if !items.is_empty() && items.iter().all(|v| matches!(v, Value::Bit(_))) {
                for item in items {
                    if let Value::Bit(b) = item {
                        out.push(if *b { '1' } else { '0' });
                    }
                }
                return;
            }
            match desc.kind {
                22 => {
                    let elem = unsafe { &*desc.elem };
                    out.push('[');
                    for (i, item) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        display_desc(item, elem, out);
                    }
                    out.push(']');
                }
                21 => {
                    let fields = unsafe { std::slice::from_raw_parts(desc.fields, desc.n_fields) };
                    out.push('(');
                    for (i, (item, f)) in items.iter().zip(fields).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        display_desc(item, unsafe { &*f.ty }, out);
                    }
                    if items.len() == 1 {
                        out.push(',');
                    }
                    out.push(')');
                }
                23 => {
                    out.push_str(unsafe { desc_str(desc.name, desc.name_len) });
                    out.push_str(" { ");
                    let fields = unsafe { std::slice::from_raw_parts(desc.fields, desc.n_fields) };
                    for (i, (item, f)) in items.iter().zip(fields).enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(unsafe { desc_str(f.name, f.name_len) });
                        out.push_str(": ");
                        display_desc(item, unsafe { &*f.ty }, out);
                    }
                    out.push_str(" }");
                }
                _ => out.push('…'),
            }
        }
        Value::Variant(tag, data) if desc.kind == TypeTag::Enum as u8 => {
            let variants = if desc.n_variants == 0 || desc.variants.is_null() {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(desc.variants, desc.n_variants) }
            };
            let Some(v) = variants.get(*tag as usize) else {
                let _ = write!(out, "<variant {tag}>");
                return;
            };
            let vname = unsafe { desc_str(v.name, v.name_len) };
            if desc.bare != 0 {
                out.push_str(vname);
            } else {
                out.push_str(unsafe { desc_str(desc.name, desc.name_len) });
                out.push_str("::");
                out.push_str(vname);
            }
            if data.is_empty() {
                return;
            }
            let fields = unsafe { std::slice::from_raw_parts(v.fields, v.n_fields) };
            out.push_str(if v.named != 0 { " { " } else { "(" });
            for (k, (item, f)) in data.iter().zip(fields).enumerate() {
                if k > 0 {
                    out.push_str(", ");
                }
                if v.named != 0 {
                    out.push_str(unsafe { desc_str(f.name, f.name_len) });
                    out.push_str(": ");
                }
                display_desc(item, unsafe { &*f.ty }, out);
            }
            out.push_str(if v.named != 0 { " }" } else { ")" });
        }
        Value::Variant(tag, _) => {
            let _ = write!(out, "<variant {tag}>");
        }
        Value::Qubit(_) => out.push_str("<qubit>"),
        Value::Clbit(_) | Value::Cond(..) => out.push('0'),
        Value::Ref(_) => out.push_str("<reference>"),
        Value::FnPtr(idx) => out.push_str(&format!("<fn#{idx}>")),
    }
}

fn format_desc(val: &Value, desc: &NoevTypeDesc, spec: Option<&FormatSpec>) -> String {
    let Some(spec) = spec else {
        let mut out = String::new();
        display_desc(val, desc, &mut out);
        return out;
    };
    let sign = |neg: bool, plus: bool| match (neg, plus) {
        (true, _) => "-".to_string(),
        (false, true) => "+".to_string(),
        (false, false) => String::new(),
    };
    let (sgn, digits, numeric) = match val {
        Value::Int(v) => {
            let digits = match spec.kind {
                Some(FormatKind::Binary) => v.magnitude_digits(2),
                Some(FormatKind::Octal) => v.magnitude_digits(8),
                Some(FormatKind::Hex) => v.magnitude_digits(16),
                Some(FormatKind::HexUpper) => v.magnitude_digits(16).to_uppercase(),
                _ => v.magnitude_digits(10),
            };
            (sign(v.is_negative(), spec.plus), digits, true)
        }
        Value::Sized(v, _) => {
            let m = v.unsigned_abs();
            let digits = match spec.kind {
                Some(FormatKind::Binary) => format!("{m:b}"),
                Some(FormatKind::Octal) => format!("{m:o}"),
                Some(FormatKind::Hex) => format!("{m:x}"),
                Some(FormatKind::HexUpper) => format!("{m:X}"),
                _ => m.to_string(),
            };
            (sign(*v < 0, spec.plus), digits, true)
        }
        Value::Complex(re, im) => {
            let p = spec.precision.map(|p| p as usize);
            (String::new(), crate::format::complex(*re, *im, p), false)
        }
        Value::C32(re, im) => {
            let p = spec.precision.map(|p| p as usize);
            (String::new(), crate::format::complex32(*re, *im, p), false)
        }
        Value::F32(v) => {
            let magnitude = v.abs();
            let digits = match (spec.kind, spec.precision) {
                (Some(FormatKind::Exponent), Some(p)) => format!("{magnitude:.*e}", p as usize),
                (Some(FormatKind::Exponent), None) => format!("{magnitude:e}"),
                (_, Some(p)) => format!("{magnitude:.*}", p as usize),
                (_, None) => format!("{magnitude:?}"),
            };
            (
                sign(v.is_sign_negative() && !v.is_nan(), spec.plus),
                digits,
                true,
            )
        }
        Value::Float(v) => {
            let magnitude = v.abs();
            let digits = match (spec.kind, spec.precision) {
                (Some(FormatKind::Exponent), Some(p)) => format!("{magnitude:.*e}", p as usize),
                (Some(FormatKind::Exponent), None) => format!("{magnitude:e}"),
                (_, Some(p)) => format!("{magnitude:.*}", p as usize),
                (_, None) => format!("{magnitude:?}"),
            };
            (
                sign(v.is_sign_negative() && !v.is_nan(), spec.plus),
                digits,
                true,
            )
        }
        other => {
            let mut s = String::new();
            display_desc(other, desc, &mut s);
            (String::new(), s, false)
        }
    };
    let len = sgn.chars().count() + digits.chars().count();
    let width = spec.width.map_or(0, |w| w as usize);
    if len >= width {
        return sgn + &digits;
    }
    let pad = width - len;
    if spec.zero && numeric && spec.align.is_none() {
        return sgn + &"0".repeat(pad) + &digits;
    }
    let text = sgn + &digits;
    let fill = spec.fill.to_string();
    let align = spec
        .align
        .unwrap_or(if numeric { Align::Right } else { Align::Left });
    match align {
        Align::Left => text + &fill.repeat(pad),
        Align::Right => fill.repeat(pad) + &text,
        Align::Center => fill.repeat(pad / 2) + &text + &fill.repeat(pad - pad / 2),
    }
}

// ---------------------------------------------------------------------------------------
// Standalone binary `main` entry/exit
// ---------------------------------------------------------------------------------------

/// Parses `--seed N` and `--backend SPEC` (or `NOEV_BACKEND`) from `argv` and initializes a
/// [`NoevRuntime`] with stdout/stderr for a compiled standalone binary.
///
/// # Safety
/// `argv` must be null or point to `argc` C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_main_enter(
    argc: i32,
    argv: *const *const c_char,
) -> *mut NoevRuntime {
    let mut seed = 0u64;
    let mut backend_spec: Option<String> = None;
    let mut circuit_opt: Option<u8> = None;
    let mut overall_opt: Option<u8> = None;
    if !argv.is_null() && argc > 1 {
        let args = unsafe { std::slice::from_raw_parts(argv, argc as usize) };
        let mut i = 1;
        while i < args.len() {
            if let Ok(s) = unsafe { CStr::from_ptr(args[i]) }.to_str() {
                if s == "--" {
                    break;
                }
                if let Some(rest) = s.strip_prefix("--seed=") {
                    if let Ok(n) = rest.parse() {
                        seed = n;
                    }
                } else if s == "--seed"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                    && let Ok(n) = next.parse()
                {
                    seed = n;
                    i += 1;
                } else if let Some(rest) = s.strip_prefix("--backend=") {
                    backend_spec = Some(rest.to_string());
                } else if s == "--backend"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                {
                    backend_spec = Some(next.to_string());
                    i += 1;
                } else if let Some(rest) = s.strip_prefix("--circuit-opt=") {
                    if let Ok(n) = rest.parse::<u8>()
                        && n <= 3
                    {
                        circuit_opt = Some(n);
                    }
                } else if s == "--circuit-opt"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                    && let Ok(n) = next.parse::<u8>()
                    && n <= 3
                {
                    circuit_opt = Some(n);
                    i += 1;
                } else if let Some(rest) = s
                    .strip_prefix("--opt-level=")
                    .or_else(|| s.strip_prefix("-O"))
                    && !rest.is_empty()
                {
                    if let Ok(n) = rest.parse::<u8>()
                        && n <= 3
                    {
                        overall_opt = Some(n);
                    }
                } else if (s == "--opt-level" || s == "-O")
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                    && let Ok(n) = next.parse::<u8>()
                    && n <= 3
                {
                    overall_opt = Some(n);
                    i += 1;
                }
            }
            i += 1;
        }
    }
    if circuit_opt.is_none()
        && let Ok(v) = std::env::var("NOEV_CIRCUIT_OPT")
        && let Ok(n) = v.parse::<u8>()
        && n <= 3
    {
        circuit_opt = Some(n);
    }
    if overall_opt.is_none()
        && let Ok(v) = std::env::var("NOEV_OPT_LEVEL")
        && let Ok(n) = v.parse::<u8>()
        && n <= 3
    {
        overall_opt = Some(n);
    }
    #[cfg(feature = "ketrs")]
    let effective_circuit_opt = circuit_opt
        .or(overall_opt)
        .unwrap_or(crate::backend::ketrs::DEFAULT_OPT_LEVEL);
    #[cfg(not(feature = "ketrs"))]
    let _ = (circuit_opt, overall_opt);
    if backend_spec.is_none()
        && let Ok(env_spec) = std::env::var("NOEV_BACKEND")
        && !env_spec.is_empty()
    {
        backend_spec = Some(env_spec);
    }
    if let Some(spec) = backend_spec.as_deref() {
        if spec == "export" {
            let rt = cabi::noev_rt_new_export(seed);
            if !rt.is_null() {
                unsafe { &mut *rt }.set_output(Rc::new(std::cell::RefCell::new(crate::Output {
                    stdout: Box::new(std::io::stdout()),
                    stderr: Box::new(std::io::stderr()),
                })));
            }
            return rt;
        }
        #[cfg(feature = "ketrs")]
        if let Some(ketrs_backend) =
            crate::backend::ketrs::KetrsBackend::from_spec(spec, effective_circuit_opt)
        {
            let mut rt = NoevRuntime::new_owned(Box::new(ketrs_backend), seed);
            rt.set_output(Rc::new(std::cell::RefCell::new(crate::Output {
                stdout: Box::new(std::io::stdout()),
                stderr: Box::new(std::io::stderr()),
            })));
            return Box::into_raw(Box::new(rt));
        }
        if spec == "ketrs" || spec.starts_with("ketrs:") {
            eprintln!("error: runtime built without `ketrs` simulator");
            std::process::exit(1);
        }
        match crate::plugin::load_plugin_spec(spec) {
            Ok(plugin) => {
                let mut rt = NoevRuntime::new_owned(Box::new(plugin), seed);
                rt.set_output(Rc::new(std::cell::RefCell::new(crate::Output {
                    stdout: Box::new(std::io::stdout()),
                    stderr: Box::new(std::io::stderr()),
                })));
                return Box::into_raw(Box::new(rt));
            }
            Err(err) => {
                eprintln!("error: {}", err.0);
                std::process::exit(1);
            }
        }
    }
    #[cfg(feature = "ketrs")]
    {
        cabi::noev_rt_new_ketrs_with_opt(seed, effective_circuit_opt)
    }
    #[cfg(not(feature = "ketrs"))]
    {
        let rt = cabi::noev_rt_new_export(seed);
        if !rt.is_null() {
            unsafe { &mut *rt }.set_output(Rc::new(std::cell::RefCell::new(Output {
                stdout: Box::new(std::io::stdout()),
                stderr: Box::new(std::io::stderr()),
            })));
        }
        rt
    }
}

/// Flushes output, reports any runtime error on `rt` to stderr, frees `rt`, and returns the
/// process exit code (`0` on success, `1` on failure).
///
/// # Safety
/// `rt` must be a valid pointer from [`noev_rt_main_enter`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_main_exit(rt: *mut NoevRuntime, ok: bool) -> i32 {
    if rt.is_null() {
        return i32::from(!ok);
    }
    let flushed = unsafe { cabi::noev_rt_flush(rt) };
    let code = if ok && flushed {
        0
    } else {
        let mut len = 0usize;
        let ptr = unsafe { cabi::noev_rt_error_message(rt, &mut len) };
        if !ptr.is_null() && len > 0 {
            let msg = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) });
            eprintln!("error: {msg}");
        } else {
            eprintln!("error: runtime error");
        }
        1
    };
    unsafe { cabi::noev_rt_free(rt) };
    code
}

// ---------------------------------------------------------------------------------------
// Arbitrary-precision `Int` C ABI
// ---------------------------------------------------------------------------------------

/// Encodes an `i64` into a tagged `Int`.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_from_i64(rt: *mut NoevRuntime, v: i64) -> i64 {
    encode_int(unsafe { &mut *rt }, Int::Small(v))
}

/// Encodes a `u64` into a tagged `Int`.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_from_u64(rt: *mut NoevRuntime, v: u64) -> i64 {
    encode_int(unsafe { &mut *rt }, Int::from_i128(i128::from(v)))
}

/// Parses a big-integer literal into a tagged `Int`.
///
/// # Safety
/// `rt` must be valid; `ptr` must point to `len` UTF-8 bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_parse(
    rt: *mut NoevRuntime,
    ptr: *const u8,
    len: usize,
    out: *mut i64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let text = unsafe { desc_str(ptr, len) };
    match Int::parse(text) {
        Some(v) => {
            unsafe { *out = encode_int(rt, v) };
            true
        }
        None => rt.fail_err(RuntimeError::new(format!("invalid integer `{text}`"))),
    }
}

/// Converts a tagged `Int` to `i64` (fails if out of `i64` range).
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_to_i64(rt: *mut NoevRuntime, v: i64, out: *mut i64) -> bool {
    let rt = unsafe { &mut *rt };
    let val = decode_int(rt, v);
    match val.to_i64() {
        Some(n) => {
            unsafe { *out = n };
            true
        }
        None => rt.fail_err(RuntimeError::new(format!("{val} is too large here"))),
    }
}

/// Converts a tagged `Int` to `f64`.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_to_f64(rt: *const NoevRuntime, v: i64) -> f64 {
    decode_int(unsafe { &*rt }, v).to_f64()
}

/// Converts a `Float` (`f64`) to a tagged `Int` (truncating toward zero; fails on NaN/inf).
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_from_f64(rt: *mut NoevRuntime, x: f64, out: *mut i64) -> bool {
    let rt = unsafe { &mut *rt };
    match Int::from_f64(x.trunc()) {
        Some(v) => {
            unsafe { *out = encode_int(rt, v) };
            true
        }
        None => rt.fail_err(RuntimeError::new(format!("cannot convert `{x}` to `Int`"))),
    }
}

/// Unary op on `Int`: `op = 0` (`-x`), `1` (`~x`), `2` (`abs`).
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_unary(rt: *mut NoevRuntime, op: u8, a: i64) -> i64 {
    let rt = unsafe { &mut *rt };
    let v = decode_int(rt, a);
    let res = match op {
        0 => v.neg(),
        1 => v.not(),
        2 => v.abs(),
        _ => v,
    };
    encode_int(rt, res)
}

/// Binary arithmetic/bitwise op on `Int`:
/// `0`=Add, `1`=Sub, `2`=Mul, `3`=Div, `4`=Rem, `5`=BitAnd, `6`=BitOr, `7`=BitXor,
/// `8`=Shl, `9`=Shr, `10`=Pow, `11`=Min, `12`=Max.
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_binary(
    rt: *mut NoevRuntime,
    op: u8,
    a: i64,
    b: i64,
    out: *mut i64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let a = decode_int(rt, a);
    let b = decode_int(rt, b);
    let res = match op {
        0 => a.add(&b),
        1 => a.sub(&b),
        2 => a.mul(&b),
        3 => match a.div(&b) {
            Some(v) => v,
            None => return rt.fail_err(RuntimeError::new("division by zero")),
        },
        4 => match a.rem(&b) {
            Some(v) => v,
            None => return rt.fail_err(RuntimeError::new("remainder by zero")),
        },
        5 => a.and(&b),
        6 => a.or(&b),
        7 => a.xor(&b),
        8 => {
            let Some(shift) = b.to_i64().and_then(|s| u32::try_from(s).ok()) else {
                return rt.fail_err(RuntimeError::new(format!("cannot shift by {b}")));
            };
            a.shl(shift)
        }
        9 => {
            let Some(shift) = b.to_i64().and_then(|s| u32::try_from(s).ok()) else {
                return rt.fail_err(RuntimeError::new(format!("cannot shift by {b}")));
            };
            a.shr(shift)
        }
        10 => {
            let Some(exp) = b.to_i64().and_then(|s| u32::try_from(s).ok()) else {
                return rt.fail_err(RuntimeError::new(format!("cannot raise to the power {b}")));
            };
            Int::from(a.to_big().pow(exp))
        }
        11 => {
            if a <= b {
                a
            } else {
                b
            }
        }
        12 => {
            if a >= b {
                a
            } else {
                b
            }
        }
        _ => return rt.fail_err(RuntimeError::new("unknown Int op")),
    };
    unsafe { *out = encode_int(rt, res) };
    true
}

/// Compares two tagged `Int`s: returns `-1` (`<`), `0` (`==`), or `1` (`>`).
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_cmp(rt: *const NoevRuntime, a: i64, b: i64) -> i8 {
    if (a & 1) == 0 && (b & 1) == 0 {
        return match a.cmp(&b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    let rt = unsafe { &*rt };
    match decode_int(rt, a).cmp(&decode_int(rt, b)) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

// ---------------------------------------------------------------------------------------
// Checked sized-integer C ABI (exact overflow messages matching `sized.rs`)
// ---------------------------------------------------------------------------------------

fn width_from_raw(bits: u32, signed: u8) -> Width {
    Width {
        bits,
        signed: signed != 0,
    }
}

fn raw_to_i128(v: u64, w: Width) -> i128 {
    if w.signed {
        match w.bits {
            8 => i128::from(v as i8),
            16 => i128::from(v as i16),
            32 => i128::from(v as i32),
            _ => i128::from(v as i64),
        }
    } else {
        match w.bits {
            8 => i128::from(v as u8),
            16 => i128::from(v as u16),
            32 => i128::from(v as u32),
            _ => i128::from(v),
        }
    }
}

/// Checked unary `-v` on a sized integer.
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sized_neg(
    rt: *mut NoevRuntime,
    v: u64,
    bits: u32,
    signed: u8,
    out: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let w = width_from_raw(bits, signed);
    match crate::sized::neg(raw_to_i128(v, w), w) {
        Ok(Value::Sized(res, _)) => {
            unsafe { *out = res as u64 };
            true
        }
        Ok(_) => false,
        Err(err) => rt.fail_err(err),
    }
}

/// Extracts `bits` bits (MSB first) from unsigned sized integer `v` into `out[0..bits]`.
///
/// # Safety
/// `out` must point to at least `bits` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sized_bits(v: u64, bits: u32, out: *mut u8) {
    let dst = unsafe { std::slice::from_raw_parts_mut(out, bits as usize) };
    for (i, slot) in dst.iter_mut().enumerate() {
        *slot = ((v >> (bits - 1 - i as u32)) & 1) as u8;
    }
}

/// Packs `bits` bits (MSB first) from `src[0..bits]` into an unsigned sized integer.
///
/// # Safety
/// `src` must point to at least `bits` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sized_from_bits(src: *const u8, bits: u32) -> u64 {
    let slice = unsafe { std::slice::from_raw_parts(src, bits as usize) };
    let mut acc = 0u64;
    for &b in slice {
        acc = (acc << 1) | u64::from(b != 0);
    }
    acc
}

/// Checked binary op on sized integers (`b_is_int`: 1 when shift RHS is a tagged `Int`).
/// `op`: `0`=Add,`1`=Sub,`2`=Mul,`3`=Div,`4`=Rem,`5`=BitAnd,`6`=BitOr,`7`=BitXor,`8`=Shl,`9`=Shr.
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sized_binary(
    rt: *mut NoevRuntime,
    op: u8,
    a: u64,
    b: u64,
    bits: u32,
    signed: u8,
    b_is_int: u8,
    out: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let w = width_from_raw(bits, signed);
    let bin = match op {
        0 => BinOp::Add,
        1 => BinOp::Sub,
        2 => BinOp::Mul,
        3 => BinOp::Div,
        4 => BinOp::Rem,
        5 => BinOp::BitAnd,
        6 => BinOp::BitOr,
        7 => BinOp::BitXor,
        8 => BinOp::Shl,
        9 => BinOp::Shr,
        _ => return rt.fail_err(RuntimeError::new("unknown sized op")),
    };
    let rhs = if b_is_int != 0 {
        Value::Int(decode_int(rt, b as i64))
    } else {
        Value::Sized(raw_to_i128(b, w), w)
    };
    match crate::sized::binary(bin, raw_to_i128(a, w), w, &rhs) {
        Ok(Value::Sized(res, _)) => {
            unsafe { *out = res as u64 };
            true
        }
        Ok(_) => false,
        Err(err) => rt.fail_err(err),
    }
}

/// Checked conversion `T(x)` to a sized integer width `(bits, signed)`: fails on overflow.
/// `src_kind`: `0` = tagged `Int`, `1` = sized (`src_bits`, `src_signed`), `2` = `f64`, `3` = `f32`.
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sized_convert(
    rt: *mut NoevRuntime,
    src_kind: u8,
    raw: u64,
    src_bits: u32,
    src_signed: u8,
    dst_bits: u32,
    dst_signed: u8,
    out: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let dst = width_from_raw(dst_bits, dst_signed);
    let val = match src_kind {
        0 => Value::Int(decode_int(rt, raw as i64)),
        1 => {
            let sw = width_from_raw(src_bits, src_signed);
            Value::Sized(raw_to_i128(raw, sw), sw)
        }
        2 => {
            let f = f64::from_bits(raw).trunc();
            let Some(i) = Int::from_f64(f) else {
                return rt.fail_err(RuntimeError::new(format!(
                    "cannot convert `{}` to `{}`",
                    f64::from_bits(raw),
                    dst.name()
                )));
            };
            Value::Int(i)
        }
        3 => {
            let f = f64::from(f32::from_bits(raw as u32)).trunc();
            let Some(i) = Int::from_f64(f) else {
                return rt.fail_err(RuntimeError::new(format!(
                    "cannot convert `{}` to `{}`",
                    f32::from_bits(raw as u32),
                    dst.name()
                )));
            };
            Value::Int(i)
        }
        _ => return rt.fail_err(RuntimeError::new("invalid sized conversion")),
    };
    let n = match &val {
        Value::Int(i) => i.to_i128(),
        Value::Sized(v, _) => Some(*v),
        _ => None,
    };
    match n.filter(|&v| dst.holds(v)) {
        Some(v) => {
            unsafe { *out = v as u64 };
            true
        }
        None => rt.fail_err(RuntimeError::new(format!(
            "{val} does not fit in `{}`, which holds {} to {}",
            dst.name(),
            dst.min(),
            dst.max()
        ))),
    }
}

/// `T::try_from(x)` for a sized integer width `(dst_bits, dst_signed)`: writes `Result<T, String>`
/// into `out_ptr` (described by `result_desc`).
///
/// # Safety
/// `rt`, `out_ptr`, and `result_desc` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sized_try_from(
    rt: *mut NoevRuntime,
    src_kind: u8,
    raw: u64,
    src_bits: u32,
    src_signed: u8,
    dst_bits: u32,
    dst_signed: u8,
    out_ptr: *mut u8,
    result_desc: *const NoevTypeDesc,
) -> bool {
    let rt = unsafe { &mut *rt };
    let dst = width_from_raw(dst_bits, dst_signed);
    let val = match src_kind {
        0 => Value::Int(decode_int(rt, raw as i64)),
        1 => {
            let sw = width_from_raw(src_bits, src_signed);
            Value::Sized(raw_to_i128(raw, sw), sw)
        }
        _ => return rt.fail_err(RuntimeError::new("invalid try_from source")),
    };
    match crate::sized::try_from(&val, dst) {
        Ok(res) => {
            unsafe { write_native(rt, &res, out_ptr, &*result_desc) };
            true
        }
        Err(err) => rt.fail_err(err),
    }
}

// ---------------------------------------------------------------------------------------
// Float, Complex, c32, and FFT math helpers
// ---------------------------------------------------------------------------------------

/// `f64` transcendental / unary / binary math helper.
/// `op`: `0`=sin,`1`=cos,`2`=tan,`3`=asin,`4`=acos,`5`=atan,`6`=atan2,`7`=sqrt,`8`=exp,
/// `9`=ln,`10`=abs,`11`=min,`12`=max,`13`=trunc,`14`=round,`15`=floor,`16`=ceil,`17`=pow.
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_f64_math(op: u8, a: f64, b: f64) -> f64 {
    match op {
        0 => a.sin(),
        1 => a.cos(),
        2 => a.tan(),
        3 => a.asin(),
        4 => a.acos(),
        5 => a.atan(),
        6 => a.atan2(b),
        7 => a.sqrt(),
        8 => a.exp(),
        9 => a.ln(),
        10 => a.abs(),
        11 => a.min(b),
        12 => a.max(b),
        13 => a.trunc(),
        14 => a.round(),
        15 => a.floor(),
        16 => a.ceil(),
        17 => a.powf(b),
        18 => a % b,
        _ => f64::NAN,
    }
}

/// `f32` transcendental / unary / binary math helper (same op codes as [`noev_rt_f64_math`]).
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_f32_math(op: u8, a: f32, b: f32) -> f32 {
    match op {
        0 => a.sin(),
        1 => a.cos(),
        2 => a.tan(),
        3 => a.asin(),
        4 => a.acos(),
        5 => a.atan(),
        6 => a.atan2(b),
        7 => a.sqrt(),
        8 => a.exp(),
        9 => a.ln(),
        10 => a.abs(),
        11 => a.min(b),
        12 => a.max(b),
        13 => a.trunc(),
        14 => a.round(),
        15 => a.floor(),
        16 => a.ceil(),
        17 => a.powf(b),
        18 => a % b,
        _ => f32::NAN,
    }
}

/// Complex `f64` operations: `0`=div, `1`=exp, `2`=sqrt, `3`=polar(r, theta), `4`=round.
///
/// # Safety
/// `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_complex_op(
    op: u8,
    ar: f64,
    ai: f64,
    br: f64,
    bi: f64,
    out: *mut NoevComplex,
) {
    let (re, im) = match op {
        0 => {
            let denom = br * br + bi * bi;
            ((ar * br + ai * bi) / denom, (ai * br - ar * bi) / denom)
        }
        1 => {
            let r = ar.exp();
            (r * ai.cos(), r * ai.sin())
        }
        2 => {
            let r = ar.hypot(ai);
            let re = ((r + ar) / 2.0).max(0.0).sqrt();
            let im = ((r - ar) / 2.0).max(0.0).sqrt().copysign(ai);
            (re, im)
        }
        3 => (ar * ai.cos(), ar * ai.sin()),
        4 => (ar.round(), ai.round()),
        _ => (0.0, 0.0),
    };
    unsafe { *out = NoevComplex { re, im } };
}

/// Complex `f64` scalar methods: `0`=abs (`hypot`), `1`=arg (`atan2`).
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_complex_scalar(op: u8, re: f64, im: f64) -> f64 {
    match op {
        0 => re.hypot(im),
        1 => im.atan2(re),
        _ => 0.0,
    }
}

/// Complex `f32` (`c32`) operations (`out` is two contiguous `f32`s: `[re, im]`).
///
/// # Safety
/// `out` must point to 2 writable `f32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_c32_op(op: u8, ar: f32, ai: f32, br: f32, bi: f32, out: *mut f32) {
    let mut c = NoevComplex::default();
    unsafe {
        noev_rt_complex_op(
            op,
            f64::from(ar),
            f64::from(ai),
            f64::from(br),
            f64::from(bi),
            &mut c,
        );
        *out = c.re as f32;
        *out.add(1) = c.im as f32;
    }
}

/// `fft` / `ifft` on `[Complex; N]` (`is_c32 == 0`) or `[c32; N]` (`is_c32 != 0`).
///
/// # Safety
/// `input` and `output` must point to `len` complex numbers of the selected precision.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_fft(
    input: *const u8,
    output: *mut u8,
    len: usize,
    inverse: bool,
    is_c32: bool,
) {
    if is_c32 {
        let src = unsafe { std::slice::from_raw_parts(input.cast::<f32>(), len * 2) };
        let pairs: Vec<(f64, f64)> = src
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| (f64::from(c[0]), f64::from(c[1])))
            .collect();
        let res = crate::fourier::transform(&pairs, inverse);
        let dst = unsafe { std::slice::from_raw_parts_mut(output.cast::<f32>(), len * 2) };
        for (i, (re, im)) in res.into_iter().enumerate() {
            dst[2 * i] = re as f32;
            dst[2 * i + 1] = im as f32;
        }
    } else {
        let src = unsafe { std::slice::from_raw_parts(input.cast::<NoevComplex>(), len) };
        let pairs: Vec<(f64, f64)> = src.iter().map(|c| (c.re, c.im)).collect();
        let res = crate::fourier::transform(&pairs, inverse);
        let dst = unsafe { std::slice::from_raw_parts_mut(output.cast::<NoevComplex>(), len) };
        for (slot, (re, im)) in dst.iter_mut().zip(res) {
            *slot = NoevComplex { re, im };
        }
    }
}

// ---------------------------------------------------------------------------------------
// Strings, formatting, printing, and deep value equality
// ---------------------------------------------------------------------------------------

/// Creates a `String` handle from UTF-8 bytes.
///
/// # Safety
/// `rt` must be valid; `ptr` must point to `len` UTF-8 bytes when `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_str_from_bytes(
    rt: *mut NoevRuntime,
    ptr: *const u8,
    len: usize,
) -> u64 {
    let rt = unsafe { &mut *rt };
    let text = unsafe { desc_str(ptr, len) }.to_string();
    store_str(rt, text)
}

/// Formats an interpolated string from `n_parts` [`NoevFormatPart`] entries, returning a
/// `String` handle.
///
/// # Safety
/// `rt` must be valid; `parts` must point to `n_parts` valid entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_format(
    rt: *mut NoevRuntime,
    parts: *const NoevFormatPart,
    n_parts: usize,
) -> u64 {
    let rt = unsafe { &mut *rt };
    let slice = if n_parts == 0 || parts.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(parts, n_parts) }
    };
    let mut out = String::new();
    for p in slice {
        if p.is_value == 0 {
            out.push_str(unsafe { desc_str(p.ptr, p.len) });
        } else {
            let desc = unsafe { &*p.ty };
            let val = unsafe { read_native(rt, p.ptr, desc) };
            let spec = if p.has_spec != 0 {
                Some(FormatSpec {
                    fill: char::from_u32(p.fill).unwrap_or(' '),
                    align: match p.align {
                        1 => Some(Align::Left),
                        2 => Some(Align::Right),
                        3 => Some(Align::Center),
                        _ => None,
                    },
                    plus: (p.flags & 1) != 0,
                    zero: (p.flags & 2) != 0,
                    width: ((p.flags & 4) != 0).then_some(p.width),
                    precision: ((p.flags & 8) != 0).then_some(p.precision),
                    kind: match p.kind {
                        1 => Some(FormatKind::Binary),
                        2 => Some(FormatKind::Octal),
                        3 => Some(FormatKind::Hex),
                        4 => Some(FormatKind::HexUpper),
                        5 => Some(FormatKind::Exponent),
                        _ => None,
                    },
                    span: noev_syntax::Span::default(),
                })
            } else {
                None
            };
            out.push_str(&format_desc(&val, desc, spec.as_ref()));
        }
    }
    store_str(rt, out)
}

/// Executes or records inline OpenQASM 3 statements whose formatted template is stored at
/// `str_handle` in `rt.strings`.
///
/// # Safety
/// `rt` must be valid; `qubits` must point to `n_qubits` valid [`NoevQubit`] handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_qis_qasm_str(
    rt: *mut NoevRuntime,
    str_handle: u64,
    qubits: *const NoevQubit,
    n_qubits: usize,
) -> bool {
    let template = {
        let rt_ref = unsafe { &*rt };
        rt_ref
            .strings
            .get(str_handle as usize)
            .map_or("", String::as_str)
            .to_string()
    };
    let bytes = template.as_bytes();
    unsafe { cabi::noev_qis_qasm(rt, bytes.as_ptr(), bytes.len(), qubits, n_qubits) }
}

/// Dispatches a standard-library operation (`Vec<T>`, `String`, `std::{io, env, fs, net}`)
/// from compiled native code (`Stmt::StdCall`).
///
/// # Safety
/// `rt` must be valid; `args` must point to `n_args` valid [`NoevPrintArg`] entries;
/// when `out_ptr` is non-null, `out_ty` must describe the destination slot at `out_ptr`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_std_call(
    rt: *mut NoevRuntime,
    op: u32,
    args: *const NoevPrintArg,
    n_args: usize,
    out_ptr: *mut u8,
    out_ty: *const NoevTypeDesc,
) -> bool {
    let rt_ref = unsafe { &mut *rt };
    let Some(std_op) = noev_sema::prelude::StdOp::from_u32(op) else {
        return rt_ref.fail_err(RuntimeError::new(format!("unknown std operation id {op}")));
    };
    let slice = if n_args == 0 || args.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(args, n_args) }
    };
    let mut vals = Vec::with_capacity(slice.len());
    for arg in slice {
        let desc = unsafe { &*arg.ty };
        vals.push(unsafe { read_native(rt_ref, arg.ptr, desc) });
    }
    match crate::std_ops::eval(rt_ref, std_op, vals) {
        Ok(res) => {
            if !out_ptr.is_null() && !out_ty.is_null() {
                unsafe { write_native(rt_ref, &res, out_ptr, &*out_ty) };
            }
            true
        }
        Err(err) => rt_ref.fail_err(err),
    }
}

/// Prints `n_args` values separated by spaces and a trailing newline.
///
/// # Safety
/// `rt` must be valid; `args` must point to `n_args` valid [`NoevPrintArg`]s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_print(
    rt: *mut NoevRuntime,
    stderr: bool,
    args: *const NoevPrintArg,
    n_args: usize,
) -> bool {
    let rt_ref = unsafe { &*rt };
    let slice = if n_args == 0 || args.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(args, n_args) }
    };
    let mut line = String::new();
    for (i, arg) in slice.iter().enumerate() {
        if i > 0 {
            line.push(' ');
        }
        let desc = unsafe { &*arg.ty };
        let val = unsafe { read_native(rt_ref, arg.ptr, desc) };
        display_desc(&val, desc, &mut line);
    }
    line.push('\n');
    let bytes = line.as_bytes();
    unsafe { cabi::noev_rt_write(rt, stderr, bytes.as_ptr(), bytes.len()) }
}

/// Deep equality of two native values of the same type `desc`.
///
/// # Safety
/// `rt`, `a`, `b`, and `desc` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_val_eq(
    rt: *const NoevRuntime,
    a: *const u8,
    b: *const u8,
    desc: *const NoevTypeDesc,
) -> u8 {
    let rt = unsafe { &*rt };
    let desc = unsafe { &*desc };
    let va = unsafe { read_native(rt, a, desc) };
    let vb = unsafe { read_native(rt, b, desc) };
    u8::from(va == vb)
}

// ---------------------------------------------------------------------------------------
// Observables & Distributions
// ---------------------------------------------------------------------------------------

/// Creates a single-qubit Pauli observable (`pauli`: 0=X, 1=Y, 2=Z) on `index` (tagged `Int`).
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_obs_pauli(
    rt: *mut NoevRuntime,
    pauli: u8,
    index: i64,
    out: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let idx_int = decode_int(rt, index);
    let Ok(idx) = idx_int
        .to_i64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or(())
    else {
        return rt.fail_err(RuntimeError::new(format!(
            "a Pauli operator's qubit index cannot be negative: {idx_int}"
        )));
    };
    let p = match pauli {
        0 => Pauli::X,
        1 => Pauli::Y,
        _ => Pauli::Z,
    };
    unsafe { *out = store_obs(rt, Observable::pauli(p, idx)) };
    true
}

/// Creates the identity observable `I()`.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_obs_identity(rt: *mut NoevRuntime) -> u64 {
    store_obs(unsafe { &mut *rt }, Observable::identity())
}

/// Creates a projector `P(index, bit)` (`x_basis == 0`) or `PX(index, bit)` (`x_basis != 0`).
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_obs_projector(
    rt: *mut NoevRuntime,
    x_basis: bool,
    index: i64,
    bit: u8,
    out: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let idx_int = decode_int(rt, index);
    let Ok(idx) = idx_int
        .to_i64()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or(())
    else {
        return rt.fail_err(RuntimeError::new(format!(
            "a projector's qubit index cannot be negative: {idx_int}"
        )));
    };
    let sign = if bit != 0 { -0.5 } else { 0.5 };
    let pauli = if x_basis { Pauli::X } else { Pauli::Z };
    let obs = Observable::identity()
        .scale(0.5)
        .add(&Observable::pauli(pauli, idx).scale(sign));
    unsafe { *out = store_obs(rt, obs) };
    true
}

/// Negates an observable.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_obs_neg(rt: *mut NoevRuntime, obs: u64) -> u64 {
    let rt = unsafe { &mut *rt };
    let neg = read_obs(rt, obs).scale(-1.0);
    store_obs(rt, neg)
}

/// Observable binary op: `0`=Add, `1`=Sub, `2`=Mul(obs, obs), `3`=Scale(obs, f64).
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_obs_binary(
    rt: *mut NoevRuntime,
    op: u8,
    a: u64,
    b: u64,
    scale: f64,
    out: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let res = match op {
        0 => read_obs(rt, a).add(read_obs(rt, b)),
        1 => read_obs(rt, a).add(&read_obs(rt, b).scale(-1.0)),
        2 => match read_obs(rt, a).mul(read_obs(rt, b)) {
            Ok(obs) => obs,
            Err(msg) => return rt.fail_err(RuntimeError::new(msg)),
        },
        3 => read_obs(rt, a).scale(scale),
        _ => Observable::default(),
    };
    unsafe { *out = store_obs(rt, res) };
    true
}

/// `dist.shots() -> Int` (tagged `Int`).
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_dist_shots(rt: *mut NoevRuntime, dist: u64) -> i64 {
    let rt = unsafe { &mut *rt };
    let shots = rt
        .distributions
        .get(dist as usize)
        .map_or(0, |d| d.shots as i64);
    encode_int(rt, Int::Small(shots))
}

/// `dist.count(x) -> Int` (tagged `Int`).
///
/// # Safety
/// `rt`, `val`, `desc`, and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_dist_count(
    rt: *mut NoevRuntime,
    dist: u64,
    val: *const u8,
    desc: *const NoevTypeDesc,
    out: *mut i64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let Some(d) = rt.distributions.get(dist as usize).cloned() else {
        return rt.fail_err(RuntimeError::new("invalid distribution"));
    };
    if d.not_run {
        return rt.fail_err(RuntimeError::unknown(
            "the program reads a `sample`'s results",
        ));
    }
    let desc = unsafe { &*desc };
    let v = unsafe { read_native(rt, val, desc) };
    let mut key = String::new();
    display_desc(&v, desc, &mut key);
    unsafe { *out = encode_int(rt, Int::Small(d.count(&key) as i64)) };
    true
}

/// `dist.probability(x) -> Float`.
///
/// # Safety
/// `rt`, `val`, `desc`, and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_dist_probability(
    rt: *mut NoevRuntime,
    dist: u64,
    val: *const u8,
    desc: *const NoevTypeDesc,
    out: *mut f64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let Some(d) = rt.distributions.get(dist as usize).cloned() else {
        return rt.fail_err(RuntimeError::new("invalid distribution"));
    };
    if d.not_run {
        return rt.fail_err(RuntimeError::unknown(
            "the program reads a `sample`'s results",
        ));
    }
    let desc = unsafe { &*desc };
    let v = unsafe { read_native(rt, val, desc) };
    let mut key = String::new();
    display_desc(&v, desc, &mut key);
    unsafe { *out = d.count(&key) as f64 / d.shots as f64 };
    true
}

/// `dist.most_likely() -> T`, writing the result into `out_ptr`.
///
/// # Safety
/// `rt`, `out_ptr`, and `desc` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_dist_most_likely(
    rt: *mut NoevRuntime,
    dist: u64,
    out_ptr: *mut u8,
    desc: *const NoevTypeDesc,
) -> bool {
    let rt = unsafe { &mut *rt };
    let Some(d) = rt.distributions.get(dist as usize).cloned() else {
        return rt.fail_err(RuntimeError::new("invalid distribution"));
    };
    if d.not_run {
        return rt.fail_err(RuntimeError::unknown(
            "the program reads a `sample`'s results",
        ));
    }
    let Some(first) = d.outcomes.first() else {
        return rt.fail_err(RuntimeError::new("empty distribution"));
    };
    unsafe { write_native(rt, &first.value, out_ptr, &*desc) };
    true
}

// ---------------------------------------------------------------------------------------
// Computed labels & unitary matrices
// ---------------------------------------------------------------------------------------

/// Prepares a fresh `qubit` in the state given by a `State` enum tag (`0..=5`).
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_prepare_state(
    rt: *mut NoevRuntime,
    q: NoevQubit,
    tag: u32,
) -> bool {
    let rt = unsafe { &mut *rt };
    let Some(label) = noev_syntax::state::Label::from_index(tag) else {
        return rt.fail_err(RuntimeError::new(format!("no State variant {tag}")));
    };
    for &gate in noev_ir::ir::label_gates(label) {
        if let Err(err) = rt.emit_gate_op(gate, false, Vec::new(), vec![q.0]) {
            return rt.fail_err(err);
        }
    }
    true
}

/// Turns `qubit` into the Z basis from a `State` enum tag (`0..=5`).
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_into_z_state(
    rt: *mut NoevRuntime,
    q: NoevQubit,
    tag: u32,
) -> bool {
    let rt = unsafe { &mut *rt };
    let Some(label) = noev_syntax::state::Label::from_index(tag) else {
        return rt.fail_err(RuntimeError::new(format!("no State variant {tag}")));
    };
    for &(gate, adjoint) in noev_ir::ir::into_z(label) {
        if let Err(err) = rt.emit_gate_op(gate, adjoint, Vec::new(), vec![q.0]) {
            return rt.fail_err(err);
        }
    }
    true
}

/// Returns the expected Z-basis measurement bit (`0` or `1`) for a `State` variant tag.
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_label_bit_state(tag: u32) -> u8 {
    noev_syntax::state::Label::from_index(tag).map_or(0, |l| u8::from(l.outcome()))
}

/// Validates a `dim x dim` `[[Complex; dim]; dim]` matrix and applies it as a unitary gate.
///
/// # Safety
/// `rt`, `targets`, and `matrix` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_unitary_matrix(
    rt: *mut NoevRuntime,
    targets: *const NoevQubit,
    n_targets: usize,
    matrix: *const NoevComplex,
    dim: usize,
    adjoint: bool,
) -> bool {
    let rt_ref = unsafe { &mut *rt };
    let slice = unsafe { std::slice::from_raw_parts(matrix, dim * dim) };
    let rows: Vec<Value> = slice
        .chunks_exact(dim)
        .map(|row| Value::Aggregate(row.iter().map(|c| Value::Complex(c.re, c.im)).collect()))
        .collect();
    let params = match crate::unitary::params(&Value::Aggregate(rows)) {
        Ok(p) => p,
        Err(msg) => return rt_ref.fail_err(RuntimeError::new(msg)),
    };
    unsafe {
        cabi::noev_qis_unitary(
            rt,
            targets,
            n_targets,
            params.as_ptr(),
            params.len(),
            adjoint,
        )
    }
}

// ---------------------------------------------------------------------------------------
// Compiled high-level quantum runs (`sample`, `Expectation`, `Overlap`, `Undo`)
// ---------------------------------------------------------------------------------------

fn measure_value_rt(rt: &mut NoevRuntime, val: Value) -> Result<Value, RuntimeError> {
    Ok(match val {
        Value::Qubit(q) => {
            let res = rt.measure_q(q)?;
            if res.is_clbit() {
                Value::Clbit(res.clbit_index())
            } else {
                Value::Bit(res.as_bit())
            }
        }
        Value::Aggregate(items) => Value::Aggregate(
            items
                .into_iter()
                .map(|v| measure_value_rt(rt, v))
                .collect::<Result<_, _>>()?,
        ),
        other => other,
    })
}

/// Runs a compiled `sample` block `shots` times and returns a `Distribution` handle.
///
/// # Safety
/// `rt`, `ret_desc`, `elem_desc`, `out_dist` must be valid; `args` must match `body`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_sample_compiled(
    rt: *mut NoevRuntime,
    shots_raw: i64,
    use_circuit: bool,
    body: NoevCompiledFn,
    args: *const u8,
    ret_desc: *const NoevTypeDesc,
    elem_desc: *const NoevTypeDesc,
    out_dist: *mut u64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let shots_int = decode_int(rt, shots_raw);
    let Ok(shots) = shots_int
        .to_i64()
        .and_then(|v| usize::try_from(v).ok())
        .filter(|&s| s > 0)
        .ok_or(())
    else {
        let msg = if shots_int.to_i64() == Some(0) {
            "`sample` needs at least one shot"
        } else {
            "`sample` needs a positive number of shots"
        };
        return rt.fail_err(RuntimeError::new(msg));
    };
    let ret_desc = unsafe { &*ret_desc };
    let elem_desc = unsafe { &*elem_desc };
    let ret_size = (ret_desc.size as usize).max(1);
    let seed = rt.next_seed();
    let mut seeds = SmallRng::seed_from_u64(seed);
    let mut counts: BTreeMap<String, Outcome> = BTreeMap::new();
    let mut add = |value: Value, count: usize| {
        let mut key = String::new();
        display_desc(&value, elem_desc, &mut key);
        counts
            .entry(key.clone())
            .or_insert_with(|| Outcome {
                key,
                value,
                count: 0,
            })
            .count += count;
    };

    if use_circuit {
        let mut builder = unsafe { rt.child_runtime(seeds.random()) };
        builder.set_mode(Mode::Build(Recording::default()));
        let mut ret_buf = vec![0u8; ret_size];
        if body(&mut builder, args, ret_buf.as_mut_ptr()) {
            let val = unsafe { read_native(&builder, ret_buf.as_ptr(), ret_desc) };
            if let Ok(template) = measure_value_rt(&mut builder, val) {
                let replay = builder.needs_values.get();
                let Mode::Build(recording) = std::mem::take(&mut builder.mode) else {
                    unreachable!();
                };
                if recording.circuit.clbits <= 64
                    && let Some(res) =
                        rt.backend()
                            .run_circuit(&recording.circuit, shots, seeds.random())
                {
                    let raw_counts = match res {
                        Ok(c) => c,
                        Err(e) => return rt.fail_err(RuntimeError::new(e.0)),
                    };
                    if !rt.backend().executes() {
                        let mut dist = Distribution::new(Vec::new(), shots);
                        dist.from_circuit = true;
                        dist.not_run = true;
                        unsafe { *out_dist = store_dist(rt, dist) };
                        return true;
                    }
                    if !replay {
                        for (bits, count) in raw_counts {
                            add(template.with_clbits(bits), count);
                        }
                    } else {
                        for (bits, count) in raw_counts {
                            let mut r = unsafe { rt.child_runtime(seeds.random()) };
                            r.set_mode(Mode::Build(Recording::knowing(bits)));
                            let mut buf = vec![0u8; ret_size];
                            if !body(&mut r, args, buf.as_mut_ptr()) {
                                let err = r.take_error();
                                return rt.fail_err(err);
                            }
                            let val = unsafe { read_native(&r, buf.as_ptr(), ret_desc) };
                            let val = match measure_value_rt(&mut r, val) {
                                Ok(v) => v,
                                Err(e) => return rt.fail_err(e),
                            };
                            add(val.with_clbits(bits), count);
                        }
                    }
                    let mut dist = Distribution::new(counts.into_values().collect(), shots);
                    dist.from_circuit = true;
                    unsafe { *out_dist = store_dist(rt, dist) };
                    return true;
                }
            }
        }
    }

    if !rt.backend().executes() {
        let mut dist = Distribution::new(Vec::new(), shots);
        dist.not_run = true;
        unsafe { *out_dist = store_dist(rt, dist) };
        return true;
    }

    let mut first = unsafe { rt.child_runtime(seeds.random()) };
    let mut ret_buf = vec![0u8; ret_size];
    if !body(&mut first, args, ret_buf.as_mut_ptr()) {
        let err = first.take_error();
        return rt.fail_err(err);
    }
    let val = unsafe { read_native(&first, ret_buf.as_ptr(), ret_desc) };
    let mut qubits = Vec::new();
    val.qubits(&mut qubits);
    let sampled = (!qubits.is_empty() && !first.measured())
        .then(|| first.sample_state(&qubits, shots))
        .flatten();
    if let Some(raw_counts) = sampled {
        for (bits, count) in raw_counts {
            add(val.with_bits(bits), count);
        }
    } else {
        let measured = match measure_value_rt(&mut first, val) {
            Ok(v) => v,
            Err(e) => return rt.fail_err(e),
        };
        add(measured, 1);
        for _ in 1..shots {
            let mut shot_rt = unsafe { rt.child_runtime(seeds.random()) };
            ret_buf.fill(0);
            if !body(&mut shot_rt, args, ret_buf.as_mut_ptr()) {
                let err = shot_rt.take_error();
                return rt.fail_err(err);
            }
            let v = unsafe { read_native(&shot_rt, ret_buf.as_ptr(), ret_desc) };
            let m = match measure_value_rt(&mut shot_rt, v) {
                Ok(v) => v,
                Err(e) => return rt.fail_err(e),
            };
            add(m, 1);
        }
    }
    let dist = Distribution::new(counts.into_values().collect(), shots);
    unsafe { *out_dist = store_dist(rt, dist) };
    true
}

/// Runs a compiled preparation thunk (or ket label string when `body` is null) and returns
/// its prepared qubit indices in order.
fn run_prep(
    rt: &mut NoevRuntime,
    ket_ptr: *const u8,
    ket_len: usize,
    body: Option<NoevCompiledFn>,
    args: *const u8,
    ret_desc: *const NoevTypeDesc,
) -> Result<Vec<usize>, RuntimeError> {
    if let Some(f) = body {
        let desc = unsafe { &*ret_desc };
        let mut buf = vec![0u8; (desc.size as usize).max(1)];
        if !f(rt, args, buf.as_mut_ptr()) {
            return Err(rt.take_error());
        }
        let val = unsafe { read_native(rt, buf.as_ptr(), desc) };
        let mut qubits = Vec::new();
        val.qubits(&mut qubits);
        Ok(qubits)
    } else {
        let state = unsafe { desc_str(ket_ptr, ket_len) };
        let mut qubits = Vec::new();
        for label in noev_syntax::state::labels(state) {
            let q = rt.alloc_q()?;
            for &gate in noev_ir::ir::label_gates(label) {
                rt.emit_gate_op(gate, false, Vec::new(), vec![q])?;
            }
            qubits.push(q);
        }
        Ok(qubits)
    }
}

fn exact_expectation_rt(
    rt: &mut NoevRuntime,
    observable: &Observable,
    qubits: &[usize],
) -> Result<f64, RuntimeError> {
    if observable.width() > qubits.len() {
        return Err(RuntimeError::new(format!(
            "the observable acts on qubit {}, but the preparation has {} qubit{}",
            observable.width() - 1,
            qubits.len(),
            if qubits.len() == 1 { "" } else { "s" }
        )));
    }
    let factor_vecs: Vec<Vec<NoevPauliFactor>> = observable
        .terms
        .iter()
        .map(|(_, product)| {
            product
                .iter()
                .map(|&(i, p)| NoevPauliFactor {
                    qubit: NoevQubit(qubits[i]),
                    pauli: match p {
                        Pauli::X => 0,
                        Pauli::Y => 1,
                        Pauli::Z => 2,
                    },
                })
                .collect()
        })
        .collect();
    let terms: Vec<NoevPauliTerm> = observable
        .terms
        .iter()
        .zip(&factor_vecs)
        .map(|((coeff, _), factors)| NoevPauliTerm {
            coeff: *coeff,
            factors: if factors.is_empty() {
                ptr::null()
            } else {
                factors.as_ptr()
            },
            len: factors.len(),
        })
        .collect();
    let mut out = 0.0;
    if unsafe { cabi::noev_rt_expectation_exact(rt, terms.as_ptr(), terms.len(), &mut out) } {
        Ok(out)
    } else {
        Err(rt.take_error())
    }
}

/// Evaluates `<prep| observable |prep> within precision` for compiled code.
///
/// # Safety
/// `rt` and `out` must be valid; `ket_ptr`/`ket_len` or `body`/`args`/`ret_desc` must match.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_expectation_compiled(
    rt: *mut NoevRuntime,
    ket_ptr: *const u8,
    ket_len: usize,
    body: Option<NoevCompiledFn>,
    args: *const u8,
    ret_desc: *const NoevTypeDesc,
    obs_handle: u64,
    precision: f64,
    out: *mut f64,
) -> bool {
    let rt = unsafe { &mut *rt };
    if !(precision > 0.0 && precision.is_finite()) {
        return rt.fail_err(RuntimeError::new(format!(
            "an expectation value's precision must be positive, not {precision}"
        )));
    }
    let observable = read_obs(rt, obs_handle).clone();
    let seed = rt.next_seed();
    let mut seeds = SmallRng::seed_from_u64(seed);
    if !rt.backend().exact_expectations() {
        return rt.fail_err(RuntimeError::unknown(
            "the program uses an expectation value",
        ));
    }
    let mut child = unsafe { rt.child_runtime(seeds.random()) };
    let qubits = match run_prep(&mut child, ket_ptr, ket_len, body, args, ret_desc) {
        Ok(qs) => qs,
        Err(e) => return rt.fail_err(e),
    };
    let first = match exact_expectation_rt(&mut child, &observable, &qubits) {
        Ok(v) => v,
        Err(e) => return rt.fail_err(e),
    };
    if !child.measured() {
        unsafe { *out = first };
        return true;
    }
    let weight: f64 = observable
        .terms
        .iter()
        .filter(|(_, p)| !p.is_empty())
        .map(|(c, _)| c.abs())
        .sum();
    let runs = ((weight * weight) / (precision * precision))
        .ceil()
        .max(1.0) as usize;
    let mut total = first;
    for _ in 1..runs {
        let mut c = unsafe { rt.child_runtime(seeds.random()) };
        let qs = match run_prep(&mut c, ket_ptr, ket_len, body, args, ret_desc) {
            Ok(qs) => qs,
            Err(e) => return rt.fail_err(e),
        };
        let v = match exact_expectation_rt(&mut c, &observable, &qs) {
            Ok(v) => v,
            Err(e) => return rt.fail_err(e),
        };
        total += v;
    }
    unsafe { *out = total / runs as f64 };
    true
}

/// Undoes a compiled preparation `body(args)` on `targets[0..n_targets]` (`<prep()| x`).
///
/// # Safety
/// `rt`, `targets`, and `ret_desc` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_undo_compiled(
    rt: *mut NoevRuntime,
    body: NoevCompiledFn,
    args: *const u8,
    ret_desc: *const NoevTypeDesc,
    targets: *const NoevQubit,
    n_targets: usize,
) -> bool {
    let rt = unsafe { &mut *rt };
    let onto: Vec<usize> = if n_targets == 0 || targets.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(targets, n_targets) }
            .iter()
            .map(|q| q.0)
            .collect()
    };
    rt.record_ops_start();
    let made = run_prep(rt, ptr::null(), 0, Some(body), args, ret_desc);
    let (ops, _) = rt.record_ops_end();
    let made = match made {
        Ok(m) => m,
        Err(e) => return rt.fail_err(e),
    };
    for &q in &made {
        if let Err(e) = rt.release_q(q) {
            return rt.fail_err(e);
        }
    }
    if let Err(e) = rt.replay_ops(ops, &made, &onto, true, None) {
        return rt.fail_err(e);
    }
    true
}

/// Evaluates `<bra|ket>` or `<bra| H |ket>` for compiled code, writing `NoevComplex` to `out`.
///
/// # Safety
/// `rt` and `out` must be valid; bra/ket descriptors and args must match.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_overlap_compiled(
    rt: *mut NoevRuntime,
    bra_ket_ptr: *const u8,
    bra_ket_len: usize,
    bra_body: Option<NoevCompiledFn>,
    bra_args: *const u8,
    bra_desc: *const NoevTypeDesc,
    ket_ptr: *const u8,
    ket_len: usize,
    ket_body: Option<NoevCompiledFn>,
    ket_args: *const u8,
    ket_desc: *const NoevTypeDesc,
    has_obs: bool,
    obs_handle: u64,
    precision: f64,
    out: *mut NoevComplex,
) -> bool {
    let rt = unsafe { &mut *rt };
    if !(precision > 0.0 && precision.is_finite()) {
        return rt.fail_err(RuntimeError::new(format!(
            "a precision must be positive, not {precision}"
        )));
    }
    let identity = [(1.0, Vec::new())];
    let obs = has_obs.then(|| read_obs(rt, obs_handle).clone());
    let terms = obs.as_ref().map_or(&identity[..], |o| &o.terms[..]);
    let seed = rt.next_seed();
    let mut seeds = SmallRng::seed_from_u64(seed);
    let (mut re, mut im) = (0.0, 0.0);
    for (coeff, product) in terms {
        if !rt.backend().exact_expectations() {
            return rt.fail_err(RuntimeError::unknown("the program uses an amplitude"));
        }
        let mut child = unsafe { rt.child_runtime(seeds.random()) };
        let qubits = match run_prep(&mut child, ket_ptr, ket_len, ket_body, ket_args, ket_desc) {
            Ok(qs) => qs,
            Err(e) => return rt.fail_err(e),
        };
        for &(index, pauli) in product {
            let Some(&q) = qubits.get(index) else {
                return rt.fail_err(RuntimeError::new(format!(
                    "the observable's qubit {index} is not prepared"
                )));
            };
            let gate = match pauli {
                Pauli::X => Gate::X,
                Pauli::Y => Gate::Y,
                Pauli::Z => Gate::Z,
            };
            if let Err(e) = child.emit_gate_op(gate, false, Vec::new(), vec![q]) {
                return rt.fail_err(e);
            }
        }
        // Unprepare bra onto `qubits`.
        child.record_ops_start();
        let made = run_prep(
            &mut child,
            bra_ket_ptr,
            bra_ket_len,
            bra_body,
            bra_args,
            bra_desc,
        );
        let (ops, _) = child.record_ops_end();
        let made = match made {
            Ok(m) => m,
            Err(e) => return rt.fail_err(e),
        };
        for &q in &made {
            if let Err(e) = child.release_q(q) {
                return rt.fail_err(e);
            }
        }
        if let Err(e) = child.replay_ops(ops, &made, &qubits, true, None) {
            return rt.fail_err(e);
        }
        let mut amp = NoevComplex::default();
        if !unsafe { cabi::noev_rt_zero_amplitude(&mut child, &mut amp) } {
            let err = child.take_error();
            return rt.fail_err(err);
        }
        re += coeff * amp.re;
        im += coeff * amp.im;
    }
    unsafe { *out = NoevComplex { re, im } };
    true
}

/// Checks an array index `idx` (tagged `Int`) against `len`, returning the `usize` index
/// or recording an out-of-bounds runtime error.
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_bounds_check(
    rt: *mut NoevRuntime,
    idx: i64,
    len: usize,
    out: *mut usize,
) -> bool {
    let rt = unsafe { &mut *rt };
    let v = decode_int(rt, idx);
    if let Some(n) = v.to_i64()
        && n >= 0
        && (n as usize) < len
    {
        unsafe { *out = n as usize };
        return true;
    }
    rt.fail_err(RuntimeError::new(format!(
        "index {v} is out of bounds for length {len}"
    )))
}

#[allow(dead_code)]
fn _backend_err(e: BackendError) -> RuntimeError {
    RuntimeError::new(e.0)
}

// ---------------------------------------------------------------------------------------
// C interop helpers (`spec/interop.md` §1, §3)
// ---------------------------------------------------------------------------------------

/// Converts a compiled tagged `Int` into a C `int64_t`, failing with a runtime error if the
/// value is outside the signed 64-bit range (`spec/interop.md` §1).
///
/// # Safety
/// `rt` and `out` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_to_c_i64(
    rt: *mut NoevRuntime,
    val: i64,
    out: *mut i64,
) -> bool {
    let rt = unsafe { &mut *rt };
    let v = decode_int(rt, val);
    if let Some(n) = v.to_i64() {
        unsafe { *out = n };
        true
    } else {
        rt.fail_err(RuntimeError::new(format!(
            "Noev `Int` value `{v}` is outside the 64-bit signed integer range across the C boundary"
        )))
    }
}

/// Converts a C `int64_t` into a compiled tagged `Int`.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_int_from_c_i64(rt: *mut NoevRuntime, v: i64) -> i64 {
    let rt = unsafe { &mut *rt };
    encode_int(rt, Int::Small(v))
}

static CALLBACK_SEED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Creates a fresh runtime for an `extern "C" fn` callback invoked from C (`spec/interop.md` §3).
#[unsafe(no_mangle)]
pub extern "C" fn noev_rt_callback_enter() -> *mut NoevRuntime {
    let seed = CALLBACK_SEED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(feature = "ketrs")]
    {
        cabi::noev_rt_new_ketrs(seed)
    }
    #[cfg(not(feature = "ketrs"))]
    {
        cabi::noev_rt_new_export(seed)
    }
}

/// Finishes an `extern "C" fn` callback. If `ok` is false, prints the runtime error and ends the
/// process (`spec/interop.md` §3: cannot unwind through C stack frames).
///
/// # Safety
/// `rt` must have been returned by [`noev_rt_callback_enter`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_callback_exit(rt: *mut NoevRuntime, ok: bool) {
    if !ok {
        let rt_ref = unsafe { &mut *rt };
        let err = rt_ref.take_error();
        eprintln!(
            "noev runtime error in extern \"C\" fn callback: {}",
            err.message
        );
        unsafe { cabi::noev_rt_free(rt) };
        std::process::exit(1);
    }
    unsafe { cabi::noev_rt_free(rt) };
}

/// Checks an assertion whose custom failure message is a compiled Noev `String` handle.
///
/// # Safety
/// `rt` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_assert_str(
    rt: *mut NoevRuntime,
    cond: bool,
    msg_handle: i64,
) -> bool {
    if cond {
        return true;
    }
    let rt_ref = unsafe { &mut *rt };
    let msg = read_str(rt_ref, msg_handle as u64).to_string();
    rt_ref.fail_err(RuntimeError::new(msg))
}

// ---------------------------------------------------------------------------------------
// Native test runner harness (`noev test --compile` / `--codegen`, `spec/testing.md`)
// ---------------------------------------------------------------------------------------

/// Deterministically derives a per-test PRNG seed from the run's base seed and the test's
/// full name (`spec/testing.md` §3).
///
/// Using the test name makes a test's seed independent of `--filter`, file ordering, and
/// interpreter vs compiled execution, so `noev test --seed <BASE> <TEST>` always reproduces.
pub fn derive_test_seed(base_seed: u64, full_name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ base_seed;
    for &b in full_name.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // SplitMix64 finalizer for strong avalanche even for short or similar names.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Shared growable byte buffer used to capture stdout/stderr for a single test run.
struct CaptureWriter(Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One registered test inside a compiled native test runner binary.
struct RegisteredTest {
    name: String,
    expect_fail: Option<Option<String>>,
    func: extern "C" fn(*mut NoevRuntime) -> bool,
}

/// Opaque native test runner harness created by a compiled test binary's `main`.
pub struct NoevTestHarness {
    seed: u64,
    backend_spec: Option<String>,
    circuit_opt: u8,
    filter: Option<String>,
    prefix: Option<String>,
    list: bool,
    nocapture: bool,
    quiet_summary: bool,
    tests: Vec<RegisteredTest>,
}

/// Parses test runner CLI flags and initializes a [`NoevTestHarness`].
///
/// # Safety
/// `argv` must be null or point to `argc` valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_test_harness_new(
    argc: i32,
    argv: *const *const c_char,
) -> *mut NoevTestHarness {
    let mut seed: Option<u64> = None;
    let mut backend_spec: Option<String> = None;
    let mut circuit_opt: Option<u8> = None;
    let mut filter: Option<String> = None;
    let mut prefix: Option<String> = None;
    let mut list = false;
    let mut nocapture = false;
    let mut quiet_summary = false;

    if !argv.is_null() && argc > 1 {
        let args = unsafe { std::slice::from_raw_parts(argv, argc as usize) };
        let mut i = 1;
        while i < args.len() {
            if let Ok(s) = unsafe { CStr::from_ptr(args[i]) }.to_str() {
                if s == "--" {
                    break;
                }
                if let Some(rest) = s.strip_prefix("--seed=") {
                    if let Ok(n) = rest.parse() {
                        seed = Some(n);
                    }
                } else if s == "--seed"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                    && let Ok(n) = next.parse()
                {
                    seed = Some(n);
                    i += 1;
                } else if let Some(rest) = s.strip_prefix("--backend=") {
                    backend_spec = Some(rest.to_string());
                } else if s == "--backend"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                {
                    backend_spec = Some(next.to_string());
                    i += 1;
                } else if let Some(rest) = s.strip_prefix("--circuit-opt=") {
                    if let Ok(n) = rest.parse::<u8>()
                        && n <= 3
                    {
                        circuit_opt = Some(n);
                    }
                } else if s == "--circuit-opt"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                    && let Ok(n) = next.parse::<u8>()
                    && n <= 3
                {
                    circuit_opt = Some(n);
                    i += 1;
                } else if let Some(rest) = s.strip_prefix("--filter=") {
                    filter = Some(rest.to_string());
                } else if s == "--filter"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                {
                    filter = Some(next.to_string());
                    i += 1;
                } else if let Some(rest) = s.strip_prefix("--prefix=") {
                    prefix = Some(rest.to_string());
                } else if s == "--prefix"
                    && i + 1 < args.len()
                    && let Ok(next) = unsafe { CStr::from_ptr(args[i + 1]) }.to_str()
                {
                    prefix = Some(next.to_string());
                    i += 1;
                } else if s == "--list" {
                    list = true;
                } else if s == "--nocapture" {
                    nocapture = true;
                } else if s == "--quiet-summary" {
                    quiet_summary = true;
                } else if !s.starts_with('-') && filter.is_none() {
                    filter = Some(s.to_string());
                }
            }
            i += 1;
        }
    }
    if backend_spec.is_none()
        && let Ok(env_spec) = std::env::var("NOEV_BACKEND")
        && !env_spec.is_empty()
    {
        backend_spec = Some(env_spec);
    }
    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });

    let circuit_opt = circuit_opt
        .or_else(|| {
            std::env::var("NOEV_CIRCUIT_OPT")
                .ok()
                .and_then(|v| v.parse::<u8>().ok())
                .filter(|&n| n <= 3)
        })
        .unwrap_or(2);

    Box::into_raw(Box::new(NoevTestHarness {
        seed,
        backend_spec,
        circuit_opt,
        filter,
        prefix,
        list,
        nocapture,
        quiet_summary,
        tests: Vec::new(),
    }))
}

/// Registers one compiled `test fn` with `harness`.
///
/// `fail_mode`: `0` = must pass, `1` = `test(fail)`, `2` = `test(fail = "…")`.
///
/// # Safety
/// `harness`, `name_ptr` (`name_len` bytes), and when `fail_mode == 2`, `fail_ptr` (`fail_len`
/// bytes) must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_test_register(
    harness: *mut NoevTestHarness,
    name_ptr: *const u8,
    name_len: usize,
    fail_mode: u8,
    fail_ptr: *const u8,
    fail_len: usize,
    func: extern "C" fn(*mut NoevRuntime) -> bool,
) {
    let h = unsafe { &mut *harness };
    let name = if name_ptr.is_null() || name_len == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(name_ptr, name_len) })
            .into_owned()
    };
    let expect_fail = match fail_mode {
        0 => None,
        1 => Some(None),
        _ => {
            let sub = if fail_ptr.is_null() || fail_len == 0 {
                String::new()
            } else {
                String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(fail_ptr, fail_len) })
                    .into_owned()
            };
            Some(Some(sub))
        }
    };
    h.tests.push(RegisteredTest {
        name,
        expect_fail,
        func,
    });
}

/// Runs all matching registered tests, frees `harness`, and returns exit code `0` or `1`.
///
/// # Safety
/// `harness` must have been returned by [`noev_rt_test_harness_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn noev_rt_test_run_all(harness: *mut NoevTestHarness) -> i32 {
    let h = unsafe { Box::from_raw(harness) };
    let display_name = |name: &str| match &h.prefix {
        Some(p) if !p.is_empty() => format!("{p}::{name}"),
        _ => name.to_string(),
    };
    let matching: Vec<(usize, &RegisteredTest, String)> = h
        .tests
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let full = display_name(&t.name);
            if h.filter.as_deref().is_none_or(|f| full.contains(f)) {
                Some((i, t, full))
            } else {
                None
            }
        })
        .collect();

    if h.list {
        for (_, _, full) in &matching {
            println!("{full}");
        }
        return 0;
    }

    // Resolve the backend once for the entire test binary.
    let spec = h.backend_spec.as_deref().unwrap_or("ketrs");
    let backend: Box<dyn crate::Backend> = if spec == "export" {
        Box::new(crate::backend::Exporter::default())
    } else if spec == "ketrs" || spec.starts_with("ketrs:") {
        #[cfg(feature = "ketrs")]
        {
            match crate::backend::ketrs::KetrsBackend::from_spec(spec, h.circuit_opt) {
                Some(b) => Box::new(b),
                None => {
                    eprintln!("error: unknown ketrs engine `{spec}`");
                    return 2;
                }
            }
        }
        #[cfg(not(feature = "ketrs"))]
        {
            let _ = h.circuit_opt;
            eprintln!("error: runtime built without `ketrs` simulator");
            return 2;
        }
    } else {
        match crate::plugin::load_plugin_spec(spec) {
            Ok(p) => Box::new(p),
            Err(err) => {
                eprintln!("error: {}", err.0);
                return 2;
            }
        }
    };

    let start = std::time::Instant::now();
    if !h.quiet_summary {
        println!(
            "running {} {}",
            matching.len(),
            if matching.len() == 1 { "test" } else { "tests" }
        );
    }

    let mut passed = 0usize;
    let mut failures: Vec<(String, String, String)> = Vec::new();
    for (_idx, test, full) in &matching {
        let test_seed = derive_test_seed(h.seed, full);
        let captured = Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
        let mut rt = unsafe { NoevRuntime::new_borrowed(backend.as_ref(), test_seed) };
        if h.nocapture {
            rt.set_output(Rc::new(std::cell::RefCell::new(crate::Output {
                stdout: Box::new(std::io::stdout()),
                stderr: Box::new(std::io::stderr()),
            })));
        } else {
            rt.set_output(Rc::new(std::cell::RefCell::new(crate::Output {
                stdout: Box::new(CaptureWriter(captured.clone())),
                stderr: Box::new(CaptureWriter(captured.clone())),
            })));
        }
        let ok = (test.func)(&mut rt);
        let _ = unsafe { cabi::noev_rt_flush(&mut rt) };
        let err_msg = if ok {
            None
        } else {
            Some(rt.take_error().message)
        };
        drop(rt);

        let failure_reason: Option<String> = match (&test.expect_fail, err_msg) {
            (None, None) => None,
            (None, Some(msg)) => Some(msg),
            (Some(None), Some(_)) => None,
            (Some(None), None) => Some("test was expected to fail, but passed".into()),
            (Some(Some(expected)), Some(actual)) => {
                if actual.contains(expected.as_str()) {
                    None
                } else {
                    Some(format!(
                        "expected failure containing `{expected}`, but got `{actual}`"
                    ))
                }
            }
            (Some(Some(expected)), None) => Some(format!(
                "test was expected to fail with `{expected}`, but passed"
            )),
        };

        if let Some(reason) = failure_reason {
            println!("test {full} ... FAILED");
            let out_str = String::from_utf8_lossy(&captured.borrow()).into_owned();
            failures.push((full.clone(), reason, out_str));
        } else {
            println!("test {full} ... ok");
            passed += 1;
        }
    }

    if !failures.is_empty() {
        println!("\nfailures:");
        for (name, reason, out) in &failures {
            println!("\n---- {name} ----");
            if !out.is_empty() {
                print!("{out}");
                if !out.ends_with('\n') {
                    println!();
                }
            }
            println!("error: {reason}");
            println!("note: reproduce with: noev test --seed {} {name}", h.seed);
        }
    }

    if !h.quiet_summary {
        let elapsed = start.elapsed().as_secs_f64();
        let status = if failures.is_empty() { "ok" } else { "FAILED" };
        println!(
            "\ntest result: {status}. {passed} passed; {} failed; finished in {elapsed:.2}s",
            failures.len()
        );
    }

    i32::from(!failures.is_empty())
}
