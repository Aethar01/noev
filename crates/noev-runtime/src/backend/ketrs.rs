//! The ketrs quantum simulation engines (`ketrs` and `ketrs:<device>`).
//!
//! Supported devices:
//! - `ketrs` / `ketrs:sv` / `ketrs:sv64` — adaptive FP64 statevector (`RealStatevector` → `Statevector`), both levels.
//! - `ketrs:sv32` — adaptive FP32 statevector (`RealStatevector32` → `Statevector32`), both levels.
//! - `ketrs:mps` — open-boundary matrix product state (`ketrs::Mps`), circuit level (supports 30+ qubits and mid-circuit control).
//! - `ketrs:pmps` — periodic-ring matrix product state (`ketrs::PeriodicMps`), circuit level (unitary + terminal measurements).
//! - `ketrs:density` — density matrix (`ketrs::DensityMatrix`), circuit level (up to 14 qubits, unitary + terminal measurements).
//! - `ketrs:gpu` / `ketrs:gpu32` — Vulkan GPU statevector (`ketrs::GpuStatevector`, requires the `ketrs-gpu` Cargo feature), circuit level.

use std::collections::BTreeMap;

use ketrs::{
    Complex32, Complex64, DensityMatrix, Mps, MpsOptions, Operation, OptimizationLevel,
    PauliString, PauliSum, PeriodicMps, RealStatevector, RealStatevector32, Statevector,
    Statevector32,
};
use noev_sema::prelude::Gate;
use rand::SeedableRng;
use rand::rngs::SmallRng;

use super::{Backend, BackendError, Circuit, Condition, Counts, Instruction, Op, Pauli, Session};

/// The most qubits a complex FP64 statevector holds: 2^30 amplitudes take 16 GiB.
const MAX_QUBITS_SV64: usize = 30;
/// The most qubits a complex FP32 statevector holds: 2^31 amplitudes take 16 GiB.
const MAX_QUBITS_SV32: usize = 31;
/// The most qubits a density matrix holds: 4^14 complex amplitudes take 4 GiB.
const MAX_QUBITS_DENSITY: usize = 14;

/// Tolerance for checking whether an angle or matrix entry is real.
const REAL_TOL: f64 = 1e-12;

/// Default circuit optimization level used by [`Ketrs`].
pub const DEFAULT_OPT_LEVEL: u8 = 2;

/// Which `ketrs` simulation engine to run on (`--backend ketrs[:DEVICE]`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KetrsEngine {
    /// Adaptive FP64 dense statevector (`ketrs`, `ketrs:sv`, `ketrs:sv64`).
    #[default]
    Statevector,
    /// Adaptive FP32 dense statevector (`ketrs:sv32`).
    Statevector32,
    /// Open-boundary Matrix Product State (`ketrs:mps`).
    Mps,
    /// Periodic-ring Matrix Product State (`ketrs:pmps`).
    PeriodicMps,
    /// Density matrix simulator (`ketrs:density`).
    Density,
    /// Vulkan GPU FP64 statevector (`ketrs:gpu`, requires `ketrs-gpu` feature).
    Gpu,
    /// Vulkan GPU FP32 statevector (`ketrs:gpu32`, requires `ketrs-gpu` feature).
    Gpu32,
}

impl KetrsEngine {
    /// Parses a `ketrs` device suffix (`sv`, `sv64`, `sv32`, `mps`, `pmps`, `density`, `gpu`, `gpu32`).
    pub fn from_device(device: Option<&str>) -> Option<Self> {
        match device {
            None | Some("sv" | "sv64") => Some(KetrsEngine::Statevector),
            Some("sv32") => Some(KetrsEngine::Statevector32),
            Some("mps") => Some(KetrsEngine::Mps),
            Some("pmps") => Some(KetrsEngine::PeriodicMps),
            Some("density") => Some(KetrsEngine::Density),
            Some("gpu") => Some(KetrsEngine::Gpu),
            Some("gpu32") => Some(KetrsEngine::Gpu32),
            Some(_) => None,
        }
    }

    /// Parses a full backend spec (`"ketrs"` or `"ketrs:<device>"`).
    pub fn from_spec(spec: &str) -> Option<Self> {
        if spec == "ketrs" {
            return Some(KetrsEngine::Statevector);
        }
        spec.strip_prefix("ketrs:")
            .and_then(|dev| Self::from_device(Some(dev)))
    }

    /// Canonical display name (`"ketrs"` or `"ketrs:<device>"`).
    pub fn display_name(self) -> &'static str {
        match self {
            KetrsEngine::Statevector => "ketrs",
            KetrsEngine::Statevector32 => "ketrs:sv32",
            KetrsEngine::Mps => "ketrs:mps",
            KetrsEngine::PeriodicMps => "ketrs:pmps",
            KetrsEngine::Density => "ketrs:density",
            KetrsEngine::Gpu => "ketrs:gpu",
            KetrsEngine::Gpu32 => "ketrs:gpu32",
        }
    }

    /// Target description for this engine.
    pub fn target_spec(self) -> noev_sema::TargetSpec {
        match self {
            KetrsEngine::Statevector => noev_sema::TargetSpec::ketrs(),
            KetrsEngine::Statevector32 => noev_sema::TargetSpec::ketrs_device("sv32").unwrap(),
            KetrsEngine::Mps => noev_sema::TargetSpec::ketrs_device("mps").unwrap(),
            KetrsEngine::PeriodicMps => noev_sema::TargetSpec::ketrs_device("pmps").unwrap(),
            KetrsEngine::Density => noev_sema::TargetSpec::ketrs_device("density").unwrap(),
            KetrsEngine::Gpu => noev_sema::TargetSpec::ketrs_device("gpu").unwrap(),
            KetrsEngine::Gpu32 => noev_sema::TargetSpec::ketrs_device("gpu32").unwrap(),
        }
    }
}

/// The ketrs simulator using the default engine (FP64 statevector) and optimization level (`2`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ketrs;

impl Ketrs {
    /// Returns a [`KetrsBackend`] configured with circuit optimization level `0`..=`3`.
    pub fn with_opt_level(opt_level: u8) -> KetrsBackend {
        KetrsBackend::new(opt_level)
    }

    /// Returns a [`KetrsBackend`] for a specific [`KetrsEngine`] and optimization level `0`..=`3`.
    pub fn with_engine(engine: KetrsEngine, opt_level: u8) -> KetrsBackend {
        KetrsBackend::with_engine(engine, opt_level)
    }
}

/// The ketrs simulator configured with a [`KetrsEngine`] and circuit optimization level (`0`..=`3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KetrsBackend {
    engine: KetrsEngine,
    opt_level: u8,
}

impl Default for KetrsBackend {
    fn default() -> Self {
        Self::new(DEFAULT_OPT_LEVEL)
    }
}

impl KetrsBackend {
    /// Creates a default FP64 statevector backend with circuit optimization level `0`..=`3`.
    pub fn new(opt_level: u8) -> Self {
        Self::with_engine(KetrsEngine::Statevector, opt_level)
    }

    /// Creates a backend for `engine` with circuit optimization level `opt_level` (clamped to `0`..=`3`).
    pub fn with_engine(engine: KetrsEngine, opt_level: u8) -> Self {
        KetrsBackend {
            engine,
            opt_level: opt_level.min(3),
        }
    }

    /// Parses `"ketrs"` or `"ketrs:<device>"` into a [`KetrsBackend`].
    pub fn from_spec(spec: &str, opt_level: u8) -> Option<Self> {
        KetrsEngine::from_spec(spec).map(|engine| Self::with_engine(engine, opt_level))
    }

    /// Selected engine.
    pub fn engine(&self) -> KetrsEngine {
        self.engine
    }

    /// Configured circuit optimization level (`0`..=`3`).
    pub fn opt_level(&self) -> u8 {
        self.opt_level
    }
}

fn ketrs_opt_level(level: u8) -> OptimizationLevel {
    match level {
        0 => OptimizationLevel::Level0,
        1 => OptimizationLevel::Level1,
        2 => OptimizationLevel::Level2,
        _ => OptimizationLevel::Level3,
    }
}

fn mps_options_from_env() -> MpsOptions {
    let mut opts = MpsOptions::default();
    if let Ok(v) = std::env::var("NOEV_MPS_MAX_BOND")
        && let Ok(n) = v.parse::<usize>()
        && n > 0
    {
        opts.max_bond = n;
    }
    if let Ok(v) = std::env::var("NOEV_MPS_CUTOFF")
        && let Ok(c) = v.parse::<f64>()
        && c >= 0.0
    {
        opts.cutoff = c;
    }
    opts
}

impl Backend for Ketrs {
    fn name(&self) -> &str {
        "ketrs"
    }

    fn exact_expectations(&self) -> bool {
        true
    }

    fn target(&self) -> noev_sema::TargetSpec {
        noev_sema::TargetSpec::ketrs()
    }

    fn session(&self, seed: u64) -> Option<Box<dyn Session>> {
        KetrsBackend::default().session(seed)
    }

    fn run_circuit(
        &self,
        circuit: &Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<Counts, BackendError>> {
        KetrsBackend::default().run_circuit(circuit, shots, seed)
    }
}

impl Backend for KetrsBackend {
    fn name(&self) -> &str {
        self.engine.display_name()
    }

    fn exact_expectations(&self) -> bool {
        matches!(
            self.engine,
            KetrsEngine::Statevector | KetrsEngine::Statevector32
        )
    }

    fn target(&self) -> noev_sema::TargetSpec {
        self.engine.target_spec()
    }

    fn session(&self, seed: u64) -> Option<Box<dyn Session>> {
        match self.engine {
            KetrsEngine::Statevector => Some(Box::new(KetrsSession {
                state: KetrsState::Real64(RealStatevector::zero(0)),
                rng: SmallRng::seed_from_u64(seed),
            })),
            KetrsEngine::Statevector32 => Some(Box::new(KetrsSession {
                state: KetrsState::Real32(RealStatevector32::zero(0)),
                rng: SmallRng::seed_from_u64(seed),
            })),
            KetrsEngine::Mps
            | KetrsEngine::PeriodicMps
            | KetrsEngine::Density
            | KetrsEngine::Gpu
            | KetrsEngine::Gpu32 => None,
        }
    }

    fn run_circuit(
        &self,
        circuit: &Circuit,
        shots: usize,
        seed: u64,
    ) -> Option<Result<Counts, BackendError>> {
        if circuit.clbits > 64 {
            return None;
        }
        let converted = match convert(circuit, &circuit.instructions) {
            Ok(converted) => converted,
            Err(e) => return Some(Err(e)),
        };
        let optimized = if self.opt_level > 0
            && (converted.ops().len() > 1 || !converted.instructions().is_empty())
        {
            converted.optimize_level(ketrs_opt_level(self.opt_level))
        } else {
            converted
        };
        let mut rng = SmallRng::seed_from_u64(seed);
        match self.engine {
            KetrsEngine::Statevector => {
                if circuit.qubits > MAX_QUBITS_SV64 {
                    return None;
                }
                if circuit_is_real(&optimized) {
                    match RealStatevector::zero(circuit.qubits)
                        .sample_circuit(&optimized, shots, &mut rng)
                    {
                        Ok(counts) => return Some(Ok(counts.into_iter().collect())),
                        Err(ketrs::Error::NotReal(_)) => {
                            rng = SmallRng::seed_from_u64(seed);
                        }
                        Err(e) => return Some(Err(sim_error(e))),
                    }
                }
                Some(
                    Statevector::zero(circuit.qubits)
                        .sample_circuit(&optimized, shots, &mut rng)
                        .map(|counts| counts.into_iter().collect())
                        .map_err(sim_error),
                )
            }
            KetrsEngine::Statevector32 => {
                if circuit.qubits > MAX_QUBITS_SV32 {
                    return None;
                }
                if circuit_is_real(&optimized) {
                    match RealStatevector32::zero(circuit.qubits)
                        .sample_circuit(&optimized, shots, &mut rng)
                    {
                        Ok(counts) => return Some(Ok(counts.into_iter().collect())),
                        Err(ketrs::Error::NotReal(_)) => {
                            rng = SmallRng::seed_from_u64(seed);
                        }
                        Err(e) => return Some(Err(sim_error(e))),
                    }
                }
                Some(
                    Statevector32::zero(circuit.qubits)
                        .sample_circuit(&optimized, shots, &mut rng)
                        .map(|counts| counts.into_iter().collect())
                        .map_err(sim_error),
                )
            }
            KetrsEngine::Mps => {
                let opts = mps_options_from_env();
                Some(
                    Mps::zero(circuit.qubits, opts)
                        .sample_circuit(&optimized, shots, &mut rng)
                        .map(|counts| counts.into_iter().collect())
                        .map_err(sim_error),
                )
            }
            KetrsEngine::PeriodicMps => Some(run_periodic_mps(&optimized, shots, &mut rng)),
            KetrsEngine::Density => {
                if circuit.qubits > MAX_QUBITS_DENSITY {
                    return Some(Err(BackendError(format!(
                        "ketrs:density supports at most {MAX_QUBITS_DENSITY} qubits (got {})",
                        circuit.qubits
                    ))));
                }
                Some(run_density(&optimized, shots, &mut rng))
            }
            KetrsEngine::Gpu => Some(run_gpu(&optimized, shots, &mut rng, true)),
            KetrsEngine::Gpu32 => Some(run_gpu(&optimized, shots, &mut rng, false)),
        }
    }
}

/// Splits a circuit that has only terminal measurements into its unitary prefix and
/// `(qubit, clbit)` measurement pairs.
fn split_terminal_measures(
    engine: &str,
    circuit: &ketrs::Circuit,
) -> Result<(ketrs::Circuit, Vec<(usize, usize)>), BackendError> {
    let op_count = circuit.ops().len();
    let mut measures = Vec::new();
    for (pos, inst) in circuit.instructions() {
        match inst {
            ketrs::Instruction::Measure { qubit, clbit } if *pos == op_count => {
                measures.push((*qubit, *clbit));
            }
            _ => {
                return Err(BackendError(format!(
                    "{engine} only supports unitary circuits with terminal measurements; \
                     use `ketrs`, `ketrs:sv32`, `ketrs:mps`, or `ketrs:gpu` for mid-circuit \
                     measurement and classical control"
                )));
            }
        }
    }
    let mut unitary = ketrs::Circuit::new(circuit.n_qubits());
    unitary.set_global_phase(circuit.global_phase());
    for op in circuit.ops() {
        unitary.push(op.clone()).map_err(sim_error)?;
    }
    Ok((unitary, measures))
}

fn run_periodic_mps(
    circuit: &ketrs::Circuit,
    shots: usize,
    rng: &mut SmallRng,
) -> Result<Counts, BackendError> {
    let (unitary, measures) = split_terminal_measures("ketrs:pmps", circuit)?;
    let mut state = PeriodicMps::zero(circuit.n_qubits(), mps_options_from_env());
    state.run(&unitary).map_err(sim_error)?;
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for sample in state.sample_bits(shots, rng) {
        let mut bits = 0u64;
        for &(qubit, clbit) in &measures {
            if sample.get(qubit).copied().unwrap_or(false) {
                bits |= 1u64 << clbit;
            }
        }
        *counts.entry(bits).or_default() += 1;
    }
    Ok(counts.into_iter().collect())
}

fn run_density(
    circuit: &ketrs::Circuit,
    shots: usize,
    rng: &mut SmallRng,
) -> Result<Counts, BackendError> {
    let (unitary, measures) = split_terminal_measures("ketrs:density", circuit)?;
    let mut state = DensityMatrix::zero(circuit.n_qubits());
    state.run(&unitary).map_err(sim_error)?;
    let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
    for index in state.sample(shots, rng) {
        let mut bits = 0u64;
        for &(qubit, clbit) in &measures {
            if (index >> qubit) & 1 == 1 {
                bits |= 1u64 << clbit;
            }
        }
        *counts.entry(bits).or_default() += 1;
    }
    Ok(counts.into_iter().collect())
}

#[cfg(feature = "ketrs-gpu")]
fn gpu_context() -> Result<std::sync::Arc<ketrs::GpuContext>, BackendError> {
    use std::sync::{Arc, OnceLock};
    static CTX: OnceLock<Result<Arc<ketrs::GpuContext>, String>> = OnceLock::new();
    CTX.get_or_init(|| ketrs::GpuContext::new().map_err(|e| e.to_string()))
        .clone()
        .map_err(|e| BackendError(format!("cannot initialise Vulkan GPU context: {e}")))
}

#[cfg(feature = "ketrs-gpu")]
fn run_gpu(
    circuit: &ketrs::Circuit,
    shots: usize,
    rng: &mut SmallRng,
    double: bool,
) -> Result<Counts, BackendError> {
    let limit = if double {
        MAX_QUBITS_SV64
    } else {
        MAX_QUBITS_SV32
    };
    if circuit.n_qubits() > limit {
        return Err(BackendError(format!(
            "circuit needs {} qubits, above GPU statevector limit ({limit})",
            circuit.n_qubits()
        )));
    }
    let ctx = gpu_context()?;
    let precision = if double {
        ketrs::GpuPrecision::Double
    } else {
        ketrs::GpuPrecision::Single
    };
    let state = ketrs::GpuStatevector::zero_with_precision(&ctx, circuit.n_qubits(), precision)
        .map_err(sim_error)?;
    state
        .sample_circuit(circuit, shots, rng)
        .map(|counts| counts.into_iter().collect())
        .map_err(sim_error)
}

#[cfg(not(feature = "ketrs-gpu"))]
fn run_gpu(
    _circuit: &ketrs::Circuit,
    _shots: usize,
    _rng: &mut SmallRng,
    double: bool,
) -> Result<Counts, BackendError> {
    let name = if double { "ketrs:gpu" } else { "ketrs:gpu32" };
    Err(BackendError(format!(
        "`{name}` requires building Noev with the `ketrs-gpu` Cargo feature (`cargo build --features ketrs-gpu`)"
    )))
}

/// The ketrs circuit for `instructions`, on `circuit`'s qubits and classical bits.
fn convert(
    circuit: &Circuit,
    instructions: &[Instruction],
) -> Result<ketrs::Circuit, BackendError> {
    let mut converted = ketrs::Circuit::with_clbits(circuit.qubits, circuit.clbits);
    for instruction in instructions {
        match instruction {
            Instruction::Gate(op) => {
                push_op(&mut converted, op)?;
            }
            &Instruction::Measure { qubit, clbit } => {
                converted.measure(qubit, clbit);
            }
            &Instruction::Reset(qubit) => {
                converted.reset(qubit);
            }
            Instruction::If {
                condition,
                then,
                otherwise,
            } => {
                let instruction = ketrs::Instruction::IfElse {
                    condition: expr(condition),
                    then: Box::new(convert(circuit, then)?),
                    otherwise: Box::new(convert(circuit, otherwise)?),
                };
                converted.push_instruction(instruction).map_err(sim_error)?;
            }
            Instruction::While { condition, body } => {
                let instruction = ketrs::Instruction::While {
                    condition: expr(condition),
                    body: Box::new(convert(circuit, body)?),
                };
                converted.push_instruction(instruction).map_err(sim_error)?;
            }
            Instruction::Assign { clbit, value } => {
                let instruction = ketrs::Instruction::Assign {
                    clbit: *clbit,
                    value: expr(value),
                };
                converted.push_instruction(instruction).map_err(sim_error)?;
            }
            Instruction::Qasm { code, .. } => {
                let parsed = parse_inline_qasm(circuit.qubits, code)?;
                if parsed.is_unitary() {
                    converted.extend(&parsed).map_err(sim_error)?;
                } else {
                    let ops = parsed.ops();
                    let mut op_idx = 0usize;
                    for (pos, inst) in parsed.instructions() {
                        while op_idx < *pos && op_idx < ops.len() {
                            converted.push(ops[op_idx].clone()).map_err(sim_error)?;
                            op_idx += 1;
                        }
                        match inst {
                            ketrs::Instruction::Reset { qubit } => {
                                converted.reset(*qubit);
                            }
                            other => {
                                return Err(BackendError(format!(
                                    "inline OpenQASM instruction `{other:?}` cannot be simulated in `qasm(\"…\")`; \
                                     use Noev's `measure` and classical control instead"
                                )));
                            }
                        }
                    }
                    while op_idx < ops.len() {
                        converted.push(ops[op_idx].clone()).map_err(sim_error)?;
                        op_idx += 1;
                    }
                }
            }
        }
    }
    Ok(converted)
}

/// Parses an inline OpenQASM 3 snippet against a register `qubit[n_qubits] q;`.
fn parse_inline_qasm(n_qubits: usize, code: &str) -> Result<ketrs::Circuit, BackendError> {
    let mut full = String::from("OPENQASM 3.0;\ninclude \"stdgates.inc\";\n");
    if n_qubits > 0 {
        use std::fmt::Write as _;
        let _ = writeln!(full, "qubit[{n_qubits}] q;");
    }
    full.push_str(code);
    full.push('\n');
    ketrs::qasm::parse(&full)
        .map_err(|e| BackendError(format!("cannot simulate inline OpenQASM on ketrs: {e}")))
}

/// A condition as a ketrs classical expression, which is 0 or 1.
fn expr(condition: &Condition) -> ketrs::Expr {
    let binary = |op, a: &Condition, b: &Condition| ketrs::Expr::Binary {
        op,
        left: Box::new(expr(a)),
        right: Box::new(expr(b)),
    };
    match condition {
        Condition::Const(b) => ketrs::Expr::Value(u64::from(*b)),
        Condition::Clbit(k) => ketrs::Expr::Bit(*k),
        Condition::Not(c) => ketrs::Expr::Not(Box::new(expr(c))),
        Condition::And(a, b) => binary(ketrs::BinaryOp::And, a, b),
        Condition::Or(a, b) => binary(ketrs::BinaryOp::Or, a, b),
        Condition::Xor(a, b) => binary(ketrs::BinaryOp::NotEqual, a, b),
    }
}

/// Returns whether every gate and global phase in `circuit` (including nested control flow)
/// has real amplitudes and can run on [`RealStatevector`] / [`RealStatevector32`].
fn circuit_is_real(circuit: &ketrs::Circuit) -> bool {
    if circuit.global_phase().sin().abs() > REAL_TOL {
        return false;
    }
    if !circuit.ops().iter().all(operation_is_real) {
        return false;
    }
    for (_, inst) in circuit.instructions() {
        match inst {
            ketrs::Instruction::IfElse {
                then, otherwise, ..
            } => {
                if !circuit_is_real(then) || !circuit_is_real(otherwise) {
                    return false;
                }
            }
            ketrs::Instruction::While { body, .. } => {
                if !circuit_is_real(body) {
                    return false;
                }
            }
            ketrs::Instruction::For { passes } => {
                if !passes.iter().all(circuit_is_real) {
                    return false;
                }
            }
            ketrs::Instruction::Measure { .. }
            | ketrs::Instruction::Reset { .. }
            | ketrs::Instruction::Assign { .. }
            | ketrs::Instruction::Break
            | ketrs::Instruction::Continue => {}
        }
    }
    true
}

/// Returns whether `op` maps real statevectors to real statevectors.
fn operation_is_real(op: &Operation) -> bool {
    match op {
        Operation::Gate { gate, .. }
        | Operation::Controlled { gate, .. }
        | Operation::MultiControlled { gate, .. } => gate_is_real(gate),
        Operation::Swap(_, _) | Operation::ControlledSwap { .. } => true,
        Operation::Unitary2 { matrix, .. } => {
            matrix.iter().flatten().all(|c| c.im.abs() <= REAL_TOL)
        }
        Operation::PauliRotation { pauli, theta } => {
            if (theta / 2.0).sin().abs() <= REAL_TOL {
                return true;
            }
            let y_count = pauli
                .ops()
                .iter()
                .filter(|(_, p)| *p == ketrs::Pauli::Y)
                .count();
            y_count % 2 == 1
        }
    }
}

fn gate_is_real(gate: &ketrs::Gate) -> bool {
    use ketrs::Gate as K;
    match *gate {
        K::H | K::X | K::Z | K::Ry(_) => true,
        K::Y | K::S | K::Sdg | K::T | K::Tdg => false,
        K::Rx(t) | K::Rz(t) => (t / 2.0).sin().abs() <= REAL_TOL,
        K::Phase(l) => l.sin().abs() <= REAL_TOL,
        K::U { .. } | K::Unitary(_) => gate
            .matrix()
            .iter()
            .flatten()
            .all(|c| c.im.abs() <= REAL_TOL),
    }
}

/// Adaptive dense statevector representation (FP64 or FP32): starts real and promotes to
/// complex on the first non-real operation.
enum KetrsState {
    Real64(RealStatevector),
    Complex64(Statevector),
    Real32(RealStatevector32),
    Complex32(Statevector32),
}

impl KetrsState {
    fn n_qubits(&self) -> usize {
        match self {
            KetrsState::Real64(s) => s.n_qubits(),
            KetrsState::Complex64(s) => s.n_qubits(),
            KetrsState::Real32(s) => s.n_qubits(),
            KetrsState::Complex32(s) => s.n_qubits(),
        }
    }

    fn max_qubits(&self) -> usize {
        match self {
            KetrsState::Real64(_) | KetrsState::Complex64(_) => MAX_QUBITS_SV64,
            KetrsState::Real32(_) | KetrsState::Complex32(_) => MAX_QUBITS_SV32,
        }
    }

    fn release_tol(&self) -> f64 {
        match self {
            KetrsState::Real64(_) | KetrsState::Complex64(_) => 1e-9,
            KetrsState::Real32(_) | KetrsState::Complex32(_) => 1e-5,
        }
    }

    fn add_qubit(&mut self) {
        match self {
            KetrsState::Real64(s) => s.add_qubit(),
            KetrsState::Complex64(s) => s.add_qubit(),
            KetrsState::Real32(s) => s.add_qubit(),
            KetrsState::Complex32(s) => s.add_qubit(),
        }
    }

    fn promote_to_complex(&mut self) {
        match self {
            KetrsState::Real64(real) => {
                let amps = std::mem::replace(real, RealStatevector::zero(0))
                    .into_amplitudes()
                    .into_iter()
                    .map(|r| Complex64::new(r, 0.0))
                    .collect();
                *self = KetrsState::Complex64(
                    Statevector::from_amplitudes(amps)
                        .expect("real statevector length is a power of two"),
                );
            }
            KetrsState::Real32(real) => {
                let amps = std::mem::replace(real, RealStatevector32::zero(0))
                    .into_amplitudes()
                    .into_iter()
                    .map(|r| Complex32::new(r, 0.0))
                    .collect();
                *self = KetrsState::Complex32(
                    Statevector32::from_amplitudes(amps)
                        .expect("real statevector length is a power of two"),
                );
            }
            KetrsState::Complex64(_) | KetrsState::Complex32(_) => {}
        }
    }

    fn apply_operation(&mut self, op: &Operation) -> Result<(), BackendError> {
        match self {
            KetrsState::Real64(s) => match s.apply(op) {
                Ok(()) => Ok(()),
                Err(ketrs::Error::NotReal(_)) => {
                    self.promote_to_complex();
                    self.apply_operation(op)
                }
                Err(e) => Err(sim_error(e)),
            },
            KetrsState::Complex64(s) => s.apply(op).map_err(sim_error),
            KetrsState::Real32(s) => match s.apply(op) {
                Ok(()) => Ok(()),
                Err(ketrs::Error::NotReal(_)) => {
                    self.promote_to_complex();
                    self.apply_operation(op)
                }
                Err(e) => Err(sim_error(e)),
            },
            KetrsState::Complex32(s) => s.apply(op).map_err(sim_error),
        }
    }

    fn run_unitary_circuit(&mut self, circuit: &ketrs::Circuit) -> Result<(), BackendError> {
        match self {
            KetrsState::Real64(s) => match s.run(circuit) {
                Ok(()) => Ok(()),
                Err(ketrs::Error::NotReal(_)) => {
                    self.promote_to_complex();
                    self.run_unitary_circuit(circuit)
                }
                Err(e) => Err(sim_error(e)),
            },
            KetrsState::Complex64(s) => s.run(circuit).map_err(sim_error),
            KetrsState::Real32(s) => match s.run(circuit) {
                Ok(()) => Ok(()),
                Err(ketrs::Error::NotReal(_)) => {
                    self.promote_to_complex();
                    self.run_unitary_circuit(circuit)
                }
                Err(e) => Err(sim_error(e)),
            },
            KetrsState::Complex32(s) => s.run(circuit).map_err(sim_error),
        }
    }
}

/// One run on a statevector.
struct KetrsSession {
    state: KetrsState,
    rng: SmallRng,
}

impl Session for KetrsSession {
    fn add_qubit(&mut self) -> Result<usize, BackendError> {
        let qubit = self.state.n_qubits();
        let max_qubits = self.state.max_qubits();
        if qubit >= max_qubits {
            return Err(BackendError(format!(
                "more than {max_qubits} qubits are alive at once, which is beyond the statevector simulator"
            )));
        }
        // The new qubit is the most significant, so existing qubits keep their indices.
        self.state.add_qubit();
        Ok(qubit)
    }

    fn apply(&mut self, op: &Op) -> Result<(), BackendError> {
        if op.gate == Gate::Unitary && op.targets.len() == 2 && !op.controls.is_empty() {
            let controlled = controlled_unitary2_circuit(self.state.n_qubits(), op)?;
            return self.state.run_unitary_circuit(&controlled);
        }
        let ketrs_op = operation(op)?;
        self.state.apply_operation(&ketrs_op)
    }

    fn measure(&mut self, qubit: usize) -> Result<bool, BackendError> {
        match &mut self.state {
            KetrsState::Real64(s) => s.measure(qubit, &mut self.rng).map_err(sim_error),
            KetrsState::Complex64(s) => s.measure(qubit, &mut self.rng).map_err(sim_error),
            KetrsState::Real32(s) => s.measure(qubit, &mut self.rng).map_err(sim_error),
            KetrsState::Complex32(s) => s.measure(qubit, &mut self.rng).map_err(sim_error),
        }
    }

    fn reset(&mut self, qubit: usize) -> Result<(), BackendError> {
        match &mut self.state {
            KetrsState::Real64(s) => s.reset(qubit, &mut self.rng).map_err(sim_error),
            KetrsState::Complex64(s) => s.reset(qubit, &mut self.rng).map_err(sim_error),
            KetrsState::Real32(s) => s.reset(qubit, &mut self.rng).map_err(sim_error),
            KetrsState::Complex32(s) => s.reset(qubit, &mut self.rng).map_err(sim_error),
        }
    }

    fn release(&mut self, qubit: usize) -> Result<(), BackendError> {
        let p1 = match &self.state {
            KetrsState::Real64(s) => s.probability_one(qubit).map_err(sim_error)?,
            KetrsState::Complex64(s) => s.probability_one(qubit).map_err(sim_error)?,
            KetrsState::Real32(s) => s.probability_one(qubit).map_err(sim_error)?,
            KetrsState::Complex32(s) => s.probability_one(qubit).map_err(sim_error)?,
        };
        if p1 > self.state.release_tol() {
            return Err(BackendError(format!(
                "a released qubit was not in |0⟩ (probability of 1: {p1:.3})"
            )));
        }
        Ok(())
    }

    fn qasm(&mut self, code: &str, _qubits: &[usize]) -> Result<(), BackendError> {
        let parsed = parse_inline_qasm(self.state.n_qubits(), code)?;
        if parsed.is_unitary() {
            return self.state.run_unitary_circuit(&parsed);
        }
        let ops = parsed.ops();
        let mut op_idx = 0usize;
        for (pos, inst) in parsed.instructions() {
            while op_idx < *pos && op_idx < ops.len() {
                self.state.apply_operation(&ops[op_idx])?;
                op_idx += 1;
            }
            match inst {
                ketrs::Instruction::Reset { qubit } => {
                    self.reset(*qubit)?;
                }
                other => {
                    return Err(BackendError(format!(
                        "inline OpenQASM instruction `{other:?}` cannot be simulated in `qasm(\"…\")`; \
                         use Noev's `measure` and classical control instead"
                    )));
                }
            }
        }
        while op_idx < ops.len() {
            self.state.apply_operation(&ops[op_idx])?;
            op_idx += 1;
        }
        Ok(())
    }

    fn expectation(
        &self,
        terms: &[(f64, Vec<(usize, Pauli)>)],
    ) -> Option<Result<f64, BackendError>> {
        let mut sum = PauliSum::new();
        for (coeff, product) in terms {
            let ops = product.iter().map(|&(q, p)| {
                let pauli = match p {
                    Pauli::X => ketrs::Pauli::X,
                    Pauli::Y => ketrs::Pauli::Y,
                    Pauli::Z => ketrs::Pauli::Z,
                };
                (q, pauli)
            });
            match PauliString::new(ops) {
                Ok(string) => {
                    sum.add(*coeff, string);
                }
                Err(e) => return Some(Err(sim_error(e))),
            }
        }
        let res = match &self.state {
            KetrsState::Real64(s) => s.expectation(&sum),
            KetrsState::Complex64(s) => s.expectation(&sum),
            KetrsState::Real32(s) => s.expectation(&sum),
            KetrsState::Complex32(s) => s.expectation(&sum),
        };
        Some(res.map_err(sim_error))
    }

    fn amplitudes(&self, qubits: &[usize]) -> Option<Result<Vec<(f64, f64)>, BackendError>> {
        let res = match &self.state {
            KetrsState::Real64(s) => register_state(s.amplitudes(), qubits, 1e-9),
            KetrsState::Complex64(s) => register_state(s.amplitudes(), qubits, 1e-9),
            KetrsState::Real32(s) => register_state(s.amplitudes(), qubits, 1e-5),
            KetrsState::Complex32(s) => register_state(s.amplitudes(), qubits, 1e-5),
        };
        Some(res)
    }

    fn zero_amplitude(&self) -> Option<Result<(f64, f64), BackendError>> {
        // With no qubits the state is the number 1.
        let zero = match &self.state {
            KetrsState::Real64(s) => s.amplitudes().first().map_or((1.0, 0.0), |&r| (r, 0.0)),
            KetrsState::Complex64(s) => s.amplitudes().first().map_or((1.0, 0.0), |a| (a.re, a.im)),
            KetrsState::Real32(s) => s
                .amplitudes()
                .first()
                .map_or((1.0, 0.0), |&r| (f64::from(r), 0.0)),
            KetrsState::Complex32(s) => s
                .amplitudes()
                .first()
                .map_or((1.0, 0.0), |a| (f64::from(a.re), f64::from(a.im))),
        };
        Some(Ok(zero))
    }

    fn sample(&mut self, qubits: &[usize], shots: usize) -> Option<Counts> {
        let raw = match &mut self.state {
            KetrsState::Real64(s) => s.sample_counts(shots, &mut self.rng),
            KetrsState::Complex64(s) => s.sample_counts(shots, &mut self.rng),
            KetrsState::Real32(s) => s.sample_counts(shots, &mut self.rng),
            KetrsState::Complex32(s) => s.sample_counts(shots, &mut self.rng),
        };
        let mut counts: BTreeMap<u64, usize> = BTreeMap::new();
        for (index, count) in raw {
            let bits = qubits
                .iter()
                .enumerate()
                .fold(0u64, |acc, (k, &q)| acc | (((index >> q) & 1) as u64) << k);
            *counts.entry(bits).or_default() += count;
        }
        Some(counts.into_iter().collect())
    }
}

trait ToC64: Copy {
    fn to_c64(self) -> Complex64;
}

impl ToC64 for f64 {
    fn to_c64(self) -> Complex64 {
        Complex64::new(self, 0.0)
    }
}

impl ToC64 for Complex64 {
    fn to_c64(self) -> Complex64 {
        self
    }
}

impl ToC64 for f32 {
    fn to_c64(self) -> Complex64 {
        Complex64::new(f64::from(self), 0.0)
    }
}

impl ToC64 for Complex32 {
    fn to_c64(self) -> Complex64 {
        Complex64::new(f64::from(self.re), f64::from(self.im))
    }
}

/// The state of the register `qubits` within the whole state `full` (qubit `q` at bit `q` of
/// an index), with `qubits[0]` the most significant bit of the result's index. The register
/// has a state of its own only if `full` is that state times one of the rest: the column of
/// the rest's largest weight gives it, and every other column must be a multiple of it.
fn register_state<T: ToC64>(
    full: &[T],
    qubits: &[usize],
    entanglement_tol: f64,
) -> Result<Vec<(f64, f64)>, BackendError> {
    let mask: usize = qubits.iter().map(|&q| 1 << q).sum();
    // The register's index for a full index, `qubits[0]` most significant.
    let local = |index: usize| {
        qubits
            .iter()
            .fold(0usize, |acc, &q| (acc << 1) | ((index >> q) & 1))
    };
    let mut weights: BTreeMap<usize, f64> = BTreeMap::new();
    for (index, &a) in full.iter().enumerate() {
        *weights.entry(index & !mask).or_default() += a.to_c64().norm_sqr();
    }
    let (&rest, &weight) = weights
        .iter()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .ok_or_else(|| BackendError("the state is empty".into()))?;
    let norm = weight.sqrt();
    let mut state = vec![Complex64::new(0.0, 0.0); 1 << qubits.len()];
    for (index, &a) in full.iter().enumerate() {
        if index & !mask == rest {
            state[local(index)] = a.to_c64() / norm;
        }
    }
    // Each column of the rest, projected onto the register's state, must account for all of
    // its weight.
    let mut overlaps: BTreeMap<usize, Complex64> = BTreeMap::new();
    for (index, &a) in full.iter().enumerate() {
        *overlaps.entry(index & !mask).or_default() += state[local(index)].conj() * a.to_c64();
    }
    let missing: f64 = weights
        .iter()
        .map(|(column, w)| w - overlaps[column].norm_sqr())
        .sum();
    if missing > entanglement_tol {
        return Err(BackendError(
            "the register is entangled with other qubits, so it has no amplitudes of its own"
                .into(),
        ));
    }
    // A register's overall phase is not its own once other qubits exist, so fix one: the
    // first amplitude that is not zero is real and positive.
    let phase = state
        .iter()
        .find(|a| a.norm() > 1e-12)
        .map_or(Complex64::new(1.0, 0.0), |a| a.conj() / a.norm());
    Ok(state
        .iter()
        .map(|a| a * phase)
        .map(|a| (a.re, a.im))
        .collect())
}

/// Appends `op` (including controlled two-qubit unitaries via [`ketrs::Circuit::control`]) to
/// `converted`.
fn push_op(converted: &mut ketrs::Circuit, op: &Op) -> Result<(), BackendError> {
    if op.gate == Gate::Unitary && op.targets.len() == 2 && !op.controls.is_empty() {
        let controlled = controlled_unitary2_circuit(converted.n_qubits(), op)?;
        converted.extend(&controlled).map_err(sim_error)?;
        return Ok(());
    }
    converted.push(operation(op)?).map_err(sim_error)?;
    Ok(())
}

/// Builds a controlled two-qubit unitary circuit using [`ketrs::Circuit::control`].
fn controlled_unitary2_circuit(n_qubits: usize, op: &Op) -> Result<ketrs::Circuit, BackendError> {
    let m = crate::unitary::rows(&op.params, op.adjoint);
    let matrix =
        std::array::from_fn(|i| std::array::from_fn(|j| Complex64::new(m[i][j].0, m[i][j].1)));
    let mut base = ketrs::Circuit::new(n_qubits);
    base.push(Operation::Unitary2 {
        matrix,
        qubits: [op.targets[1], op.targets[0]],
    })
    .map_err(sim_error)?;
    base.control(&op.controls).map_err(sim_error)
}

/// The ketrs operation for a gate with its controls.
fn operation(op: &Op) -> Result<Operation, BackendError> {
    // A two-qubit matrix: ketrs counts its qubits from the least significant, so the first
    // (the left label, most significant) goes second.
    if op.gate == Gate::Unitary && op.targets.len() == 2 {
        let m = crate::unitary::rows(&op.params, op.adjoint);
        let matrix =
            std::array::from_fn(|i| std::array::from_fn(|j| Complex64::new(m[i][j].0, m[i][j].1)));
        return Ok(Operation::Unitary2 {
            matrix,
            qubits: [op.targets[1], op.targets[0]],
        });
    }
    let gate = ketrs_gate(op.gate, op.adjoint, &op.params);
    Ok(
        match (op.controls.as_slice(), gate, op.targets.as_slice()) {
            ([], None, &[a, b]) => Operation::Swap(a, b),
            ([], Some(gate), &[target]) => Operation::Gate { gate, target },
            (&[control], Some(gate), &[target]) => Operation::Controlled {
                gate,
                control,
                target,
            },
            (controls, Some(gate), &[target]) => Operation::MultiControlled {
                gate,
                controls: controls.to_vec(),
                target,
            },
            (controls, None, &[a, b]) => Operation::ControlledSwap {
                controls: controls.to_vec(),
                a,
                b,
            },
            _ => {
                return Err(BackendError(
                    "a gate with the wrong number of qubits".into(),
                ));
            }
        },
    )
}

/// The ketrs gate for a Noev gate, inverted for `adj`; `None` for `swap`.
fn ketrs_gate(gate: Gate, adjoint: bool, params: &[f64]) -> Option<ketrs::Gate> {
    use ketrs::Gate as K;
    let sign = if adjoint { -1.0 } else { 1.0 };
    let param = |i: usize| params.get(i).copied().unwrap_or(0.0);
    Some(match gate {
        Gate::X => K::X,
        Gate::Y => K::Y,
        Gate::Z => K::Z,
        Gate::H => K::H,
        Gate::S if adjoint => K::Sdg,
        Gate::S => K::S,
        Gate::T if adjoint => K::Tdg,
        Gate::T => K::T,
        Gate::Rx => K::Rx(sign * param(0)),
        Gate::Ry => K::Ry(sign * param(0)),
        Gate::Rz => K::Rz(sign * param(0)),
        Gate::P => K::Phase(sign * param(0)),
        // U(θ, φ, λ)† = U(−θ, −λ, −φ).
        Gate::U if adjoint => K::U {
            theta: -param(0),
            phi: -param(2),
            lambda: -param(1),
        },
        Gate::U => K::U {
            theta: param(0),
            phi: param(1),
            lambda: param(2),
        },
        Gate::Swap => return None,
        Gate::Unitary => {
            let m = crate::unitary::rows(params, adjoint);
            let c = |(re, im): (f64, f64)| Complex64::new(re, im);
            K::Unitary([[c(m[0][0]), c(m[0][1])], [c(m[1][0]), c(m[1][1])]])
        }
    })
}

fn sim_error(error: ketrs::Error) -> BackendError {
    BackendError(format!("simulator error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bell_circuit() -> Circuit {
        Circuit {
            qubits: 2,
            clbits: 2,
            instructions: vec![
                Instruction::Gate(Op {
                    gate: Gate::H,
                    adjoint: false,
                    params: vec![],
                    controls: vec![],
                    targets: vec![0],
                }),
                Instruction::Gate(Op {
                    gate: Gate::X,
                    adjoint: false,
                    params: vec![],
                    controls: vec![0],
                    targets: vec![1],
                }),
                Instruction::Measure { qubit: 0, clbit: 0 },
                Instruction::Measure { qubit: 1, clbit: 1 },
            ],
        }
    }

    #[test]
    fn real_statevector_promotes_on_complex_gate() {
        for engine in [KetrsEngine::Statevector, KetrsEngine::Statevector32] {
            let mut session = Ketrs::with_engine(engine, 2).session(1).expect("session");
            let q0 = session.add_qubit().unwrap();
            let q1 = session.add_qubit().unwrap();

            session
                .apply(&Op {
                    gate: Gate::H,
                    adjoint: false,
                    params: vec![],
                    controls: vec![],
                    targets: vec![q0],
                })
                .unwrap();
            session
                .apply(&Op {
                    gate: Gate::X,
                    adjoint: false,
                    params: vec![],
                    controls: vec![q0],
                    targets: vec![q1],
                })
                .unwrap();
            let amps = session.amplitudes(&[q0, q1]).unwrap().unwrap();
            assert!((amps[0].0 - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
            assert!((amps[3].0 - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);

            session
                .apply(&Op {
                    gate: Gate::S,
                    adjoint: false,
                    params: vec![],
                    controls: vec![],
                    targets: vec![q1],
                })
                .unwrap();
            let amps = session.amplitudes(&[q0, q1]).unwrap().unwrap();
            assert!((amps[0].0 - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
            assert!(amps[3].0.abs() < 1e-6);
            assert!((amps[3].1 - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        }
    }

    #[test]
    fn all_cpu_engines_sample_bell_circuit() {
        let circuit = bell_circuit();
        for spec in [
            "ketrs",
            "ketrs:sv",
            "ketrs:sv32",
            "ketrs:mps",
            "ketrs:pmps",
            "ketrs:density",
        ] {
            let backend = KetrsBackend::from_spec(spec, 2).expect(spec);
            let counts = backend.run_circuit(&circuit, 200, 7).unwrap().unwrap();
            let total: usize = counts.iter().map(|(_, c)| c).sum();
            assert_eq!(total, 200, "engine {spec}");
            for (bits, count) in counts {
                assert!(
                    bits == 0 || bits == 3,
                    "engine {spec}: unexpected outcome {bits} ({count})"
                );
            }
        }
    }

    #[test]
    fn mps_handles_more_than_30_qubits() {
        // 32-qubit GHZ state: beyond the 30-qubit dense statevector cap, trivial for MPS.
        let n = 32usize;
        let mut instructions = vec![Instruction::Gate(Op {
            gate: Gate::H,
            adjoint: false,
            params: vec![],
            controls: vec![],
            targets: vec![0],
        })];
        for q in 0..(n - 1) {
            instructions.push(Instruction::Gate(Op {
                gate: Gate::X,
                adjoint: false,
                params: vec![],
                controls: vec![q],
                targets: vec![q + 1],
            }));
        }
        for q in 0..n {
            instructions.push(Instruction::Measure { qubit: q, clbit: q });
        }
        let circuit = Circuit {
            qubits: n,
            clbits: n,
            instructions,
        };
        let backend = Ketrs::with_engine(KetrsEngine::Mps, 2);
        let counts = backend.run_circuit(&circuit, 50, 7).unwrap().unwrap();
        let all_ones = (1u64 << n) - 1;
        let total: usize = counts.iter().map(|(_, c)| c).sum();
        assert_eq!(total, 50);
        for (bits, _) in counts {
            assert!(
                bits == 0 || bits == all_ones,
                "unexpected GHZ outcome {bits:#x}"
            );
        }
    }

    #[test]
    fn circuit_optimization_levels_agree() {
        let mut instructions = Vec::new();
        instructions.push(Instruction::Gate(Op {
            gate: Gate::H,
            adjoint: false,
            params: vec![],
            controls: vec![],
            targets: vec![0],
        }));
        for _ in 0..9 {
            instructions.push(Instruction::Gate(Op {
                gate: Gate::X,
                adjoint: false,
                params: vec![],
                controls: vec![0],
                targets: vec![1],
            }));
        }
        instructions.push(Instruction::Measure { qubit: 0, clbit: 0 });
        instructions.push(Instruction::Measure { qubit: 1, clbit: 1 });
        let circuit = Circuit {
            qubits: 2,
            clbits: 2,
            instructions,
        };
        for level in 0u8..=3 {
            let counts = Ketrs::with_opt_level(level)
                .run_circuit(&circuit, 200, 7)
                .unwrap()
                .unwrap();
            let total: usize = counts.iter().map(|(_, c)| c).sum();
            assert_eq!(total, 200);
            for (bits, count) in counts {
                assert!(
                    bits == 0 || bits == 3,
                    "level {level}: unexpected outcome {bits} ({count})"
                );
            }
        }
    }

    #[test]
    fn controlled_two_qubit_unitary_executes() {
        let swap_params: Vec<f64> = [
            (1.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (1.0, 0.0),
        ]
        .into_iter()
        .flat_map(|(r, i)| [r, i])
        .collect();

        let circuit = Circuit {
            qubits: 3,
            clbits: 3,
            instructions: vec![
                Instruction::Gate(Op {
                    gate: Gate::X,
                    adjoint: false,
                    params: vec![],
                    controls: vec![],
                    targets: vec![0],
                }),
                Instruction::Gate(Op {
                    gate: Gate::X,
                    adjoint: false,
                    params: vec![],
                    controls: vec![],
                    targets: vec![1],
                }),
                Instruction::Gate(Op {
                    gate: Gate::Unitary,
                    adjoint: false,
                    params: swap_params,
                    controls: vec![0],
                    targets: vec![1, 2],
                }),
                Instruction::Measure { qubit: 0, clbit: 0 },
                Instruction::Measure { qubit: 1, clbit: 1 },
                Instruction::Measure { qubit: 2, clbit: 2 },
            ],
        };
        let counts = Ketrs.run_circuit(&circuit, 10, 1).unwrap().unwrap();
        assert_eq!(counts, vec![(5, 10)]);
    }
}
