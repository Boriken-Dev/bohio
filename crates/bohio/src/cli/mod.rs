//! The `bohio` command line.
//!
//! Only this module prints, reads the process environment and installs the diagnostics
//! subscriber; the library below it does none of those.

mod commands;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::Environment;

/// Bohío: a remote host runtime.
#[derive(Debug, Parser)]
#[command(
    name = "bohio",
    version,
    about = "Bohío: a remote host runtime serving files, processes and shells to proven peers",
    after_help = "Environment variables:\n  \
        BOHIO_CONFIG   the configuration file, when --config is not given\n  \
        BOHIO_LOG      the log level or filter, for example `info` or `bohio=debug` (default `warn`)\n  \
        NO_COLOR       set to disable colour in log lines\n  \
        PATH           where programs and shells are looked for\n  \
        APPDATA, LOCALAPPDATA, ProgramData (Windows) and XDG_CONFIG_HOME, XDG_DATA_HOME, HOME (Unix)\n                 \
        where the configuration and data directory are looked for\n\n\
        Exit status: 0 success, 1 the operation failed, 2 the invocation was wrong."
)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Work with the configuration file.
    Config(ConfigArgs),
    /// Print this host's capabilities.
    Info(InfoArgs),
}

/// `bohio config`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// What to do with the configuration.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// The `bohio config` actions.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Read the configuration and report whether it is valid.
    Check(CheckArgs),
}

/// How a result is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Lines for a person.
    Text,
    /// One JSON document, stable across releases.
    Json,
}

/// `bohio config check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// The configuration file; otherwise `BOHIO_CONFIG`, then the user's, then the system's.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// The output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
}

/// `bohio info`.
#[derive(Debug, Args)]
pub struct InfoArgs {
    /// The configuration file; otherwise `BOHIO_CONFIG`, then the user's, then the system's.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// The output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
}

/// Parses the process arguments, runs the command and returns its exit status.
pub fn main() -> ExitCode {
    run(std::env::args_os(), &capture_environment())
}

/// Runs the command `arguments` name (the first is the program) with `environment`.
pub fn run(arguments: impl IntoIterator<Item = OsString>, environment: &Environment) -> ExitCode {
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
        }
    };
    install_subscriber();
    match commands::run(&cli.command, environment) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("bohio: {error}");
            ExitCode::from(1)
        }
    }
}

/// The process environment the library's searches read.
pub fn capture_environment() -> Environment {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    Environment {
        windows: cfg!(windows),
        config: var("BOHIO_CONFIG"),
        app_data: var("APPDATA"),
        local_app_data: var("LOCALAPPDATA"),
        program_data: var("ProgramData"),
        xdg_config_home: var("XDG_CONFIG_HOME"),
        xdg_data_home: var("XDG_DATA_HOME"),
        home: var("HOME"),
        path: std::env::var_os("PATH"),
    }
}

/// Sends diagnostics to standard error at `BOHIO_LOG`'s level, `warn` by default.
fn install_subscriber() {
    let filter = std::env::var("BOHIO_LOG")
        .ok()
        .and_then(|spec| tracing_subscriber::EnvFilter::try_new(spec).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("warn"));
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(color)
        .try_init();
}
