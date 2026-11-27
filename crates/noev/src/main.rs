//! The `noev` command-line interface.

mod lsp;
mod report;

use noev::package;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand, ValueEnum};

const NOTE: anstyle::Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);
const ERROR: anstyle::Style = AnsiColor::Red.on_default().effects(Effects::BOLD);

/// Help colours, matching Cargo's.
const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Green.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Cyan.on_default())
    .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
    .valid(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD));

/// The Noev compiler: one language, two computing paradigms.
#[derive(Parser)]
#[command(name = "noev", version, styles = STYLES, propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check a program for type, ownership, and target-capability errors without building it
    Check {
        /// The `.nv` file or package directory to check
        #[arg(default_value = ".")]
        file: PathBuf,

        /// Backend target (`ketrs`, `export`, a `.toml`/plug-in path, or cached `NAME[:DEVICE]`)
        #[arg(long, value_name = "NAME[:DEVICE]")]
        backend: Option<String>,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Compile a program
    Build {
        /// The `.nv` file or package directory to build
        #[arg(default_value = ".")]
        file: PathBuf,

        /// What to produce
        #[arg(long, value_enum, default_value_t = Emit::Binary)]
        emit: Emit,

        /// Native code generator to use for `binary`, `object`, and `llvm-ir`
        #[arg(long, value_enum)]
        codegen: Option<CodegenChoice>,

        #[command(flatten)]
        opt: OptOptions,

        /// Emit QIR runtime calls (`__quantum__qis__*`, `__quantum__rt__*`) instead of Noev's
        /// runtime C ABI (LLVM code generator only)
        #[arg(long)]
        qir_runtime: bool,

        /// Extra C objects, static libraries, or library names to link into the binary
        #[arg(short = 'l', long = "link", value_name = "LIB")]
        link: Vec<String>,

        /// Backend target to check capabilities against (`ketrs`, `export`, `.toml`/plug-in, or cached `NAME[:DEVICE]`)
        #[arg(long, value_name = "NAME[:DEVICE]")]
        backend: Option<String>,

        /// Where to write the output (default: file stem for binary/object, stdout for ir/plan/qir/llvm-ir)
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Run a program, with its quantum parts on a backend (the ketrs simulator unless chosen)
    Run {
        /// The `.nv` file or package directory to run
        #[arg(default_value = ".")]
        file: PathBuf,

        /// Seed for the simulator's random numbers, to reproduce a run
        #[arg(long, value_name = "N")]
        seed: Option<u64>,

        /// Backend to run on (`ketrs`, `export`, a plug-in `.so`/`.dylib` path, or cached `NAME[:DEVICE]`)
        #[arg(long, value_name = "NAME[:DEVICE]")]
        backend: Option<String>,

        /// Overall optimization level (`0` = none, `1` = light, `2` = default, `3` = aggressive)
        #[arg(short = 'O', long = "opt-level", value_name = "0..3", value_parser = clap::value_parser!(u8).range(0..=3), default_value_t = 2)]
        opt_level: u8,

        /// Override quantum circuit optimization level independently of `-O`
        #[arg(long = "circuit-opt", value_name = "0..3", value_parser = clap::value_parser!(u8).range(0..=3), help_heading = "Optimization")]
        circuit_opt: Option<u8>,

        /// Save every circuit the run sends to the backend as OpenQASM 3, one file per job,
        /// in this directory
        #[arg(long, value_name = "DIR")]
        circuits: Option<PathBuf>,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Run `test fn` tests in a file, package, or directory (`spec/testing.md`)
    Test {
        /// The `.nv` file, package directory, or directory of `.nv` files to test
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Only run tests whose name contains this substring
        filter: Option<String>,

        /// List matching tests without running them
        #[arg(long)]
        list: bool,

        /// Seed for the simulator's random numbers, to reproduce a run
        #[arg(long, value_name = "N")]
        seed: Option<u64>,

        /// Backend to run tests on (`ketrs`, a plug-in path, or cached `NAME[:DEVICE]`)
        #[arg(long, value_name = "NAME[:DEVICE]")]
        backend: Option<String>,

        /// Compile tests to a native binary instead of running in the interpreter
        #[arg(long)]
        compile: bool,

        /// Native code generator to use (`cranelift` or `llvm`; implies `--compile`)
        #[arg(long, value_enum)]
        codegen: Option<CodegenChoice>,

        #[command(flatten)]
        opt: OptOptions,

        /// Override quantum circuit optimization level independently of `-O`
        #[arg(long = "circuit-opt", value_name = "0..3", value_parser = clap::value_parser!(u8).range(0..=3), help_heading = "Optimization")]
        circuit_opt: Option<u8>,

        /// Print test standard output as it runs instead of hiding output for passing tests
        #[arg(long)]
        nocapture: bool,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Manage cached backend target descriptions (`spec/runtime.md` §4)
    Target {
        #[command(subcommand)]
        command: TargetCommand,
    },

    /// Start the language server, for editors (speaks LSP on standard input and output)
    Lsp,

    /// Save a program's circuits without running anything quantum (the same as
    /// `run --backend=export`)
    Export {
        /// The `.nv` file or package directory
        #[arg(default_value = ".")]
        file: PathBuf,

        /// A directory for the circuits, one file each (default: all on standard output)
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,

        /// The circuit format
        #[arg(long, value_enum, default_value_t = ExportFormat::Qasm3)]
        format: ExportFormat,

        #[command(flatten)]
        messages: MessageOptions,
    },
}

#[derive(Subcommand)]
enum TargetCommand {
    /// Fetch and cache a backend target description (from a built-in, `.toml`, or plug-in library)
    Fetch {
        /// Target name (`NAME` or `NAME:DEVICE`), or path to a `.toml`/plug-in
        target: String,

        /// Path to a target `.toml` or plug-in library (`.so`/`.dylib`) to fetch from
        #[arg(long, value_name = "PATH")]
        from: Option<PathBuf>,
    },

    /// List built-in and locally cached backend targets
    List,

    /// Show a target's TOML description
    Show {
        /// Target name (`NAME` or `NAME:DEVICE`), built-in, or path
        target: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ExportFormat {
    /// OpenQASM 3
    Qasm3,
    /// QIR 1.0 (LLVM IR text, `.ll`)
    Qir,
}

#[derive(clap::Args)]
struct MessageOptions {
    /// How to print errors and warnings
    #[arg(long, value_enum, default_value_t = MessageFormat::Human)]
    message_format: MessageFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum MessageFormat {
    /// Source excerpts with labels and help
    Human,
    /// One line per diagnostic: `FILE:LINE:COLUMN: error[CODE]: message`
    Short,
}

#[derive(Clone, clap::Args)]
struct OptOptions {
    /// Overall optimization level (`0` = none, `1` = light, `2` = default, `3` = aggressive)
    #[arg(
        short = 'O',
        long = "opt-level",
        value_name = "0..3",
        value_parser = clap::value_parser!(u8).range(0..=3),
        default_value_t = 2,
        help_heading = "Optimization"
    )]
    opt_level: u8,

    /// Override Cranelift `opt_level` independently of `-O`
    #[arg(long = "cranelift-opt", value_enum, help_heading = "Optimization")]
    cranelift_opt: Option<CraneliftOptChoice>,

    /// Override LLVM optimization level independently of `-O`
    #[arg(long = "llvm-opt", value_enum, help_heading = "Optimization")]
    llvm_opt: Option<LlvmOptChoice>,

    /// Target CPU for native code generation (e.g. `native`, `generic`, `znver4`, `apple-m3`)
    #[arg(
        long = "target-cpu",
        value_name = "CPU",
        help_heading = "Codegen Options"
    )]
    target_cpu: Option<String>,

    /// Comma-separated target feature flags (e.g. `+avx2,+fma`)
    #[arg(
        long = "target-feature",
        value_name = "FEATURES",
        help_heading = "Codegen Options"
    )]
    target_feature: Option<String>,

    /// Forward a native `KEY=VALUE` setting to Cranelift (repeatable, e.g. `enable_simd=true`)
    #[arg(
        long = "cranelift-arg",
        value_name = "KEY=VALUE",
        value_parser = parse_key_val,
        help_heading = "Codegen Options"
    )]
    cranelift_args: Vec<(String, String)>,

    /// Custom LLVM new-pass-manager pipeline (overrides `default<O*>`)
    #[arg(
        long = "llvm-passes",
        value_name = "PIPELINE",
        help_heading = "Codegen Options"
    )]
    llvm_passes: Option<String>,

    /// Forward a native CLI flag to LLVM (repeatable)
    #[arg(
        long = "llvm-arg",
        value_name = "ARG",
        allow_hyphen_values = true,
        help_heading = "Codegen Options"
    )]
    llvm_args: Vec<String>,
}

fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let Some((k, v)) = s.split_once('=') else {
        return Err(format!("expected `KEY=VALUE`, got `{s}`"));
    };
    if k.is_empty() {
        return Err(format!("empty setting key in `{s}`"));
    }
    Ok((k.to_string(), v.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CraneliftOptChoice {
    /// `opt_level = "none"`
    None,
    /// `opt_level = "speed"`
    Speed,
    /// `opt_level = "speed_and_size"`
    SpeedAndSize,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LlvmOptChoice {
    /// `-O0`
    #[value(name = "0")]
    O0,
    /// `-O1`
    #[value(name = "1")]
    O1,
    /// `-O2`
    #[value(name = "2")]
    O2,
    /// `-O3`
    #[value(name = "3")]
    O3,
    /// `-Os` (optimize for size)
    #[value(name = "s")]
    Os,
    /// `-Oz` (optimize aggressively for size)
    #[value(name = "z")]
    Oz,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CodegenChoice {
    /// Pure-Rust Cranelift code generator (fast compile times, default)
    Cranelift,
    /// LLVM code generator (optimised native code, optional QIR runtime calls)
    Llvm,
}

#[derive(Clone, Copy, ValueEnum)]
enum Emit {
    /// Noev's intermediate representation, for inspecting the compiler
    Ir,
    /// How each `sample` run and quantum function runs when repeated: sampled from one
    /// simulation, or shot by shot and why
    Plan,
    /// QIR 1.0 (LLVM IR text, `.ll`): base profile for circuits, adaptive profile for
    /// classical control
    Qir,
    /// Full-program LLVM IR text (`.ll`) produced by the LLVM code generator
    LlvmIr,
    /// A native object file (`.o`)
    Object,
    /// A standalone native executable linked with the runtime
    Binary,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check {
            file,
            backend,
            messages,
        } => match front_end(
            &file,
            backend.as_deref(),
            TargetCheck::Check,
            messages.message_format,
        ) {
            Ok(_) => {
                if matches!(messages.message_format, MessageFormat::Human) {
                    anstream::eprintln!(
                        "{NOTE}note:{NOTE:#} syntax, types and ownership were checked"
                    );
                }
                ExitCode::SUCCESS
            }
            Err(code) => code,
        },
        Command::Build {
            file,
            emit,
            codegen,
            opt,
            qir_runtime,
            link,
            backend,
            output,
            messages,
        } => build(
            &file,
            emit,
            codegen,
            &opt,
            qir_runtime,
            &link,
            backend.as_deref(),
            output.as_deref(),
            messages.message_format,
        ),
        Command::Run {
            file,
            seed,
            backend,
            opt_level,
            circuit_opt,
            circuits,
            messages,
        } => run(
            &file,
            seed,
            backend.as_deref(),
            circuit_opt.unwrap_or(opt_level),
            circuits.as_deref(),
            messages.message_format,
        ),
        Command::Test {
            path,
            filter,
            list,
            seed,
            backend,
            compile,
            codegen,
            opt,
            circuit_opt,
            nocapture,
            messages,
        } => run_tests(
            &path,
            filter.as_deref(),
            list,
            seed,
            backend.as_deref(),
            compile || codegen.is_some(),
            codegen,
            &opt,
            circuit_opt.unwrap_or(opt.opt_level),
            nocapture,
            messages.message_format,
        ),
        Command::Target { command } => target_cmd(command),
        Command::Lsp => match lsp::serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                anstream::eprintln!("noev lsp: {error}");
                ExitCode::from(1)
            }
        },
        Command::Export {
            file,
            output,
            format: export_format,
            messages,
        } => match front_end(&file, None, TargetCheck::None, messages.message_format) {
            Ok(checked) => export(
                &file,
                &checked,
                output.as_deref(),
                export_format,
                messages.message_format,
            ),
            Err(code) => code,
        },
    }
}

/// How `front_end` applies target-capability checking.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetCheck {
    /// Check capabilities against the chosen backend (or `ketrs` by default).
    Check,
    /// Check capabilities unless the backend is `export` (where `run --backend=export`
    /// behaves as `noev export` and stops gracefully at unknown results).
    Run,
    /// Skip target-capability checking (used by `noev export`).
    None,
}

/// A program that passed every check.
struct Checked {
    sources: noev_syntax::SourceMap,
    ast: noev_syntax::Ast,
    typed: noev_sema::Typed,
    circuits: noev_sema::Circuits,
    backend: Option<String>,
}

/// Reads and checks the `.nv` file or package at `file`, printing its diagnostics. Fails with
/// exit status 1 when there are errors, and 2 when nothing could be checked.
fn front_end(
    file: &Path,
    backend_arg: Option<&str>,
    target_check: TargetCheck,
    format: MessageFormat,
) -> Result<Checked, ExitCode> {
    let loaded = match package::load(file) {
        Ok(loaded) => loaded,
        Err(error) => {
            anstream::eprintln!("noev: {error}");
            return Err(ExitCode::from(2));
        }
    };
    front_end_loaded(loaded, backend_arg, target_check, format)
}

fn front_end_loaded(
    loaded: package::Loaded,
    backend_arg: Option<&str>,
    target_check: TargetCheck,
    format: MessageFormat,
) -> Result<Checked, ExitCode> {
    let package::Loaded {
        sources,
        ast,
        mut diagnostics,
        default_backend,
    } = loaded;
    let effective_backend = backend_arg.map(str::to_string).or(default_backend);
    let target_spec = match target_check {
        TargetCheck::None => None,
        TargetCheck::Run if effective_backend.as_deref() == Some("export") => None,
        TargetCheck::Check | TargetCheck::Run => {
            let spec = effective_backend.as_deref().unwrap_or("ketrs");
            match noev_runtime::plugin::resolve_target_spec(spec) {
                Ok(target) => Some(target),
                Err(error) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
                    return Err(ExitCode::from(2));
                }
            }
        }
    };
    let (typed, type_errors) = noev_sema::check(&ast);
    // Ownership is only meaningful for a well-typed program, and circuits for one that
    // passes ownership checking.
    let mut circuits = noev_sema::Circuits::default();
    let errors = |d: &[noev_syntax::Diagnostic]| {
        d.iter().any(|d| d.severity == noev_syntax::Severity::Error)
    };
    if !errors(&diagnostics) && !errors(&type_errors) {
        diagnostics.extend(noev_sema::check_ownership(&ast, &typed));
        if !errors(&diagnostics) {
            let (found, warnings) = noev_sema::check_circuits(&ast, &typed);
            circuits = found;
            diagnostics.extend(warnings);
            if let Some(target) = &target_spec {
                diagnostics.extend(noev_sema::check_target(&ast, &typed, &circuits, target));
            }
        }
    }
    diagnostics.extend(type_errors);
    diagnostics.sort_by_key(|d| d.span.start);
    report::print(&sources, &diagnostics, format);
    let failed = diagnostics
        .iter()
        .any(|d| d.severity == noev_syntax::Severity::Error);
    if failed {
        return Err(ExitCode::from(1));
    }
    Ok(Checked {
        sources,
        ast,
        typed,
        circuits,
        backend: effective_backend,
    })
}

/// Handles `noev target fetch | list | show` (`spec/runtime.md` §4).
fn target_cmd(command: TargetCommand) -> ExitCode {
    match command {
        TargetCommand::Fetch { target, from } => {
            let (base, device) = noev_runtime::plugin::split_backend_spec(&target);
            let spec_res = match from {
                Some(path) => if path.extension().is_some_and(|e| e == "toml") {
                    std::fs::read_to_string(&path)
                        .map_err(|e| format!("cannot read `{}`: {e}", path.display()))
                        .and_then(|t| noev_sema::TargetSpec::from_toml(&t))
                } else {
                    noev_runtime::plugin::PluginBackend::probe_target(&path, device)
                        .map_err(|e| e.0)
                }
                .map(|mut spec| {
                    spec.name = base.to_string();
                    if let Some(dev) = device {
                        spec.device = Some(dev.to_string());
                    }
                    spec
                }),
                None => noev_runtime::plugin::resolve_target_spec(&target),
            };
            let spec = match spec_res {
                Ok(s) => s,
                Err(error) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
                    return ExitCode::from(1);
                }
            };
            let key = match &spec.device {
                Some(dev) => format!("{}:{dev}", spec.name),
                None => spec.name.clone(),
            };
            match noev_runtime::plugin::write_cached_target(&key, &spec) {
                Ok(path) => {
                    anstream::eprintln!(
                        "{NOTE}note:{NOTE:#} cached target `{key}` at {}",
                        path.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
                    ExitCode::from(2)
                }
            }
        }
        TargetCommand::List => {
            println!("ketrs          (built-in, level=both, 30 qubits, FP64 statevector)");
            for spec_name in noev_sema::TargetSpec::builtin_ketrs_devices() {
                if let Some(t) = noev_sema::TargetSpec::builtin(spec_name) {
                    let qubits = t
                        .qubits
                        .map_or_else(|| "unbounded".to_string(), |q| format!("{q} qubits"));
                    println!(
                        "{spec_name:<14} (built-in, level={}, {qubits})",
                        t.level.as_str()
                    );
                }
            }
            println!("export         (built-in, level=circuit, unbounded)");
            match noev_runtime::plugin::list_cached_targets() {
                Ok(entries) => {
                    for (key, target) in entries {
                        let qubits = target
                            .qubits
                            .map_or_else(|| "unbounded".to_string(), |q| format!("{q} qubits"));
                        println!("{key}  (cached, level={}, {qubits})", target.level.as_str());
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
                    ExitCode::from(1)
                }
            }
        }
        TargetCommand::Show { target } => {
            match noev_runtime::plugin::resolve_target_spec(&target) {
                Ok(spec) => {
                    print!("{}", spec.to_toml());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
                    ExitCode::from(1)
                }
            }
        }
    }
}

/// Compiles `file` and writes the requested output to `output`, or to stdout (or a default file
/// name for `--emit=object` and `--emit=binary`).
#[allow(clippy::too_many_arguments)]
fn build(
    file: &Path,
    emit: Emit,
    codegen: Option<CodegenChoice>,
    opt: &OptOptions,
    qir_runtime: bool,
    link: &[String],
    backend: Option<&str>,
    output: Option<&Path>,
    format: MessageFormat,
) -> ExitCode {
    let checked = match front_end(file, backend, TargetCheck::Check, format) {
        Ok(checked) => checked,
        Err(code) => return code,
    };
    let program = noev_ir::lower(
        &checked.ast,
        &checked.typed,
        &checked.circuits,
        &checked.sources,
    );
    match emit {
        Emit::Ir | Emit::Plan => {
            let text = match emit {
                Emit::Ir => program.to_string(),
                Emit::Plan => plan(&checked),
                _ => unreachable!(),
            };
            match output {
                Some(path) => match std::fs::write(path, text) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        anstream::eprintln!("noev: cannot write {}: {error}", path.display());
                        ExitCode::from(2)
                    }
                },
                None => {
                    print!("{text}");
                    ExitCode::SUCCESS
                }
            }
        }
        Emit::Qir => build_qir(file, &checked, &program, output, format),
        Emit::LlvmIr => build_llvm_ir(&program, opt, qir_runtime, output),
        Emit::Object => build_object(file, &program, codegen, opt, qir_runtime, output),
        Emit::Binary => build_binary(file, &program, codegen, opt, qir_runtime, link, output),
    }
}

/// Records `program`'s circuits without a simulator and writes one QIR 1.0 LLVM IR module to
/// `output`, or to standard output.
fn build_qir(
    file: &Path,
    checked: &Checked,
    program: &noev_ir::Program,
    output: Option<&Path>,
    format: MessageFormat,
) -> ExitCode {
    let exporter = noev_runtime::backend::Exporter::default();
    let sink = noev_runtime::Output {
        stdout: Box::new(std::io::sink()),
        stderr: Box::new(std::io::sink()),
    };
    let result = noev_runtime::run_main(program, &exporter, 0, sink);
    let jobs = exporter.jobs.borrow();
    if jobs
        .iter()
        .any(|(c, _)| noev_runtime::qir::has_inline_qasm(c))
    {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} inline OpenQASM (`qasm(\"…\")`) cannot be exported to QIR; use `noev export` for OpenQASM 3"
        );
        return ExitCode::from(1);
    }
    let header = [format!("Generated from {} by noev", file.display())];
    let text = noev_runtime::qir::to_qir_module(&jobs, &header);
    let write_res = match output {
        Some(path) => std::fs::write(path, text).map_err(|error| {
            anstream::eprintln!("noev: cannot write {}: {error}", path.display());
            ExitCode::from(2)
        }),
        None => {
            print!("{text}");
            Ok(())
        }
    };
    if let Err(code) = write_res {
        return code;
    }
    let human = matches!(format, MessageFormat::Human);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.unknown => {
            if human {
                anstream::eprintln!(
                    "{NOTE}note:{NOTE:#} stopped where {}, which needs a simulator or quantum \
                     computer; any circuits after that need `noev run --circuits`",
                    error.message
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            report_runtime_error(checked, error, format);
            ExitCode::from(1)
        }
    }
}

/// Default output filename when `-o` is omitted for `--emit=object` or `--emit=binary`.
#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn default_artifact_path(file: &Path, ext: Option<&str>) -> PathBuf {
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && *s != ".")
        .unwrap_or("a");
    let ext = ext.or(if cfg!(windows) { Some("exe") } else { None });
    match ext {
        Some(e) => PathBuf::from(format!("{stem}.{e}")),
        None => PathBuf::from(stem),
    }
}

/// Locates `libnoev_runtime.a` for linking standalone native binaries.
#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn find_runtime_lib() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("NOEV_RUNTIME_LIB") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let lib_name = if cfg!(windows) {
        "noev_runtime.lib"
    } else {
        "libnoev_runtime.a"
    };
    [
        dir.join(lib_name),
        dir.parent().map(|p| p.join(lib_name)).unwrap_or_default(),
        dir.parent()
            .map(|p| p.join("lib").join(lib_name))
            .unwrap_or_default(),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn select_codegen(
    choice: Option<CodegenChoice>,
    qir_runtime: bool,
) -> Result<&'static dyn noev_codegen::CodeGenerator, ExitCode> {
    let chosen = choice.unwrap_or(if qir_runtime {
        CodegenChoice::Llvm
    } else if cfg!(feature = "cranelift") {
        CodegenChoice::Cranelift
    } else {
        CodegenChoice::Llvm
    });
    match chosen {
        CodegenChoice::Cranelift => {
            #[cfg(feature = "cranelift")]
            {
                Ok(&noev_codegen::CraneliftCodegen)
            }
            #[cfg(not(feature = "cranelift"))]
            {
                anstream::eprintln!(
                    "{ERROR}error:{ERROR:#} `noev` was built without the `cranelift` feature"
                );
                Err(ExitCode::from(2))
            }
        }
        CodegenChoice::Llvm => {
            #[cfg(feature = "llvm")]
            {
                Ok(&noev_codegen::llvm::LlvmCodegen)
            }
            #[cfg(not(feature = "llvm"))]
            {
                anstream::eprintln!(
                    "{ERROR}error:{ERROR:#} `noev` was built without the `llvm` feature"
                );
                Err(ExitCode::from(2))
            }
        }
    }
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn codegen_options(
    opt: &OptOptions,
    qir_runtime: bool,
    test_harness: bool,
) -> noev_codegen::CodegenOptions {
    noev_codegen::CodegenOptions {
        opt: noev_codegen::OptLevel::from_u8(opt.opt_level).unwrap_or_default(),
        cranelift_opt: opt.cranelift_opt.map(|c| match c {
            CraneliftOptChoice::None => noev_codegen::CraneliftOpt::None,
            CraneliftOptChoice::Speed => noev_codegen::CraneliftOpt::Speed,
            CraneliftOptChoice::SpeedAndSize => noev_codegen::CraneliftOpt::SpeedAndSize,
        }),
        llvm_opt: opt.llvm_opt.map(|l| match l {
            LlvmOptChoice::O0 => noev_codegen::LlvmOpt::O0,
            LlvmOptChoice::O1 => noev_codegen::LlvmOpt::O1,
            LlvmOptChoice::O2 => noev_codegen::LlvmOpt::O2,
            LlvmOptChoice::O3 => noev_codegen::LlvmOpt::O3,
            LlvmOptChoice::Os => noev_codegen::LlvmOpt::Os,
            LlvmOptChoice::Oz => noev_codegen::LlvmOpt::Oz,
        }),
        target_cpu: opt.target_cpu.clone(),
        target_features: opt.target_feature.clone(),
        cranelift_args: opt.cranelift_args.clone(),
        llvm_passes: opt.llvm_passes.clone(),
        llvm_args: opt.llvm_args.clone(),
        runtime_abi: if qir_runtime {
            noev_codegen::RuntimeAbi::Qir
        } else {
            noev_codegen::RuntimeAbi::Noev
        },
        test_harness,
    }
}

#[cfg(feature = "llvm")]
fn build_llvm_ir(
    program: &noev_ir::Program,
    opt: &OptOptions,
    qir_runtime: bool,
    output: Option<&Path>,
) -> ExitCode {
    let opts = codegen_options(opt, qir_runtime, false);
    match noev_codegen::llvm::LlvmCodegen.emit_llvm_ir(program, &opts) {
        Ok(ir) => match output {
            Some(path) => match std::fs::write(path, ir) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    anstream::eprintln!("noev: cannot write {}: {error}", path.display());
                    ExitCode::from(2)
                }
            },
            None => {
                print!("{ir}");
                ExitCode::SUCCESS
            }
        },
        Err(error) => {
            anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(feature = "llvm"))]
fn build_llvm_ir(
    _program: &noev_ir::Program,
    _opt: &OptOptions,
    _qir_runtime: bool,
    _output: Option<&Path>,
) -> ExitCode {
    anstream::eprintln!("{ERROR}error:{ERROR:#} `noev` was built without the `llvm` feature");
    ExitCode::from(2)
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn build_object(
    file: &Path,
    program: &noev_ir::Program,
    codegen: Option<CodegenChoice>,
    opt: &OptOptions,
    qir_runtime: bool,
    output: Option<&Path>,
) -> ExitCode {
    let generator = match select_codegen(codegen, qir_runtime) {
        Ok(g) => g,
        Err(code) => return code,
    };
    let dest = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_artifact_path(file, Some("o")));
    let opts = codegen_options(opt, qir_runtime, false);
    match generator.emit_object(program, &opts) {
        Ok(bytes) => match std::fs::write(&dest, bytes) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                anstream::eprintln!("noev: cannot write {}: {error}", dest.display());
                ExitCode::from(2)
            }
        },
        Err(error) => {
            anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(any(feature = "cranelift", feature = "llvm")))]
fn build_object(
    _file: &Path,
    _program: &noev_ir::Program,
    _codegen: Option<CodegenChoice>,
    _opt: &OptOptions,
    _qir_runtime: bool,
    _output: Option<&Path>,
) -> ExitCode {
    anstream::eprintln!(
        "{ERROR}error:{ERROR:#} `noev` was built without a native code generator (`cranelift` or `llvm`)"
    );
    ExitCode::from(2)
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn build_binary(
    file: &Path,
    program: &noev_ir::Program,
    codegen: Option<CodegenChoice>,
    opt: &OptOptions,
    qir_runtime: bool,
    link: &[String],
    output: Option<&Path>,
) -> ExitCode {
    if program.main.is_none() {
        anstream::eprintln!("{ERROR}error:{ERROR:#} no `main` function to build into a binary");
        return ExitCode::from(1);
    }
    if qir_runtime {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} `--qir-runtime` links against an external QIR runtime: \
             use `--emit=object` or `--emit=llvm-ir` and link with your QIR runtime library"
        );
        return ExitCode::from(2);
    }
    let generator = match select_codegen(codegen, qir_runtime) {
        Ok(g) => g,
        Err(code) => return code,
    };
    let Some(runtime_lib) = find_runtime_lib() else {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} cannot find `libnoev_runtime.a` (set `NOEV_RUNTIME_LIB` or build `noev-runtime`)"
        );
        return ExitCode::from(2);
    };
    let dest = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_artifact_path(file, None));
    let opts = codegen_options(opt, qir_runtime, false);
    let res = if link.is_empty() {
        generator.emit_binary(program, &dest, &runtime_lib, &opts)
    } else {
        generator
            .emit_object(program, &opts)
            .and_then(|obj| noev_codegen::link_object_with_args(&obj, &dest, &runtime_lib, link))
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            anstream::eprintln!("{ERROR}error:{ERROR:#} {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(any(feature = "cranelift", feature = "llvm")))]
fn build_binary(
    _file: &Path,
    _program: &noev_ir::Program,
    _codegen: Option<CodegenChoice>,
    _opt: &OptOptions,
    _qir_runtime: bool,
    _link: &[String],
    _output: Option<&Path>,
) -> ExitCode {
    anstream::eprintln!(
        "{ERROR}error:{ERROR:#} `noev` was built without a native code generator (`cranelift` or `llvm`)"
    );
    ExitCode::from(2)
}

/// The simulator `noev run` uses for `spec` (`"ketrs"` or `"ketrs:<device>"`), configured
/// with circuit optimization level `opt_level`, if this build has `ketrs`.
fn simulator(spec: &str, opt_level: u8) -> Result<Box<dyn noev_runtime::Backend>, String> {
    #[cfg(feature = "ketrs")]
    {
        noev_runtime::KetrsBackend::from_spec(spec, opt_level)
            .map(|b| Box::new(b) as Box<dyn noev_runtime::Backend>)
            .ok_or_else(|| {
                format!(
                    "unknown ketrs device `{spec}`; valid built-ins are `ketrs`, `ketrs:sv32`, \
                     `ketrs:mps`, `ketrs:pmps`, `ketrs:density`, `ketrs:gpu`, `ketrs:gpu32`"
                )
            })
    }
    #[cfg(not(feature = "ketrs"))]
    {
        let _ = (spec, opt_level);
        Err(
            "this noev was built without a simulator (its `ketrs` feature); \
             `--backend=export` saves the program's circuits instead"
                .into(),
        )
    }
}

/// `noev export` and `run --backend=export`: runs the program's classical code with every
/// circuit recorded instead of run, so nothing is simulated, then writes the circuits.
fn export(
    file: &Path,
    checked: &Checked,
    output: Option<&Path>,
    export_format: ExportFormat,
    format: MessageFormat,
) -> ExitCode {
    let program = &noev_ir::lower(
        &checked.ast,
        &checked.typed,
        &checked.circuits,
        &checked.sources,
    );
    let exporter = noev_runtime::backend::Exporter::default();
    // What the program prints depends on results that are not known, so it is dropped.
    let sink = noev_runtime::Output {
        stdout: Box::new(std::io::sink()),
        stderr: Box::new(std::io::sink()),
    };
    let result = noev_runtime::run_main(program, &exporter, 0, sink);
    let jobs = exporter.jobs.borrow();
    let saved = match output {
        Some(dir) => save_circuits(file, dir, &jobs, export_format, format),
        None => {
            for (i, (circuit, shots)) in jobs.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                let header = circuit_header(file, i, *shots);
                let text = match export_format {
                    ExportFormat::Qasm3 => noev_runtime::qasm::to_qasm3(circuit, &header),
                    ExportFormat::Qir => {
                        noev_runtime::qir::to_qir(circuit, &format!("circuit_{}", i + 1), &header)
                    }
                };
                print!("{text}");
            }
            Ok(())
        }
    };
    if let Err(code) = saved {
        return code;
    }
    let human = matches!(format, MessageFormat::Human);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // Usually the program printing what it sampled. Circuits after this point would need
        // these results, so the export ends here, which is not a failure.
        Err(error) if error.unknown => {
            if human {
                anstream::eprintln!(
                    "{NOTE}note:{NOTE:#} stopped where {}, which needs a simulator or quantum \
                     computer; any circuits after that need `noev run --circuits`",
                    error.message
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            report_runtime_error(checked, error, format);
            ExitCode::from(1)
        }
    }
}

fn circuit_header(file: &Path, index: usize, shots: usize) -> [String; 2] {
    [
        format!(
            "Circuit {} of {}, generated by noev",
            index + 1,
            file.display()
        ),
        format!("shots: {shots}"),
    ]
}

fn report_runtime_error(
    checked: &Checked,
    error: noev_runtime::RuntimeError,
    format: MessageFormat,
) {
    match error.span {
        Some(span) => {
            let diagnostic = noev_syntax::Diagnostic::error(span, error.message);
            report::print(&checked.sources, &[diagnostic], format);
        }
        None => anstream::eprintln!("{ERROR}error:{ERROR:#} {}", error.message),
    }
}

/// Runs `file`'s `main` once. Standard output is only what the program prints.
fn run(
    file: &Path,
    seed: Option<u64>,
    backend: Option<&str>,
    circuit_opt: u8,
    circuits: Option<&Path>,
    format: MessageFormat,
) -> ExitCode {
    use std::io::{IsTerminal, Write};
    let checked = match front_end(file, backend, TargetCheck::Run, format) {
        Ok(checked) => checked,
        Err(code) => return code,
    };
    let chosen = checked.backend.as_deref().unwrap_or("ketrs");
    if chosen == "export" {
        return export(file, &checked, circuits, ExportFormat::Qasm3, format);
    }
    let program = noev_ir::lower(
        &checked.ast,
        &checked.typed,
        &checked.circuits,
        &checked.sources,
    );
    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    // Line by line on a terminal; in large blocks into a file or pipe (spec/std.md §4).
    let stdout: Box<dyn Write> = if std::io::stdout().is_terminal() {
        Box::new(std::io::stdout())
    } else {
        Box::new(std::io::BufWriter::with_capacity(
            64 * 1024,
            std::io::stdout(),
        ))
    };
    let output = noev_runtime::Output {
        stdout,
        stderr: Box::new(std::io::stderr()),
    };
    let sim_holder: Option<Box<dyn noev_runtime::Backend>>;
    let plugin_holder: Option<noev_runtime::plugin::PluginBackend>;
    let chosen_backend: &dyn noev_runtime::Backend =
        if chosen == "ketrs" || chosen.starts_with("ketrs:") {
            match simulator(chosen, circuit_opt) {
                Ok(sim) => {
                    sim_holder = Some(sim);
                    sim_holder.as_deref().expect("set")
                }
                Err(msg) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {msg}");
                    return ExitCode::from(2);
                }
            }
        } else {
            match noev_runtime::plugin::load_plugin_spec(chosen) {
                Ok(plugin) => {
                    plugin_holder = Some(plugin);
                    plugin_holder.as_ref().expect("set")
                }
                Err(error) => {
                    anstream::eprintln!("{ERROR}error:{ERROR:#} {}", error.0);
                    return ExitCode::from(2);
                }
            }
        };
    let recorder = noev_runtime::backend::Recorder::new(chosen_backend);
    let result = noev_runtime::run_main(&program, &recorder, seed, output);
    if let Some(dir) = circuits
        && let Err(code) = save_circuits(
            file,
            dir,
            &recorder.jobs.borrow(),
            ExportFormat::Qasm3,
            format,
        )
    {
        return code;
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_runtime_error(&checked, error, format);
            anstream::eprintln!("{NOTE}note:{NOTE:#} to reproduce this run, use --seed {seed}");
            ExitCode::from(1)
        }
    }
}

/// The text of `--emit=plan`: one line per `sample` run, then one per function with
/// quantum operations.
fn plan(checked: &Checked) -> String {
    let ast = &checked.ast;
    let circuits = &checked.circuits;
    // `line 12` for a program of one file; `src/ansatz.nv:12` when there are several.
    let several = checked.sources.files().len() > 1;
    // Each file's line starts, found once: finding them per row is quadratic in the length.
    let lines: Vec<_> = checked
        .sources
        .files()
        .iter()
        .map(report::Lines::new)
        .collect();
    let line = |span: noev_syntax::Span| {
        let id = checked.sources.find(span.start);
        let file = checked.sources.file(id);
        let line = lines[id.index()].line_col(span.start).0;
        if several {
            format!("{}:{line}", file.name)
        } else {
            format!("line {line}")
        }
    };
    // Why a run is not sampled from one simulation, if it is not.
    let why = |info: &noev_sema::CircuitInfo| {
        if let Some((span, what)) = &info.not_circuit {
            Some(format!("{what} ({})", line(*span)))
        } else if let Some((span, why)) = &info.feedback_unsupported {
            Some(format!("{why}, not compiled yet ({})", line(*span)))
        } else if let Some(span) = info.inspects {
            Some(format!("it reads amplitudes ({})", line(span)))
        } else {
            info.releases
                .map(|span| format!("the simulator checks its `release` ({})", line(span)))
        }
    };
    let run = |info: &noev_sema::CircuitInfo| match why(info) {
        _ if !info.quantum => "classical: no quantum operations".to_string(),
        None => "circuit: sampled from one simulation".to_string(),
        Some(why) => format!("shot by shot: {why}"),
    };
    let function = |info: &noev_sema::CircuitInfo| match (why(info), &info.not_circuit) {
        (None, _) => "circuit".to_string(),
        (Some(why), Some(_)) => format!("not a circuit: {why}"),
        (Some(why), None) => format!("circuit, run shot by shot: {why}"),
    };
    let mut runs: Vec<_> = circuits
        .runs
        .iter()
        .map(|(&id, info)| (ast.expr(id).span, info))
        .collect();
    runs.sort_by_key(|(span, _)| span.start);
    let mut rows: Vec<(String, String)> = runs
        .into_iter()
        .map(|(span, info)| (format!("sample ({})", line(span)), run(info)))
        .collect();
    rows.extend(
        ast.all_fns()
            .iter()
            .zip(&circuits.fns)
            .filter(|(_, info)| info.quantum)
            .map(|(decl, info)| (format!("fn {}", ast.name(decl.name)), function(info))),
    );
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    rows.iter()
        .map(|(name, how)| format!("{name:<width$}  {how}\n"))
        .collect()
}

/// Writes each circuit job as `DIR/circuit-N.qasm` or `DIR/circuit-N.ll`, numbered in the
/// order they ran.
fn save_circuits(
    file: &Path,
    dir: &Path,
    jobs: &[(noev_runtime::backend::Circuit, usize)],
    export_format: ExportFormat,
    format: MessageFormat,
) -> Result<(), ExitCode> {
    let fail = |error: std::io::Error| {
        anstream::eprintln!("noev: cannot write {}: {error}", dir.display());
        ExitCode::from(2)
    };
    std::fs::create_dir_all(dir).map_err(fail)?;
    for (i, (circuit, shots)) in jobs.iter().enumerate() {
        let header = circuit_header(file, i, *shots);
        let (ext, text) = match export_format {
            ExportFormat::Qasm3 => ("qasm", noev_runtime::qasm::to_qasm3(circuit, &header)),
            ExportFormat::Qir => (
                "ll",
                noev_runtime::qir::to_qir(circuit, &format!("circuit_{}", i + 1), &header),
            ),
        };
        let path = dir.join(format!("circuit-{}.{ext}", i + 1));
        std::fs::write(&path, text).map_err(fail)?;
    }
    if matches!(format, MessageFormat::Human) {
        let what = match jobs.len() {
            0 => "no circuits: no run was sampled as a circuit (see `noev build --emit=plan`)"
                .to_string(),
            1 => format!("1 circuit to {}", dir.display()),
            n => format!("{n} circuits to {}", dir.display()),
        };
        let verb = if jobs.is_empty() { "saved" } else { "wrote" };
        anstream::eprintln!("{NOTE}note:{NOTE:#} {verb} {what}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// `noev test` runner (`spec/testing.md`)
// ---------------------------------------------------------------------------------------

/// Shared byte buffer writer for capturing stdout/stderr per test.
struct CaptureWriter(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One checked compilation unit discovered by `noev test`.
struct TestUnit {
    /// Optional prefix (`file_stem` for integration tests or directory mode).
    prefix: Option<String>,
    checked: Checked,
    program: noev_ir::Program,
}

fn qualify_test_name(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}::{name}"),
        _ => name.to_string(),
    }
}

/// Discovers and checks all test units under `path` (`spec/testing.md` §2).
fn discover_test_units(
    path: &Path,
    backend_arg: Option<&str>,
    format: MessageFormat,
) -> Result<Vec<TestUnit>, ExitCode> {
    let mut units = Vec::new();
    let push_unit = |units: &mut Vec<TestUnit>, prefix: Option<String>, checked: Checked| {
        let program = noev_ir::lower(
            &checked.ast,
            &checked.typed,
            &checked.circuits,
            &checked.sources,
        );
        units.push(TestUnit {
            prefix,
            checked,
            program,
        });
    };

    if path.is_file() {
        let checked = front_end(path, backend_arg, TargetCheck::Check, format)?;
        push_unit(&mut units, None, checked);
        return Ok(units);
    }

    if !path.is_dir() {
        anstream::eprintln!("noev: `{}` does not exist", path.display());
        return Err(ExitCode::from(2));
    }

    if path.join("noev.toml").is_file() {
        // Package mode: unit tests in `src/`, plus integration tests in `tests/*.nv`.
        let checked = front_end(path, backend_arg, TargetCheck::Check, format)?;
        push_unit(&mut units, None, checked);

        let tests_dir = path.join("tests");
        if tests_dir.is_dir() {
            let mut files: Vec<PathBuf> = match std::fs::read_dir(&tests_dir) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "nv"))
                    .collect(),
                Err(error) => {
                    anstream::eprintln!("noev: cannot read {}: {error}", tests_dir.display());
                    return Err(ExitCode::from(2));
                }
            };
            files.sort();
            for file in files {
                let loaded = match package::load_integration_test(path, &file) {
                    Ok(l) => l,
                    Err(error) => {
                        anstream::eprintln!("noev: {error}");
                        return Err(ExitCode::from(2));
                    }
                };
                let checked = front_end_loaded(loaded, backend_arg, TargetCheck::Check, format)?;
                let stem = file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string);
                push_unit(&mut units, stem, checked);
            }
        }
    } else {
        // Directory of `.nv` files without `noev.toml`.
        let mut files: Vec<PathBuf> = match std::fs::read_dir(path) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "nv"))
                .collect(),
            Err(error) => {
                anstream::eprintln!("noev: cannot read {}: {error}", path.display());
                return Err(ExitCode::from(2));
            }
        };
        files.sort();
        for file in files {
            let checked = front_end(&file, backend_arg, TargetCheck::Check, format)?;
            let stem = file
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string);
            push_unit(&mut units, stem, checked);
        }
    }

    Ok(units)
}

#[allow(clippy::too_many_arguments)]
fn run_tests(
    path: &Path,
    filter: Option<&str>,
    list: bool,
    seed: Option<u64>,
    backend_arg: Option<&str>,
    compile: bool,
    codegen: Option<CodegenChoice>,
    opt: &OptOptions,
    circuit_opt: u8,
    nocapture: bool,
    format: MessageFormat,
) -> ExitCode {
    let units = match discover_test_units(path, backend_arg, format) {
        Ok(u) => u,
        Err(code) => return code,
    };

    // Collect matching tests across all units.
    let mut matching_names = Vec::new();
    for unit in &units {
        for t in &unit.program.tests {
            let full = qualify_test_name(unit.prefix.as_deref(), &t.name);
            if filter.is_none_or(|f| full.contains(f)) {
                matching_names.push(full);
            }
        }
    }

    if list {
        for name in &matching_names {
            println!("{name}");
        }
        return ExitCode::SUCCESS;
    }

    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });

    // Resolve the effective backend spec from CLI or the first unit's manifest.
    let effective_backend = backend_arg
        .map(str::to_string)
        .or_else(|| units.first().and_then(|u| u.checked.backend.clone()));

    if compile {
        return run_tests_compiled(
            &units,
            filter,
            seed,
            effective_backend.as_deref(),
            codegen,
            opt,
            circuit_opt,
            nocapture,
        );
    }

    run_tests_interpreted(
        &units,
        filter,
        seed,
        effective_backend.as_deref(),
        circuit_opt,
        nocapture,
        format,
    )
}

fn check_test_outcome(
    expect_fail: Option<&Option<String>>,
    result: Result<(), noev_runtime::RuntimeError>,
) -> Option<(String, Option<noev_syntax::Span>)> {
    match (expect_fail, result) {
        (None, Ok(())) => None,
        (None, Err(err)) => Some((err.message, err.span)),
        (Some(None), Err(_)) => None,
        (Some(None), Ok(())) => Some(("test was expected to fail, but passed".into(), None)),
        (Some(Some(expected)), Err(err)) => {
            if err.message.contains(expected.as_str()) {
                None
            } else {
                Some((
                    format!(
                        "expected failure containing `{expected}`, but got `{}`",
                        err.message
                    ),
                    err.span,
                ))
            }
        }
        (Some(Some(expected)), Ok(())) => Some((
            format!("test was expected to fail with `{expected}`, but passed"),
            None,
        )),
    }
}

fn run_tests_interpreted(
    units: &[TestUnit],
    filter: Option<&str>,
    seed: u64,
    backend_spec: Option<&str>,
    circuit_opt: u8,
    nocapture: bool,
    format: MessageFormat,
) -> ExitCode {
    let chosen = backend_spec.unwrap_or("ketrs");
    if chosen == "export" {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} `--backend=export` only records circuits and cannot execute tests; \
             use `ketrs` or a runnable plug-in backend"
        );
        return ExitCode::from(2);
    }
    let sim_holder;
    let plugin_holder;
    let backend: &dyn noev_runtime::Backend = if chosen == "ketrs" || chosen.starts_with("ketrs:") {
        match simulator(chosen, circuit_opt) {
            Ok(sim) => {
                sim_holder = sim;
                &*sim_holder
            }
            Err(msg) => {
                anstream::eprintln!("{ERROR}error:{ERROR:#} {msg}");
                return ExitCode::from(2);
            }
        }
    } else {
        match noev_runtime::plugin::load_plugin_spec(chosen) {
            Ok(p) => {
                plugin_holder = p;
                &plugin_holder
            }
            Err(err) => {
                anstream::eprintln!("{ERROR}error:{ERROR:#} {}", err.0);
                return ExitCode::from(2);
            }
        }
    };

    let mut total = 0usize;
    for unit in units {
        for t in &unit.program.tests {
            let full = qualify_test_name(unit.prefix.as_deref(), &t.name);
            if filter.is_none_or(|f| full.contains(f)) {
                total += 1;
            }
        }
    }

    let start = std::time::Instant::now();
    println!(
        "running {total} {}",
        if total == 1 { "test" } else { "tests" }
    );

    let mut passed = 0usize;
    struct Failure<'a> {
        name: String,
        message: String,
        span: Option<noev_syntax::Span>,
        unit: &'a TestUnit,
        captured: String,
    }
    let mut failures: Vec<Failure<'_>> = Vec::new();

    for unit in units {
        for t in &unit.program.tests {
            let full = qualify_test_name(unit.prefix.as_deref(), &t.name);
            if !filter.is_none_or(|f| full.contains(f)) {
                continue;
            }
            let test_seed = noev_runtime::cabi_compiled::derive_test_seed(seed, &full);
            let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
            let output = if nocapture {
                noev_runtime::Output {
                    stdout: Box::new(std::io::stdout()),
                    stderr: Box::new(std::io::stderr()),
                }
            } else {
                noev_runtime::Output {
                    stdout: Box::new(CaptureWriter(captured.clone())),
                    stderr: Box::new(CaptureWriter(captured.clone())),
                }
            };
            let result = noev_runtime::run_func(&unit.program, t.func, backend, test_seed, output);
            if let Some((message, span)) = check_test_outcome(t.expect_fail.as_ref(), result) {
                println!("test {full} ... FAILED");
                let out_text = String::from_utf8_lossy(&captured.borrow()).into_owned();
                failures.push(Failure {
                    name: full,
                    message,
                    span,
                    unit,
                    captured: out_text,
                });
            } else {
                println!("test {full} ... ok");
                passed += 1;
            }
        }
    }

    if !failures.is_empty() {
        println!("\nfailures:");
        for fail in &failures {
            println!("\n---- {} ----", fail.name);
            if !fail.captured.is_empty() {
                print!("{}", fail.captured);
                if !fail.captured.ends_with('\n') {
                    println!();
                }
            }
            if let Some(span) = fail.span {
                let diag = noev_syntax::Diagnostic::error(span, fail.message.clone());
                report::print(&fail.unit.checked.sources, &[diag], format);
            } else {
                anstream::eprintln!("{ERROR}error:{ERROR:#} {}", fail.message);
            }
            println!(
                "note: reproduce with: noev test --seed {seed} {}",
                fail.name
            );
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let status = if failures.is_empty() { "ok" } else { "FAILED" };
    println!(
        "\ntest result: {status}. {passed} passed; {} failed; finished in {elapsed:.2}s",
        failures.len()
    );
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
#[allow(clippy::too_many_arguments)]
fn run_tests_compiled(
    units: &[TestUnit],
    filter: Option<&str>,
    seed: u64,
    backend_spec: Option<&str>,
    codegen: Option<CodegenChoice>,
    opt: &OptOptions,
    circuit_opt: u8,
    nocapture: bool,
) -> ExitCode {
    let generator = match select_codegen(codegen, false) {
        Ok(g) => g,
        Err(code) => return code,
    };
    let Some(runtime_lib) = find_runtime_lib() else {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} cannot find `libnoev_runtime.a` (set `NOEV_RUNTIME_LIB` or build `noev-runtime`)"
        );
        return ExitCode::from(2);
    };

    // Only compile units that actually contain at least one matching test.
    let active: Vec<(usize, &TestUnit)> = units
        .iter()
        .enumerate()
        .filter(|(_, u)| {
            u.program.tests.iter().any(|t| {
                let full = qualify_test_name(u.prefix.as_deref(), &t.name);
                filter.is_none_or(|f| full.contains(f))
            })
        })
        .collect();

    if active.is_empty() {
        println!("running 0 tests\n\ntest result: ok. 0 passed; 0 failed; finished in 0.00s");
        return ExitCode::SUCCESS;
    }

    if backend_spec == Some("export") {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} `--backend=export` only records circuits and cannot execute tests; \
             use `ketrs` or a runnable plug-in backend"
        );
        return ExitCode::from(2);
    }

    // Create an exclusively owned temporary directory (`create_dir` fails if it already exists).
    let base_tmp = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut tmp_dir = None;
    for attempt in 0u64..100 {
        let candidate = base_tmp.join(format!(
            "noev-test-bin-{}-{:x}-{attempt}",
            std::process::id(),
            nanos ^ u128::from(seed)
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                tmp_dir = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                anstream::eprintln!("noev: cannot create {}: {error}", candidate.display());
                return ExitCode::from(2);
            }
        }
    }
    let Some(tmp_dir) = tmp_dir else {
        anstream::eprintln!("noev: cannot create a unique temporary directory for compiled tests");
        return ExitCode::from(2);
    };

    let opts = codegen_options(opt, false, true);

    // Compile units in parallel across available cores.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().min(8).min(active.len()))
        .unwrap_or(1);
    let next = std::sync::Mutex::new(0usize);
    let compiled: std::sync::Mutex<Vec<(usize, Result<PathBuf, String>)>> =
        std::sync::Mutex::new(Vec::new());

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let pos = {
                        let mut g = next.lock().unwrap();
                        if *g >= active.len() {
                            break;
                        }
                        let i = *g;
                        *g += 1;
                        i
                    };
                    let (unit_idx, unit) = active[pos];
                    let bin_name = if cfg!(windows) {
                        format!("test_unit_{unit_idx}.exe")
                    } else {
                        format!("test_unit_{unit_idx}")
                    };
                    let bin_path = tmp_dir.join(bin_name);
                    let res = generator
                        .emit_binary(&unit.program, &bin_path, &runtime_lib, &opts)
                        .map(|()| bin_path)
                        .map_err(|e| e.0);
                    compiled.lock().unwrap().push((pos, res));
                }
            });
        }
    });

    let mut bins = compiled.into_inner().unwrap();
    bins.sort_by_key(|(pos, _)| *pos);

    let mut ready_bins: Vec<(usize, PathBuf)> = Vec::with_capacity(bins.len());
    for (pos, res) in bins {
        match res {
            Ok(p) => ready_bins.push((pos, p)),
            Err(err) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                anstream::eprintln!("{ERROR}error:{ERROR:#} {err}");
                return ExitCode::from(1);
            }
        }
    }

    let single = ready_bins.len() == 1;
    let total: usize = active
        .iter()
        .map(|(_, u)| {
            u.program
                .tests
                .iter()
                .filter(|t| {
                    let full = qualify_test_name(u.prefix.as_deref(), &t.name);
                    filter.is_none_or(|f| full.contains(f))
                })
                .count()
        })
        .sum();

    let start = std::time::Instant::now();
    if !single {
        println!(
            "running {total} {}",
            if total == 1 { "test" } else { "tests" }
        );
    }

    let mut total_passed = 0usize;
    let mut total_failed = 0usize;
    let mut any_failed = false;

    for (pos, bin) in &ready_bins {
        let (_, unit) = active[*pos];
        let mut cmd = std::process::Command::new(bin);
        cmd.arg("--seed").arg(seed.to_string());
        cmd.arg("--circuit-opt").arg(circuit_opt.to_string());
        if let Some(spec) = backend_spec {
            cmd.arg("--backend").arg(spec);
        }
        if let Some(f) = filter {
            cmd.arg("--filter").arg(f);
        }
        if let Some(prefix) = &unit.prefix {
            cmd.arg("--prefix").arg(prefix);
        }
        if nocapture {
            cmd.arg("--nocapture");
        }
        if !single {
            cmd.arg("--quiet-summary");
        }
        if single {
            let status = match cmd.status() {
                Ok(s) => s,
                Err(error) => {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    anstream::eprintln!("noev: cannot run test binary: {error}");
                    return ExitCode::from(1);
                }
            };
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            };
        }
        match cmd.output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                print!("{text}");
                if !out.stderr.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&out.stderr));
                }
                for line in text.lines() {
                    if line.starts_with("test ") && line.ends_with(" ... ok") {
                        total_passed += 1;
                    } else if line.starts_with("test ") && line.ends_with(" ... FAILED") {
                        total_failed += 1;
                    }
                }
                if !out.status.success() {
                    any_failed = true;
                }
            }
            Err(error) => {
                anstream::eprintln!("noev: cannot run test binary: {error}");
                any_failed = true;
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let elapsed = start.elapsed().as_secs_f64();
    let status = if !any_failed && total_failed == 0 {
        "ok"
    } else {
        "FAILED"
    };
    println!(
        "\ntest result: {status}. {total_passed} passed; {total_failed} failed; finished in {elapsed:.2}s"
    );
    if !any_failed && total_failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[cfg(not(any(feature = "cranelift", feature = "llvm")))]
#[allow(clippy::too_many_arguments)]
fn run_tests_compiled(
    _units: &[TestUnit],
    _filter: Option<&str>,
    _seed: u64,
    _backend_spec: Option<&str>,
    _codegen: Option<CodegenChoice>,
    _opt: &OptOptions,
    _circuit_opt: u8,
    _nocapture: bool,
) -> ExitCode {
    anstream::eprintln!(
        "{ERROR}error:{ERROR:#} `noev` was built without a native code generator (`cranelift` or `llvm`)"
    );
    ExitCode::from(2)
}
