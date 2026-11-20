//! The `bohio` command line.
//!
//! Only this module prints, reads the process environment and installs the diagnostics
//! subscriber; the library below it does none of those.

mod access;
mod commands;
mod host;
mod remote;
pub mod service;

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
        BOHIO_CONFIG   the configuration file (the only layer), when --config is not given\n  \
        BOHIO_LOG      the log level or filter, for example `info` or `bohio=debug` (default `warn`)\n  \
        NO_COLOR       set to disable colour in log lines\n  \
        PATH           where programs and shells are looked for\n  \
        APPDATA, LOCALAPPDATA, ProgramData (Windows) and XDG_CONFIG_HOME, XDG_DATA_HOME, HOME (Unix)\n                 \
        where the system and user configuration files and the data directory are looked for\n\n\
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
    /// Serve links on the configured endpoints until stopped.
    Serve(ServeArgs),
    /// Print this host's id, data directory and peers.
    Status(StatusArgs),
    /// Print this host's id.
    Id(StatusArgs),
    /// List, approve and revoke peers.
    Peer(PeerArgs),
    /// Show a PIN and accept the key of the one peer that joins with it.
    Pair(PairArgs),
    /// Join a host that shows a PIN: each side then keeps the other's key.
    Join(JoinArgs),
    /// Install, remove or run the host as a Windows service.
    Service(service::ServiceArgs),
    /// Use a host: info, run a program, read a file or open a shell.
    Remote(RemoteArgs),
    /// List the links this node's running `bohio serve` holds, whichever side made each.
    Links(LinksArgs),
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
    /// A configuration layer, lowest first (repeat for more); they are read alone. Otherwise
    /// `BOHIO_CONFIG` names the one layer; otherwise the system file is read and the user's merged over it.
    #[arg(long, value_name = "FILE", env = "BOHIO_CONFIG")]
    pub config: Vec<PathBuf>,
    /// The output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
}

/// `bohio info`.
#[derive(Debug, Args)]
pub struct InfoArgs {
    /// A configuration layer, lowest first (repeat for more); they are read alone. Otherwise
    /// `BOHIO_CONFIG` names the one layer; otherwise the system file is read and the user's merged over it.
    #[arg(long, value_name = "FILE", env = "BOHIO_CONFIG")]
    pub config: Vec<PathBuf>,
    /// The output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
}

/// The configuration layers every command takes.
#[derive(Debug, Args)]
pub struct ConfigOption {
    /// A configuration layer, lowest first (repeat for more); they are read alone. Otherwise
    /// `BOHIO_CONFIG` names the one layer; otherwise the system file is read and the user's merged over it.
    #[arg(long, value_name = "FILE", env = "BOHIO_CONFIG")]
    pub config: Vec<PathBuf>,
}

/// `bohio serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// An endpoint to listen on besides the configured ones: `tcp://host:port` (port 0 picks a free
    /// one), `pipe://name`, `ws://host:port` or `serial://port?baud=N`. Standard output says
    /// `listening <endpoint>` for each once it is bound.
    #[arg(long, value_name = "ENDPOINT")]
    pub listen: Vec<String>,
    /// Serve one link on standard input and output instead, for `docker exec -i`, a jump host or a
    /// provider that owns the pipes: nothing else is written to standard output, diagnostics go to
    /// standard error, and no `[[listen]]` entry is bound.
    #[arg(long, conflicts_with = "listen")]
    pub stdio: bool,
}

/// `bohio status`, `bohio id` and `bohio peer list`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
}

/// `bohio peer`.
#[derive(Debug, Args)]
pub struct PeerArgs {
    /// What to do.
    #[command(subcommand)]
    pub action: PeerAction,
}

/// The `bohio peer` actions.
#[derive(Debug, Subcommand)]
pub enum PeerAction {
    /// Accepted peers with what each may do, the proven keys waiting for approval, and stray grants.
    List(StatusArgs),
    /// Accept a peer by id (waiting, paired or new): write its `[peer.<id>]` table to a layer; with `--allow`,
    /// `--fs-root` or `--program`, set its grant.
    Approve(ApproveArgs),
    /// Remove a peer's table from every layer that holds it, and its waiting entry; its running streams end
    /// within `[link] recheck`.
    Revoke(RevokeArgs),
}

/// `bohio peer approve`.
#[derive(Debug, Args)]
pub struct ApproveArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// The peer's id, as `bohio peer list` prints it.
    pub id: String,
    /// What the peer may do: info, fs.read, fs.write, exec, shell, admin (repeat or separate with
    /// commas). Setting any of `--allow`, `--fs-root` and `--program` replaces the peer's whole table;
    /// without them an existing table stays and a new peer holds nothing.
    #[arg(long, value_delimiter = ',', value_name = "PERMISSION")]
    pub allow: Vec<String>,
    /// A directory file operations are confined to (repeat).
    #[arg(long, value_name = "DIR")]
    pub fs_root: Vec<PathBuf>,
    /// A program the peer may run, as the host finds it on its own PATH (repeat); with none, any.
    #[arg(long, value_name = "NAME")]
    pub program: Vec<String>,
    /// With `--program`: an environment variable the peer's requests may set or remove (repeat);
    /// with none, they may change none. A request may change PATH only when it is named here: a program that
    /// starts others then runs what the peer's PATH finds.
    #[arg(long, value_name = "NAME", requires = "program")]
    pub env: Vec<String>,
    /// Where this node dials the peer (any endpoint `bohio remote` takes): `bohio serve` then keeps a link to it,
    /// reconnecting after a failure. Written as the table's `endpoint`; the peer's table keeps its other settings.
    #[arg(long, value_name = "ENDPOINT")]
    pub endpoint: Option<String>,
}

/// `bohio peer revoke`.
#[derive(Debug, Args)]
pub struct RevokeArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// The peer's id.
    pub id: String,
}

/// `bohio pair`.
#[derive(Debug, Args)]
pub struct PairArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// Where to wait for the joiner, `tcp://host:port`; default `tcp://127.0.0.1:0`. Anyone who can
    /// reach it may try the PIN, once.
    #[arg(long, value_name = "ENDPOINT")]
    pub listen: Option<String>,
    /// How long to wait for the joiner: a number of seconds or a unit (`30s`, `5m`). Default `5m`.
    #[arg(long, value_name = "TIME")]
    pub timeout: Option<String>,
}

/// `bohio join`.
#[derive(Debug, Args)]
pub struct JoinArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// The host that shows the PIN, `tcp://host:port`.
    pub endpoint: String,
    /// The PIN the host shows; read from standard input when absent, so it stays out of process lists.
    #[arg(long, value_name = "PIN")]
    pub pin: Option<String>,
}

/// `bohio remote`.
#[derive(Debug, Args)]
pub struct RemoteArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// The host: `tcp://host:port`, `pipe://name`, `ws://host:port`, `wss://host:port`, `webtransport://host:port`, `serial://port?baud=N` or
    /// `stdio://program args` (the program is started and its standard streams are the link). With
    /// `--command` there is none.
    #[arg(required_unless_present_any = ["command", "via"])]
    pub endpoint: Option<String>,
    /// Start this program (a command line, run directly, never through a shell) and use its standard
    /// input and output as the link, for example `docker exec -i NAME bohio serve --stdio`. The same
    /// as the endpoint `stdio://COMMAND`; the program is ended when the command does.
    #[arg(long, value_name = "COMMAND", conflicts_with_all = ["endpoint", "via"])]
    pub command: Option<String>,
    /// Use the link this node's running `bohio serve` holds to the peer ID, whichever side made it (a host that dialed
    /// in is reached this way), through the node's local control endpoint (`[control]`). Exclusive with an endpoint and
    /// `--command`.
    #[arg(long, value_name = "ID", conflicts_with_all = ["endpoint", "command"])]
    pub via: Option<String>,
    /// The host's id to trust instead of the keys `bohio join` stored.
    #[arg(long, value_name = "ID")]
    pub server: Option<String>,
    /// For `wss://` and `webtransport://`: trust the certificates in this PEM file instead of the system's roots, for a host
    /// whose certificate no public authority issued. May be repeated. The link's proof, not TLS, says
    /// who the host is.
    #[arg(long, value_name = "FILE")]
    pub ca: Vec<PathBuf>,
    /// For `wss://` and `webtransport://`: trust the certificate with this SHA-256 hash (`sha256:` and 64 hex digits)
    /// whatever its name and issuer, as a self-signed host needs. May be repeated.
    #[arg(long, value_name = "HASH")]
    pub cert_hash: Vec<String>,
    /// For a `webtransport://` host: a `ws://` or `wss://` endpoint to dial when the UDP path is
    /// blocked. A certificate that is refused never falls back. A `ws://` one is plain until the
    /// link's proof has run, and one on another host sends the proof there: naming either is the choice.
    #[arg(long, value_name = "ENDPOINT")]
    pub fallback: Option<String>,
    /// What to ask of it.
    #[command(subcommand)]
    pub action: RemoteAction,
}

/// The `bohio remote` actions.
#[derive(Debug, Subcommand)]
pub enum RemoteAction {
    /// Print the host's capabilities.
    Info(RemoteInfoArgs),
    /// Run a program on the host and exit with its status.
    Exec(RemoteExecArgs),
    /// Write a file of the host to standard output.
    Read(RemoteReadArgs),
    /// Open an interactive shell on the host.
    Shell(RemoteShellArgs),
}

/// `bohio remote ... info`.
#[derive(Debug, Args)]
pub struct RemoteInfoArgs {
    /// The output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
}

/// `bohio remote ... exec`.
#[derive(Debug, Args)]
pub struct RemoteExecArgs {
    /// The program's working directory on the host.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,
    /// End the program's tree after this many milliseconds.
    #[arg(long, value_name = "MS")]
    pub timeout_ms: Option<u64>,
    /// A variable to set for the program, `NAME=VALUE` (repeat).
    #[arg(long, value_name = "NAME=VALUE")]
    pub env: Vec<String>,
    /// Send this process's standard input to the program.
    #[arg(long)]
    pub stdin: bool,
    /// The program and its arguments, after `--`; run directly, never through a shell.
    #[arg(last = true, required = true, value_name = "ARGV")]
    pub argv: Vec<String>,
}

/// `bohio remote ... read`.
#[derive(Debug, Args)]
pub struct RemoteReadArgs {
    /// The file, in the host's own path text.
    pub path: String,
}

/// `bohio remote ... shell`.
#[derive(Debug, Args)]
pub struct RemoteShellArgs {
    /// One of the host's shells: sh, bash, zsh, cmd, powershell or pwsh. The host's `sh` or `cmd` by default.
    #[arg(long, value_name = "SHELL")]
    pub kind: Option<String>,
    /// The shell's working directory on the host.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,
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
        Ok(code) => code,
        Err(error) => {
            eprintln!("bohio: {error}");
            ExitCode::from(1)
        }
    }
}

/// The process environment the data directory and the programs are looked for in.
pub fn capture_environment() -> Environment {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    Environment {
        windows: cfg!(windows),
        local_app_data: var("LOCALAPPDATA"),
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

/// `bohio links`.
#[derive(Debug, Args)]
pub struct LinksArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// How the links are written.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
    /// How long to wait for each peer to say what it offers.
    #[arg(long, value_name = "TIME", default_value = "5s")]
    pub wait: String,
}
