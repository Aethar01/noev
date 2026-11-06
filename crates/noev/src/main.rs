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
        #[arg(long, value_enum, default_value_t = Emit::Qasm3)]
        emit: Emit,

        /// Where to write the output (default: standard output)
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,

        #[command(flatten)]
        messages: MessageOptions,
    },

    /// Run a program on the simulator and print its results
    Run {
        /// The `.nv` file to run
        file: PathBuf,

        /// How many times to run the program
        #[arg(long, default_value_t = 1000, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        shots: u64,

        /// Seed for the simulator's random number generator
        #[arg(long, value_name = "N")]
        seed: Option<u64>,

        /// How many of the most frequent results to show (0 shows all)
        #[arg(long, default_value_t = 20, value_name = "N")]
        limit: usize,

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
    /// OpenQASM 3 source
    Qasm3,
    /// Noev's intermediate representation, for inspecting the compiler
    Ir,
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
            shots,
            seed,
            limit,
            messages,
        } => run(&file, shots, seed, limit, messages.message_format),
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
    // Ownership is only meaningful for a well-typed program.
    if diagnostics.is_empty() && type_errors.is_empty() {
        diagnostics.extend(noev_sema::check_ownership(&parsed.ast, &typed));
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
        Emit::Qasm3 => return not_implemented("build --emit=qasm3"),
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

/// Runs `file`'s `main` `shots` times on the simulator and prints how often each result
/// occurred.
fn run(
    file: &Path,
    shots: u64,
    seed: Option<u64>,
    limit: usize,
    format: MessageFormat,
) -> ExitCode {
    let checked = match front_end(file, format) {
        Ok(checked) => checked,
        Err(code) => return code,
    };
    let program = noev_ir::lower(&checked.parsed.ast, &checked.typed, &checked.source);
    let Some(main) = program.main else {
        anstream::eprintln!(
            "{ERROR}error:{ERROR:#} `{}` has no `fn main()` to run",
            file.display()
        );
        return ExitCode::from(1);
    };
    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    let start = std::time::Instant::now();
    let counts = match noev_runtime::run(&program, main, shots as usize, seed) {
        Ok(counts) => counts,
        Err(error) => {
            let diagnostic =
                noev_syntax::Diagnostic::error(error.span.unwrap_or_default(), error.message);
            match error.span {
                Some(_) => report::print(
                    &file.display().to_string(),
                    &checked.source,
                    &[diagnostic],
                    format,
                ),
                None => anstream::eprintln!("{ERROR}error:{ERROR:#} {}", diagnostic.message),
            }
            return ExitCode::from(1);
        }
    };
    report::histogram(&counts, limit, start.elapsed(), seed);
    ExitCode::SUCCESS
}

fn not_implemented(command: &str) -> ExitCode {
    anstream::eprintln!("noev: `{command}` is not implemented yet");
    ExitCode::from(2)
}
