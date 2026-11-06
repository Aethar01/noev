//! The `noev` command-line interface.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand, ValueEnum};

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
    let name = match Cli::parse().command {
        Command::Check { .. } => "check",
        Command::Build { .. } => "build",
        Command::Run { .. } => "run",
    };
    eprintln!("noev: `{name}` is not implemented yet");
    ExitCode::from(2)
}
