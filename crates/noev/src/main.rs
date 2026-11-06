//! The `noev` command-line interface.

mod report;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand, ValueEnum};

const NOTE: anstyle::Style = AnsiColor::Cyan.on_default().effects(Effects::BOLD);

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

        /// Where to write the output (default: next to the source file)
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
        #[arg(long, default_value_t = 1000, value_name = "N")]
        shots: u64,

        /// Seed for the simulator's random number generator
        #[arg(long, value_name = "N")]
        seed: Option<u64>,

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
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Check { file, messages } => check(&file, messages.message_format),
        Command::Build { .. } => not_implemented("build"),
        Command::Run { .. } => not_implemented("run"),
    }
}

/// Checks `file`, printing its diagnostics. Exits with 0 when there are no errors, 1 when
/// there are, and 2 when the file could not be checked at all.
fn check(file: &Path, format: MessageFormat) -> ExitCode {
    let source = match std::fs::read_to_string(file) {
        Ok(source) => source,
        Err(error) => {
            anstream::eprintln!("noev: cannot read {}: {error}", file.display());
            return ExitCode::from(2);
        }
    };
    let parsed = noev_syntax::parse(&source);
    let (typed, type_errors) = noev_sema::check(&parsed.ast);
    let mut diagnostics = parsed.diagnostics;
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
    if matches!(format, MessageFormat::Human) && !failed {
        anstream::eprintln!("{NOTE}note:{NOTE:#} syntax, types and ownership were checked");
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn not_implemented(command: &str) -> ExitCode {
    anstream::eprintln!("noev: `{command}` is not implemented yet");
    ExitCode::from(2)
}
