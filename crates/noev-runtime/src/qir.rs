//! Writing a [`Circuit`] as QIR 1.0 (LLVM IR text, `.ll`) (`spec/runtime.md` §4, §7).
//!
//! Circuits without classical control or mid-circuit operations emit the **QIR Base Profile**
//! (`"qir_profiles"="base_profile"`). Circuits with `if`, `while`, `reset`, classical bit
//! assignments, or operations after measurements emit the **QIR Adaptive Profile**
//! (`"qir_profiles"="adaptive_profile"`).
//!
//! All gates (including `U`, `P`, matrix unitaries, and arbitrary multi-controlled gates) are
//! decomposed into standard QIR 1.0 QIS intrinsics (`x`, `y`, `z`, `h`, `s`, `s__adj`, `t`,
//! `t__adj`, `rx`, `ry`, `rz`, `cnot`, `cz`, `swap`, `mz`, `reset`).

use std::collections::BTreeSet;
use std::f64::consts::FRAC_PI_4;
use std::fmt::Write;

use noev_sema::prelude::Gate;

use crate::backend::{Circuit, Condition, Instruction, Op};

/// Which QIR 1.0 profile a circuit requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QirProfile {
    /// Straight-line gates followed by terminal measurements (`"base_profile"`).
    Base,
    /// Mid-circuit measurement, reset, or classical control (`"adaptive_profile"`).
    Adaptive,
}

impl QirProfile {
    /// The QIR 1.0 attribute value for `"qir_profiles"`.
    pub fn as_str(self) -> &'static str {
        match self {
            QirProfile::Base => "base_profile",
            QirProfile::Adaptive => "adaptive_profile",
        }
    }
}

/// Capabilities used by a circuit, driving profile selection and module flags.
#[derive(Clone, Copy, Debug, Default)]
struct Capabilities {
    adaptive: bool,
    has_reset: bool,
    has_classical: bool,
    has_backwards_branch: bool,
}

impl Capabilities {
    fn analyze(circuit: &Circuit) -> Self {
        let mut caps = Self::default();
        let mut measured = false;
        let mut measured_qubits = BTreeSet::new();
        scan_caps(
            &circuit.instructions,
            &mut caps,
            &mut measured,
            &mut measured_qubits,
        );
        caps
    }

    fn merge(&mut self, other: Self) {
        self.adaptive |= other.adaptive;
        self.has_reset |= other.has_reset;
        self.has_classical |= other.has_classical;
        self.has_backwards_branch |= other.has_backwards_branch;
    }

    fn profile(self) -> QirProfile {
        if self.adaptive {
            QirProfile::Adaptive
        } else {
            QirProfile::Base
        }
    }
}

fn scan_caps(
    list: &[Instruction],
    caps: &mut Capabilities,
    measured: &mut bool,
    measured_qubits: &mut BTreeSet<usize>,
) {
    for inst in list {
        match inst {
            Instruction::Gate(_) => {
                if *measured {
                    caps.adaptive = true;
                }
            }
            Instruction::Measure { qubit, .. } => {
                *measured = true;
                if !measured_qubits.insert(*qubit) {
                    caps.adaptive = true;
                }
            }
            Instruction::Reset(_) => {
                caps.adaptive = true;
                caps.has_reset = true;
            }
            Instruction::If {
                then, otherwise, ..
            } => {
                caps.adaptive = true;
                caps.has_classical = true;
                scan_caps(then, caps, measured, measured_qubits);
                scan_caps(otherwise, caps, measured, measured_qubits);
            }
            Instruction::While { body, .. } => {
                caps.adaptive = true;
                caps.has_classical = true;
                caps.has_backwards_branch = true;
                scan_caps(body, caps, measured, measured_qubits);
            }
            Instruction::Assign { .. } => {
                caps.adaptive = true;
                caps.has_classical = true;
            }
        }
    }
}

/// Returns whether `circuit` fits QIR's Base Profile or needs the Adaptive Profile.
pub fn profile(circuit: &Circuit) -> QirProfile {
    Capabilities::analyze(circuit).profile()
}

/// Emits a standalone QIR 1.0 LLVM IR module for a single `circuit` with entry point
/// `@<entry_name>`.
pub fn to_qir(circuit: &Circuit, entry_name: &str, header: &[String]) -> String {
    let mut emitter = ModuleEmitter::new();
    for line in header {
        let _ = writeln!(emitter.header, "; {line}");
    }
    emitter.add_entry(circuit, entry_name, None);
    emitter.finish()
}

/// Emits one QIR 1.0 LLVM IR module containing every exported circuit as an entry point
/// (`@main` when there is a single circuit, or `@circuit_1`, `@circuit_2`, … otherwise).
pub fn to_qir_module(circuits: &[(Circuit, usize)], header: &[String]) -> String {
    let mut emitter = ModuleEmitter::new();
    for line in header {
        let _ = writeln!(emitter.header, "; {line}");
    }
    let single = circuits.len() == 1;
    for (i, (circuit, shots)) in circuits.iter().enumerate() {
        let name = if single {
            "main".to_string()
        } else {
            format!("circuit_{}", i + 1)
        };
        emitter.add_entry(circuit, &name, Some(*shots));
    }
    emitter.finish()
}

/// Intrinsics that may be declared at the bottom of a QIR module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Intrinsic {
    X,
    Y,
    Z,
    H,
    S,
    SAdj,
    T,
    TAdj,
    Rx,
    Ry,
    Rz,
    Cnot,
    Cz,
    Swap,
    Mz,
    Reset,
    ReadResult,
    ArrayRecordOutput,
    ResultRecordOutput,
    BoolRecordOutput,
}

impl Intrinsic {
    fn declaration(self) -> &'static str {
        match self {
            Intrinsic::X => "declare void @__quantum__qis__x__body(%Qubit*)",
            Intrinsic::Y => "declare void @__quantum__qis__y__body(%Qubit*)",
            Intrinsic::Z => "declare void @__quantum__qis__z__body(%Qubit*)",
            Intrinsic::H => "declare void @__quantum__qis__h__body(%Qubit*)",
            Intrinsic::S => "declare void @__quantum__qis__s__body(%Qubit*)",
            Intrinsic::SAdj => "declare void @__quantum__qis__s__adj(%Qubit*)",
            Intrinsic::T => "declare void @__quantum__qis__t__body(%Qubit*)",
            Intrinsic::TAdj => "declare void @__quantum__qis__t__adj(%Qubit*)",
            Intrinsic::Rx => "declare void @__quantum__qis__rx__body(double, %Qubit*)",
            Intrinsic::Ry => "declare void @__quantum__qis__ry__body(double, %Qubit*)",
            Intrinsic::Rz => "declare void @__quantum__qis__rz__body(double, %Qubit*)",
            Intrinsic::Cnot => "declare void @__quantum__qis__cnot__body(%Qubit*, %Qubit*)",
            Intrinsic::Cz => "declare void @__quantum__qis__cz__body(%Qubit*, %Qubit*)",
            Intrinsic::Swap => "declare void @__quantum__qis__swap__body(%Qubit*, %Qubit*)",
            Intrinsic::Mz => {
                "declare void @__quantum__qis__mz__body(%Qubit*, %Result* writeonly) #irreversible"
            }
            Intrinsic::Reset => "declare void @__quantum__qis__reset__body(%Qubit*)",
            Intrinsic::ReadResult => "declare i1 @__quantum__rt__read_result(%Result*)",
            Intrinsic::ArrayRecordOutput => {
                "declare void @__quantum__rt__array_record_output(i64, i8*)"
            }
            Intrinsic::ResultRecordOutput => {
                "declare void @__quantum__rt__result_record_output(%Result*, i8*)"
            }
            Intrinsic::BoolRecordOutput => {
                "declare void @__quantum__rt__bool_record_output(i1, i8*)"
            }
        }
    }
}

struct EntryAttr {
    profile: QirProfile,
    qubits: usize,
    results: usize,
}

struct ModuleEmitter {
    header: String,
    bodies: String,
    intrinsics: BTreeSet<Intrinsic>,
    attrs: Vec<EntryAttr>,
    caps: Capabilities,
}

impl ModuleEmitter {
    fn new() -> Self {
        Self {
            header: String::new(),
            bodies: String::new(),
            intrinsics: BTreeSet::new(),
            attrs: Vec::new(),
            caps: Capabilities::default(),
        }
    }

    fn add_entry(&mut self, circuit: &Circuit, name: &str, shots: Option<usize>) {
        let caps = Capabilities::analyze(circuit);
        self.caps.merge(caps);
        let attr_idx = self.attrs.len();
        self.attrs.push(EntryAttr {
            profile: caps.profile(),
            qubits: circuit.qubits,
            results: circuit.clbits,
        });

        if !self.bodies.is_empty() {
            self.bodies.push('\n');
        }
        if let Some(shots) = shots {
            let _ = writeln!(self.bodies, "; shots: {shots}");
        }
        let _ = writeln!(self.bodies, "define void @{name}() #{attr_idx} {{");
        self.bodies.push_str("entry:\n");

        let mut fn_emit = FnEmitter {
            out: &mut self.bodies,
            intrinsics: &mut self.intrinsics,
            adaptive: caps.adaptive,
            next_ssa: 0,
            next_label: 0,
            assigned_clbits: BTreeSet::new(),
        };

        if caps.adaptive && circuit.clbits > 0 {
            for k in 0..circuit.clbits {
                let _ = writeln!(fn_emit.out, "  %c{k} = alloca i1, align 1");
                let _ = writeln!(fn_emit.out, "  store i1 false, i1* %c{k}, align 1");
            }
        }

        fn_emit.emit_instructions(&circuit.instructions);

        // QIR 1.0 output recording.
        if circuit.clbits > 0 {
            if circuit.clbits > 1 {
                fn_emit.intrinsics.insert(Intrinsic::ArrayRecordOutput);
                let _ = writeln!(
                    fn_emit.out,
                    "  call void @__quantum__rt__array_record_output(i64 {}, i8* null)",
                    circuit.clbits
                );
            }
            for k in 0..circuit.clbits {
                if fn_emit.assigned_clbits.contains(&k) {
                    let v = fn_emit.fresh_ssa();
                    let _ = writeln!(fn_emit.out, "  {v} = load i1, i1* %c{k}, align 1");
                    fn_emit.intrinsics.insert(Intrinsic::BoolRecordOutput);
                    let _ = writeln!(
                        fn_emit.out,
                        "  call void @__quantum__rt__bool_record_output(i1 {v}, i8* null)"
                    );
                } else {
                    fn_emit.intrinsics.insert(Intrinsic::ResultRecordOutput);
                    let r = result_ptr(k);
                    let _ = writeln!(
                        fn_emit.out,
                        "  call void @__quantum__rt__result_record_output(%Result* {r}, i8* null)"
                    );
                }
            }
        }

        self.bodies.push_str("  ret void\n}\n");
    }

    fn finish(self) -> String {
        let mut out = String::new();
        out.push_str(&self.header);
        if !self.header.is_empty() {
            out.push('\n');
        }
        out.push_str("%Qubit = type opaque\n");
        out.push_str("%Result = type opaque\n\n");
        out.push_str(&self.bodies);

        let has_mz = self.intrinsics.contains(&Intrinsic::Mz);
        let irrev_idx = self.attrs.len();

        if !self.intrinsics.is_empty() {
            out.push('\n');
            for intr in &self.intrinsics {
                let decl = intr.declaration();
                if *intr == Intrinsic::Mz {
                    let _ = writeln!(
                        out,
                        "{}",
                        decl.replace("#irreversible", &format!("#{irrev_idx}"))
                    );
                } else {
                    let _ = writeln!(out, "{decl}");
                }
            }
        }

        out.push('\n');
        for (i, attr) in self.attrs.iter().enumerate() {
            let _ = writeln!(
                out,
                "attributes #{i} = {{ \"entry_point\" \"output_labeling_schema\"=\"schema_id\" \
                 \"qir_profiles\"=\"{}\" \"required_num_qubits\"=\"{}\" \
                 \"required_num_results\"=\"{}\" }}",
                attr.profile.as_str(),
                attr.qubits,
                attr.results
            );
        }
        if has_mz {
            let _ = writeln!(out, "attributes #{irrev_idx} = {{ \"irreversible\" }}");
        }

        // QIR 1.0 module flags.
        let mut flags: Vec<String> = vec![
            "!{i32 1, !\"qir_major_version\", i32 1}".into(),
            "!{i32 7, !\"qir_minor_version\", i32 0}".into(),
            "!{i32 1, !\"dynamic_qubit_management\", i1 false}".into(),
            "!{i32 1, !\"dynamic_result_management\", i1 false}".into(),
        ];
        if self.caps.adaptive {
            flags.push(format!(
                "!{{i32 1, !\"qubit_resetting\", i1 {}}}",
                self.caps.has_reset
            ));
            flags.push(format!(
                "!{{i32 1, !\"classical_ints\", i1 {}}}",
                self.caps.has_classical
            ));
            flags.push(format!(
                "!{{i32 1, !\"int_computations\", i1 {}}}",
                self.caps.has_classical
            ));
            flags.push(format!(
                "!{{i32 1, !\"backwards_branching\", i1 {}}}",
                self.caps.has_backwards_branch
            ));
        }
        let refs: Vec<String> = (0..flags.len()).map(|i| format!("!{i}")).collect();
        let _ = writeln!(out, "\n!llvm.module.flags = !{{{}}}", refs.join(", "));
        for (i, flag) in flags.iter().enumerate() {
            let _ = writeln!(out, "!{i} = {flag}");
        }

        out
    }
}

struct FnEmitter<'a> {
    out: &'a mut String,
    intrinsics: &'a mut BTreeSet<Intrinsic>,
    adaptive: bool,
    next_ssa: usize,
    next_label: usize,
    assigned_clbits: BTreeSet<usize>,
}

impl FnEmitter<'_> {
    fn fresh_ssa(&mut self) -> String {
        let id = self.next_ssa;
        self.next_ssa += 1;
        format!("%t{id}")
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        let id = self.next_label;
        self.next_label += 1;
        format!("{prefix}.{id}")
    }

    fn emit_instructions(&mut self, list: &[Instruction]) {
        for inst in list {
            match inst {
                Instruction::Gate(op) => self.emit_op(op),
                Instruction::Measure { qubit, clbit } => {
                    self.intrinsics.insert(Intrinsic::Mz);
                    let q = qubit_ptr(*qubit);
                    let r = result_ptr(*clbit);
                    let _ = writeln!(
                        self.out,
                        "  call void @__quantum__qis__mz__body(%Qubit* {q}, %Result* {r})"
                    );
                    if self.adaptive {
                        self.intrinsics.insert(Intrinsic::ReadResult);
                        let v = self.fresh_ssa();
                        let _ = writeln!(
                            self.out,
                            "  {v} = call i1 @__quantum__rt__read_result(%Result* {r})"
                        );
                        let _ = writeln!(self.out, "  store i1 {v}, i1* %c{clbit}, align 1");
                    }
                }
                Instruction::Reset(qubit) => {
                    self.intrinsics.insert(Intrinsic::Reset);
                    let q = qubit_ptr(*qubit);
                    let _ = writeln!(
                        self.out,
                        "  call void @__quantum__qis__reset__body(%Qubit* {q})"
                    );
                }
                Instruction::Assign { clbit, value } => {
                    self.assigned_clbits.insert(*clbit);
                    let v = self.emit_cond(value);
                    let _ = writeln!(self.out, "  store i1 {v}, i1* %c{clbit}, align 1");
                }
                Instruction::If {
                    condition,
                    then,
                    otherwise,
                } => {
                    let cond = self.emit_cond(condition);
                    let then_lbl = self.fresh_label("if.then");
                    let end_lbl = self.fresh_label("if.end");
                    if otherwise.is_empty() {
                        let _ = writeln!(
                            self.out,
                            "  br i1 {cond}, label %{then_lbl}, label %{end_lbl}"
                        );
                        let _ = writeln!(self.out, "{then_lbl}:");
                        self.emit_instructions(then);
                        let _ = writeln!(self.out, "  br label %{end_lbl}");
                        let _ = writeln!(self.out, "{end_lbl}:");
                    } else {
                        let else_lbl = self.fresh_label("if.else");
                        let _ = writeln!(
                            self.out,
                            "  br i1 {cond}, label %{then_lbl}, label %{else_lbl}"
                        );
                        let _ = writeln!(self.out, "{then_lbl}:");
                        self.emit_instructions(then);
                        let _ = writeln!(self.out, "  br label %{end_lbl}");
                        let _ = writeln!(self.out, "{else_lbl}:");
                        self.emit_instructions(otherwise);
                        let _ = writeln!(self.out, "  br label %{end_lbl}");
                        let _ = writeln!(self.out, "{end_lbl}:");
                    }
                }
                Instruction::While { condition, body } => {
                    let cond_lbl = self.fresh_label("while.cond");
                    let body_lbl = self.fresh_label("while.body");
                    let end_lbl = self.fresh_label("while.end");
                    let _ = writeln!(self.out, "  br label %{cond_lbl}");
                    let _ = writeln!(self.out, "{cond_lbl}:");
                    let cond = self.emit_cond(condition);
                    let _ = writeln!(
                        self.out,
                        "  br i1 {cond}, label %{body_lbl}, label %{end_lbl}"
                    );
                    let _ = writeln!(self.out, "{body_lbl}:");
                    self.emit_instructions(body);
                    let _ = writeln!(self.out, "  br label %{cond_lbl}");
                    let _ = writeln!(self.out, "{end_lbl}:");
                }
            }
        }
    }

    fn emit_cond(&mut self, cond: &Condition) -> String {
        match cond {
            Condition::Const(b) => b.to_string(),
            Condition::Clbit(k) => {
                let v = self.fresh_ssa();
                let _ = writeln!(self.out, "  {v} = load i1, i1* %c{k}, align 1");
                v
            }
            Condition::Not(inner) => {
                let a = self.emit_cond(inner);
                let v = self.fresh_ssa();
                let _ = writeln!(self.out, "  {v} = xor i1 {a}, true");
                v
            }
            Condition::And(a, b) => {
                let va = self.emit_cond(a);
                let vb = self.emit_cond(b);
                let v = self.fresh_ssa();
                let _ = writeln!(self.out, "  {v} = and i1 {va}, {vb}");
                v
            }
            Condition::Or(a, b) => {
                let va = self.emit_cond(a);
                let vb = self.emit_cond(b);
                let v = self.fresh_ssa();
                let _ = writeln!(self.out, "  {v} = or i1 {va}, {vb}");
                v
            }
            Condition::Xor(a, b) => {
                let va = self.emit_cond(a);
                let vb = self.emit_cond(b);
                let v = self.fresh_ssa();
                let _ = writeln!(self.out, "  {v} = xor i1 {va}, {vb}");
                v
            }
        }
    }

    fn call_1q(&mut self, intr: Intrinsic, q: usize) {
        self.intrinsics.insert(intr);
        let name = match intr {
            Intrinsic::X => "__quantum__qis__x__body",
            Intrinsic::Y => "__quantum__qis__y__body",
            Intrinsic::Z => "__quantum__qis__z__body",
            Intrinsic::H => "__quantum__qis__h__body",
            Intrinsic::S => "__quantum__qis__s__body",
            Intrinsic::SAdj => "__quantum__qis__s__adj",
            Intrinsic::T => "__quantum__qis__t__body",
            Intrinsic::TAdj => "__quantum__qis__t__adj",
            _ => unreachable!("not a 1q gate"),
        };
        let qp = qubit_ptr(q);
        let _ = writeln!(self.out, "  call void @{name}(%Qubit* {qp})");
    }

    fn call_rot(&mut self, intr: Intrinsic, theta: f64, q: usize) {
        if theta.abs() <= 1e-12 {
            return;
        }
        self.intrinsics.insert(intr);
        let name = match intr {
            Intrinsic::Rx => "__quantum__qis__rx__body",
            Intrinsic::Ry => "__quantum__qis__ry__body",
            Intrinsic::Rz => "__quantum__qis__rz__body",
            _ => unreachable!("not a rotation gate"),
        };
        let qp = qubit_ptr(q);
        let angle = llvm_double(theta);
        let _ = writeln!(
            self.out,
            "  call void @{name}(double {angle}, %Qubit* {qp})"
        );
    }

    fn call_2q(&mut self, intr: Intrinsic, q0: usize, q1: usize) {
        self.intrinsics.insert(intr);
        let name = match intr {
            Intrinsic::Cnot => "__quantum__qis__cnot__body",
            Intrinsic::Cz => "__quantum__qis__cz__body",
            Intrinsic::Swap => "__quantum__qis__swap__body",
            _ => unreachable!("not a 2q gate"),
        };
        let p0 = qubit_ptr(q0);
        let p1 = qubit_ptr(q1);
        let _ = writeln!(self.out, "  call void @{name}(%Qubit* {p0}, %Qubit* {p1})");
    }

    fn emit_op(&mut self, op: &Op) {
        if op.gate == Gate::Swap {
            let (a, b) = (op.targets[0], op.targets[1]);
            if op.controls.is_empty() {
                self.call_2q(Intrinsic::Swap, a, b);
            } else {
                // Controlled-Swap = CNOT(b, a); MCX(controls + [a], b); CNOT(b, a).
                let mut ctrls = op.controls.clone();
                ctrls.push(a);
                self.call_2q(Intrinsic::Cnot, b, a);
                self.emit_mcx(&ctrls, b);
                self.call_2q(Intrinsic::Cnot, b, a);
            }
            return;
        }

        let t = op.targets[0];
        let angle = |i: usize| {
            let x = op.params.get(i).copied().unwrap_or(0.0);
            if op.adjoint { -x } else { x }
        };

        if op.controls.is_empty() {
            match op.gate {
                Gate::X => self.call_1q(Intrinsic::X, t),
                Gate::Y => self.call_1q(Intrinsic::Y, t),
                Gate::Z => self.call_1q(Intrinsic::Z, t),
                Gate::H => self.call_1q(Intrinsic::H, t),
                Gate::S => self.call_1q(
                    if op.adjoint {
                        Intrinsic::SAdj
                    } else {
                        Intrinsic::S
                    },
                    t,
                ),
                Gate::T => self.call_1q(
                    if op.adjoint {
                        Intrinsic::TAdj
                    } else {
                        Intrinsic::T
                    },
                    t,
                ),
                Gate::Rx => self.call_rot(Intrinsic::Rx, angle(0), t),
                Gate::Ry => self.call_rot(Intrinsic::Ry, angle(0), t),
                Gate::Rz | Gate::P => self.call_rot(Intrinsic::Rz, angle(0), t),
                Gate::U => {
                    let p = |i: usize| op.params.get(i).copied().unwrap_or(0.0);
                    let (theta, phi, lambda) = if op.adjoint {
                        (-p(0), -p(2), -p(1))
                    } else {
                        (p(0), p(1), p(2))
                    };
                    self.call_rot(Intrinsic::Rz, lambda, t);
                    self.call_rot(Intrinsic::Ry, theta, t);
                    self.call_rot(Intrinsic::Rz, phi, t);
                }
                Gate::Unitary => {
                    let (theta, phi, lambda, _gamma) =
                        crate::unitary::euler(&op.params, op.adjoint);
                    self.call_rot(Intrinsic::Rz, lambda, t);
                    self.call_rot(Intrinsic::Ry, theta, t);
                    self.call_rot(Intrinsic::Rz, phi, t);
                }
                Gate::Swap => unreachable!(),
            }
            return;
        }

        if op.controls.len() == 1 {
            let c = op.controls[0];
            match op.gate {
                Gate::X => self.call_2q(Intrinsic::Cnot, c, t),
                Gate::Z => self.call_2q(Intrinsic::Cz, c, t),
                Gate::Y => {
                    self.call_1q(Intrinsic::SAdj, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                    self.call_1q(Intrinsic::S, t);
                }
                Gate::H => {
                    self.call_rot(Intrinsic::Ry, -FRAC_PI_4, t);
                    self.call_2q(Intrinsic::Cz, c, t);
                    self.call_rot(Intrinsic::Ry, FRAC_PI_4, t);
                }
                Gate::S => {
                    let theta = if op.adjoint {
                        -std::f64::consts::FRAC_PI_2
                    } else {
                        std::f64::consts::FRAC_PI_2
                    };
                    self.emit_cp(c, t, theta);
                }
                Gate::T => {
                    let theta = if op.adjoint { -FRAC_PI_4 } else { FRAC_PI_4 };
                    self.emit_cp(c, t, theta);
                }
                Gate::P => self.emit_cp(c, t, angle(0)),
                Gate::Rz => {
                    let half = angle(0) / 2.0;
                    self.call_rot(Intrinsic::Rz, half, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                    self.call_rot(Intrinsic::Rz, -half, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                }
                Gate::Ry => {
                    let half = angle(0) / 2.0;
                    self.call_rot(Intrinsic::Ry, half, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                    self.call_rot(Intrinsic::Ry, -half, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                }
                Gate::Rx => {
                    let half = angle(0) / 2.0;
                    self.call_1q(Intrinsic::S, t);
                    self.call_rot(Intrinsic::Ry, half, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                    self.call_rot(Intrinsic::Ry, -half, t);
                    self.call_2q(Intrinsic::Cnot, c, t);
                    self.call_1q(Intrinsic::SAdj, t);
                }
                Gate::U => {
                    let p = |i: usize| op.params.get(i).copied().unwrap_or(0.0);
                    let (theta, phi, lambda) = if op.adjoint {
                        (-p(0), -p(2), -p(1))
                    } else {
                        (p(0), p(1), p(2))
                    };
                    self.emit_cu(c, t, theta, phi, lambda, 0.0);
                }
                Gate::Unitary => {
                    let (theta, phi, lambda, gamma) = crate::unitary::euler(&op.params, op.adjoint);
                    self.emit_cu(c, t, theta, phi, lambda, gamma);
                }
                Gate::Swap => unreachable!(),
            }
            return;
        }

        // Multi-controlled gate (2+ controls).
        if op.gate == Gate::X {
            self.emit_mcx(&op.controls, t);
        } else if op.gate == Gate::Z {
            self.call_1q(Intrinsic::H, t);
            self.emit_mcx(&op.controls, t);
            self.call_1q(Intrinsic::H, t);
        } else {
            let m = gate_matrix(op);
            self.emit_mcu(&op.controls, t, m);
        }
    }

    /// Controlled-P(theta) = diag(1, 1, 1, e^{i theta}) up to global phase.
    fn emit_cp(&mut self, c: usize, t: usize, theta: f64) {
        let half = theta / 2.0;
        self.call_rot(Intrinsic::Rz, half, c);
        self.call_rot(Intrinsic::Rz, half, t);
        self.call_2q(Intrinsic::Cnot, c, t);
        self.call_rot(Intrinsic::Rz, -half, t);
        self.call_2q(Intrinsic::Cnot, c, t);
    }

    /// Controlled-`e^{i gamma} U(theta, phi, lambda)` via Barenco ABC decomposition.
    fn emit_cu(&mut self, c: usize, t: usize, theta: f64, phi: f64, lambda: f64, gamma: f64) {
        self.call_rot(Intrinsic::Rz, (lambda - phi) / 2.0, t);
        self.call_2q(Intrinsic::Cnot, c, t);
        self.call_rot(Intrinsic::Rz, -(phi + lambda) / 2.0, t);
        self.call_rot(Intrinsic::Ry, -theta / 2.0, t);
        self.call_2q(Intrinsic::Cnot, c, t);
        self.call_rot(Intrinsic::Ry, theta / 2.0, t);
        self.call_rot(Intrinsic::Rz, phi, t);
        self.call_rot(Intrinsic::Rz, gamma + (phi + lambda) / 2.0, c);
    }

    /// Standard 6-CNOT Toffoli decomposition (`CCX(c0, c1, t)`).
    fn emit_ccx(&mut self, c0: usize, c1: usize, t: usize) {
        self.call_1q(Intrinsic::H, t);
        self.call_2q(Intrinsic::Cnot, c1, t);
        self.call_1q(Intrinsic::TAdj, t);
        self.call_2q(Intrinsic::Cnot, c0, t);
        self.call_1q(Intrinsic::T, t);
        self.call_2q(Intrinsic::Cnot, c1, t);
        self.call_1q(Intrinsic::TAdj, t);
        self.call_2q(Intrinsic::Cnot, c0, t);
        self.call_1q(Intrinsic::T, c1);
        self.call_1q(Intrinsic::T, t);
        self.call_1q(Intrinsic::H, t);
        self.call_2q(Intrinsic::Cnot, c0, c1);
        self.call_1q(Intrinsic::T, c0);
        self.call_1q(Intrinsic::TAdj, c1);
        self.call_2q(Intrinsic::Cnot, c0, c1);
    }

    /// Multi-controlled X with `controls.len() >= 1` and zero extra ancillas.
    fn emit_mcx(&mut self, controls: &[usize], t: usize) {
        match controls {
            [] => self.call_1q(Intrinsic::X, t),
            &[c] => self.call_2q(Intrinsic::Cnot, c, t),
            &[c0, c1] => self.emit_ccx(c0, c1, t),
            _ => {
                let x = [[(0.0, 0.0), (1.0, 0.0)], [(1.0, 0.0), (0.0, 0.0)]];
                self.emit_mcu(controls, t, x);
            }
        }
    }

    /// Barenco Lemma 7.5 zero-ancilla recursion for any multi-controlled 1-qubit unitary `m`.
    fn emit_mcu(&mut self, controls: &[usize], t: usize, m: Mat2) {
        match controls {
            [] => {
                let params = mat2_to_params(m);
                let (theta, phi, lambda, _) = crate::unitary::euler(&params, false);
                self.call_rot(Intrinsic::Rz, lambda, t);
                self.call_rot(Intrinsic::Ry, theta, t);
                self.call_rot(Intrinsic::Rz, phi, t);
            }
            &[c] => {
                let params = mat2_to_params(m);
                let (theta, phi, lambda, gamma) = crate::unitary::euler(&params, false);
                self.emit_cu(c, t, theta, phi, lambda, gamma);
            }
            _ => {
                let v = mat2_sqrt(m);
                let v_dag = mat2_adjoint(v);
                let last = controls[controls.len() - 1];
                let rest = &controls[..controls.len() - 1];
                self.emit_mcu(&[last], t, v);
                self.emit_mcx(rest, last);
                self.emit_mcu(&[last], t, v_dag);
                self.emit_mcx(rest, last);
                self.emit_mcu(rest, t, v);
            }
        }
    }
}

type C = (f64, f64);
type Mat2 = [[C; 2]; 2];

fn c_mul((a, b): C, (c, d): C) -> C {
    (a * c - b * d, a * d + b * c)
}

fn c_add((a, b): C, (c, d): C) -> C {
    (a + c, b + d)
}

fn c_sub((a, b): C, (c, d): C) -> C {
    (a - c, b - d)
}

fn c_scale((a, b): C, s: f64) -> C {
    (a * s, b * s)
}

fn c_exp_i(theta: f64) -> C {
    (theta.cos(), theta.sin())
}

fn c_sqrt((re, im): C) -> C {
    let r = re.hypot(im);
    let a = ((r + re) / 2.0).max(0.0).sqrt();
    let b = ((r - re) / 2.0).max(0.0).sqrt().copysign(im);
    (a, b)
}

fn mat2_adjoint(m: Mat2) -> Mat2 {
    [
        [(m[0][0].0, -m[0][0].1), (m[1][0].0, -m[1][0].1)],
        [(m[0][1].0, -m[0][1].1), (m[1][1].0, -m[1][1].1)],
    ]
}

fn mat2_to_params(m: Mat2) -> [f64; 8] {
    [
        m[0][0].0, m[0][0].1, m[0][1].0, m[0][1].1, m[1][0].0, m[1][0].1, m[1][1].0, m[1][1].1,
    ]
}

/// Exact matrix square root `V` of a 2x2 unitary matrix `M` (`V * V = M`).
fn mat2_sqrt(m: Mat2) -> Mat2 {
    // Cayley-Hamilton for 2x2 matrices:
    //   M^2 - tr(M)*M + det(M)*I = 0
    // If s = sqrt(det(M)) and t = sqrt(tr(M) + 2*s), then V = (M + s*I) / t satisfies V^2 = M.
    // Choose the sign of s that maximizes |tr(M) + 2*s| so t is never near zero (even for M = -I
    // or Pauli matrices with tr = 0, det = -1).
    let det = c_sub(c_mul(m[0][0], m[1][1]), c_mul(m[0][1], m[1][0]));
    let tr = c_add(m[0][0], m[1][1]);
    let s1 = c_sqrt(det);
    let s2 = (-s1.0, -s1.1);
    let sum1 = c_add(tr, c_scale(s1, 2.0));
    let sum2 = c_add(tr, c_scale(s2, 2.0));
    let (s, sum) = if sum1.0.hypot(sum1.1) >= sum2.0.hypot(sum2.1) {
        (s1, sum1)
    } else {
        (s2, sum2)
    };
    let t = c_sqrt(sum);
    let denom = t.0 * t.0 + t.1 * t.1;
    if denom < 1e-24 {
        // Only occurs for M = e^{i alpha} I when tr + 2s == 0 (cannot happen since we picked max,
        // unless M = 0 which is non-unitary). Fallback to diagonal sqrt.
        return [[c_sqrt(m[0][0]), (0.0, 0.0)], [(0.0, 0.0), c_sqrt(m[1][1])]];
    }
    let inv_t = (t.0 / denom, -t.1 / denom);
    [
        [c_mul(c_add(m[0][0], s), inv_t), c_mul(m[0][1], inv_t)],
        [c_mul(m[1][0], inv_t), c_mul(c_add(m[1][1], s), inv_t)],
    ]
}

fn gate_matrix(op: &Op) -> Mat2 {
    let angle = |i: usize| {
        let x = op.params.get(i).copied().unwrap_or(0.0);
        if op.adjoint { -x } else { x }
    };
    match op.gate {
        Gate::X => [[(0.0, 0.0), (1.0, 0.0)], [(1.0, 0.0), (0.0, 0.0)]],
        Gate::Y => [[(0.0, 0.0), (0.0, -1.0)], [(0.0, 1.0), (0.0, 0.0)]],
        Gate::Z => [[(1.0, 0.0), (0.0, 0.0)], [(0.0, 0.0), (-1.0, 0.0)]],
        Gate::H => {
            let s = std::f64::consts::FRAC_1_SQRT_2;
            [[(s, 0.0), (s, 0.0)], [(s, 0.0), (-s, 0.0)]]
        }
        Gate::S => {
            let im = if op.adjoint { -1.0 } else { 1.0 };
            [[(1.0, 0.0), (0.0, 0.0)], [(0.0, 0.0), (0.0, im)]]
        }
        Gate::T => {
            let theta = if op.adjoint { -FRAC_PI_4 } else { FRAC_PI_4 };
            [[(1.0, 0.0), (0.0, 0.0)], [(0.0, 0.0), c_exp_i(theta)]]
        }
        Gate::P => [[(1.0, 0.0), (0.0, 0.0)], [(0.0, 0.0), c_exp_i(angle(0))]],
        Gate::Rx => {
            let (c, s) = ((angle(0) / 2.0).cos(), (angle(0) / 2.0).sin());
            [[(c, 0.0), (0.0, -s)], [(0.0, -s), (c, 0.0)]]
        }
        Gate::Ry => {
            let (c, s) = ((angle(0) / 2.0).cos(), (angle(0) / 2.0).sin());
            [[(c, 0.0), (-s, 0.0)], [(s, 0.0), (c, 0.0)]]
        }
        Gate::Rz => {
            let half = angle(0) / 2.0;
            [[c_exp_i(-half), (0.0, 0.0)], [(0.0, 0.0), c_exp_i(half)]]
        }
        Gate::U => {
            let p = |i: usize| op.params.get(i).copied().unwrap_or(0.0);
            let (theta, phi, lambda) = if op.adjoint {
                (-p(0), -p(2), -p(1))
            } else {
                (p(0), p(1), p(2))
            };
            let (c, s) = ((theta / 2.0).cos(), (theta / 2.0).sin());
            [
                [(c, 0.0), c_scale(c_exp_i(lambda), -s)],
                [c_scale(c_exp_i(phi), s), c_scale(c_exp_i(phi + lambda), c)],
            ]
        }
        Gate::Unitary => {
            let rows = crate::unitary::rows(&op.params, op.adjoint);
            [[rows[0][0], rows[0][1]], [rows[1][0], rows[1][1]]]
        }
        Gate::Swap => unreachable!(),
    }
}

fn qubit_ptr(index: usize) -> String {
    if index == 0 {
        "null".to_string()
    } else {
        format!("inttoptr (i64 {index} to %Qubit*)")
    }
}

fn result_ptr(index: usize) -> String {
    if index == 0 {
        "null".to_string()
    } else {
        format!("inttoptr (i64 {index} to %Result*)")
    }
}

fn llvm_double(x: f64) -> String {
    let x = if x == 0.0 { 0.0 } else { x };
    format!("{x:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(gate: Gate, params: &[f64], controls: &[usize], targets: &[usize]) -> Instruction {
        Instruction::Gate(Op {
            gate,
            adjoint: false,
            params: params.to_vec(),
            controls: controls.to_vec(),
            targets: targets.to_vec(),
        })
    }

    #[test]
    fn base_profile_bell_pair() {
        let circuit = Circuit {
            qubits: 2,
            clbits: 2,
            instructions: vec![
                op(Gate::H, &[], &[], &[0]),
                op(Gate::X, &[], &[0], &[1]),
                Instruction::Measure { qubit: 0, clbit: 0 },
                Instruction::Measure { qubit: 1, clbit: 1 },
            ],
        };
        assert_eq!(profile(&circuit), QirProfile::Base);
        let ir = to_qir(&circuit, "main", &["shots: 1000".into()]);
        assert!(ir.contains("\"qir_profiles\"=\"base_profile\""));
        assert!(ir.contains("\"required_num_qubits\"=\"2\""));
        assert!(ir.contains("\"required_num_results\"=\"2\""));
        assert!(ir.contains("call void @__quantum__qis__h__body(%Qubit* null)"));
        assert!(ir.contains(
            "call void @__quantum__qis__cnot__body(%Qubit* null, %Qubit* inttoptr (i64 1 to %Qubit*))"
        ));
        assert!(ir.contains("call void @__quantum__qis__mz__body(%Qubit* null, %Result* null)"));
        assert!(ir.contains("call void @__quantum__rt__array_record_output(i64 2, i8* null)"));
        assert!(ir.contains("!{i32 1, !\"qir_major_version\", i32 1}"));
        assert!(ir.contains("!{i32 1, !\"dynamic_qubit_management\", i1 false}"));
    }

    #[test]
    fn adaptive_profile_if_while_and_reset() {
        let circuit = Circuit {
            qubits: 2,
            clbits: 2,
            instructions: vec![
                Instruction::Assign {
                    clbit: 1,
                    value: Condition::Const(false),
                },
                Instruction::While {
                    condition: Condition::Not(Box::new(Condition::Clbit(1))),
                    body: vec![
                        op(Gate::H, &[], &[], &[0]),
                        Instruction::Measure { qubit: 0, clbit: 0 },
                        Instruction::If {
                            condition: Condition::Clbit(0),
                            then: vec![
                                op(Gate::X, &[], &[], &[1]),
                                Instruction::Assign {
                                    clbit: 1,
                                    value: Condition::Const(true),
                                },
                            ],
                            otherwise: vec![Instruction::Reset(0)],
                        },
                    ],
                },
            ],
        };
        assert_eq!(profile(&circuit), QirProfile::Adaptive);
        let ir = to_qir(&circuit, "main", &[]);
        assert!(ir.contains("\"qir_profiles\"=\"adaptive_profile\""));
        assert!(ir.contains("call i1 @__quantum__rt__read_result(%Result* null)"));
        assert!(ir.contains("call void @__quantum__qis__reset__body(%Qubit* null)"));
        assert!(ir.contains("br label %while.cond."));
        assert!(ir.contains("!{i32 1, !\"qubit_resetting\", i1 true}"));
        assert!(ir.contains("!{i32 1, !\"backwards_branching\", i1 true}"));
        assert!(ir.contains("@__quantum__rt__bool_record_output(i1"));
    }

    /// Parses QIR gate calls back into `Op`s and verifies that our controlled-gate
    /// decompositions match the exact statevector produced by `ketrs` up to global phase.
    #[cfg(feature = "ketrs")]
    #[test]
    fn decompositions_match_ketrs_statevector() {
        use crate::backend::Backend;

        let parse_qir_ops = |ir: &str| -> Vec<Op> {
            let parse_q = |s: &str| -> usize {
                let s = s.trim();
                if s == "%Qubit* null" {
                    0
                } else {
                    s.split("i64 ")
                        .nth(1)
                        .unwrap()
                        .split(" to")
                        .next()
                        .unwrap()
                        .parse()
                        .unwrap()
                }
            };
            let mut ops = Vec::new();
            for line in ir.lines() {
                let line = line.trim();
                let Some(rest) = line.strip_prefix("call void @__quantum__qis__") else {
                    continue;
                };
                let (name, args_str) = rest.split_once('(').unwrap();
                let args: Vec<&str> = args_str.trim_end_matches(')').split(',').collect();
                let op = match name {
                    "x__body" => Op {
                        gate: Gate::X,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "y__body" => Op {
                        gate: Gate::Y,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "z__body" => Op {
                        gate: Gate::Z,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "h__body" => Op {
                        gate: Gate::H,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "s__body" => Op {
                        gate: Gate::S,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "s__adj" => Op {
                        gate: Gate::S,
                        adjoint: true,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "t__body" => Op {
                        gate: Gate::T,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "t__adj" => Op {
                        gate: Gate::T,
                        adjoint: true,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0])],
                    },
                    "rx__body" | "ry__body" | "rz__body" => {
                        let theta: f64 = args[0]
                            .trim()
                            .strip_prefix("double ")
                            .unwrap()
                            .parse()
                            .unwrap();
                        let gate = match name {
                            "rx__body" => Gate::Rx,
                            "ry__body" => Gate::Ry,
                            _ => Gate::Rz,
                        };
                        Op {
                            gate,
                            adjoint: false,
                            params: vec![theta],
                            controls: vec![],
                            targets: vec![parse_q(args[1])],
                        }
                    }
                    "cnot__body" => Op {
                        gate: Gate::X,
                        adjoint: false,
                        params: vec![],
                        controls: vec![parse_q(args[0])],
                        targets: vec![parse_q(args[1])],
                    },
                    "cz__body" => Op {
                        gate: Gate::Z,
                        adjoint: false,
                        params: vec![],
                        controls: vec![parse_q(args[0])],
                        targets: vec![parse_q(args[1])],
                    },
                    "swap__body" => Op {
                        gate: Gate::Swap,
                        adjoint: false,
                        params: vec![],
                        controls: vec![],
                        targets: vec![parse_q(args[0]), parse_q(args[1])],
                    },
                    other => panic!("unexpected QIS intrinsic: {other}"),
                };
                ops.push(op);
            }
            ops
        };

        let check_op = |n_qubits: usize, target_op: Op| {
            // Prepare an entangled non-trivial superposition across all qubits so any phase or
            // amplitude error shows up in the statevector.
            let prep: Vec<Op> = (0..n_qubits)
                .flat_map(|q| {
                    vec![
                        Op {
                            gate: Gate::Ry,
                            adjoint: false,
                            params: vec![0.37 * (q as f64 + 1.0)],
                            controls: vec![],
                            targets: vec![q],
                        },
                        Op {
                            gate: Gate::Rz,
                            adjoint: false,
                            params: vec![0.61 * (q as f64 + 1.0)],
                            controls: vec![],
                            targets: vec![q],
                        },
                    ]
                })
                .collect();

            let circuit = Circuit {
                qubits: n_qubits,
                clbits: 0,
                instructions: vec![Instruction::Gate(target_op.clone())],
            };
            let ir = to_qir(&circuit, "test", &[]);
            let decomposed = parse_qir_ops(&ir);

            let run_ops = |ops: &[Op]| -> Vec<(f64, f64)> {
                let mut sess = crate::Ketrs.session(0).unwrap();
                let qs: Vec<usize> = (0..n_qubits).map(|_| sess.add_qubit().unwrap()).collect();
                for p in &prep {
                    sess.apply(p).unwrap();
                }
                for o in ops {
                    sess.apply(o).unwrap();
                }
                sess.amplitudes(&qs).unwrap().unwrap()
            };

            let expected = run_ops(std::slice::from_ref(&target_op));
            let actual = run_ops(&decomposed);

            // Compute |<expected | actual>|^2 (fidelity up to global phase).
            let (mut re, mut im) = (0.0, 0.0);
            for ((er, ei), (ar, ai)) in expected.iter().zip(&actual) {
                re += er * ar + ei * ai;
                im += er * ai - ei * ar;
            }
            let fidelity = re * re + im * im;
            assert!(
                (fidelity - 1.0).abs() < 1e-9,
                "fidelity {fidelity} != 1.0 for {target_op:?}\nQIR:\n{ir}"
            );
        };

        // 1-control gates.
        for gate in [Gate::X, Gate::Y, Gate::Z, Gate::H, Gate::S, Gate::T] {
            for adj in [false, true] {
                check_op(
                    2,
                    Op {
                        gate,
                        adjoint: adj,
                        params: vec![],
                        controls: vec![0],
                        targets: vec![1],
                    },
                );
            }
        }
        for gate in [Gate::Rx, Gate::Ry, Gate::Rz, Gate::P] {
            for adj in [false, true] {
                check_op(
                    2,
                    Op {
                        gate,
                        adjoint: adj,
                        params: vec![0.73],
                        controls: vec![0],
                        targets: vec![1],
                    },
                );
            }
        }
        check_op(
            2,
            Op {
                gate: Gate::U,
                adjoint: false,
                params: vec![0.5, 1.1, -0.4],
                controls: vec![0],
                targets: vec![1],
            },
        );
        check_op(
            2,
            Op {
                gate: Gate::U,
                adjoint: true,
                params: vec![0.5, 1.1, -0.4],
                controls: vec![0],
                targets: vec![1],
            },
        );
        // Controlled-Swap (Fredkin).
        check_op(
            3,
            Op {
                gate: Gate::Swap,
                adjoint: false,
                params: vec![],
                controls: vec![0],
                targets: vec![1, 2],
            },
        );
        // 2-control and 3-control gates (Toffoli, CCZ, CCRy, CCCX, CCU).
        check_op(
            3,
            Op {
                gate: Gate::X,
                adjoint: false,
                params: vec![],
                controls: vec![0, 1],
                targets: vec![2],
            },
        );
        check_op(
            3,
            Op {
                gate: Gate::Z,
                adjoint: false,
                params: vec![],
                controls: vec![0, 1],
                targets: vec![2],
            },
        );
        check_op(
            3,
            Op {
                gate: Gate::Ry,
                adjoint: false,
                params: vec![0.9],
                controls: vec![0, 1],
                targets: vec![2],
            },
        );
        check_op(
            3,
            Op {
                gate: Gate::U,
                adjoint: false,
                params: vec![0.4, 0.8, 1.2],
                controls: vec![0, 1],
                targets: vec![2],
            },
        );
        check_op(
            4,
            Op {
                gate: Gate::X,
                adjoint: false,
                params: vec![],
                controls: vec![0, 1, 2],
                targets: vec![3],
            },
        );
    }
}
