//! The `bohio` command line.
//!
//! Only this module prints, reads the process environment and installs the diagnostics
//! subscriber; the library below it does none of those.

mod access;
mod commands;
mod gui;
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
    /// Show a PIN, armed on the running node's listeners (or the configured ones when none runs), and accept the one peer that joins with it.
    Pair(PairArgs),
    /// Join a host that shows a PIN, over any endpoint `remote` takes: each side then keeps the other's key.
    Join(JoinArgs),
    /// Install, remove or run the host as a Windows service.
    Service(service::ServiceArgs),
    /// Use a host: info, run a program, read a file or open a shell.
    Remote(RemoteArgs),
    /// List the links this node's running `bohio serve` holds, whichever side made each.
    Links(LinksArgs),
    /// Answer a peer's request to look at or drive this screen, on the running node (`pending`, `allow`, `deny`).
    Gui(gui::GuiArgs),
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
    /// What the peer may do: info, fs.read, fs.write, exec, shell, admin, forward.connect, forward.listen (repeat or
    /// separate with commas). Setting any of `--allow`, `--fs-root`, `--program`, `--connect` and `--listen` replaces the
    /// peer's whole table;
    /// without them an existing table stays and a new peer holds nothing.
    #[arg(long, value_delimiter = ',', value_name = "PERMISSION")]
    pub allow: Vec<String>,
    /// A directory file operations are confined to (repeat).
    #[arg(long, value_name = "DIR")]
    pub fs_root: Vec<PathBuf>,
    /// A program the peer may run, as the host finds it on its own PATH (repeat); with none, any.
    #[arg(long, value_name = "NAME")]
    pub program: Vec<String>,
    /// A target the host may connect to for the peer (repeat; with none, the peer's list is empty): `HOST:PORT` where HOST is
    /// an exact name, `*.suffix`, `*`, an address or a CIDR network (`10.0.0.0/8`, an IPv6 one in brackets) and PORT is `N`,
    /// `A-B` or `*`; `unix:PATH` or `pipe:NAME`, exactly or with `*` and `?`. Naming a rule grants `forward.connect`, which a
    /// rule needs; a name is matched as a name and never resolved, so an address rule matches only an address target.
    #[arg(long, value_name = "RULE")]
    pub connect: Vec<String>,
    /// A bind the host may make for the peer (repeat; with none, the peer's list is empty): `BIND:PORT` where BIND is an
    /// address, a CIDR network or `*` and PORT is `N`, `A-B` or `*` (a host-chosen port, `0`, is allowed only by a rule that
    /// names it, or `*`); `unix:PATH` or `pipe:NAME`, exactly or with `*` and `?`. Naming a rule grants `forward.listen`.
    /// A rule on a non-loopback address lets the peer open a port on the host's network.
    #[arg(long, value_name = "RULE")]
    pub listen: Vec<String>,
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
    /// Where to serve the pairing when no node runs and the configuration has no `[[listen]]` entry, as an entry
    /// would name it. Anyone who can reach it may try the PIN, once. Refused when the configuration has entries.
    #[arg(long, value_name = "ENDPOINT")]
    pub listen: Option<String>,
    /// How long the PIN lasts: a number of seconds or a unit (`30s`, `5m`). Default `[pairing] timeout`, `5m`.
    #[arg(long, value_name = "TIME", value_parser = parse_span)]
    pub timeout: Option<std::time::Duration>,
}

/// For a `webtransport://` host: the UDP payload sizes and path-MTU discovery of the QUIC connection. A flag not given
/// keeps quinn's default (start at 1200, probe up to 1452). On a path with a smaller MTU, such as a tunnel, each probe
/// above it is lost and the OS may report a send error: lower `--mtu-upper-bound` to the path's size, or turn discovery
/// off. A flag with another endpoint is refused.
#[derive(Debug, Args, Default)]
pub struct MtuArgs {
    /// The UDP payload size, in bytes, the connection starts from. Default `1200`, the least QUIC allows. A size above
    /// what the path carries stalls the link until black-hole detection falls to `--mtu-min`.
    #[arg(long, value_name = "BYTES")]
    pub mtu_initial: Option<u16>,
    /// The size the path is known to carry, in bytes: Initial packets are padded to it, and it is the floor. Default `1200`,
    /// the least QUIC allows. Above what the path carries, no connection opens.
    #[arg(long, value_name = "BYTES")]
    pub mtu_min: Option<u16>,
    /// Whether to search for a larger size than `--mtu-initial` (`true` or `false`). Default `true`. Off, no probe is lost
    /// and the size stays at the initial one.
    #[arg(long, value_name = "BOOL")]
    pub mtu_discovery: Option<bool>,
    /// The largest size the search tries, in bytes. Default `1452`; QUIC caps it at 65527. Each probe above the path's
    /// size is lost: set it to the path's size (a tunnel's MTU less 28).
    #[arg(long, value_name = "BYTES")]
    pub mtu_upper_bound: Option<u16>,
}

impl MtuArgs {
    /// `endpoint` with the sizes the flags give, or as it is when none is given.
    ///
    /// # Errors
    ///
    /// Why a flag is refused: the endpoint is not a `webtransport://` one, or a size is below the 1200 bytes QUIC requires.
    pub fn apply(
        &self,
        endpoint: bohio_client::dial::Endpoint,
    ) -> Result<bohio_client::dial::Endpoint, String> {
        if self.mtu_initial.is_none()
            && self.mtu_min.is_none()
            && self.mtu_discovery.is_none()
            && self.mtu_upper_bound.is_none()
        {
            return Ok(endpoint);
        }
        let mut mtu = bohio_client::dial::Mtu::default();
        mtu.initial = self.mtu_initial.unwrap_or(mtu.initial);
        mtu.min = self.mtu_min.unwrap_or(mtu.min);
        mtu.discovery = self.mtu_discovery.unwrap_or(mtu.discovery);
        mtu.upper_bound = self.mtu_upper_bound.unwrap_or(mtu.upper_bound);
        endpoint.with_mtu(mtu)
    }
}

/// `bohio join`.
#[derive(Debug, Args)]
pub struct JoinArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// The host that shows the PIN: any endpoint `bohio remote` takes (`tcp://`, `pipe://`, `ws://`, `wss://`,
    /// `webtransport://`, `serial://`, `stdio://`).
    pub endpoint: String,
    /// The PIN the host shows; read from standard input when absent, so it stays out of process lists.
    #[arg(long, value_name = "PIN")]
    pub pin: Option<String>,
    /// For `wss://` and `webtransport://`: trust the certificates in this PEM file instead of the system's roots.
    /// May be repeated. The pairing's PIN, not TLS, says who the host is.
    #[arg(long, value_name = "FILE")]
    pub ca: Vec<PathBuf>,
    /// For `wss://` and `webtransport://`: trust the certificate with this SHA-256 hash (`sha256:` and 64 hex
    /// digits) whatever its name and issuer. May be repeated.
    #[arg(long, value_name = "HASH")]
    pub cert_hash: Vec<String>,
    /// For `wss://` and `webtransport://`: accept a certificate whose validity starts or ends within this long of
    /// this machine's clock, as for `bohio remote`.
    #[arg(long, value_name = "SPAN", value_parser = parse_span)]
    pub clock_skew: Option<std::time::Duration>,
    /// For a `webtransport://` host: a `ws://` or `wss://` endpoint to dial when the UDP path is blocked.
    #[arg(long, value_name = "ENDPOINT")]
    pub fallback: Option<String>,
    /// The QUIC sizes of a `webtransport://` host.
    #[command(flatten)]
    pub mtu: MtuArgs,
}

/// A time as the configuration writes it: seconds, or a number with `ms`, `s`, `m` or `h`.
fn parse_span(text: &str) -> Result<std::time::Duration, String> {
    crate::config::Span::parse(text)
        .map(|span| span.0)
        .ok_or_else(|| {
            format!("`{text}` is not a time: use a number of seconds or a unit of ms, s, m or h")
        })
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
    /// For `wss://` and `webtransport://`: accept a certificate whose validity starts or ends within this long of this
    /// machine's clock (`30`, `5m`, `1h`; a bare number is seconds), for a clock that differs from the host's. Default
    /// none: a certificate made just now, by a host whose clock is ahead, is refused until this clock reaches its start,
    /// and one that has just expired is refused. Allowing it weakens only the dates, never the hash or the issuer.
    #[arg(long, value_name = "SPAN", value_parser = parse_span)]
    pub clock_skew: Option<std::time::Duration>,
    /// For a `webtransport://` host: a `ws://` or `wss://` endpoint to dial when the UDP path is
    /// blocked. A certificate that is refused never falls back. A `ws://` one is plain until the
    /// link's proof has run, and one on another host sends the proof there: naming either is the choice.
    #[arg(long, value_name = "ENDPOINT")]
    pub fallback: Option<String>,
    /// The QUIC sizes of a `webtransport://` host.
    #[command(flatten)]
    pub mtu: MtuArgs,
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
    /// Write standard input to a file of the host (replacing it, atomically, unless --append or --create).
    Write(RemoteWriteArgs),
    /// Print digests of files on the host, computed there: `HEX  PATH` for one algorithm, `NAME HEX  PATH` for several.
    Hash(RemoteHashArgs),
    /// Open an interactive shell on the host.
    Shell(RemoteShellArgs),
    /// Have the host connect to a target and bridge it to standard input and output (netcat); `ssh -o ProxyCommand` can
    /// use it.
    Connect(RemoteConnectArgs),
    /// Forward a port: listen on this machine and have the host connect to a target for each connection (`ssh -L`), or have
    /// the host listen and bring each connection to a target on this side (`ssh -R`).
    Forward(RemoteForwardArgs),
    /// Look at and drive the host's desktop: snapshots, clicks, keys, text and the clipboard.
    Gui(gui::RemoteGuiArgs),
    /// Have the host listen and bridge one arriving connection to standard input and output (netcat `-l` on the far side);
    /// `--keep` takes the next one after each ends.
    Listen(RemoteListenArgs),
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
    /// Compress the program's output and the input this sends: `ALG` or `ALG:LEVEL`, where ALG is zstd, lz4,
    /// deflate, none or auto (the host's default). The host clamps the level to its caps.
    #[arg(long, value_name = "ALG[:LEVEL]")]
    pub compress: Option<String>,
    /// The program and its arguments, after `--`; run directly, never through a shell.
    #[arg(last = true, required = true, value_name = "ARGV")]
    pub argv: Vec<String>,
}

/// `bohio remote ... read`.
#[derive(Debug, Args)]
pub struct RemoteReadArgs {
    /// The file, in the host's own path text.
    pub path: String,
    /// Have the host compress what it sends: `ALG` or `ALG:LEVEL`, where ALG is zstd, lz4, deflate, none or auto (the
    /// host's default). A file that does not compress (already compressed, or its start does not) is sent raw.
    #[arg(long, value_name = "ALG[:LEVEL]")]
    pub compress: Option<String>,
    /// Have the host compress the whole file first, then send it: the best ratio for a slow link, at the price of the
    /// host's time and disk before the first byte. Needs --compress.
    #[arg(long, requires = "compress")]
    pub prepare: bool,
    /// The zstd window as a power of two (a long window finds repeats further back, at the cost of memory on both
    /// ends); the host clamps it to its own limit.
    #[arg(long, value_name = "LOG2", requires = "compress")]
    pub window_log: Option<u32>,
}

/// `bohio remote ... write`.
#[derive(Debug, Args)]
pub struct RemoteWriteArgs {
    /// The file, in the host's own path text.
    pub path: String,
    /// Compress what is sent: `ALG` or `ALG:LEVEL`, where ALG is zstd, lz4, deflate or auto (the host's default,
    /// read from its `info`). The host decodes within its `[compression]` bounds.
    #[arg(long, value_name = "ALG[:LEVEL]")]
    pub compress: Option<String>,
    /// Compress everything into a temporary file first, then send it: one compression context over the whole input
    /// (the best ratio) at the price of the time and disk before the first byte leaves. Needs --compress.
    #[arg(long, requires = "compress")]
    pub prepare: bool,
    /// The zstd window as a power of two; the host's own limit applies to what it accepts.
    #[arg(long, value_name = "LOG2", requires = "compress")]
    pub window_log: Option<u32>,
    /// Add to the end of the file, creating it, instead of replacing it.
    #[arg(long, conflicts_with = "create")]
    pub append: bool,
    /// Create the file, which must not exist, instead of replacing it.
    #[arg(long)]
    pub create: bool,
    /// Create missing directories above the file.
    #[arg(long)]
    pub parents: bool,
}

/// `bohio remote ... hash`.
#[derive(Debug, Args)]
pub struct RemoteHashArgs {
    /// A digest to compute: md5, sha1, sha256, sha384, sha512, blake3, xxh3, xxh128, xxh64 or crc32c (repeat for
    /// several, computed in one pass). Default sha256.
    #[arg(long, value_name = "NAME")]
    pub algorithm: Vec<String>,
    /// The files, in the host's own path text.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<String>,
}

/// `bohio remote ... connect`.
#[derive(Debug, Args)]
pub struct RemoteConnectArgs {
    /// Where the host connects: `HOST:PORT` (an IPv6 address in brackets), `unix:PATH` or `pipe:NAME`. The peer's grant
    /// must hold `forward.connect` and a rule that matches.
    pub target: String,
    /// Connect, print `connected ADDRESS` and end, with status 0 when the host connected and 1 when it did not.
    #[arg(long)]
    pub check: bool,
}

/// `bohio remote ... forward`.
#[derive(Debug, Args)]
pub struct RemoteForwardArgs {
    /// Listen here and have the host connect to a target for each connection: `[BIND:]PORT=TARGET`,
    /// `unix:PATH=TARGET` or `pipe:NAME=TARGET`, where TARGET is `HOST:PORT`, `unix:PATH` or `pipe:NAME` (repeat). BIND is
    /// an address and defaults to `127.0.0.1`; port 0 picks a free one. Another address is used as named and reported as
    /// `non-default: bind ADDRESS`: whoever can reach it has what this peer is granted. A local socket or pipe is for this
    /// user alone, mode `0o600`. Prints `local BOUND -> TARGET` for each, then runs until interrupted or the link ends
    /// (status 1).
    #[arg(
        short = 'L',
        long = "local",
        value_name = "SPEC",
        required_unless_present = "remote"
    )]
    pub local: Vec<String>,
    /// Have the host listen and bring each connection to a target on this side: `[BIND:]PORT=LOCAL`, `unix:PATH=LOCAL` or
    /// `pipe:NAME=LOCAL`, where LOCAL is `HOST:PORT`, `unix:PATH` or `pipe:NAME`, connected from this machine (repeat; mix
    /// with `-L`). BIND is an address on the host and defaults to `127.0.0.1`; port 0 lets the host choose. Prints
    /// `remote ADDRESS -> LOCAL` for each, then runs until interrupted or the link ends (status 1). The peer needs
    /// `forward.listen` and a `listen` rule that covers the bind; a bind beyond loopback opens the host's network to this
    /// side's target and is reported as `non-default: bind ADDRESS`. A socket or pipe the host creates is for the host's
    /// own account.
    #[arg(
        short = 'R',
        long = "remote",
        value_name = "SPEC",
        required_unless_present = "local"
    )]
    pub remote: Vec<String>,
}

/// `bohio remote ... listen`.
#[derive(Debug, Args)]
pub struct RemoteListenArgs {
    /// What the host binds: `BIND:PORT` (an IPv6 address in brackets; port 0 lets the host choose), `unix:PATH` or
    /// `pipe:NAME`. The peer's grant must hold `forward.listen` and a rule that covers it.
    pub bind: String,
    /// Take the next arrival after each connection ends, until interrupted or the link ends. Without it, the first
    /// connection ends the command: status 0 when both directions ended in order.
    #[arg(long)]
    pub keep: bool,
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
