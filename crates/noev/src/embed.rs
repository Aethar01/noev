//! Safe Rust embedding API for compiling and running Noev programs (`spec/interop.md` §1, §2).
//!
//! Host code never needs `unsafe`: every parameter and return value crossing the boundary is
//! checked to be classical, copyable, and reference-free, and every `extern fn` declaration is
//! verified against its registered Rust implementation before execution.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::rc::Rc;

use noev_ir::ir::{FuncId, Function, Program as IrProgram};
use noev_runtime::backend::Backend;
use noev_runtime::host::{HostCallContext, HostTable};
use noev_runtime::int::Int;
use noev_runtime::machine::{Machine, Output, RuntimeError};
use noev_runtime::value::{Observable, Value};
use noev_sema::ty::{Kind, Width};
use noev_sema::{Ty, TyKind, Types};
use noev_syntax::{Ast, Severity, SourceMap};

use crate::package;

/// A Noev `Bit` value (`0` or `1`) crossing the host boundary (`spec/interop.md` §1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bit(pub bool);

impl From<bool> for Bit {
    fn from(value: bool) -> Self {
        Bit(value)
    }
}

impl From<Bit> for bool {
    fn from(bit: Bit) -> Self {
        bit.0
    }
}

/// A Noev `Complex` (`c64`) value crossing the host boundary (`spec/interop.md` §1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    /// Real part.
    pub re: f64,
    /// Imaginary part.
    pub im: f64,
}

impl Complex {
    /// Creates a `Complex` number `re + im·i`.
    pub const fn new(re: f64, im: f64) -> Self {
        Complex { re, im }
    }
}

/// A Noev `c32` value crossing the host boundary (`spec/interop.md` §1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct C32 {
    /// Real part.
    pub re: f32,
    /// Imaginary part.
    pub im: f32,
}

impl C32 {
    /// Creates a `c32` number `re + im·i`.
    pub const fn new(re: f32, im: f32) -> Self {
        C32 { re, im }
    }
}

/// Errors produced when compiling, validating, or running an embedded Noev program.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// Could not load source files from disk.
    Load(String),
    /// Parsing, type checking, or ownership checking failed.
    Compile(String),
    /// An `extern fn` declared by the Noev program was not registered on the runtime.
    MissingHostFn(String),
    /// A host implementation was registered for a name that is not an `extern fn` in the program.
    UnknownHostFn(String),
    /// Called a Noev function that does not exist in the program.
    UnknownFunction(String),
    /// Rust argument or return types did not match the Noev function's signature.
    SignatureMismatch(String),
    /// A Noev function's parameter or return type cannot cross the host boundary.
    BoundaryType(String),
    /// A value could not be converted across the boundary (e.g., an `Int` outside `i64` range).
    Conversion(String),
    /// The Noev program failed at runtime (e.g., failed `assert` or backend error).
    Runtime(RuntimeError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Load(msg)
            | Error::Compile(msg)
            | Error::MissingHostFn(msg)
            | Error::UnknownHostFn(msg)
            | Error::UnknownFunction(msg)
            | Error::SignatureMismatch(msg)
            | Error::BoundaryType(msg)
            | Error::Conversion(msg) => f.write_str(msg),
            Error::Runtime(err) => f.write_str(&err.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<RuntimeError> for Error {
    fn from(err: RuntimeError) -> Self {
        Error::Runtime(err)
    }
}

/// A compiled Noev program ready to be executed in a [`Runtime`].
#[derive(Clone, Debug)]
pub struct Program {
    ir: IrProgram,
    by_name: HashMap<String, FuncId>,
}

impl Program {
    /// Compiles a `.nv` file or package directory (`noev.toml`) from disk.
    pub fn compile_file(path: impl AsRef<Path>) -> Result<Self, Error> {
        let loaded = package::load(path.as_ref()).map_err(|e| Error::Load(e.to_string()))?;
        Self::from_loaded(loaded.sources, loaded.ast, loaded.diagnostics)
    }

    /// Compiles a single-file Noev program from source text.
    pub fn compile_source(source: &str) -> Result<Self, Error> {
        let mut sources = SourceMap::default();
        let file_id = sources
            .add("<embedded>", source.to_string())
            .map_err(|_| Error::Load("source is larger than 4 GiB".into()))?;
        let mut ast = Ast::new();
        let pkg = ast.add_package("main");
        let root = ast.package(pkg).root;
        let file = sources.file(file_id);
        let diagnostics = noev_syntax::parse_module(&mut ast, root, &file.text, file.start);
        ast.collect_root();
        Self::from_loaded(sources, ast, diagnostics)
    }

    fn from_loaded(
        sources: SourceMap,
        ast: Ast,
        mut diagnostics: Vec<noev_syntax::Diagnostic>,
    ) -> Result<Self, Error> {
        let (typed, type_errors) = noev_sema::check(&ast);
        let mut circuits = noev_sema::Circuits::default();
        let has_errors =
            |ds: &[noev_syntax::Diagnostic]| ds.iter().any(|d| d.severity == Severity::Error);
        if !has_errors(&diagnostics) && !has_errors(&type_errors) {
            diagnostics.extend(noev_sema::check_ownership(&ast, &typed));
            if !has_errors(&diagnostics) {
                let (found, warnings) = noev_sema::check_circuits(&ast, &typed);
                circuits = found;
                diagnostics.extend(warnings);
            }
        }
        diagnostics.extend(type_errors);
        diagnostics.sort_by_key(|d| d.span.start);
        if has_errors(&diagnostics) {
            let mut msg = String::new();
            for d in diagnostics.iter().filter(|d| d.severity == Severity::Error) {
                if !msg.is_empty() {
                    msg.push('\n');
                }
                let file = sources.lookup(d.span.start);
                let rel = (d.span.start.saturating_sub(file.start) as usize).min(file.text.len());
                let prefix = &file.text[..rel];
                let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
                let col = prefix.rsplit('\n').next().map_or(0, str::len) + 1;
                if let Some(code) = d.code {
                    msg.push_str(&format!(
                        "{}:{line}:{col}: error[{code}]: {}",
                        file.name, d.message
                    ));
                } else {
                    msg.push_str(&format!("{}:{line}:{col}: error: {}", file.name, d.message));
                }
            }
            return Err(Error::Compile(msg));
        }

        let ir = noev_ir::lower(&ast, &typed, &circuits, &sources);
        let by_name = ir
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.clone(), FuncId(i as u32)))
            .collect();
        Ok(Program { ir, by_name })
    }

    /// The names of all `extern fn` host functions declared by the program.
    pub fn extern_fns(&self) -> impl Iterator<Item = &str> {
        self.ir
            .functions
            .iter()
            .filter(|f| f.is_extern)
            .map(|f| f.name.as_str())
    }

    /// The names of all non-`extern` functions in the program.
    pub fn functions(&self) -> impl Iterator<Item = &str> {
        self.ir
            .functions
            .iter()
            .filter(|f| !f.is_extern)
            .map(|f| f.name.as_str())
    }

    /// Whether the program contains a function named `name`.
    pub fn has_function(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Looks up a function's IR [`FuncId`] by name.
    pub fn function_id(&self, name: &str) -> Option<FuncId> {
        self.by_name.get(name).copied()
    }

    /// The lowered IR program.
    pub fn ir(&self) -> &IrProgram {
        &self.ir
    }
}

/// Checks that `ty` can cross the host boundary (`spec/interop.md` §1): classical, copyable,
/// and holding no references.
pub fn check_boundary_ty(
    program: &IrProgram,
    ty: Ty,
    role: &str,
    fn_name: &str,
) -> Result<(), Error> {
    if program.types.has_ref(ty) {
        return Err(Error::BoundaryType(format!(
            "`{fn_name}` {role} has type `{}`, which holds a reference and cannot cross the host boundary",
            show_ty(program, ty)
        )));
    }
    if program.types.linearity(ty) != Kind::Copyable {
        return Err(Error::BoundaryType(format!(
            "`{fn_name}` {role} has type `{}`, which is not classical and copyable and cannot cross the host boundary",
            show_ty(program, ty)
        )));
    }
    Ok(())
}

/// Formats a Noev [`Ty`] for error messages.
pub fn show_ty(program: &IrProgram, ty: Ty) -> String {
    match program.types.kind(ty) {
        TyKind::Error => "<error>".into(),
        TyKind::Never => "Never".into(),
        TyKind::Unit => "()".into(),
        TyKind::Int => "Int".into(),
        TyKind::Float => "Float".into(),
        TyKind::Bool => "Bool".into(),
        TyKind::Bit => "Bit".into(),
        TyKind::Qubit => "Qubit".into(),
        TyKind::Observable => "Observable".into(),
        TyKind::String => "String".into(),
        TyKind::I8 => "i8".into(),
        TyKind::I16 => "i16".into(),
        TyKind::I32 => "i32".into(),
        TyKind::I64 => "i64".into(),
        TyKind::U8 => "u8".into(),
        TyKind::U16 => "u16".into(),
        TyKind::U32 => "u32".into(),
        TyKind::U64 => "u64".into(),
        TyKind::F32 => "f32".into(),
        TyKind::Complex => "Complex".into(),
        TyKind::C32 => "c32".into(),
        TyKind::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(|&t| show_ty(program, t)).collect();
            if parts.len() == 1 {
                format!("({},)", parts[0])
            } else {
                format!("({})", parts.join(", "))
            }
        }
        TyKind::Array(elem, len) => format!("[{}; {len}]", show_ty(program, *elem)),
        TyKind::Distribution(elem) => format!("Distribution<{}>", show_ty(program, *elem)),
        TyKind::Struct(id) => program
            .struct_names
            .get(id.index())
            .cloned()
            .unwrap_or_else(|| format!("struct#{}", id.index())),
        TyKind::Ref { mutable, inner } => {
            let kw = if *mutable { "mut" } else { "ref" };
            format!("{kw} {}", show_ty(program, *inner))
        }
        _ => format!("{:?}", program.types.kind(ty)),
    }
}

/// Returns a function's parameter types and return type.
fn fn_sig_tys(func: &Function) -> (Vec<Ty>, Ty) {
    let params = func.locals[1..=func.params].iter().map(|l| l.ty).collect();
    let ret = func.locals[0].ty;
    (params, ret)
}

/// A Rust type that maps to a Noev type across the host boundary (`spec/interop.md` §1).
pub trait NoevType: Sized {
    /// Whether this Rust type is compatible with the Noev type `ty`.
    fn matches(types: &Types, ty: Ty) -> bool;

    /// Human-readable Rust type name for signature mismatch diagnostics.
    fn rust_name() -> String;
}

/// Converts a Rust value into a Noev runtime [`Value`].
pub trait IntoNoev: NoevType {
    /// Converts `self` into a [`Value`] of Noev type `ty`.
    fn into_noev(self, types: &Types, ty: Ty) -> Result<Value, Error>;
}

/// Converts a Noev runtime [`Value`] into a Rust value.
pub trait FromNoev: NoevType {
    /// Converts `value` (of Noev type `ty`) into `Self`.
    fn from_noev(value: Value, types: &Types, ty: Ty) -> Result<Self, Error>;
}

impl NoevType for () {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Unit)
    }
    fn rust_name() -> String {
        "()".into()
    }
}

impl IntoNoev for () {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Unit)
    }
}

impl FromNoev for () {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Unit => Ok(()),
            other => Err(Error::Conversion(format!("expected `()`, got `{other:?}`"))),
        }
    }
}

impl NoevType for i64 {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Int | TyKind::I64)
    }
    fn rust_name() -> String {
        "i64".into()
    }
}

impl IntoNoev for i64 {
    fn into_noev(self, types: &Types, ty: Ty) -> Result<Value, Error> {
        match types.kind(ty) {
            TyKind::Int => Ok(Value::Int(Int::Small(self))),
            TyKind::I64 => Ok(Value::Sized(
                i128::from(self),
                Width {
                    bits: 64,
                    signed: true,
                },
            )),
            _ => Err(Error::Conversion(
                "cannot convert `i64` to non-integer Noev type".into(),
            )),
        }
    }
}

impl FromNoev for i64 {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Int(n) => n.to_i64().ok_or_else(|| {
                Error::Conversion(format!(
                    "Noev `Int` value `{n}` is outside the 64-bit signed integer range"
                ))
            }),
            Value::Sized(
                v,
                Width {
                    bits: 64,
                    signed: true,
                },
            ) => Ok(v as i64),
            other => Err(Error::Conversion(format!(
                "expected `Int` or `i64`, got `{other:?}`"
            ))),
        }
    }
}

macro_rules! impl_sized_int {
    ($($rust:ty => $kind:ident, $bits:expr, $signed:expr;)*) => {
        $(
            impl NoevType for $rust {
                fn matches(types: &Types, ty: Ty) -> bool {
                    matches!(types.kind(ty), TyKind::$kind)
                }
                fn rust_name() -> String {
                    stringify!($rust).into()
                }
            }

            impl IntoNoev for $rust {
                fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
                    Ok(Value::Sized(
                        i128::from(self),
                        Width {
                            bits: $bits,
                            signed: $signed,
                        },
                    ))
                }
            }

            impl FromNoev for $rust {
                fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
                    match value {
                        Value::Sized(v, w) if w.bits == $bits && w.signed == $signed => {
                            <$rust>::try_from(v).map_err(|_| {
                                Error::Conversion(format!(
                                    "value `{v}` out of range for `{}`",
                                    stringify!($rust)
                                ))
                            })
                        }
                        other => Err(Error::Conversion(format!(
                            "expected `{}`, got `{other:?}`",
                            stringify!($rust)
                        ))),
                    }
                }
            }
        )*
    };
}

impl_sized_int! {
    i8 => I8, 8, true;
    i16 => I16, 16, true;
    i32 => I32, 32, true;
    u8 => U8, 8, false;
    u16 => U16, 16, false;
    u32 => U32, 32, false;
    u64 => U64, 64, false;
}

impl NoevType for f64 {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Float)
    }
    fn rust_name() -> String {
        "f64".into()
    }
}

impl IntoNoev for f64 {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Float(self))
    }
}

impl FromNoev for f64 {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Float(v) => Ok(v),
            other => Err(Error::Conversion(format!(
                "expected `Float`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for f32 {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::F32)
    }
    fn rust_name() -> String {
        "f32".into()
    }
}

impl IntoNoev for f32 {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::F32(self))
    }
}

impl FromNoev for f32 {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::F32(v) => Ok(v),
            other => Err(Error::Conversion(format!(
                "expected `f32`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for bool {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Bool)
    }
    fn rust_name() -> String {
        "bool".into()
    }
}

impl IntoNoev for bool {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Bool(self))
    }
}

impl FromNoev for bool {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Bool(v) => Ok(v),
            other => Err(Error::Conversion(format!(
                "expected `Bool`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for Bit {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Bit)
    }
    fn rust_name() -> String {
        "noev::Bit".into()
    }
}

impl IntoNoev for Bit {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Bit(self.0))
    }
}

impl FromNoev for Bit {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Bit(v) => Ok(Bit(v)),
            other => Err(Error::Conversion(format!(
                "expected `Bit`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for Complex {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Complex)
    }
    fn rust_name() -> String {
        "noev::Complex".into()
    }
}

impl IntoNoev for Complex {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Complex(self.re, self.im))
    }
}

impl FromNoev for Complex {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Complex(re, im) => Ok(Complex { re, im }),
            other => Err(Error::Conversion(format!(
                "expected `Complex`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for C32 {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::C32)
    }
    fn rust_name() -> String {
        "noev::C32".into()
    }
}

impl IntoNoev for C32 {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::C32(self.re, self.im))
    }
}

impl FromNoev for C32 {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::C32(re, im) => Ok(C32 { re, im }),
            other => Err(Error::Conversion(format!(
                "expected `c32`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for String {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::String)
    }
    fn rust_name() -> String {
        "String".into()
    }
}

impl IntoNoev for String {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Str(Rc::from(self)))
    }
}

impl FromNoev for String {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Str(s) => Ok(s.to_string()),
            other => Err(Error::Conversion(format!(
                "expected `String`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for &str {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::String)
    }
    fn rust_name() -> String {
        "&str".into()
    }
}

impl IntoNoev for &str {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Str(Rc::from(self)))
    }
}

impl NoevType for Observable {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Observable)
    }
    fn rust_name() -> String {
        "noev::Observable".into()
    }
}

impl IntoNoev for Observable {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(Value::Observable(self))
    }
}

impl FromNoev for Observable {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        match value {
            Value::Observable(o) => Ok(o),
            other => Err(Error::Conversion(format!(
                "expected `Observable`, got `{other:?}`"
            ))),
        }
    }
}

impl NoevType for Value {
    fn matches(types: &Types, ty: Ty) -> bool {
        !types.has_ref(ty) && types.linearity(ty) == Kind::Copyable
    }
    fn rust_name() -> String {
        "noev::Value".into()
    }
}

impl IntoNoev for Value {
    fn into_noev(self, _types: &Types, _ty: Ty) -> Result<Value, Error> {
        Ok(self)
    }
}

impl FromNoev for Value {
    fn from_noev(value: Value, _types: &Types, _ty: Ty) -> Result<Self, Error> {
        Ok(value)
    }
}

impl<T: NoevType, const N: usize> NoevType for [T; N] {
    fn matches(types: &Types, ty: Ty) -> bool {
        matches!(types.kind(ty), TyKind::Array(elem, len) if *len == N as u64 && T::matches(types, *elem))
    }
    fn rust_name() -> String {
        format!("[{}; {N}]", T::rust_name())
    }
}

impl<T: IntoNoev, const N: usize> IntoNoev for [T; N] {
    fn into_noev(self, types: &Types, ty: Ty) -> Result<Value, Error> {
        let TyKind::Array(elem_ty, _) = *types.kind(ty) else {
            return Err(Error::Conversion("expected a Noev array type".into()));
        };
        let mut out = Vec::with_capacity(N);
        for item in self {
            out.push(item.into_noev(types, elem_ty)?);
        }
        Ok(Value::Aggregate(out))
    }
}

impl<T: FromNoev, const N: usize> FromNoev for [T; N] {
    fn from_noev(value: Value, types: &Types, ty: Ty) -> Result<Self, Error> {
        let TyKind::Array(elem_ty, _) = *types.kind(ty) else {
            return Err(Error::Conversion("expected a Noev array type".into()));
        };
        let Value::Aggregate(items) = value else {
            return Err(Error::Conversion(format!(
                "expected array aggregate, got `{value:?}`"
            )));
        };
        if items.len() != N {
            return Err(Error::Conversion(format!(
                "expected array of length {N}, got {}",
                items.len()
            )));
        }
        let mut out = Vec::with_capacity(N);
        for item in items {
            out.push(T::from_noev(item, types, elem_ty)?);
        }
        out.try_into()
            .map_err(|_| Error::Conversion(format!("expected array of length {N}")))
    }
}

macro_rules! impl_tuple_value {
    ($($T:ident : $idx:tt),+) => {
        impl<$($T: NoevType),+> NoevType for ($($T,)+) {
            fn matches(types: &Types, ty: Ty) -> bool {
                let TyKind::Tuple(elems) = types.kind(ty) else {
                    return false;
                };
                let expected = [$(<$T>::matches),+];
                elems.len() == expected.len()
                    && elems.iter().zip(expected).all(|(&e, f)| f(types, e))
            }
            fn rust_name() -> String {
                let names = vec![$(<$T>::rust_name()),+];
                format!("({})", names.join(", "))
            }
        }

        impl<$($T: IntoNoev),+> IntoNoev for ($($T,)+) {
            fn into_noev(self, types: &Types, ty: Ty) -> Result<Value, Error> {
                let TyKind::Tuple(elems) = types.kind(ty) else {
                    return Err(Error::Conversion("expected a Noev tuple type".into()));
                };
                let elems: Vec<Ty> = elems.iter().copied().collect();
                let out = vec![
                    $(self.$idx.into_noev(types, elems[$idx])?),+
                ];
                Ok(Value::Aggregate(out))
            }
        }

        impl<$($T: FromNoev),+> FromNoev for ($($T,)+) {
            fn from_noev(value: Value, types: &Types, ty: Ty) -> Result<Self, Error> {
                let TyKind::Tuple(elems) = types.kind(ty) else {
                    return Err(Error::Conversion("expected a Noev tuple type".into()));
                };
                let elems: Vec<Ty> = elems.iter().copied().collect();
                let Value::Aggregate(items) = value else {
                    return Err(Error::Conversion(format!(
                        "expected tuple aggregate, got `{value:?}`"
                    )));
                };
                let mut iter = items.into_iter();
                Ok((
                    $({
                        let v = iter.next().ok_or_else(|| {
                            Error::Conversion("tuple had fewer elements than expected".into())
                        })?;
                        <$T>::from_noev(v, types, elems[$idx])?
                    },)+
                ))
            }
        }
    };
}

impl_tuple_value!(A: 0, B: 1);
impl_tuple_value!(A: 0, B: 1, C: 2);
impl_tuple_value!(A: 0, B: 1, C: 2, D: 3);
impl_tuple_value!(A: 0, B: 1, C: 2, D: 3, E: 4);
impl_tuple_value!(A: 0, B: 1, C: 2, D: 3, E: 4, F: 5);

/// Tuple of arguments passed to [`Runtime::call`] or [`HostContext::call`].
pub trait NoevArgs: Sized {
    /// Number of arguments.
    fn arity() -> usize;

    /// Checks that these Rust argument types match the Noev parameter list `params`.
    fn check_params(program: &IrProgram, fn_name: &str, params: &[Ty]) -> Result<(), Error>;

    /// Converts these Rust arguments into Noev runtime [`Value`]s.
    fn into_values(self, types: &Types, params: &[Ty]) -> Result<Vec<Value>, Error>;
}

impl NoevArgs for () {
    fn arity() -> usize {
        0
    }

    fn check_params(program: &IrProgram, fn_name: &str, params: &[Ty]) -> Result<(), Error> {
        if !params.is_empty() {
            let expected: Vec<String> = params.iter().map(|&t| show_ty(program, t)).collect();
            return Err(Error::SignatureMismatch(format!(
                "`{fn_name}` expects {} argument(s) ({}), got 0",
                params.len(),
                expected.join(", ")
            )));
        }
        Ok(())
    }

    fn into_values(self, _types: &Types, _params: &[Ty]) -> Result<Vec<Value>, Error> {
        Ok(Vec::new())
    }
}

macro_rules! impl_noev_args {
    ($count:expr => $($T:ident : $idx:tt),+) => {
        impl<$($T: IntoNoev),+> NoevArgs for ($($T,)+) {
            fn arity() -> usize {
                $count
            }

            fn check_params(program: &IrProgram, fn_name: &str, params: &[Ty]) -> Result<(), Error> {
                if params.len() != $count {
                    let expected: Vec<String> = params.iter().map(|&t| show_ty(program, t)).collect();
                    return Err(Error::SignatureMismatch(format!(
                        "`{fn_name}` expects {} argument(s) ({}), got {}",
                        params.len(),
                        expected.join(", "),
                        $count
                    )));
                }
                $(
                    if !<$T>::matches(&program.types, params[$idx]) {
                        return Err(Error::SignatureMismatch(format!(
                            "`{fn_name}` parameter {} has Noev type `{}`, which does not match Rust type `{}`",
                            $idx + 1,
                            show_ty(program, params[$idx]),
                            <$T>::rust_name()
                        )));
                    }
                )+
                Ok(())
            }

            fn into_values(self, types: &Types, params: &[Ty]) -> Result<Vec<Value>, Error> {
                Ok(vec![
                    $(self.$idx.into_noev(types, params[$idx])?),+
                ])
            }
        }
    };
}

impl_noev_args!(1 => A: 0);
impl_noev_args!(2 => A: 0, B: 1);
impl_noev_args!(3 => A: 0, B: 1, C: 2);
impl_noev_args!(4 => A: 0, B: 1, C: 2, D: 3);
impl_noev_args!(5 => A: 0, B: 1, C: 2, D: 3, E: 4);
impl_noev_args!(6 => A: 0, B: 1, C: 2, D: 3, E: 4, F: 5);

/// Return type accepted from a Rust host function registered via [`Runtime::host_fn`] or
/// [`Runtime::host_fn_with_context`].
///
/// Implemented for any [`IntoNoev`] value `T` as well as `Result<T, Error>`.
pub trait HostReturn {
    /// The underlying Noev-compatible value type.
    type Output: IntoNoev;

    /// Converts the host function's return value into a `Result`.
    fn into_result(self) -> Result<Self::Output, Error>;
}

impl<T: IntoNoev> HostReturn for T {
    type Output = T;

    fn into_result(self) -> Result<T, Error> {
        Ok(self)
    }
}

impl<T: IntoNoev> HostReturn for Result<T, Error> {
    type Output = T;

    fn into_result(self) -> Result<T, Error> {
        self
    }
}

/// Safe handle passed to [`Runtime::host_fn_with_context`] callbacks so host functions can call
/// back into the Noev program (`spec/interop.md` §2).
pub struct HostContext<'a, 'p> {
    inner: &'a mut HostCallContext<'p>,
}

impl<'a, 'p> HostContext<'a, 'p> {
    /// Calls a Noev function named `name` with `args` and returns its result.
    ///
    /// Runs on a fresh quantum state (creating and consuming its own qubits), sharing the same
    /// backend, registered host functions, and output streams. Runtime errors in the called Noev
    /// function are returned as `Err(Error::Runtime(_))`.
    pub fn call<Args: NoevArgs, Ret: FromNoev>(
        &mut self,
        name: &str,
        args: Args,
    ) -> Result<Ret, Error> {
        let program = self.inner.program();
        let (func_id, func) = find_function(program, name)?;
        let (params, ret_ty) = check_call_signature::<Args, Ret>(program, func)?;
        let values = args.into_values(&program.types, &params)?;
        let result = self.inner.call(func_id, values)?;
        Ret::from_noev(result, &program.types, ret_ty)
    }
}

/// Rust closure accepted by [`Runtime::host_fn`].
pub trait HostHandler<Args, Ret> {
    /// Verifies that the closure's Rust types match the Noev `extern fn` signature.
    fn check_signature(program: &IrProgram, func: &Function) -> Result<(), Error>;

    /// Invokes the closure with raw Noev runtime [`Value`]s.
    fn invoke(
        &mut self,
        program: &IrProgram,
        func: &Function,
        args: Vec<Value>,
    ) -> Result<Value, Error>;
}

/// Rust closure accepted by [`Runtime::host_fn_with_context`].
pub trait HostContextHandler<Args, Ret> {
    /// Verifies that the closure's Rust types match the Noev `extern fn` signature.
    fn check_signature(program: &IrProgram, func: &Function) -> Result<(), Error>;

    /// Invokes the closure with a [`HostContext`] and raw Noev runtime [`Value`]s.
    fn invoke(
        &mut self,
        ctx: &mut HostContext<'_, '_>,
        program: &IrProgram,
        func: &Function,
        args: Vec<Value>,
    ) -> Result<Value, Error>;
}

fn check_host_ret<Ret: HostReturn>(program: &IrProgram, func: &Function) -> Result<(), Error> {
    let ret_ty = func.locals[0].ty;
    if !Ret::Output::matches(&program.types, ret_ty) {
        return Err(Error::SignatureMismatch(format!(
            "`extern fn {}` returns Noev type `{}`, which does not match host return type `{}`",
            func.name,
            show_ty(program, ret_ty),
            Ret::Output::rust_name()
        )));
    }
    Ok(())
}

macro_rules! impl_host_handlers {
    ($count:expr => $($T:ident : $idx:tt),*) => {
        impl<F, $($T,)* Ret> HostHandler<($($T,)*), Ret> for F
        where
            F: FnMut($($T),*) -> Ret,
            $($T: FromNoev,)*
            Ret: HostReturn,
        {
            fn check_signature(program: &IrProgram, func: &Function) -> Result<(), Error> {
                let (params, _) = fn_sig_tys(func);
                if params.len() != $count {
                    let expected: Vec<String> = params.iter().map(|&t| show_ty(program, t)).collect();
                    return Err(Error::SignatureMismatch(format!(
                        "`extern fn {}` declares {} parameter(s) ({}), but host function takes {}",
                        func.name,
                        params.len(),
                        expected.join(", "),
                        $count
                    )));
                }
                $(
                    if !<$T>::matches(&program.types, params[$idx]) {
                        return Err(Error::SignatureMismatch(format!(
                            "`extern fn {}` parameter {} has Noev type `{}`, which does not match host parameter type `{}`",
                            func.name,
                            $idx + 1,
                            show_ty(program, params[$idx]),
                            <$T>::rust_name()
                        )));
                    }
                )*
                check_host_ret::<Ret>(program, func)
            }

            #[allow(unused_variables, unused_mut)]
            fn invoke(
                &mut self,
                program: &IrProgram,
                func: &Function,
                args: Vec<Value>,
            ) -> Result<Value, Error> {
                let ret_ty = func.locals[0].ty;
                let mut iter = args.into_iter();
                let res = (self)(
                    $({
                        let v = iter.next().ok_or_else(|| {
                            Error::Conversion("missing host-function argument".into())
                        })?;
                        <$T>::from_noev(v, &program.types, func.locals[1 + $idx].ty)?
                    }),*
                );
                let val = res.into_result()?;
                val.into_noev(&program.types, ret_ty)
            }
        }

        impl<F, $($T,)* Ret> HostContextHandler<($($T,)*), Ret> for F
        where
            F: FnMut(&mut HostContext<'_, '_>, $($T),*) -> Ret,
            $($T: FromNoev,)*
            Ret: HostReturn,
        {
            fn check_signature(program: &IrProgram, func: &Function) -> Result<(), Error> {
                let (params, _) = fn_sig_tys(func);
                if params.len() != $count {
                    let expected: Vec<String> = params.iter().map(|&t| show_ty(program, t)).collect();
                    return Err(Error::SignatureMismatch(format!(
                        "`extern fn {}` declares {} parameter(s) ({}), but host function takes {}",
                        func.name,
                        params.len(),
                        expected.join(", "),
                        $count
                    )));
                }
                $(
                    if !<$T>::matches(&program.types, params[$idx]) {
                        return Err(Error::SignatureMismatch(format!(
                            "`extern fn {}` parameter {} has Noev type `{}`, which does not match host parameter type `{}`",
                            func.name,
                            $idx + 1,
                            show_ty(program, params[$idx]),
                            <$T>::rust_name()
                        )));
                    }
                )*
                check_host_ret::<Ret>(program, func)
            }

            #[allow(unused_variables, unused_mut)]
            fn invoke(
                &mut self,
                ctx: &mut HostContext<'_, '_>,
                program: &IrProgram,
                func: &Function,
                args: Vec<Value>,
            ) -> Result<Value, Error> {
                let ret_ty = func.locals[0].ty;
                let mut iter = args.into_iter();
                let res = (self)(
                    ctx,
                    $({
                        let v = iter.next().ok_or_else(|| {
                            Error::Conversion("missing host-function argument".into())
                        })?;
                        <$T>::from_noev(v, &program.types, func.locals[1 + $idx].ty)?
                    }),*
                );
                let val = res.into_result()?;
                val.into_noev(&program.types, ret_ty)
            }
        }
    };
}

impl_host_handlers!(0 => );
impl_host_handlers!(1 => A: 0);
impl_host_handlers!(2 => A: 0, B: 1);
impl_host_handlers!(3 => A: 0, B: 1, C: 2);
impl_host_handlers!(4 => A: 0, B: 1, C: 2, D: 3);
impl_host_handlers!(5 => A: 0, B: 1, C: 2, D: 3, E: 4);
impl_host_handlers!(6 => A: 0, B: 1, C: 2, D: 3, E: 4, G: 5);

fn find_function<'a>(program: &'a IrProgram, name: &str) -> Result<(FuncId, &'a Function), Error> {
    program
        .functions
        .iter()
        .enumerate()
        .find(|(_, f)| f.name == name)
        .map(|(i, f)| (FuncId(i as u32), f))
        .ok_or_else(|| Error::UnknownFunction(format!("no function named `{name}` in program")))
}

fn check_call_signature<Args: NoevArgs, Ret: FromNoev>(
    program: &IrProgram,
    func: &Function,
) -> Result<(Vec<Ty>, Ty), Error> {
    let (params, ret_ty) = fn_sig_tys(func);
    for (i, &ty) in params.iter().enumerate() {
        check_boundary_ty(program, ty, &format!("parameter {}", i + 1), &func.name)?;
    }
    check_boundary_ty(program, ret_ty, "return type", &func.name)?;
    Args::check_params(program, &func.name, &params)?;
    if !Ret::matches(&program.types, ret_ty) {
        return Err(Error::SignatureMismatch(format!(
            "`{}` returns Noev type `{}`, which does not match Rust return type `{}`",
            func.name,
            show_ty(program, ret_ty),
            Ret::rust_name()
        )));
    }
    Ok((params, ret_ty))
}

type HostCheckFn = Box<dyn Fn(&IrProgram, &Function) -> Result<(), Error>>;

/// An execution environment for a compiled [`Program`], holding registered host functions,
/// quantum backend, seed, and output streams (`spec/interop.md` §2).
pub struct Runtime<'p, 'h>
where
    'p: 'h,
{
    program: &'p Program,
    backend: &'p dyn Backend,
    hosts: HostTable<'h>,
    hosts_rc: Option<Rc<HostTable<'h>>>,
    host_checks: Vec<(String, HostCheckFn)>,
    seed_state: u64,
    output: Option<Rc<RefCell<Output>>>,
    validated: bool,
}

impl<'p, 'h> Runtime<'p, 'h>
where
    'p: 'h,
{
    /// Creates a runtime for `program` backed by the `ketrs` simulator.
    #[cfg(feature = "ketrs")]
    pub fn new(program: &'p Program) -> Self {
        Self::with_backend(program, &noev_runtime::Ketrs)
    }

    /// Creates a runtime for `program` using a specific [`Backend`].
    pub fn with_backend(program: &'p Program, backend: &'p dyn Backend) -> Self {
        Runtime {
            program,
            backend,
            hosts: HostTable::new(),
            hosts_rc: None,
            host_checks: Vec::new(),
            seed_state: 0,
            output: None,
            validated: false,
        }
    }

    /// Sets the PRNG seed for deterministic simulation.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed_state = seed;
        self
    }

    fn next_seed(&mut self) -> u64 {
        self.seed_state = self.seed_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.seed_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Routes `print` and `eprint` output from Noev code to `output`.
    pub fn output(mut self, output: Output) -> Self {
        self.output = Some(Rc::new(RefCell::new(output)));
        self
    }

    /// Registers a Rust closure as the host implementation for `extern fn <name>`.
    ///
    /// Type and arity compatibility are checked when [`Runtime::validate`] or [`Runtime::call`]
    /// runs.
    pub fn host_fn<F, Args, Ret>(mut self, name: &str, mut f: F) -> Self
    where
        F: HostHandler<Args, Ret> + 'h,
        Args: 'static,
        Ret: 'static,
    {
        let owned = name.to_string();
        self.host_checks.push((
            owned.clone(),
            Box::new(|program, func| F::check_signature(program, func)),
        ));
        self.hosts.register(owned, move |ctx, func_id, args| {
            let program = ctx.program();
            let func = &program.functions[func_id.index()];
            f.invoke(program, func, args).map_err(|e| match e {
                Error::Runtime(r) => r,
                other => RuntimeError::new(other.to_string()),
            })
        });
        self.hosts_rc = None;
        self.validated = false;
        self
    }

    /// Registers a Rust closure that receives a [`HostContext`] as its first argument, allowing
    /// it to call back into Noev functions while running (`spec/interop.md` §2).
    pub fn host_fn_with_context<F, Args, Ret>(mut self, name: &str, mut f: F) -> Self
    where
        F: HostContextHandler<Args, Ret> + 'h,
        Args: 'static,
        Ret: 'static,
    {
        let owned = name.to_string();
        self.host_checks.push((
            owned.clone(),
            Box::new(|program, func| F::check_signature(program, func)),
        ));
        self.hosts.register(owned, move |ctx, func_id, args| {
            let program = ctx.program();
            let func = &program.functions[func_id.index()];
            let mut host_ctx = HostContext { inner: ctx };
            f.invoke(&mut host_ctx, program, func, args)
                .map_err(|e| match e {
                    Error::Runtime(r) => r,
                    other => RuntimeError::new(other.to_string()),
                })
        });
        self.hosts_rc = None;
        self.validated = false;
        self
    }

    /// Validates that every `extern fn` in the program has a registered host implementation with
    /// matching parameter and return types, and that no unknown host functions were registered.
    pub fn validate(&self) -> Result<(), Error> {
        for (name, check) in &self.host_checks {
            let Some(&id) = self.program.by_name.get(name) else {
                return Err(Error::UnknownHostFn(format!(
                    "registered host function `{name}` is not declared as an `extern fn` in the program"
                )));
            };
            let func = &self.program.ir.functions[id.index()];
            if !func.is_extern {
                return Err(Error::UnknownHostFn(format!(
                    "`{name}` is a Noev function, not an `extern fn`"
                )));
            }
            check(&self.program.ir, func)?;
        }
        for func in &self.program.ir.functions {
            if func.is_extern && !self.hosts.contains(&func.name) {
                return Err(Error::MissingHostFn(format!(
                    "missing host implementation for `extern fn {}`",
                    func.name
                )));
            }
        }
        Ok(())
    }

    /// Calls a Noev function named `name` with `args` and returns its result.
    ///
    /// Runs [`Runtime::validate`] on first call, verifies that all boundary types are classical
    /// and copyable without references, checks argument/return types against the Noev signature,
    /// and executes the function on a fresh quantum state.
    pub fn call<Args: NoevArgs, Ret: FromNoev>(
        &mut self,
        name: &str,
        args: Args,
    ) -> Result<Ret, Error> {
        if !self.validated {
            self.validate()?;
            self.validated = true;
        }
        let &func_id = self.program.by_name.get(name).ok_or_else(|| {
            Error::UnknownFunction(format!("no function named `{name}` in program"))
        })?;
        let func = &self.program.ir.functions[func_id.index()];
        let (params, ret_ty) = check_call_signature::<Args, Ret>(&self.program.ir, func)?;
        let values = args.into_values(&self.program.ir.types, &params)?;
        let hosts = self
            .hosts_rc
            .get_or_insert_with(|| Rc::new(self.hosts.clone()))
            .clone();
        let seed = self.next_seed();
        let mut machine = Machine::new(&self.program.ir, self.backend, seed).with_hosts(hosts);
        if let Some(output) = &self.output {
            machine = machine.with_output(output.clone());
        }
        let result = machine.call(func_id, values)?;
        Ret::from_noev(result, &self.program.ir.types, ret_ty)
    }
}
