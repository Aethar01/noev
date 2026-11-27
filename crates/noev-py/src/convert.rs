//! Type-checked conversions between Python objects and Noev runtime [`Value`]s
//! (`spec/interop.md` §1).

use std::rc::Rc;

use noev::embed::show_ty;
use noev_ir::ir::Program as IrProgram;
use noev_runtime::int::Int;
use noev_runtime::value::{Distribution, Observable, Pauli, Value};
use noev_sema::ty::Width;
use noev_sema::{Ty, TyKind};
use num_bigint::BigInt;
use pyo3::exceptions::{
    PyAttributeError, PyIndexError, PyKeyError, PyOverflowError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyComplex, PyDict, PyInt, PyList, PySequence, PyString, PyTuple};

/// A Noev `Observable` (weighted sum of Pauli products) exposed to Python (`spec/interop.md` §1).
#[pyclass(name = "Observable", module = "noev._noev", from_py_object)]
#[derive(Clone, Debug, PartialEq)]
pub struct PyObservable {
    pub(crate) inner: Observable,
}

type PauliTermInput = (f64, Vec<(usize, String)>);
type PauliTermOutput = (f64, Vec<(usize, &'static str)>);

#[pymethods]
impl PyObservable {
    /// Creates an `Observable` from a list of `(coeff, [(qubit, "X"|"Y"|"Z"), ...])` terms.
    #[new]
    #[pyo3(signature = (terms = None))]
    fn new(terms: Option<Vec<PauliTermInput>>) -> PyResult<Self> {
        let mut parsed = Vec::new();
        for (coeff, product) in terms.unwrap_or_default() {
            let mut factors = Vec::with_capacity(product.len());
            for (q, label) in product {
                let pauli = match label.as_str() {
                    "X" | "x" => Pauli::X,
                    "Y" | "y" => Pauli::Y,
                    "Z" | "z" => Pauli::Z,
                    other => {
                        return Err(PyValueError::new_err(format!(
                            "Pauli label must be 'X', 'Y', or 'Z', got {other:?}"
                        )));
                    }
                };
                factors.push((q, pauli));
            }
            parsed.push((coeff, factors));
        }
        Ok(PyObservable {
            inner: Observable { terms: parsed },
        })
    }

    /// Single-qubit Pauli `X(qubit)`.
    #[staticmethod]
    fn x(qubit: usize) -> Self {
        PyObservable {
            inner: Observable {
                terms: vec![(1.0, vec![(qubit, Pauli::X)])],
            },
        }
    }

    /// Single-qubit Pauli `Y(qubit)`.
    #[staticmethod]
    fn y(qubit: usize) -> Self {
        PyObservable {
            inner: Observable {
                terms: vec![(1.0, vec![(qubit, Pauli::Y)])],
            },
        }
    }

    /// Single-qubit Pauli `Z(qubit)`.
    #[staticmethod]
    fn z(qubit: usize) -> Self {
        PyObservable {
            inner: Observable {
                terms: vec![(1.0, vec![(qubit, Pauli::Z)])],
            },
        }
    }

    /// Scalar multiple of the identity observable.
    #[staticmethod]
    #[pyo3(signature = (coeff = 1.0))]
    fn identity(coeff: f64) -> Self {
        PyObservable {
            inner: Observable {
                terms: vec![(coeff, Vec::new())],
            },
        }
    }

    /// The observable's terms as `[(coeff, [(qubit, "X"|"Y"|"Z"), ...]), ...]`.
    #[getter]
    fn terms(&self) -> Vec<PauliTermOutput> {
        self.inner
            .terms
            .iter()
            .map(|(coeff, prod)| {
                let factors = prod
                    .iter()
                    .map(|&(q, p)| {
                        let name = match p {
                            Pauli::X => "X",
                            Pauli::Y => "Y",
                            Pauli::Z => "Z",
                        };
                        (q, name)
                    })
                    .collect();
                (*coeff, factors)
            })
            .collect()
    }

    fn __add__(&self, other: &Self) -> Self {
        let mut terms = self.inner.terms.clone();
        terms.extend(other.inner.terms.iter().cloned());
        PyObservable {
            inner: Observable { terms },
        }
    }

    fn __sub__(&self, other: &Self) -> Self {
        let mut terms = self.inner.terms.clone();
        terms.extend(other.inner.terms.iter().map(|(c, p)| (-c, p.clone())));
        PyObservable {
            inner: Observable { terms },
        }
    }

    fn __neg__(&self) -> Self {
        PyObservable {
            inner: Observable {
                terms: self
                    .inner
                    .terms
                    .iter()
                    .map(|(c, p)| (-c, p.clone()))
                    .collect(),
            },
        }
    }

    fn __mul__(&self, other: &Bound<'_, PyAny>) -> PyResult<Self> {
        if let Ok(scale) = other.extract::<f64>() {
            return Ok(PyObservable {
                inner: Observable {
                    terms: self
                        .inner
                        .terms
                        .iter()
                        .map(|(c, p)| (c * scale, p.clone()))
                        .collect(),
                },
            });
        }
        if let Ok(rhs) = other.extract::<PyRef<'_, PyObservable>>() {
            let mut terms = Vec::with_capacity(self.inner.terms.len() * rhs.inner.terms.len());
            for (c1, p1) in &self.inner.terms {
                for (c2, p2) in &rhs.inner.terms {
                    let mut prod = p1.clone();
                    prod.extend(p2.iter().copied());
                    terms.push((c1 * c2, prod));
                }
            }
            return Ok(PyObservable {
                inner: Observable { terms },
            });
        }
        Err(PyTypeError::new_err(
            "Observable can only be multiplied by a float or another Observable",
        ))
    }

    fn __rmul__(&self, scale: f64) -> Self {
        PyObservable {
            inner: Observable {
                terms: self
                    .inner
                    .terms
                    .iter()
                    .map(|(c, p)| (scale * c, p.clone()))
                    .collect(),
            },
        }
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }

    fn __repr__(&self) -> String {
        format!("Observable({:?})", self.terms())
    }
}

/// A classical Noev struct value exposed as a Python object with attribute access
/// (`spec/interop.md` §1).
#[pyclass(name = "Struct", module = "noev._noev", unsendable)]
pub struct PyStruct {
    /// The Noev struct's type name.
    #[pyo3(get)]
    pub type_name: String,
    fields: Vec<(String, Py<PyAny>)>,
}

#[pymethods]
impl PyStruct {
    /// Creates a Noev struct value with the given `type_name` and keyword fields.
    #[new]
    #[pyo3(signature = (type_name, **kwargs))]
    fn new(type_name: String, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let mut fields = Vec::new();
        if let Some(dict) = kwargs {
            for (k, v) in dict.iter() {
                let name: String = k.extract()?;
                fields.push((name, v.unbind()));
            }
        }
        Ok(PyStruct { type_name, fields })
    }

    /// Returns a dict mapping each field name to its value.
    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for (k, v) in &self.fields {
            dict.set_item(k, v.bind(py))?;
        }
        Ok(dict)
    }

    fn __getattr__<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
        for (k, v) in &self.fields {
            if k == name {
                return Ok(v.bind(py).clone());
            }
        }
        Err(PyAttributeError::new_err(format!(
            "`{}` has no field `{name}`",
            self.type_name
        )))
    }

    fn __getitem__<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Bound<'py, PyAny>> {
        for (k, v) in &self.fields {
            if k == name {
                return Ok(v.bind(py).clone());
            }
        }
        Err(PyKeyError::new_err(format!(
            "`{}` has no field `{name}`",
            self.type_name
        )))
    }

    fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        let Ok(other) = other.cast::<PyStruct>() else {
            return Ok(false);
        };
        let other = other.borrow();
        if self.type_name != other.type_name || self.fields.len() != other.fields.len() {
            return Ok(false);
        }
        for ((k1, v1), (k2, v2)) in self.fields.iter().zip(&other.fields) {
            if k1 != k2 || !v1.bind(py).eq(v2.bind(py))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        let mut parts = Vec::with_capacity(self.fields.len());
        for (k, v) in &self.fields {
            let r: String = v.bind(py).repr()?.extract()?;
            parts.push(format!("{k}={r}"));
        }
        Ok(format!("{}({})", self.type_name, parts.join(", ")))
    }
}

/// A classical Noev enum value exposed to Python.
#[pyclass(name = "Enum", module = "noev._noev", unsendable)]
pub struct PyEnum {
    /// The Noev enum's type name.
    #[pyo3(get)]
    pub type_name: String,
    /// The active variant's name.
    #[pyo3(get)]
    pub variant: String,
    fields: Vec<Py<PyAny>>,
}

#[pymethods]
impl PyEnum {
    /// Creates an enum value from `type_name`, `variant`, and optional positional payload fields.
    #[new]
    #[pyo3(signature = (type_name, variant, *fields))]
    fn new(type_name: String, variant: String, fields: &Bound<'_, PyTuple>) -> Self {
        let items = fields.iter().map(|v| v.unbind()).collect();
        PyEnum {
            type_name,
            variant,
            fields: items,
        }
    }

    /// The variant's payload fields as a tuple.
    #[getter]
    fn fields<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyTuple>> {
        let items: Vec<Bound<'py, PyAny>> =
            self.fields.iter().map(|v| v.bind(py).clone()).collect();
        PyTuple::new(py, items)
    }

    /// The single payload value when the variant has exactly one field, or `None` when unit.
    #[getter]
    fn value<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match self.fields.as_slice() {
            [] => Ok(py.None().into_bound(py)),
            [only] => Ok(only.bind(py).clone()),
            _ => Err(PyIndexError::new_err(format!(
                "{}::{} has {} fields; use `.fields`",
                self.type_name,
                self.variant,
                self.fields.len()
            ))),
        }
    }

    fn __eq__(&self, py: Python<'_>, other: &Bound<'_, PyAny>) -> PyResult<bool> {
        let Ok(other) = other.cast::<PyEnum>() else {
            return Ok(false);
        };
        let other = other.borrow();
        if self.type_name != other.type_name
            || self.variant != other.variant
            || self.fields.len() != other.fields.len()
        {
            return Ok(false);
        }
        for (a, b) in self.fields.iter().zip(&other.fields) {
            if !a.bind(py).eq(b.bind(py))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        if self.fields.is_empty() {
            return Ok(format!("{}::{}", self.type_name, self.variant));
        }
        let mut parts = Vec::with_capacity(self.fields.len());
        for v in &self.fields {
            let r: String = v.bind(py).repr()?.extract()?;
            parts.push(r);
        }
        Ok(format!(
            "{}::{}({})",
            self.type_name,
            self.variant,
            parts.join(", ")
        ))
    }
}

/// A sampled `Distribution<T>` returned from a Noev function.
#[pyclass(name = "Distribution", module = "noev._noev", unsendable)]
pub struct PyDistribution {
    /// Total number of shots in this distribution.
    #[pyo3(get)]
    pub shots: usize,
    outcomes: Vec<(String, Py<PyAny>, usize)>,
    rendered: String,
}

#[pymethods]
impl PyDistribution {
    /// List of `(value, count)` pairs, most frequent first.
    fn outcomes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        let list = PyList::empty(py);
        for (_, val, count) in &self.outcomes {
            let pair = PyTuple::new(
                py,
                [val.bind(py).clone(), count.into_pyobject(py)?.into_any()],
            )?;
            list.append(pair)?;
        }
        Ok(list)
    }

    /// Number of shots that produced `value` (or formatted key string).
    fn count(&self, py: Python<'_>, target: &Bound<'_, PyAny>) -> PyResult<usize> {
        for (key, val, count) in &self.outcomes {
            if val.bind(py).eq(target)? {
                return Ok(*count);
            }
            if let Ok(s) = target.extract::<&str>()
                && key == s
            {
                return Ok(*count);
            }
        }
        Ok(0)
    }

    /// Fraction of shots that produced `value` (or formatted key string).
    fn probability(&self, py: Python<'_>, target: &Bound<'_, PyAny>) -> PyResult<f64> {
        if self.shots == 0 {
            return Ok(0.0);
        }
        Ok(self.count(py, target)? as f64 / self.shots as f64)
    }

    fn __str__(&self) -> &str {
        &self.rendered
    }

    fn __repr__(&self) -> String {
        format!(
            "Distribution(shots={}, outcomes={})",
            self.shots,
            self.outcomes.len()
        )
    }
}

/// Converts a Noev runtime [`Value`] of type `ty` into a Python object.
pub fn noev_to_py<'py>(
    py: Python<'py>,
    program: &IrProgram,
    value: &Value,
    ty: Ty,
) -> PyResult<Bound<'py, PyAny>> {
    match (program.types.kind(ty), value) {
        (TyKind::Unit, Value::Unit) => Ok(py.None().into_bound(py)),
        (TyKind::Int, Value::Int(n)) => match n {
            Int::Small(v) => Ok(v.into_pyobject(py)?.into_any()),
            Int::Big(b) => {
                let int_ty = py.get_type::<PyInt>();
                int_ty.call1((b.to_string(),))
            }
        },
        (
            TyKind::I8
            | TyKind::I16
            | TyKind::I32
            | TyKind::I64
            | TyKind::U8
            | TyKind::U16
            | TyKind::U32
            | TyKind::U64,
            Value::Sized(v, _),
        ) => Ok(v.into_pyobject(py)?.into_any()),
        (TyKind::Float, Value::Float(v)) => Ok(v.into_pyobject(py)?.into_any()),
        (TyKind::F32, Value::F32(v)) => Ok((*v as f64).into_pyobject(py)?.into_any()),
        (TyKind::Complex, Value::Complex(re, im)) => {
            Ok(PyComplex::from_doubles(py, *re, *im).into_any())
        }
        (TyKind::C32, Value::C32(re, im)) => {
            Ok(PyComplex::from_doubles(py, f64::from(*re), f64::from(*im)).into_any())
        }
        (TyKind::Bool, Value::Bool(b)) => Ok(PyBool::new(py, *b).to_owned().into_any()),
        (TyKind::Bit, Value::Bit(b)) => Ok(u8::from(*b).into_pyobject(py)?.into_any()),
        (TyKind::String, Value::Str(s)) => Ok(PyString::new(py, s).into_any()),
        (TyKind::Observable, Value::Observable(obs)) => {
            Ok(Py::new(py, PyObservable { inner: obs.clone() })?
                .into_bound(py)
                .into_any())
        }
        (TyKind::Tuple(elems), Value::Aggregate(items)) => {
            let mut out = Vec::with_capacity(elems.len());
            for (item, &elem_ty) in items.iter().zip(elems.iter()) {
                out.push(noev_to_py(py, program, item, elem_ty)?);
            }
            Ok(PyTuple::new(py, out)?.into_any())
        }
        (&TyKind::Array(elem_ty, _), Value::Aggregate(items)) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(noev_to_py(py, program, item, elem_ty)?)?;
            }
            Ok(list.into_any())
        }
        (&TyKind::Distribution(elem_ty), Value::Distribution(dist)) => {
            let py_dist = dist_to_py(py, program, dist, elem_ty)?;
            Ok(Py::new(py, py_dist)?.into_bound(py).into_any())
        }
        (&TyKind::Struct(id), Value::Aggregate(items)) => {
            let def = &program.types.structs[id.index()];
            let type_name = program
                .struct_names
                .get(id.index())
                .cloned()
                .unwrap_or_else(|| format!("struct#{}", id.index()));
            let names = &program.struct_fields[id.index()];
            let mut fields = Vec::with_capacity(items.len());
            for ((item, name), &(_, field_ty)) in items.iter().zip(names).zip(&def.fields) {
                let py_val = noev_to_py(py, program, item, field_ty)?;
                fields.push((name.clone(), py_val.unbind()));
            }
            Ok(Py::new(py, PyStruct { type_name, fields })?
                .into_bound(py)
                .into_any())
        }
        (&TyKind::Struct(id), Value::Variant(tag, data)) => {
            let def = &program.types.structs[id.index()];
            let type_name = program
                .struct_names
                .get(id.index())
                .cloned()
                .unwrap_or_else(|| format!("enum#{}", id.index()));
            let variant_idx = *tag as usize;
            let variant = program
                .variant_names
                .get(id.index())
                .and_then(|vs| vs.get(variant_idx))
                .cloned()
                .unwrap_or_else(|| format!("variant#{variant_idx}"));
            let field_tys = def
                .variants
                .as_ref()
                .and_then(|vs| vs.get(variant_idx))
                .map(|v| v.data.as_slice())
                .unwrap_or(&[]);
            let mut fields = Vec::with_capacity(data.len());
            for (item, &field_ty) in data.iter().zip(field_tys) {
                fields.push(noev_to_py(py, program, item, field_ty)?.unbind());
            }
            Ok(Py::new(
                py,
                PyEnum {
                    type_name,
                    variant,
                    fields,
                },
            )?
            .into_bound(py)
            .into_any())
        }
        _ => Err(PyTypeError::new_err(format!(
            "cannot convert Noev value of type `{}` to Python",
            show_ty(program, ty)
        ))),
    }
}

fn dist_to_py(
    py: Python<'_>,
    program: &IrProgram,
    dist: &Distribution,
    elem_ty: Ty,
) -> PyResult<PyDistribution> {
    let mut outcomes = Vec::with_capacity(dist.outcomes.len());
    for o in &dist.outcomes {
        let py_val = noev_to_py(py, program, &o.value, elem_ty)?;
        outcomes.push((o.key.clone(), py_val.unbind(), o.count));
    }
    Ok(PyDistribution {
        shots: dist.shots,
        outcomes,
        rendered: dist.to_string(),
    })
}

/// Converts a Python object into a Noev runtime [`Value`] matching expected type `ty`.
pub fn py_to_noev(
    py: Python<'_>,
    program: &IrProgram,
    obj: &Bound<'_, PyAny>,
    ty: Ty,
) -> PyResult<Value> {
    match program.types.kind(ty) {
        TyKind::Unit => {
            if obj.is_none() {
                Ok(Value::Unit)
            } else {
                Err(PyTypeError::new_err(format!(
                    "expected `None` for Noev `()`, got `{}`",
                    obj.get_type().name()?
                )))
            }
        }
        TyKind::Int => {
            if obj.is_instance_of::<PyBool>() {
                return Err(PyTypeError::new_err(
                    "expected `int` for Noev `Int`, got `bool`",
                ));
            }
            if !obj.is_instance_of::<PyInt>() {
                return Err(PyTypeError::new_err(format!(
                    "expected `int` for Noev `Int`, got `{}`",
                    obj.get_type().name()?
                )));
            }
            if let Ok(v) = obj.extract::<i64>() {
                return Ok(Value::Int(Int::Small(v)));
            }
            let text = obj.str()?;
            let big = BigInt::parse_bytes(text.to_str()?.as_bytes(), 10).ok_or_else(|| {
                PyValueError::new_err("could not parse Python int into Noev `Int`")
            })?;
            Ok(Value::Int(Int::from(big)))
        }
        TyKind::I8 => py_to_sized(
            obj,
            Width {
                bits: 8,
                signed: true,
            },
        ),
        TyKind::I16 => py_to_sized(
            obj,
            Width {
                bits: 16,
                signed: true,
            },
        ),
        TyKind::I32 => py_to_sized(
            obj,
            Width {
                bits: 32,
                signed: true,
            },
        ),
        TyKind::I64 => py_to_sized(
            obj,
            Width {
                bits: 64,
                signed: true,
            },
        ),
        TyKind::U8 => py_to_sized(
            obj,
            Width {
                bits: 8,
                signed: false,
            },
        ),
        TyKind::U16 => py_to_sized(
            obj,
            Width {
                bits: 16,
                signed: false,
            },
        ),
        TyKind::U32 => py_to_sized(
            obj,
            Width {
                bits: 32,
                signed: false,
            },
        ),
        TyKind::U64 => py_to_sized(
            obj,
            Width {
                bits: 64,
                signed: false,
            },
        ),
        TyKind::Float => {
            if obj.is_instance_of::<PyBool>() {
                return Err(PyTypeError::new_err(
                    "expected `float` for Noev `Float`, got `bool`",
                ));
            }
            let v: f64 = obj.extract().map_err(|_| {
                PyTypeError::new_err(format!(
                    "expected `float` for Noev `Float`, got `{}`",
                    obj.get_type()
                        .name()
                        .map(|n| n.to_string())
                        .unwrap_or_default()
                ))
            })?;
            Ok(Value::Float(v))
        }
        TyKind::F32 => {
            if obj.is_instance_of::<PyBool>() {
                return Err(PyTypeError::new_err(
                    "expected `float` for Noev `f32`, got `bool`",
                ));
            }
            let v: f64 = obj
                .extract()
                .map_err(|_| PyTypeError::new_err("expected `float` for Noev `f32`"))?;
            Ok(Value::F32(v as f32))
        }
        TyKind::Complex => {
            let (re, im) = py_to_complex(obj)?;
            Ok(Value::Complex(re, im))
        }
        TyKind::C32 => {
            let (re, im) = py_to_complex(obj)?;
            Ok(Value::C32(re as f32, im as f32))
        }
        TyKind::Bool => {
            if !obj.is_instance_of::<PyBool>() {
                return Err(PyTypeError::new_err(format!(
                    "expected `bool` for Noev `Bool`, got `{}`",
                    obj.get_type().name()?
                )));
            }
            Ok(Value::Bool(obj.extract::<bool>()?))
        }
        TyKind::Bit => {
            if obj.is_instance_of::<PyBool>() || !obj.is_instance_of::<PyInt>() {
                return Err(PyTypeError::new_err(format!(
                    "expected `int` (0 or 1) for Noev `Bit`, got `{}`",
                    obj.get_type().name()?
                )));
            }
            match obj.extract::<i64>() {
                Ok(0) => Ok(Value::Bit(false)),
                Ok(1) => Ok(Value::Bit(true)),
                _ => Err(PyValueError::new_err("Noev `Bit` value must be 0 or 1")),
            }
        }
        TyKind::String => {
            if !obj.is_instance_of::<PyString>() {
                return Err(PyTypeError::new_err(format!(
                    "expected `str` for Noev `String`, got `{}`",
                    obj.get_type().name()?
                )));
            }
            let s: String = obj.extract()?;
            Ok(Value::Str(Rc::from(s)))
        }
        TyKind::Observable => {
            let obs: PyRef<'_, PyObservable> = obj.extract().map_err(|_| {
                PyTypeError::new_err("expected `noev.Observable` for Noev `Observable`")
            })?;
            Ok(Value::Observable(obs.inner.clone()))
        }
        TyKind::Tuple(elems) => {
            let tuple = obj.cast::<PyTuple>().map_err(|_| {
                PyTypeError::new_err(format!(
                    "expected `tuple` for Noev `{}`",
                    show_ty(program, ty)
                ))
            })?;
            if tuple.len() != elems.len() {
                return Err(PyValueError::new_err(format!(
                    "expected tuple of length {} for Noev `{}`, got {}",
                    elems.len(),
                    show_ty(program, ty),
                    tuple.len()
                )));
            }
            let mut out = Vec::with_capacity(elems.len());
            for (item, &elem_ty) in tuple.iter().zip(elems.iter()) {
                out.push(py_to_noev(py, program, &item, elem_ty)?);
            }
            Ok(Value::Aggregate(out))
        }
        &TyKind::Array(elem_ty, len) => {
            if obj.is_instance_of::<PyString>() {
                return Err(PyTypeError::new_err(format!(
                    "expected sequence for Noev `{}`, got `str`",
                    show_ty(program, ty)
                )));
            }
            let seq = obj.cast::<PySequence>().map_err(|_| {
                PyTypeError::new_err(format!(
                    "expected list/sequence for Noev `{}`",
                    show_ty(program, ty)
                ))
            })?;
            let actual = seq.len()?;
            if actual as u64 != len {
                return Err(PyValueError::new_err(format!(
                    "expected sequence of length {len} for Noev `{}`, got {actual}",
                    show_ty(program, ty)
                )));
            }
            let mut out = Vec::with_capacity(actual);
            for i in 0..actual {
                let item = seq.get_item(i)?;
                out.push(py_to_noev(py, program, &item, elem_ty)?);
            }
            Ok(Value::Aggregate(out))
        }
        &TyKind::Struct(id) => {
            let def = &program.types.structs[id.index()];
            let expected_name = program
                .struct_names
                .get(id.index())
                .map(String::as_str)
                .unwrap_or("<struct>");
            if let Some(variants) = &def.variants {
                let py_enum = obj.cast::<PyEnum>().map_err(|_| {
                    PyTypeError::new_err(format!(
                        "expected `noev.Enum` for Noev enum `{expected_name}`"
                    ))
                })?;
                let py_enum = py_enum.borrow();
                if py_enum.type_name != expected_name {
                    return Err(PyTypeError::new_err(format!(
                        "expected enum `{expected_name}`, got `{}`",
                        py_enum.type_name
                    )));
                }
                let variant_names = &program.variant_names[id.index()];
                let tag = variant_names
                    .iter()
                    .position(|n| n == &py_enum.variant)
                    .ok_or_else(|| {
                        PyValueError::new_err(format!(
                            "enum `{expected_name}` has no variant `{}`",
                            py_enum.variant
                        ))
                    })?;
                let field_tys = &variants[tag].data;
                if py_enum.fields.len() != field_tys.len() {
                    return Err(PyValueError::new_err(format!(
                        "`{expected_name}::{}` expects {} field(s), got {}",
                        py_enum.variant,
                        field_tys.len(),
                        py_enum.fields.len()
                    )));
                }
                let mut data = Vec::with_capacity(field_tys.len());
                for (field, &field_ty) in py_enum.fields.iter().zip(field_tys) {
                    data.push(py_to_noev(py, program, field.bind(py), field_ty)?);
                }
                Ok(Value::Variant(tag as u32, data))
            } else {
                let field_names = &program.struct_fields[id.index()];
                let mut out = Vec::with_capacity(field_names.len());
                if let Ok(py_struct) = obj.cast::<PyStruct>() {
                    let py_struct = py_struct.borrow();
                    if py_struct.type_name != expected_name {
                        return Err(PyTypeError::new_err(format!(
                            "expected struct `{expected_name}`, got `{}`",
                            py_struct.type_name
                        )));
                    }
                    for (name, &(_, field_ty)) in field_names.iter().zip(&def.fields) {
                        let (_, val) = py_struct
                            .fields
                            .iter()
                            .find(|(k, _)| k == name)
                            .ok_or_else(|| {
                                PyValueError::new_err(format!(
                                    "missing field `{name}` on struct `{expected_name}`"
                                ))
                            })?;
                        out.push(py_to_noev(py, program, val.bind(py), field_ty)?);
                    }
                } else if let Ok(dict) = obj.cast::<PyDict>() {
                    for (name, &(_, field_ty)) in field_names.iter().zip(&def.fields) {
                        let val = dict.get_item(name)?.ok_or_else(|| {
                            PyValueError::new_err(format!(
                                "missing field `{name}` for struct `{expected_name}`"
                            ))
                        })?;
                        out.push(py_to_noev(py, program, &val, field_ty)?);
                    }
                } else {
                    for (name, &(_, field_ty)) in field_names.iter().zip(&def.fields) {
                        let val = obj.getattr(name.as_str()).map_err(|_| {
                            PyTypeError::new_err(format!(
                                "expected `noev.Struct`, `dict`, or object with field `{name}` for `{expected_name}`"
                            ))
                        })?;
                        out.push(py_to_noev(py, program, &val, field_ty)?);
                    }
                }
                Ok(Value::Aggregate(out))
            }
        }
        _ => Err(PyTypeError::new_err(format!(
            "Noev type `{}` cannot cross the Python boundary",
            show_ty(program, ty)
        ))),
    }
}

fn py_to_sized(obj: &Bound<'_, PyAny>, width: Width) -> PyResult<Value> {
    if obj.is_instance_of::<PyBool>() || !obj.is_instance_of::<PyInt>() {
        return Err(PyTypeError::new_err(format!(
            "expected `int` for Noev `{}`, got `{}`",
            width.name(),
            obj.get_type().name()?
        )));
    }
    let v: i128 = obj.extract().map_err(|_| {
        PyOverflowError::new_err(format!("integer out of range for Noev `{}`", width.name()))
    })?;
    if v < width.min() || v > width.max() {
        return Err(PyOverflowError::new_err(format!(
            "integer {v} out of range for Noev `{}` ({}..={})",
            width.name(),
            width.min(),
            width.max()
        )));
    }
    Ok(Value::Sized(v, width))
}

fn py_to_complex(obj: &Bound<'_, PyAny>) -> PyResult<(f64, f64)> {
    if let Ok(c) = obj.cast::<PyComplex>() {
        return Ok((c.real(), c.imag()));
    }
    if !obj.is_instance_of::<PyBool>()
        && let Ok(re) = obj.extract::<f64>()
    {
        return Ok((re, 0.0));
    }
    Err(PyTypeError::new_err(format!(
        "expected `complex` or `float`, got `{}`",
        obj.get_type().name()?
    )))
}
