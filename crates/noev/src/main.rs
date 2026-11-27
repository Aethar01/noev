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
    /// Check a program for type and ownership errors without building it
    Check {
        /// The `.nv` file or package directory to check
        #[arg(default_value = ".")]
        file: PathBuf,

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

        /// Emit QIR runtime calls (`__quantum__qis__*`, `__quantum__rt__*`) instead of Noev's
        /// runtime C ABI (LLVM code generator only)
        #[arg(long)]
        qir_runtime: bool,

        /// Extra C objects, static libraries, or library names to link into the binary
        #[arg(short = 'l', long = "link", value_name = "LIB")]
        link: Vec<String>,

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

        /// What runs the quantum parts [default: ketrs, when noev is built with it]
        #[arg(long, value_enum)]
        backend: Option<BackendChoice>,

        /// Save every circuit the run sends to the backend as OpenQASM 3, one file per job,
        /// in this directory
        #[arg(long, value_name = "DIR")]
        circuits: Option<PathBuf>,

        #[command(flatten)]
        messages: MessageOptions,
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

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum BackendChoice {
    /// The ketrs statevector simulator (the `ketrs` feature, on by default)
    Ketrs,
    /// Record each circuit instead of running it, and save it; nothing quantum runs, and
    /// what the program prints is dropped, since its results are not known
    Export,
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
        Command::Check { file, messages } => match front_end(&file, messages.message_format) {
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
            qir_runtime,
            link,
            output,
            messages,
        } => build(
            &file,
            emit,
            codegen,
            qir_runtime,
            &link,
            output.as_deref(),
            messages.message_format,
        ),
        Command::Run {
            file,
            seed,
            backend,
            circuits,
            messages,
        } => run(
            &file,
            seed,
            backend,
            circuits.as_deref(),
            messages.message_format,
        ),
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
        } => match front_end(&file, messages.message_format) {
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

/// A program that passed every check.
struct Checked {
    sources: noev_syntax::SourceMap,
    ast: noev_syntax::Ast,
    typed: noev_sema::Typed,
    circuits: noev_sema::Circuits,
}

/// Reads and checks the `.nv` file or package at `file`, printing its diagnostics. Fails with
/// exit status 1 when there are errors, and 2 when nothing could be checked.
fn front_end(file: &Path, format: MessageFormat) -> Result<Checked, ExitCode> {
    let loaded = match package::load(file) {
        Ok(loaded) => loaded,
        Err(error) => {
            anstream::eprintln!("noev: {error}");
            return Err(ExitCode::from(2));
        }
    };
    let package::Loaded {
        sources,
        ast,
        mut diagnostics,
    } = loaded;
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
    })
}

/// Compiles `file` and writes the requested output to `output`, or to stdout (or a default file
/// name for `--emit=object` and `--emit=binary`).
fn build(
    file: &Path,
    emit: Emit,
    codegen: Option<CodegenChoice>,
    qir_runtime: bool,
    link: &[String],
    output: Option<&Path>,
    format: MessageFormat,
) -> ExitCode {
    let checked = match front_end(file, format) {
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
        Emit::LlvmIr => build_llvm_ir(&program, qir_runtime, output),
        Emit::Object => build_object(file, &program, codegen, qir_runtime, output),
        Emit::Binary => build_binary(file, &program, codegen, qir_runtime, link, output),
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
fn codegen_options(qir_runtime: bool) -> noev_codegen::CodegenOptions {
    noev_codegen::CodegenOptions {
        opt: noev_codegen::OptLevel::Speed,
        runtime_abi: if qir_runtime {
            noev_codegen::RuntimeAbi::Qir
        } else {
            noev_codegen::RuntimeAbi::Noev
        },
    }
}

#[cfg(feature = "llvm")]
fn build_llvm_ir(program: &noev_ir::Program, qir_runtime: bool, output: Option<&Path>) -> ExitCode {
    let opts = codegen_options(qir_runtime);
    match noev_codegen::llvm::LlvmCodegen.emit_llvm_ir(program, opts) {
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
    match generator.emit_object(program, codegen_options(qir_runtime)) {
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
    let res = if link.is_empty() {
        generator.emit_binary(program, &dest, &runtime_lib, codegen_options(qir_runtime))
    } else {
        generator
            .emit_object(program, codegen_options(qir_runtime))
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
    _qir_runtime: bool,
    _link: &[String],
    _output: Option<&Path>,
) -> ExitCode {
    anstream::eprintln!(
        "{ERROR}error:{ERROR:#} `noev` was built without a native code generator (`cranelift` or `llvm`)"
    );
    ExitCode::from(2)
}

/// The simulator `noev run` uses, if this build has one.
fn simulator() -> Option<&'static dyn noev_runtime::Backend> {
    #[cfg(feature = "ketrs")]
    return Some(&noev_runtime::Ketrs);
    #[cfg(not(feature = "ketrs"))]
    return None;
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
    backend: Option<BackendChoice>,
    circuits: Option<&Path>,
    format: MessageFormat,
) -> ExitCode {
    use std::io::{IsTerminal, Write};
    let checked = match front_end(file, format) {
        Ok(checked) => checked,
        Err(code) => return code,
    };
    if backend == Some(BackendChoice::Export) {
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
    let Some(simulator) = simulator() else {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} this noev was built without a simulator (its `ketrs` feature); \
             `--backend=export` saves the program's circuits instead"
        );
        return ExitCode::from(2);
    };
    let backend = noev_runtime::backend::Recorder::new(simulator);
    let result = noev_runtime::run_main(&program, &backend, seed, output);
    if let Some(dir) = circuits
        && let Err(code) = save_circuits(
            file,
            dir,
            &backend.jobs.borrow(),
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
