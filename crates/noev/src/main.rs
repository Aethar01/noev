//! The `noev` command-line interface.

mod report;

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
        /// The `.nv` file to check
        file: PathBuf,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Compile a program
    Build {
        /// The `.nv` file to build
        file: PathBuf,

        /// What to produce
        #[arg(long, value_enum)]
        emit: Emit,

        /// Where to write the output (default: standard output)
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Run a program, on the simulator for its quantum parts
    Run {
        /// The `.nv` file to run
        file: PathBuf,

        /// Seed for the simulator's random numbers, to reproduce a run
        #[arg(long, value_name = "N")]
        seed: Option<u64>,

        /// Save every circuit the run sends to the backend as OpenQASM 3, one file per job,
        /// in this directory
        #[arg(long, value_name = "DIR")]
        circuits: Option<PathBuf>,

        #[command(flatten)]
        messages: MessageOptions,
    },
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

#[derive(Clone, Copy, ValueEnum)]
enum Emit {
    /// Noev's intermediate representation, for inspecting the compiler
    Ir,
    /// How each `sample` run and quantum function runs when repeated: sampled from one
    /// simulation, or shot by shot and why
    Plan,
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
            output,
            messages,
        } => build(&file, emit, output.as_deref(), messages.message_format),
        Command::Run {
            file,
            seed,
            circuits,
            messages,
        } => run(&file, seed, circuits.as_deref(), messages.message_format),
    }
}

/// A program that passed every check.
struct Checked {
    source: String,
    parsed: noev_syntax::Parsed,
    typed: noev_sema::Typed,
}

/// Reads and checks `file`, printing its diagnostics. Fails with exit status 1 when there
/// are errors, and 2 when the file could not be read.
fn front_end(file: &Path, format: MessageFormat) -> Result<Checked, ExitCode> {
    let source = match std::fs::read_to_string(file) {
        Ok(source) => source,
        Err(error) => {
            anstream::eprintln!("noev: cannot read {}: {error}", file.display());
            return Err(ExitCode::from(2));
        }
    };
    let parsed = noev_syntax::parse(&source);
    let (typed, type_errors) = noev_sema::check(&parsed.ast);
    let mut diagnostics = parsed.diagnostics.clone();
    // Ownership is only meaningful for a well-typed program, and circuits for one that
    // passes ownership checking.
    if diagnostics.is_empty() && type_errors.is_empty() {
        diagnostics.extend(noev_sema::check_ownership(&parsed.ast, &typed));
        if diagnostics.is_empty() {
            diagnostics.extend(noev_sema::check_circuits(&parsed.ast, &typed).1);
        }
    }
    diagnostics.extend(type_errors);
    diagnostics.sort_by_key(|d| d.span.start);
    report::print(&file.display().to_string(), &source, &diagnostics, format);
    let failed = diagnostics
        .iter()
        .any(|d| d.severity == noev_syntax::Severity::Error);
    if failed {
        return Err(ExitCode::from(1));
    }
    Ok(Checked {
        source,
        parsed,
        typed,
    })
}

/// Compiles `file` and writes the requested output to `output`, or to stdout.
fn build(file: &Path, emit: Emit, output: Option<&Path>, format: MessageFormat) -> ExitCode {
    let checked = match front_end(file, format) {
        Ok(checked) => checked,
        Err(code) => return code,
    };
    let program = noev_ir::lower(&checked.parsed.ast, &checked.typed, &checked.source);
    let text = match emit {
        Emit::Ir => program.to_string(),
        Emit::Plan => plan(&checked),
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

/// Runs `file`'s `main` once. Standard output is only what the program prints.
fn run(file: &Path, seed: Option<u64>, circuits: Option<&Path>, format: MessageFormat) -> ExitCode {
    use std::io::{IsTerminal, Write};
    let checked = match front_end(file, format) {
        Ok(checked) => checked,
        Err(code) => return code,
    };
    let program = noev_ir::lower(&checked.parsed.ast, &checked.typed, &checked.source);
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
    let backend = noev_runtime::backend::Recorder::new(&noev_runtime::Ketrs);
    let result = noev_runtime::run_main(&program, &backend, seed, output);
    if let Some(dir) = circuits
        && let Err(code) = save_circuits(file, dir, &backend.jobs.borrow(), format)
    {
        return code;
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            match error.span {
                Some(span) => {
                    let diagnostic = noev_syntax::Diagnostic::error(span, error.message);
                    report::print(
                        &file.display().to_string(),
                        &checked.source,
                        &[diagnostic],
                        format,
                    );
                }
                None => anstream::eprintln!("{ERROR}error:{ERROR:#} {}", error.message),
            }
            anstream::eprintln!("{NOTE}note:{NOTE:#} to reproduce this run, use --seed {seed}");
            ExitCode::from(1)
        }
    }
}

/// The text of `--emit=plan`: one line per `sample` run, then one per function with
/// quantum operations.
fn plan(checked: &Checked) -> String {
    let ast = &checked.parsed.ast;
    let (circuits, _) = noev_sema::check_circuits(ast, &checked.typed);
    let lines = report::Lines::new(&checked.source);
    let line = |span: noev_syntax::Span| lines.line_col(span.start).0;
    // Why a run is not sampled from one simulation, if it is not.
    let why = |info: &noev_sema::CircuitInfo| {
        if let Some((span, what)) = &info.not_circuit {
            Some(format!(
                "a measurement result is {what} (line {})",
                line(*span)
            ))
        } else if info.feedback {
            Some("conditions on measurement results are not compiled yet".to_string())
        } else {
            info.releases
                .map(|span| format!("the simulator checks its `release` (line {})", line(span)))
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
        .map(|(span, info)| (format!("sample (line {})", line(span)), run(info)))
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

/// Writes each circuit job as `DIR/circuit-N.qasm`, numbered in the order they ran.
fn save_circuits(
    file: &Path,
    dir: &Path,
    jobs: &[(noev_runtime::backend::Circuit, usize)],
    format: MessageFormat,
) -> Result<(), ExitCode> {
    let fail = |error: std::io::Error| {
        anstream::eprintln!("noev: cannot write {}: {error}", dir.display());
        ExitCode::from(2)
    };
    std::fs::create_dir_all(dir).map_err(fail)?;
    for (i, (circuit, shots)) in jobs.iter().enumerate() {
        let header = [
            format!("Circuit {} of {}, generated by noev", i + 1, file.display()),
            format!("shots: {shots}"),
        ];
        let path = dir.join(format!("circuit-{}.qasm", i + 1));
        std::fs::write(&path, noev_runtime::qasm::to_qasm3(circuit, &header)).map_err(fail)?;
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
