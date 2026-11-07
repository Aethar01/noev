//! What hover says about built-ins, keywords and quantum literals: the signatures and meanings
//! of `spec/core.md`, `spec/std.md` and `spec/grammar.md`, in short.

use noev_sema::Ty;
use noev_sema::prelude::{Builtin, DistMethod, FloatConst, Gate, Math, Pauli};
use noev_syntax::state::{self, Label, labels};

/// A built-in's signature and what it does.
pub fn builtin(b: Builtin) -> (String, String) {
    let (sig, doc): (&str, &str) = match b {
        Builtin::Gate(g) => return gate(g),
        Builtin::Measure => (
            "fn measure(mv self) -> Bit",
            "Measures the qubit in the Z basis and consumes it. On `[Qubit; N]`, measures every \
             qubit and gives `[Bit; N]`.\n\nNot reversible. Also a free function: `measure(mv q)`.",
        ),
        Builtin::MeasureIn => (
            "fn measure_in(mv self, basis: Basis) -> Bit",
            "Measures the qubit in `Basis::X`, `Basis::Y` or `Basis::Z` and consumes it: 0 for \
             the +1 eigenstate (|0⟩, |+⟩, |+i⟩), 1 for the −1 eigenstate. On `[Qubit; N]`, \
             measures every qubit in that basis.\n\nStandard library: a rotation into the Z \
             basis (X: `h`; Y: `adj s`, then `h`), then `measure`. Not reversible.",
        ),
        Builtin::Reset => (
            "fn reset(mut self) -> mut Self",
            "Returns the qubit to |0⟩; it stays usable. Not reversible.",
        ),
        Builtin::Discard => (
            "fn discard(mv self)",
            "Resets the qubit and releases it: the safe way to give back a qubit in an unknown \
             state.\n\nStandard library: `q.reset()`, then `unsafe { mv q.release() }`.",
        ),
        Builtin::Release => (
            "unsafe fn release(mv self)",
            "Releases the qubit without a reset, trusting that it is in |0⟩. Needs `unsafe`; a \
             simulator checks the claim (spec/ownership.md §11). Prefer `discard`.",
        ),
        Builtin::Pauli(p) => {
            let (sig, name) = match p {
                Pauli::X => ("fn X(i: Int) -> Observable", "X"),
                Pauli::Y => ("fn Y(i: Int) -> Observable", "Y"),
                Pauli::Z => ("fn Z(i: Int) -> Observable", "Z"),
            };
            return (
                sig.into(),
                format!(
                    "Pauli {name} on qubit `i` of an expectation value's preparation. Combine \
                     with `+`, `-` and `*`: `-1.0 * Z(0) * Z(1) + 0.5 * X(0)`."
                ),
            );
        }
        Builtin::Identity => (
            "fn I() -> Observable",
            "The identity observable, for constant offsets: `H + 0.25 * I()`.",
        ),
        Builtin::ProjectorZ => (
            "fn P(i: Int, b: Bit) -> Observable",
            "The projector onto |b⟩ of qubit `i`, so `<ψ| P(0, 1) |ψ>` is the probability of \
             measuring 1. A product of projectors on different qubits projects onto the joint \
             state.\n\nStandard library: `0.5 * (I() ± Z(i))`.",
        ),
        Builtin::ProjectorX => (
            "fn PX(i: Int, b: Bit) -> Observable",
            "The projector onto |+⟩ (b = 0) or |−⟩ (b = 1) of qubit `i`.\n\nStandard library: \
             `0.5 * (I() ± X(i))`.",
        ),
        Builtin::Math(m) => return math(m),
        Builtin::Convert(ty) => {
            let name = match ty {
                Ty::INT => "Int",
                Ty::FLOAT => "Float",
                Ty::BIT => "Bit",
                Ty::BOOL => "Bool",
                _ => "a type",
            };
            return (
                format!("{name}(x)"),
                format!(
                    "Converts `x` to `{name}`, only where nothing is lost (spec/types.md §4). \
                     Conversions that lose information are methods: `x.round()`, `x.trunc()`."
                ),
            );
        }
        Builtin::Const(c) => {
            return match c {
                FloatConst::Pi => ("Float::PI: Float".into(), "π = 3.14159…".into()),
                FloatConst::Tau => ("Float::TAU: Float".into(), "τ = 2π = 6.28318…".into()),
                FloatConst::E => ("Float::E: Float".into(), "e = 2.71828…".into()),
            };
        }
        Builtin::Dist(d) => match d {
            DistMethod::Shots => ("fn shots(ref self) -> Int", "How many runs it counts."),
            DistMethod::Count => ("fn count(ref self, x: T) -> Int", "How many runs gave `x`."),
            DistMethod::Probability => (
                "fn probability(ref self, x: T) -> Float",
                "The share of runs that gave `x`: `count(x) / shots()`.",
            ),
            DistMethod::MostLikely => (
                "fn most_likely(ref self) -> T",
                "The most frequent result; the smallest one on a tie.",
            ),
        },
        Builtin::Print { stderr: false } => (
            "fn print(values…)",
            "Writes its arguments to standard output, separated by spaces, then a line break. \
             Strings interpolate: `print(\"e = {e:.3}\")`.\n\nNot allowed in a run that is \
             repeated (N022).",
        ),
        Builtin::Print { stderr: true } => {
            ("fn eprint(values…)", "Like `print`, on standard error.")
        }
    };
    (sig.into(), doc.into())
}

fn gate(g: Gate) -> (String, String) {
    let (sig, doc) = match g {
        Gate::X => ("fn x(mut self) -> mut Self", "Pauli X: a bit flip."),
        Gate::Y => ("fn y(mut self) -> mut Self", "Pauli Y."),
        Gate::Z => ("fn z(mut self) -> mut Self", "Pauli Z: a phase flip."),
        Gate::H => (
            "fn h(mut self) -> mut Self",
            "Hadamard: |0⟩ ↔ |+⟩, |1⟩ ↔ |−⟩.",
        ),
        Gate::S => (
            "fn s(mut self) -> mut Self",
            "S = √Z. Its inverse is `adj q.s()`.",
        ),
        Gate::T => (
            "fn t(mut self) -> mut Self",
            "T = ⁴√Z. Its inverse is `adj q.t()`.",
        ),
        Gate::Rx => (
            "fn rx(mut self, theta: Float) -> mut Self",
            "Rotation exp(−iθX/2).",
        ),
        Gate::Ry => (
            "fn ry(mut self, theta: Float) -> mut Self",
            "Rotation exp(−iθY/2).",
        ),
        Gate::Rz => (
            "fn rz(mut self, theta: Float) -> mut Self",
            "Rotation exp(−iθZ/2).",
        ),
        Gate::P => (
            "fn p(mut self, lambda: Float) -> mut Self",
            "Phase: diag(1, e^iλ).",
        ),
        Gate::U => (
            "fn u(mut self, theta: Float, phi: Float, lambda: Float) -> mut Self",
            "OpenQASM's general single-qubit gate U(θ, φ, λ).",
        ),
        Gate::Swap => (
            "fn swap(mut self, other: mut Qubit) -> mut Self",
            "Swaps two qubits.",
        ),
    };
    let doc = format!(
        "{doc}\n\nA gate: reversible, so allowed under `adj`, in `ctrl` and in `rev fn`. \
         Returns the qubit, so gates chain (`q.h().s()`); also a free function with the qubits \
         first. Controlled versions are written with `ctrl`: `ctrl a {{ b.x() }}`."
    );
    (sig.into(), doc)
}

fn math(m: Math) -> (String, String) {
    let (sig, doc) = match m {
        Math::Sin => ("fn sin(self) -> Float", "Sine."),
        Math::Cos => ("fn cos(self) -> Float", "Cosine."),
        Math::Tan => ("fn tan(self) -> Float", "Tangent."),
        Math::Asin => ("fn asin(self) -> Float", "Arcsine."),
        Math::Acos => ("fn acos(self) -> Float", "Arccosine."),
        Math::Atan => ("fn atan(self) -> Float", "Arctangent."),
        Math::Atan2 => (
            "fn atan2(self, x: Float) -> Float",
            "The angle of the point (x, self).",
        ),
        Math::Sqrt => ("fn sqrt(self) -> Float", "Square root."),
        Math::Exp => ("fn exp(self) -> Float", "eˣ."),
        Math::Ln => ("fn ln(self) -> Float", "Natural logarithm."),
        Math::Abs => (
            "fn abs(self) -> Self",
            "Absolute value, of a float or a signed integer.",
        ),
        Math::Min => (
            "fn min(self, other: Self) -> Self",
            "The smaller of the two.",
        ),
        Math::Max => (
            "fn max(self, other: Self) -> Self",
            "The larger of the two.",
        ),
        Math::Trunc => ("fn trunc(self) -> Int", "Rounds towards zero."),
        Math::Round => (
            "fn round(self) -> Int",
            "Rounds to the nearest, halves away from zero.",
        ),
        Math::Floor => ("fn floor(self) -> Int", "Rounds down."),
        Math::Ceil => ("fn ceil(self) -> Int", "Rounds up."),
        Math::Bits => (
            "fn bits(self) -> [Bit; N]",
            "The bits of an unsigned integer, most significant first, so they print as the \
             number in binary: `u8(5).bits()` is `00000101`. `u8(bits)` reads them back.",
        ),
    };
    (
        sig.into(),
        format!("{doc} Pure, so allowed in reversible code."),
    )
}

/// The standard library's enums and their variants; `variant` is empty for the enum itself.
pub fn std_variant(name: &str, variant: &str) -> String {
    match (name, variant) {
        ("Basis", "") => "A measurement basis, for `measure_in`: the eigenbasis of a Pauli \
             operator. Standard library."
            .into(),
        ("Basis", v) => format!(
            "The {v} basis, for `measure_in`: 0 for the +1 eigenstate of {v}, 1 for the −1 \
             eigenstate."
        ),
        ("Option", "Some") => "A value that is there. `Option` is built in.".into(),
        ("Option", "None") => "No value. `Option` is built in.".into(),
        ("Result", "Ok") => "Success, with its value. `Result` is built in.".into(),
        ("Result", "Err") => "Failure, with its error. `Result` is built in.".into(),
        _ => String::new(),
    }
}

/// What a keyword means, if `word` is one hover explains.
pub fn keyword(word: &str) -> Option<(&'static str, &'static str)> {
    Some(match word {
        "mv" => (
            "mv place",
            "Gives the value away: the place can't be used afterwards (unless reassigned). \
             Qubits are always moved with `mv`, and a qubit must be moved or consumed exactly \
             once.",
        ),
        "ref" => (
            "ref place / ref T",
            "Lends the value to be read. Usually implicit: a call lends its arguments by `ref` \
             unless told otherwise.",
        ),
        "mut" => (
            "mut place / mut T / let mut",
            "Lends the value to be changed, or declares a variable whose classical data can \
             change (`let mut`). Gates on qubits lend them `mut` implicitly.",
        ),
        "ctrl" => (
            "ctrl a, b { … }",
            "Runs the block under quantum control of the qubits `a`, `b`, …: each gate inside \
             applies only on the part of the state where they are all |1⟩. The block must be \
             reversible.",
        ),
        "adj" => (
            "adj f(…)",
            "Runs a reversible call backwards: its gates inverted, in reverse order. `adj q.s()` \
             is S†.",
        ),
        "sample" => (
            "sample n { … } -> Distribution<T>",
            "Runs the block `n` times and counts its results; qubits in the result are \
             measured. Each run is separate and gets copies of the classical values it reads. \
             When the run is a circuit it is simulated once for all shots (`noev build \
             --emit=plan` says which).",
        ),
        "within" => (
            "<ψ| H |ψ> within ε",
            "Asks for a standard error of at most `ε` when the expectation value is estimated \
             (default 0.01). Exact simulators ignore it.",
        ),
        "rev" => (
            "rev fn",
            "Promises the function is reversible, so it can be used under `adj` and in `ctrl`; \
             the compiler checks it (N013). Without `rev`, reversibility is inferred.",
        ),
        "circuit" => (
            "circuit fn",
            "Promises the function is a circuit (spec/execution.md §4): until its last quantum \
             operation, measurement results are only used in conditions, logic or its result. \
             The compiler checks it (N021).",
        ),
        "unsafe" => (
            "unsafe { … } / unsafe fn",
            "Allows operations whose claims the compiler can't check, such as `release`.",
        ),
        "match" => (
            "match x { pattern => value, … }",
            "Runs the first arm whose pattern fits `x`. The arms must cover every case: every \
             variant of an enum, `true` and `false`, or a catch-all `_`. A value holding qubits \
             is taken apart with `match mv x`, and each arm must consume what it takes.",
        ),
        "enum" => (
            "enum Name { A, B(T), C { field: T } }",
            "A value that is one of several variants, taken apart with `match`. Written \
             `Name::A`; methods go inside, after the variants.",
        ),
        "assert" => (
            "assert cond",
            "Stops the program if `cond` is false, and lets the compiler use it as a fact (for \
             example that two indices differ).",
        ),
        _ => return None,
    })
}

/// A built-in type, by name.
pub fn builtin_type(name: &str) -> Option<(String, String)> {
    let doc = match name {
        "Int" => {
            "An integer of any size: it never overflows. Values that fit in 64 bits are as \
                  fast as a machine integer."
        }
        "Float" => "A 64-bit IEEE 754 float.",
        "Bool" => "`true` or `false`: logic, for conditions, `and`, `or` and `not`.",
        "Bit" => {
            "A classical bit, usually a measurement result: data, where `Bool` is logic. \
                  `0` and `1` are `Bit`s where one is expected."
        }
        "Qubit" => {
            "The right to act on one qubit. Linear: it must be consumed exactly once, by \
                    measuring, discarding, moving or returning it."
        }
        "String" => "Immutable text, copied cheaply.",
        "Observable" => {
            "A real-weighted sum of products of Pauli operators, for expectation \
                         values: `-1.0 * Z(0) * Z(1) + 0.5 * X(0)`."
        }
        "f32" | "f64" => {
            "An IEEE 754 float of that many bits. Converts to and from `Float` with \
                          `Float(x)` and `f32(x)`."
        }
        _ => {
            let width = noev_sema::prelude::builtin_type(name)?.width()?;
            let kind = if width.signed {
                "A signed"
            } else {
                "An unsigned"
            };
            let doc = format!(
                "{kind} {}-bit integer: {} to {}. Overflow is a runtime error. Convert with \
                 `{name}(x)` when every value fits, and `{name}::try_from(x)` otherwise.",
                width.bits,
                width.min(),
                width.max()
            );
            return Some((name.to_string(), doc));
        }
    };
    Some((name.to_string(), doc.to_string()))
}

/// `T::try_from(x)`, for a sized integer type `T`.
pub fn try_convert(ty: &str) -> (String, String) {
    let ty = ty.trim_matches('`');
    (
        format!("fn {ty}::try_from(x) -> Result<{ty}, String>"),
        format!(
            "Converts any integer to `{ty}`: `Ok` when it is in range, otherwise `Err` saying \
             why. Use `{ty}(x)` when every value fits."
        ),
    )
}

/// What a ket literal such as `|0+1>` prepares.
pub fn ket(text: &str) -> (String, String) {
    let body = text.trim_start_matches('|').trim_end_matches('>');
    let labels: Vec<Label> = labels(body).collect();
    let states: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            if labels.len() == 1 {
                label.ket().to_string()
            } else {
                format!("`[{i}]` {}", label.ket())
            }
        })
        .collect();
    let (ty, what) = if labels.len() == 1 {
        ("Qubit".to_string(), "A fresh qubit".to_string())
    } else {
        (
            format!("[Qubit; {}]", labels.len()),
            format!("{} fresh qubits", labels.len()),
        )
    };
    (
        format!("{text}: {ty}"),
        format!(
            "{what}: {}. Linear: each must be measured, discarded or returned exactly once.",
            states.join(", ")
        ),
    )
}

/// What a bra such as `<1|` does.
pub fn bra(text: &str) -> (String, String) {
    let body = text.trim_start_matches('<').trim_end_matches('|');
    let bases: Vec<&str> = labels(body)
        .map(|label| match label.basis() {
            state::Pauli::X => "X",
            state::Pauli::Y => "Y",
            state::Pauli::Z => "Z",
        })
        .collect();
    // `|0+i⟩`, as physics writes it.
    let physics: String = labels(body)
        .map(|l| l.ket().trim_start_matches('|').trim_end_matches('⟩'))
        .collect();
    let basis = if bases.iter().all(|&b| b == bases[0]) {
        format!("the {} basis", bases[0])
    } else {
        format!("the {} bases, one per qubit", bases.join(", "))
    };
    (
        format!("{text} q"),
        format!(
            "A measurement test: measures `q`, consuming it, in {basis}, and is `true` if the \
             outcome is |{physics}⟩. Followed by an observable and a ket, it starts an expectation \
             value instead: `{text} H |{body}>`."
        ),
    )
}
