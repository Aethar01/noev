//! Target descriptions and compile-time capability checking (`spec/runtime.md` §5,
//! `spec/interop.md` §5).
//!
//! Every quantum backend (simulator or hardware) has a [`TargetSpec`] describing the
//! execution levels it accepts, its qubit count, native gate set, connectivity, and
//! capabilities. [`check_target`] checks a compiled program against a target and reports
//! diagnostics pointing at the exact source lines that need what the target lacks.

use noev_syntax::ast::{BlockId, ExprId, ExprKind, FnId, StmtId, StmtKind};
use noev_syntax::{Ast, Diagnostic, Span};

use crate::check::{Callee, Typed};
use crate::circuit::Circuits;
use crate::prelude::{Builtin, Gate};
use crate::ty::{Kind, TyKind};

/// Which execution levels a backend accepts (`spec/runtime.md` §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendLevel {
    /// Whole circuits submitted as shot jobs.
    Circuit,
    /// Operations sent one gate at a time.
    Gate,
    /// Both circuit-level and gate-level execution.
    Both,
}

impl BackendLevel {
    /// Canonical lowercase name used in target TOML files.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendLevel::Circuit => "circuit",
            BackendLevel::Gate => "gate",
            BackendLevel::Both => "both",
        }
    }

    /// Parses a level name (`circuit`, `gate`, or `both`).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "circuit" => Some(BackendLevel::Circuit),
            "gate" => Some(BackendLevel::Gate),
            "both" => Some(BackendLevel::Both),
            _ => None,
        }
    }

    /// Encodes as C ABI integer (`0 = Circuit`, `1 = Gate`, `2 = Both`).
    pub fn to_abi(self) -> u8 {
        match self {
            BackendLevel::Circuit => 0,
            BackendLevel::Gate => 1,
            BackendLevel::Both => 2,
        }
    }

    /// Decodes from C ABI integer (`0 = Circuit`, `1 = Gate`, `2 = Both`).
    pub fn from_abi(v: u8) -> Option<Self> {
        match v {
            0 => Some(BackendLevel::Circuit),
            1 => Some(BackendLevel::Gate),
            2 => Some(BackendLevel::Both),
            _ => None,
        }
    }
}

/// Optional capabilities declared by a target (`spec/runtime.md` §3, §5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TargetCapabilities {
    /// Can read a register's statevector (`std::debug::amplitudes`).
    pub read_state: bool,
    /// Verifies that `release`d qubits are in |0⟩.
    pub check_release: bool,
    /// Computes exact expectation values and overlaps from the state.
    pub exact_expectations: bool,
    /// Samples a final state without destructive measurement.
    pub sample_state: bool,
    /// Allows foreign calls (`extern fn`, `extern "C"`) while qubits are alive.
    pub foreign_while_live: bool,
}

impl TargetCapabilities {
    /// Bitmask bit for `read_state`.
    pub const BIT_READ_STATE: u32 = 1 << 0;
    /// Bitmask bit for `check_release`.
    pub const BIT_CHECK_RELEASE: u32 = 1 << 1;
    /// Bitmask bit for `exact_expectations`.
    pub const BIT_EXACT_EXPECTATIONS: u32 = 1 << 2;
    /// Bitmask bit for `sample_state`.
    pub const BIT_SAMPLE_STATE: u32 = 1 << 3;
    /// Bitmask bit for `foreign_while_live`.
    pub const BIT_FOREIGN_WHILE_LIVE: u32 = 1 << 4;

    /// Encodes capabilities as a C ABI bitmask.
    pub fn to_bits(self) -> u32 {
        let mut bits = 0u32;
        if self.read_state {
            bits |= Self::BIT_READ_STATE;
        }
        if self.check_release {
            bits |= Self::BIT_CHECK_RELEASE;
        }
        if self.exact_expectations {
            bits |= Self::BIT_EXACT_EXPECTATIONS;
        }
        if self.sample_state {
            bits |= Self::BIT_SAMPLE_STATE;
        }
        if self.foreign_while_live {
            bits |= Self::BIT_FOREIGN_WHILE_LIVE;
        }
        bits
    }

    /// Decodes capabilities from a C ABI bitmask.
    pub fn from_bits(bits: u32) -> Self {
        TargetCapabilities {
            read_state: (bits & Self::BIT_READ_STATE) != 0,
            check_release: (bits & Self::BIT_CHECK_RELEASE) != 0,
            exact_expectations: (bits & Self::BIT_EXACT_EXPECTATIONS) != 0,
            sample_state: (bits & Self::BIT_SAMPLE_STATE) != 0,
            foreign_while_live: (bits & Self::BIT_FOREIGN_WHILE_LIVE) != 0,
        }
    }
}

/// Complete target description for a quantum backend (`spec/runtime.md` §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSpec {
    /// Backend name (`NAME`).
    pub name: String,
    /// Optional device name (`DEVICE` in `NAME:DEVICE`).
    pub device: Option<String>,
    /// Supported execution levels.
    pub level: BackendLevel,
    /// Maximum number of simultaneous qubits (`None` = unbounded).
    pub qubits: Option<usize>,
    /// Native gate names (`None` = universal / all standard gates).
    pub gates: Option<Vec<String>>,
    /// Optional qubit connectivity pairs `(a, b)` (`None` = all-to-all).
    pub coupling: Option<Vec<(usize, usize)>>,
    /// Declared target capabilities.
    pub capabilities: TargetCapabilities,
    /// Optional shared-library path for a plug-in backend.
    pub plugin: Option<String>,
}

impl TargetSpec {
    /// Target description for the built-in `ketrs` statevector simulator.
    pub fn ketrs() -> Self {
        TargetSpec {
            name: "ketrs".into(),
            device: None,
            level: BackendLevel::Both,
            qubits: Some(30),
            gates: None,
            coupling: None,
            capabilities: TargetCapabilities {
                read_state: true,
                check_release: true,
                exact_expectations: true,
                sample_state: true,
                foreign_while_live: true,
            },
            plugin: None,
        }
    }

    /// Target description for the built-in `export` circuit recorder.
    pub fn export() -> Self {
        TargetSpec {
            name: "export".into(),
            device: None,
            level: BackendLevel::Circuit,
            qubits: None,
            gates: None,
            coupling: None,
            capabilities: TargetCapabilities {
                read_state: false,
                check_release: false,
                exact_expectations: false,
                sample_state: false,
                foreign_while_live: true,
            },
            plugin: None,
        }
    }

    /// Looks up a built-in target description by name (`ketrs` or `export`).
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "ketrs" => Some(Self::ketrs()),
            "export" => Some(Self::export()),
            _ => None,
        }
    }

    /// Serialises this target description to a human-readable TOML file.
    pub fn to_toml(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(out, "name = {}", escape_toml_str(&self.name));
        if let Some(ref dev) = self.device {
            let _ = writeln!(out, "device = {}", escape_toml_str(dev));
        }
        let _ = writeln!(out, "level = {}", escape_toml_str(self.level.as_str()));
        if let Some(q) = self.qubits {
            let _ = writeln!(out, "qubits = {q}");
        }
        if let Some(ref gates) = self.gates {
            let items: Vec<String> = gates.iter().map(|g| escape_toml_str(g)).collect();
            let _ = writeln!(out, "gates = [{}]", items.join(", "));
        }
        if let Some(ref coupling) = self.coupling {
            let pairs: Vec<String> = coupling
                .iter()
                .map(|(a, b)| format!("[{a}, {b}]"))
                .collect();
            let _ = writeln!(out, "coupling = [{}]", pairs.join(", "));
        }
        if let Some(ref plugin) = self.plugin {
            let _ = writeln!(out, "plugin = {}", escape_toml_str(plugin));
        }
        out.push_str("\n[capabilities]\n");
        let c = self.capabilities;
        let _ = writeln!(out, "read_state = {}", c.read_state);
        let _ = writeln!(out, "check_release = {}", c.check_release);
        let _ = writeln!(out, "exact_expectations = {}", c.exact_expectations);
        let _ = writeln!(out, "sample_state = {}", c.sample_state);
        let _ = writeln!(out, "foreign_while_live = {}", c.foreign_while_live);
        out
    }

    /// Parses a target description from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let mut name: Option<String> = None;
        let mut device: Option<String> = None;
        let mut level: Option<BackendLevel> = None;
        let mut qubits: Option<usize> = None;
        let mut gates: Option<Vec<String>> = None;
        let mut coupling: Option<Vec<(usize, usize)>> = None;
        let mut plugin: Option<String> = None;
        let mut caps = TargetCapabilities::default();
        let mut in_caps = false;

        for (lineno, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let section = line[1..line.len() - 1].trim();
                in_caps = section == "capabilities";
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return Err(format!("line {}: expected `key = value`", lineno + 1));
            };
            let key = k.trim();
            let val = v.trim();
            if in_caps {
                let b = parse_bool(val)
                    .ok_or_else(|| format!("line {}: expected boolean for `{key}`", lineno + 1))?;
                match key {
                    "read_state" => caps.read_state = b,
                    "check_release" => caps.check_release = b,
                    "exact_expectations" => caps.exact_expectations = b,
                    "sample_state" => caps.sample_state = b,
                    "foreign_while_live" => caps.foreign_while_live = b,
                    _ => {}
                }
            } else {
                match key {
                    "name" => {
                        name = Some(parse_str(val).ok_or_else(|| {
                            format!("line {}: expected string for `name`", lineno + 1)
                        })?);
                    }
                    "device" => {
                        device = Some(parse_str(val).ok_or_else(|| {
                            format!("line {}: expected string for `device`", lineno + 1)
                        })?);
                    }
                    "level" => {
                        let s = parse_str(val).ok_or_else(|| {
                            format!("line {}: expected string for `level`", lineno + 1)
                        })?;
                        level = Some(BackendLevel::parse(&s).ok_or_else(|| {
                            format!(
                                "line {}: unknown level `{s}` (expected `circuit`, `gate`, or `both`)",
                                lineno + 1
                            )
                        })?);
                    }
                    "qubits" => {
                        let n: usize = val.parse().map_err(|_| {
                            format!("line {}: expected integer for `qubits`", lineno + 1)
                        })?;
                        qubits = Some(n);
                    }
                    "gates" => {
                        gates = Some(parse_str_array(val).ok_or_else(|| {
                            format!("line {}: expected string array for `gates`", lineno + 1)
                        })?);
                    }
                    "coupling" => {
                        coupling = Some(parse_pair_array(val).ok_or_else(|| {
                            format!(
                                "line {}: expected `[[a, b], ...]` for `coupling`",
                                lineno + 1
                            )
                        })?);
                    }
                    "plugin" => {
                        plugin = Some(parse_str(val).ok_or_else(|| {
                            format!("line {}: expected string for `plugin`", lineno + 1)
                        })?);
                    }
                    _ => {}
                }
            }
        }

        Ok(TargetSpec {
            name: name.ok_or_else(|| "target description is missing `name`".to_string())?,
            device,
            level: level.unwrap_or(BackendLevel::Circuit),
            qubits,
            gates,
            coupling,
            capabilities: caps,
            plugin,
        })
    }
}

fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_str {
            escaped = true;
        } else if ch == '"' {
            in_str = !in_str;
        } else if ch == '#' && !in_str {
            return &line[..i];
        }
    }
    line
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn escape_toml_str(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn parse_str(s: &str) -> Option<String> {
    let s = s.trim();
    if !(s.starts_with('"') && s.ends_with('"') && s.len() >= 2) {
        return None;
    }
    let mut out = String::new();
    let mut chars = s[1..s.len() - 1].chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'u' => {
                    let mut hex = String::with_capacity(4);
                    for _ in 0..4 {
                        hex.push(chars.next()?);
                    }
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn parse_str_array(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    if !(s.starts_with('[') && s.ends_with(']')) {
        return None;
    }
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(parse_str(item)?);
    }
    Some(out)
}

fn parse_pair_array(s: &str) -> Option<Vec<(usize, usize)>> {
    let s = s.trim();
    if !(s.starts_with('[') && s.ends_with(']')) {
        return None;
    }
    let inner = s[1..s.len() - 1].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    let mut rest = inner;
    while !rest.is_empty() {
        rest = rest.trim_start_matches([',', ' ', '\t']);
        if rest.is_empty() {
            break;
        }
        if !rest.starts_with('[') {
            return None;
        }
        let end = rest.find(']')?;
        let pair = &rest[1..end];
        let (a, b) = pair.split_once(',')?;
        out.push((a.trim().parse().ok()?, b.trim().parse().ok()?));
        rest = &rest[end + 1..];
    }
    Some(out)
}

/// Checks `ast` against `target`, reporting compile-time diagnostics for anything `target`
/// lacks (`spec/runtime.md` §5, `spec/interop.md` §5).
pub fn check_target(
    ast: &Ast,
    typed: &Typed,
    circuits: &Circuits,
    target: &TargetSpec,
) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let std_pkg = ast.std_package();

    // 1. Circuit-level-only target: reject any `sample` run (or `main`) that is not a circuit.
    if target.level == BackendLevel::Circuit {
        for (&expr_id, info) in &circuits.runs {
            if let Some((span, ref why)) = info.not_circuit {
                let mut d = Diagnostic::error(
                    span,
                    format!(
                        "target `{}` only accepts circuits, but this run needs gate-level control: {why}",
                        target.name
                    ),
                );
                let run_span = ast.expr(expr_id).span;
                if run_span != span {
                    d.labels.push((
                        run_span,
                        "this `sample` run cannot be compiled to a circuit".into(),
                    ));
                }
                diags.push(d);
            }
        }
    }

    let universal = target
        .gates
        .as_ref()
        .is_none_or(|allowed| is_universal(allowed));

    // Walk every user function body for capabilities, gates, qubit counts, and foreign calls.
    for (idx, decl) in ast.all_fns().iter().enumerate() {
        let f = FnId::from_index(idx);
        let module = typed.fn_modules[f.index()];
        if Some(ast.module(module).package) == std_pkg {
            continue;
        }
        let Some(body) = decl.body else { continue };
        let sig = &typed.fns[f.index()].sig;
        let param_qubits: usize = sig
            .params
            .iter()
            .filter_map(|&t| typed.types.qubit_count(typed.types.deref(t)))
            .map(|n| n as usize)
            .sum();
        let has_param_qubits = param_qubits > 0
            || sig
                .params
                .iter()
                .any(|&t| typed.types.linearity(typed.types.deref(t)) == Kind::Linear);

        let mut checker = TargetWalker {
            ast,
            typed,
            target,
            universal,
            diags: &mut diags,
            live_qubits: param_qubits,
            any_live: has_param_qubits,
        };
        if let Some(max_q) = target.qubits
            && param_qubits > max_q
        {
            checker.diags.push(Diagnostic::error(
                decl.name_span,
                format!(
                    "function `{}` takes {param_qubits} qubits, but target `{}` has only {max_q}",
                    ast.name(decl.name),
                    target.name
                ),
            ));
        }
        checker.block(body);
    }

    diags
}

/// Whether `allowed` contains a universal 1-qubit rotation set (allowing decomposition of any
/// standard gate).
fn is_universal(allowed: &[String]) -> bool {
    let has = |g: &str| allowed.iter().any(|a| a.eq_ignore_ascii_case(g));
    has("u")
        || (has("rx") && has("rz"))
        || (has("ry") && has("rz"))
        || (has("sx") && has("rz"))
        || (has("h") && has("t"))
}

fn gate_name(g: Gate) -> &'static str {
    match g {
        Gate::X => "x",
        Gate::Y => "y",
        Gate::Z => "z",
        Gate::H => "h",
        Gate::S => "s",
        Gate::T => "t",
        Gate::Rx => "rx",
        Gate::Ry => "ry",
        Gate::Rz => "rz",
        Gate::P => "p",
        Gate::U => "u",
        Gate::Swap => "swap",
        Gate::Unitary => "unitary",
    }
}

struct TargetWalker<'a> {
    ast: &'a Ast,
    typed: &'a Typed,
    target: &'a TargetSpec,
    universal: bool,
    diags: &'a mut Vec<Diagnostic>,
    live_qubits: usize,
    any_live: bool,
}

impl TargetWalker<'_> {
    fn block(&mut self, id: BlockId) {
        let saved_q = self.live_qubits;
        let saved_any = self.any_live;
        for &stmt in self.ast.list(self.ast.block(id).stmts) {
            self.stmt(stmt);
        }
        self.live_qubits = saved_q;
        self.any_live = saved_any;
    }

    fn stmt(&mut self, id: StmtId) {
        match self.ast.stmt(id).kind {
            StmtKind::Let { init, .. } => self.expr(init),
            StmtKind::Assign { place, value, .. } => {
                self.expr(place);
                self.expr(value);
            }
            StmtKind::Assert { cond, msg } => {
                self.expr(cond);
                if let Some(m) = msg {
                    self.expr(m);
                }
            }
            StmtKind::Expr { expr, .. } => self.expr(expr),
            StmtKind::Return(Some(expr)) => self.expr(expr),
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
        }
    }

    fn expr(&mut self, id: ExprId) {
        let node = *self.ast.expr(id);
        match node.kind {
            ExprKind::Ket { parts, .. } => {
                let count = self
                    .typed
                    .types
                    .qubit_count(self.typed.ty(id))
                    .map(|n| n as usize)
                    .unwrap_or_else(|| self.ast.list(parts).len().max(1));
                self.note_alloc(count, node.span);
                for &part in self.ast.list(parts) {
                    if let noev_syntax::ast::KetPart::Value(e) = part {
                        self.expr(e);
                    }
                }
            }
            ExprKind::ArrayRepeat { value, .. } => {
                if let TyKind::Array(elem, len) = *self.typed.types.kind(self.typed.ty(id))
                    && let Some(per) = self.typed.types.qubit_count(elem)
                {
                    let total = (len as usize).saturating_mul(per as usize);
                    self.note_alloc(total, node.span);
                } else {
                    self.expr(value);
                }
            }
            ExprKind::Call { callee, args } => {
                self.check_callee(id, node.span);
                self.expr(callee);
                for &a in self.ast.list(args) {
                    self.expr(a);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.check_callee(id, node.span);
                self.expr(receiver);
                for &a in self.ast.list(args) {
                    self.expr(a);
                }
            }
            ExprKind::Sample { shots, body } => {
                self.expr(shots);
                // A `sample` block runs in its own isolated quantum state.
                let saved_q = std::mem::replace(&mut self.live_qubits, 0);
                let saved_any = std::mem::replace(&mut self.any_live, false);
                self.block(body);
                self.live_qubits = saved_q;
                self.any_live = saved_any;
            }
            ExprKind::Expectation {
                prep,
                observable,
                precision,
            } => {
                self.expr(prep);
                self.expr(observable);
                if let Some(p) = precision {
                    self.expr(p);
                }
            }
            ExprKind::Overlap {
                bra,
                ket,
                observable,
                precision,
            } => {
                self.expr(bra);
                self.expr(ket);
                if let Some(o) = observable {
                    self.expr(o);
                }
                if let Some(p) = precision {
                    self.expr(p);
                }
            }
            ExprKind::Block(b) | ExprKind::Unsafe(b) => self.block(b),
            ExprKind::If {
                cond,
                then,
                otherwise,
            } => {
                self.expr(cond);
                self.block(then);
                if let Some(e) = otherwise {
                    self.expr(e);
                }
            }
            ExprKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            ExprKind::For {
                start, end, body, ..
            } => {
                self.expr(start);
                self.expr(end);
                self.block(body);
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in self.ast.list(arms) {
                    self.expr(arm.body);
                }
            }
            ExprKind::Ctrl { controls, body } => {
                for &c in self.ast.list(controls) {
                    self.expr(c);
                }
                self.block(body);
            }
            ExprKind::With { with, body } => {
                self.block(with);
                self.block(body);
            }
            ExprKind::Tuple(xs) | ExprKind::Array(xs) => {
                for &x in self.ast.list(xs) {
                    self.expr(x);
                }
            }
            ExprKind::StructLit { fields, .. } => {
                for f in self.ast.list(fields) {
                    if let Some(v) = f.value {
                        self.expr(v);
                    }
                }
            }
            ExprKind::Unary { operand, .. }
            | ExprKind::Adj(operand)
            | ExprKind::Own { place: operand, .. }
            | ExprKind::Field { base: operand, .. }
            | ExprKind::TupleIndex { base: operand, .. } => self.expr(operand),
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::Index {
                base: lhs,
                index: rhs,
            } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::BraTest { operand, parts, .. } => {
                self.expr(operand);
                for &p in self.ast.list(parts) {
                    if let noev_syntax::ast::KetPart::Value(e) = p {
                        self.expr(e);
                    }
                }
                let freed = self
                    .typed
                    .types
                    .qubit_count(self.typed.ty(operand))
                    .unwrap_or(1) as usize;
                self.live_qubits = self.live_qubits.saturating_sub(freed);
            }
            ExprKind::PrepTest { prep, operand } => {
                self.expr(prep);
                self.expr(operand);
            }
            ExprKind::Str(parts) => {
                for p in self.ast.list(parts) {
                    if let noev_syntax::ast::StrPart::Value { expr, .. } = *p {
                        self.expr(expr);
                    }
                }
            }
            _ => {}
        }
    }

    fn note_alloc(&mut self, count: usize, span: Span) {
        self.live_qubits = self.live_qubits.saturating_add(count);
        if count > 0 {
            self.any_live = true;
        }
        if let Some(max_q) = self.target.qubits
            && self.live_qubits > max_q
        {
            self.diags.push(Diagnostic::error(
                span,
                format!(
                    "this needs at least {} qubits at once, but target `{}` has only {max_q}",
                    self.live_qubits, self.target.name
                ),
            ));
        }
    }

    fn check_callee(&mut self, id: ExprId, span: Span) {
        let Some(callee) = self.typed.callees[id.index()] else {
            return;
        };
        match callee {
            Callee::Builtin(Builtin::Amplitudes) if !self.target.capabilities.read_state => {
                self.diags.push(Diagnostic::error(
                    span,
                    format!(
                        "`amplitudes` requires capability `read_state`, which target `{}` does not support",
                        self.target.name
                    ),
                ));
            }
            Callee::Builtin(Builtin::Gate(g)) => {
                if !self.universal
                    && let Some(ref allowed) = self.target.gates
                {
                    let name = gate_name(g);
                    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(name)) {
                        self.diags.push(Diagnostic::error(
                            span,
                            format!(
                                "gate `{name}` is not supported by target `{}` (and cannot be decomposed into its gate set)",
                                self.target.name
                            ),
                        ));
                    }
                }
            }
            Callee::Builtin(
                Builtin::Measure | Builtin::MeasureIn | Builtin::Discard | Builtin::Release,
            ) => {
                // Approximate qubit release for static count tracking within the current block.
                let consumed = match self.ast.expr(id).kind {
                    ExprKind::MethodCall { receiver, .. } => {
                        self.typed
                            .types
                            .qubit_count(self.typed.ty(receiver))
                            .unwrap_or(1) as usize
                    }
                    ExprKind::Call { args, .. } => self
                        .ast
                        .list(args)
                        .first()
                        .and_then(|&a| self.typed.types.qubit_count(self.typed.ty(a)))
                        .unwrap_or(1) as usize,
                    _ => 1,
                };
                self.live_qubits = self.live_qubits.saturating_sub(consumed);
                if self.live_qubits == 0 {
                    self.any_live = false;
                }
            }
            Callee::Fn(f)
                if self.ast.function(f).is_foreign_import()
                    && self.any_live
                    && !self.target.capabilities.foreign_while_live =>
            {
                let name = self.ast.name(self.ast.function(f).name);
                self.diags.push(Diagnostic::error(
                    span,
                    format!(
                        "foreign call `{name}` while qubits are alive is not supported by target `{}`",
                        self.target.name
                    ),
                ));
            }
            Callee::FnPtr if self.any_live && !self.target.capabilities.foreign_while_live => {
                self.diags.push(Diagnostic::error(
                    span,
                    format!(
                        "calling a foreign function pointer while qubits are alive is not supported by target `{}`",
                        self.target.name
                    ),
                ));
            }
            _ => {}
        }
    }
}

/// Checks `spec/runtime.md` §3: an `impl std::backend::Backend for T` must have a classical
/// (non-linear) receiver type `T`, and its methods cannot create qubits or run quantum ops.
pub fn check_backend_impls(ast: &Ast, typed: &Typed) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let Some(std_pkg) = ast.std_package() else {
        return diags;
    };
    let backend_trait = typed.traits.iter().position(|t| {
        ast.name(t.name) == "Backend"
            && t.fns.first().is_some_and(|&f| {
                let m = ast.module(typed.fn_modules[f.index()]);
                m.package == std_pkg && ast.name(m.name) == "backend"
            })
    });
    let Some(backend_trait) = backend_trait else {
        return diags;
    };

    for imp in &typed.impls {
        if imp.trait_id.index() != backend_trait {
            continue;
        }
        if typed.types.linearity(imp.self_ty) == Kind::Linear {
            diags.push(Diagnostic::error(
                imp.span,
                "a `Backend` implementation cannot hold qubits (`spec/runtime.md` §3)",
            ));
        }
        for &f in &imp.fns {
            let Some(body) = ast.function(f).body else {
                continue;
            };
            find_quantum_in_backend(ast, typed, body, &mut diags);
        }
    }
    diags
}

fn find_quantum_in_backend(ast: &Ast, typed: &Typed, block: BlockId, diags: &mut Vec<Diagnostic>) {
    let mut stack_blocks = vec![block];
    let mut stack_exprs: Vec<ExprId> = Vec::new();
    while let Some(b) = stack_blocks.pop() {
        for &s in ast.list(ast.block(b).stmts) {
            match ast.stmt(s).kind {
                StmtKind::Let { init, .. } => stack_exprs.push(init),
                StmtKind::Assign { place, value, .. } => {
                    stack_exprs.push(place);
                    stack_exprs.push(value);
                }
                StmtKind::Assert { cond, msg } => {
                    stack_exprs.push(cond);
                    if let Some(m) = msg {
                        stack_exprs.push(m);
                    }
                }
                StmtKind::Expr { expr, .. } | StmtKind::Return(Some(expr)) => {
                    stack_exprs.push(expr)
                }
                StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue => {}
            }
        }
        while let Some(e) = stack_exprs.pop() {
            let node = *ast.expr(e);
            match node.kind {
                ExprKind::Ket { .. }
                | ExprKind::BraTest { .. }
                | ExprKind::PrepTest { .. }
                | ExprKind::Sample { .. }
                | ExprKind::Expectation { .. }
                | ExprKind::Overlap { .. }
                | ExprKind::Ctrl { .. }
                | ExprKind::With { .. } => {
                    diags.push(Diagnostic::error(
                        node.span,
                        "code implementing `std::backend::Backend` cannot create or act on qubits",
                    ));
                }
                _ => {
                    if let Some(Callee::Builtin(
                        Builtin::Gate(_)
                        | Builtin::Measure
                        | Builtin::MeasureIn
                        | Builtin::Reset
                        | Builtin::Discard
                        | Builtin::Release
                        | Builtin::Amplitudes
                        | Builtin::Qasm,
                    )) = typed.callees[e.index()]
                    {
                        diags.push(Diagnostic::error(
                            node.span,
                            "code implementing `std::backend::Backend` cannot perform quantum operations",
                        ));
                    }
                }
            }
            match node.kind {
                ExprKind::Block(b) | ExprKind::Unsafe(b) => stack_blocks.push(b),
                ExprKind::If {
                    cond,
                    then,
                    otherwise,
                } => {
                    stack_exprs.push(cond);
                    stack_blocks.push(then);
                    if let Some(o) = otherwise {
                        stack_exprs.push(o);
                    }
                }
                ExprKind::While { cond, body } => {
                    stack_exprs.push(cond);
                    stack_blocks.push(body);
                }
                ExprKind::For {
                    start, end, body, ..
                } => {
                    stack_exprs.push(start);
                    stack_exprs.push(end);
                    stack_blocks.push(body);
                }
                ExprKind::Match { scrutinee, arms } => {
                    stack_exprs.push(scrutinee);
                    for arm in ast.list(arms) {
                        stack_exprs.push(arm.body);
                    }
                }
                ExprKind::Call { callee, args } => {
                    stack_exprs.push(callee);
                    stack_exprs.extend_from_slice(ast.list(args));
                }
                ExprKind::MethodCall { receiver, args, .. } => {
                    stack_exprs.push(receiver);
                    stack_exprs.extend_from_slice(ast.list(args));
                }
                ExprKind::Tuple(xs) | ExprKind::Array(xs) => {
                    stack_exprs.extend_from_slice(ast.list(xs));
                }
                ExprKind::ArrayRepeat { value, .. } => stack_exprs.push(value),
                ExprKind::StructLit { fields, .. } => {
                    for f in ast.list(fields) {
                        if let Some(v) = f.value {
                            stack_exprs.push(v);
                        }
                    }
                }
                ExprKind::Unary { operand, .. }
                | ExprKind::Adj(operand)
                | ExprKind::Own { place: operand, .. }
                | ExprKind::Field { base: operand, .. }
                | ExprKind::TupleIndex { base: operand, .. } => stack_exprs.push(operand),
                ExprKind::Binary { lhs, rhs, .. }
                | ExprKind::Index {
                    base: lhs,
                    index: rhs,
                } => {
                    stack_exprs.push(lhs);
                    stack_exprs.push(rhs);
                }
                ExprKind::Str(parts) => {
                    for p in ast.list(parts) {
                        if let noev_syntax::ast::StrPart::Value { expr, .. } = *p {
                            stack_exprs.push(expr);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
