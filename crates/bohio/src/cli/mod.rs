//! The `bohio` command line.
//!
//! Only this module prints, reads the process environment and installs the diagnostics
//! subscriber; the library below it does none of those.

#![forbid(unsafe_code)]

mod access;
mod commands;
mod gui;
mod host;
mod mcp;
pub(crate) mod output;
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
    /// Print results as JSON for a program to read: one object, or for a command that streams one object on each line; a
    /// command whose output is the payload (a file, a connection, a terminal) writes it unchanged and reports its status as
    /// one object on standard error. Errors are `{"error": {"code", "message"}}` on standard error. See docs/cli-json.md.
    #[arg(long, global = true)]
    pub json: bool,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The commands.
#[derive(Debug, Subcommand)]
#[allow(
    clippy::large_enum_variant,
    reason = "parsed once at the start of a process: boxing a variant buys nothing"
)]
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
    /// Serve the agent tools over the Model Context Protocol on standard input and output: files, programs and sessions of the named hosts.
    Mcp(McpArgs),
    /// List the hosts named in `[remote]`: `bohio remote NAME ...` and a jump list take the name.
    Hosts(HostsArgs),
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

/// `bohio config check`.
#[derive(Debug, Args)]
pub struct CheckArgs {
    /// A configuration layer, lowest first (repeat for more); they are read alone. Otherwise
    /// `BOHIO_CONFIG` names the one layer; otherwise the system file is read and the user's merged over it.
    #[arg(long, value_name = "FILE", env = "BOHIO_CONFIG")]
    pub config: Vec<PathBuf>,
}

/// `bohio info`.
#[derive(Debug, Args)]
pub struct InfoArgs {
    /// A configuration layer, lowest first (repeat for more); they are read alone. Otherwise
    /// `BOHIO_CONFIG` names the one layer; otherwise the system file is read and the user's merged over it.
    #[arg(long, value_name = "FILE", env = "BOHIO_CONFIG")]
    pub config: Vec<PathBuf>,
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
    /// What the peer may do: info, fs.read, fs.write, exec, shell, admin, forward.connect, forward.listen, session (repeat or
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
    /// Reach the host through another one, as `ssh -J` does (repeat for several hops, in order): `tcp://`, `pipe://` or `stdio://`
    /// endpoints, the first dialed from here and each next one carried by the one before it. This side proves itself to every
    /// hop and each hop's host to this side end to end, so a jump sees only sealed bytes and may be any host that holds a
    /// `forward.connect` rule for the next hop (or `exec` of the program, for `stdio://`). Not with `--via`.
    #[arg(long, value_name = "ENDPOINT", conflicts_with = "via")]
    pub jump: Vec<String>,
    /// The id each `--jump` host must prove, in the same order (repeat). A hop with no id here trusts the keys `bohio join` stored,
    /// as the host does without `--server`.
    #[arg(long, value_name = "ID", requires = "jump")]
    pub jump_server: Vec<String>,
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
    /// How long to wait for the host's first answer to a question (`info`, the start of a read, a shell or a desktop
    /// session): a number of seconds or text with a unit. Default `[client] reply_timeout` (30s).
    #[arg(long, value_name = "TIME", value_parser = parse_span)]
    pub reply_timeout: Option<std::time::Duration>,
    /// How long one step of a transfer (`put`, `get`, `write`) may wait for the host; `0` waits without limit. Default
    /// `[client] transfer_timeout` (5m).
    #[arg(long, value_name = "TIME", value_parser = parse_span)]
    pub transfer_timeout: Option<std::time::Duration>,
    /// What to ask of it.
    #[command(subcommand)]
    pub action: RemoteAction,
}

/// The `bohio remote` actions.
#[derive(Debug, Subcommand)]
pub enum RemoteAction {
    /// Print the host's capabilities.
    Info,
    /// Run a program on the host and exit with its status.
    Exec(RemoteExecArgs),
    /// Write a file of the host to standard output.
    Read(RemoteReadArgs),
    /// Write standard input to a file of the host (replacing it, atomically, unless --append or --create).
    Write(RemoteWriteArgs),
    /// Print digests of files on the host, computed there: `HEX  PATH` for one algorithm, `NAME HEX  PATH` for several.
    Hash(RemoteHashArgs),
    /// Send a local file to the host, replacing it atomically; only what changed travels when the host has an older copy.
    Put(RemotePutArgs),
    /// Fetch a file of the host to a local file, replacing it atomically; only what changed travels when this side has an
    /// older copy.
    Get(RemoteGetArgs),
    /// List a tree of the host, one line for each entry, in one stream.
    Walk(RemoteWalkArgs),
    /// List the paths below a root that match a glob, on the host, in one stream (`**/*.rs`).
    Glob(RemoteGlobArgs),
    /// Print what the host is running on: name, uptime, processors, load, memory and disks.
    Stats,
    /// Print where each name is on the host's own `PATH` (`NAME  PATH`, or `NAME  -` when there is none). Exits 1 when a
    /// name was not found.
    Which(RemoteWhichArgs),
    /// Print the lines of the host's files that match a pattern, found on the host: `PATH:LINE:TEXT`. Exits 0 when a line
    /// matched, 1 when none did, 2 when something could not be read.
    Search(RemoteSearchArgs),
    /// Make a tree of the host and a local tree the same, moving only what differs: small files in bundles, large ones by delta.
    Sync(RemoteSyncArgs),
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
    /// Pick up a detached session (started with `exec --detach` or `shell --detach`): its kept output from an offset, then
    /// live, with typing and Ctrl-C going to its program.
    Attach(RemoteAttachArgs),
    /// List the detached sessions this peer holds on the host.
    Sessions,
    /// End a detached session: its program is sent a signal (`kill`, ending the whole tree, unless `--signal` says otherwise).
    End(RemoteEndArgs),
}

/// `bohio remote ... which`.
#[derive(Debug, Args)]
pub struct RemoteWhichArgs {
    /// The program names to look up on the host's own `PATH`.
    #[arg(required = true, value_name = "NAME")]
    pub names: Vec<String>,
}

/// `bohio remote ... search`.
#[derive(Debug, Args)]
pub struct RemoteSearchArgs {
    /// The pattern: a regular expression (linear-time, so none can run away), or text with `-F`.
    pub pattern: String,
    /// The directories (or files) to search, in the host's own path text. The host's first root when absent.
    #[arg(value_name = "PATH", default_value = ".")]
    pub paths: Vec<String>,
    /// Search only files whose path or name matches this glob (repeatable).
    #[arg(short = 'g', long = "glob", value_name = "GLOB")]
    pub include: Vec<String>,
    /// Leave out entries matching this glob, and what is under a directory that does (repeatable).
    #[arg(short = 'x', long = "exclude", value_name = "GLOB")]
    pub exclude: Vec<String>,
    /// Ignore case.
    #[arg(short = 'i', long)]
    pub ignore_case: bool,
    /// Take the pattern as plain text.
    #[arg(short = 'F', long)]
    pub fixed_strings: bool,
    /// Lines of context before and after each match (the host's `[search] max_context` bounds it).
    #[arg(short = 'C', long, value_name = "N", default_value_t = 0)]
    pub context: u64,
    /// Search names that start with a dot too.
    #[arg(long)]
    pub hidden: bool,
    /// How far below a path to go: `0` is the paths themselves. The whole tree when absent.
    #[arg(long, value_name = "N")]
    pub depth: Option<u64>,
    /// The most matches to print (at most the host's `[search] max_matches`).
    #[arg(long, value_name = "N")]
    pub max_matches: Option<u64>,
    /// The most files to read (at most the host's `[search] max_files`).
    #[arg(long, value_name = "N")]
    pub max_files: Option<u64>,
}

/// `bohio remote ... glob`.
#[derive(Debug, Args)]
pub struct RemoteGlobArgs {
    /// The glob: a path below each root, or a name, matched as `**/*.rs` is.
    pub glob: String,
    /// The directories to look in, in the host's own path text. The host's first root when absent.
    #[arg(value_name = "PATH", default_value = ".")]
    pub paths: Vec<String>,
    /// How far below a path to go. The whole tree when absent.
    #[arg(long, value_name = "N")]
    pub depth: Option<u64>,
}

/// What `exec` and `shell` take to start a detached session.
#[derive(Debug, Args)]
pub struct SessionArgs {
    /// Start a detached session: the program keeps running when this command ends or the link is lost, its output is kept on
    /// the host, and the command prints the session's id and exits. Pick it up again with `attach`. Needs `session` in the
    /// peer's grant; the host's `[sessions]` bound what it keeps.
    #[arg(long)]
    pub detach: bool,
    /// A name for the session in `sessions`.
    #[arg(long, value_name = "NAME", requires = "detach")]
    pub name: Option<String>,
    /// How many bytes of output the host keeps (at most its `max_scrollback`; its `scrollback` when absent).
    #[arg(long, value_name = "BYTES", requires = "detach")]
    pub scrollback: Option<u64>,
    /// What the host does when the kept output is full: `pause` the program until a client has taken it (the default for an
    /// exec) or forget the oldest (`drop-oldest`, the default for a shell).
    #[arg(long, value_enum, requires = "detach")]
    pub on_full: Option<OnFullArg>,
    /// How long the host keeps the session while nobody is attached before it ends it (`10m`, `2h`; at most its own
    /// `linger`).
    #[arg(long, value_name = "SPAN", requires = "detach", value_parser = parse_span)]
    pub linger: Option<std::time::Duration>,
}

/// `--on-full`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum OnFullArg {
    /// Stop the program until the output is taken.
    Pause,
    /// Forget the oldest output.
    DropOldest,
}

/// `bohio remote ... attach`.
#[derive(Debug, Args)]
pub struct RemoteAttachArgs {
    /// The session's id, from `exec --detach`, `shell --detach` or `sessions`.
    pub id: String,
    /// Read from this output offset: `0` is all the host still keeps, and the host says on standard error how much of
    /// what was asked for it had already forgotten. Without it only what comes next is shown.
    #[arg(long, value_name = "OFFSET")]
    pub from: Option<u64>,
    /// For an exec session: send this process's standard input to the program.
    #[arg(long)]
    pub stdin: bool,
    /// How soon a second Ctrl-C after the first (`1s`, `500ms`; a bare number is seconds) ends the program's whole tree
    /// instead of interrupting it again.
    #[arg(long, value_name = "SPAN", default_value = "2s", value_parser = parse_span)]
    pub kill_after: std::time::Duration,
}

/// `bohio remote ... end`.
#[derive(Debug, Args)]
pub struct RemoteEndArgs {
    /// The session's id.
    pub id: String,
    /// The signal to end it with: interrupt, terminate, hangup, quit, user1, user2 or kill. The session then reports its exit
    /// and is kept for reading, as the host's `keep_exited` says.
    #[arg(long, value_name = "SIGNAL", value_parser = parse_signal)]
    pub signal: Option<bohio_proto::signal::Signal>,
}

fn parse_signal(text: &str) -> Result<bohio_proto::signal::Signal, String> {
    bohio_proto::signal::Signal::from_name(text).ok_or_else(|| {
        "a signal is one of interrupt, terminate, hangup, quit, user1, user2, kill".to_owned()
    })
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
    #[arg(long, value_name = "ALG[:LEVEL]", conflicts_with = "pty")]
    pub compress: Option<String>,
    /// Run the program on a terminal (the host's `bohio-shell/1`, under the `shell` grant and its program list): its output
    /// and error are one stream, it takes this terminal's typing and size, and a Ctrl-C typed here reaches it as a typed
    /// Ctrl-C. Without a terminal here the first Ctrl-C sends an `interrupt`.
    #[arg(long)]
    pub pty: bool,
    /// How soon a second Ctrl-C after the first (`1s`, `500ms`; a bare number is seconds) ends the program's whole tree
    /// instead of interrupting it again. The first Ctrl-C of this command is an `interrupt` to the program, not the end of
    /// this command.
    #[arg(long, value_name = "SPAN", default_value = "2s", value_parser = parse_span)]
    pub kill_after: std::time::Duration,
    /// A detached session instead of a run to the end.
    #[command(flatten)]
    pub session: SessionArgs,
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
    /// Keep the file open and print what is appended, as `tail -f` does, until interrupted. The host's `[filesystem]
    /// follow_interval` says how often it looks; the file is sent from its start unless `--tail` says otherwise. Not with
    /// the compression options.
    #[arg(long, short = 'f', conflicts_with_all = ["compress", "prepare", "window_log"])]
    pub follow: bool,
    /// As `--follow`, and when another file takes the path (a log rotation) follow that one from its start.
    #[arg(short = 'F', conflicts_with_all = ["compress", "prepare", "window_log", "follow"])]
    pub follow_name: bool,
    /// With `--follow` or `-F`: start this many bytes before the end of the file (`0` prints only what is appended).
    #[arg(long, value_name = "BYTES")]
    pub tail: Option<u64>,
    /// With `--follow` or `-F`: how often the host looks for more bytes (`500ms`, `2s`).
    #[arg(long, value_name = "SPAN", value_parser = parse_span)]
    pub interval: Option<std::time::Duration>,
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

/// Whether a transfer uses a delta against the copy already at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DeltaChoice {
    /// A delta when the destination's copy is at least the host's `[delta] min_size`.
    Auto,
    /// A delta whenever the destination has a copy.
    Always,
    /// The whole file.
    Never,
}

/// How far to compare before a file is moved at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CompareChoice {
    /// Size and modification time.
    Meta,
    /// Size, then a digest (blake3) of each side.
    Hash,
    /// Size, then the block signature of each side.
    Blocks,
}

/// The options `put` and `get` share.
#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Whether to send only what differs from the copy already at the destination.
    #[arg(long, value_enum, default_value = "auto")]
    pub delta: DeltaChoice,
    /// Move nothing when the two files are the same by this comparison.
    #[arg(long, value_enum)]
    pub compare: Option<CompareChoice>,
    /// Compress what travels: `ALG` or `ALG:LEVEL`, where ALG is zstd, lz4, deflate or auto (the host's default).
    #[arg(long, value_name = "ALG[:LEVEL]")]
    pub compress: Option<String>,
    /// The zstd window as a power of two; the host clamps it to its own limit.
    #[arg(long, value_name = "LOG2", requires = "compress")]
    pub window_log: Option<u32>,
    /// The size of a block of the signature, in bytes; by default the host's bounds and the file's size choose it.
    #[arg(long, value_name = "BYTES")]
    pub block_size: Option<u32>,
}

/// `bohio remote ... put`.
#[derive(Debug, Args)]
pub struct RemotePutArgs {
    /// The local file.
    pub local: PathBuf,
    /// The file on the host, in the host's own path text.
    pub remote: String,
    /// The options of a transfer.
    #[command(flatten)]
    pub sync: SyncArgs,
}

/// `bohio remote ... get`.
#[derive(Debug, Args)]
pub struct RemoteGetArgs {
    /// The file on the host, in the host's own path text.
    pub remote: String,
    /// The local file.
    pub local: PathBuf,
    /// The options of a transfer.
    #[command(flatten)]
    pub sync: SyncArgs,
}

/// `bohio remote ... walk`.
#[derive(Debug, Args)]
pub struct RemoteWalkArgs {
    /// The directories (or files) to list, in the host's own path text.
    #[arg(required = true, value_name = "PATH")]
    pub paths: Vec<String>,
    /// How far below a path to go: `0` is the paths themselves (a batched stat). The whole tree when absent.
    #[arg(long, value_name = "N")]
    pub depth: Option<u64>,
    /// Enter links to directories (inside the path only).
    #[arg(long)]
    pub follow: bool,
    /// List only entries whose path or name matches this glob (repeatable).
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,
    /// Leave out entries matching this glob, and what is under a directory that does (repeatable).
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,
    /// A digest of each file's content, computed on the host (see `hash` for the names).
    #[arg(long, value_name = "NAME")]
    pub hash: Option<String>,
    /// Compress the listing: `ALG` or `ALG:LEVEL`, where ALG is zstd, lz4, deflate or auto (the host's default).
    #[arg(long, value_name = "ALG[:LEVEL]")]
    pub compress: Option<String>,
}

/// Which way `sync` copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SyncDirection {
    /// The local tree is copied to the host.
    Push,
    /// The host's tree is copied here.
    Pull,
}

/// `bohio remote ... sync`.
#[derive(Debug, Args)]
pub struct RemoteSyncArgs {
    /// Which way to copy.
    #[arg(value_enum)]
    pub direction: SyncDirection,
    /// The local directory.
    pub local: PathBuf,
    /// The directory on the host, in the host's own path text.
    pub remote: String,
    /// Remove from the destination what the source does not have.
    #[arg(long)]
    pub delete: bool,
    /// Change nothing: say what would be done.
    #[arg(long)]
    pub dry_run: bool,
    /// How a file on both sides is told to be the same: `meta` (size and time), `hash` (blake3 of each side) or `blocks`.
    #[arg(long, value_enum, default_value = "meta")]
    pub compare: CompareChoice,
    /// The largest file moved in a bundle, in bytes; larger ones go one by one, by delta against the old copy.
    #[arg(long, value_name = "BYTES", default_value_t = 1_048_576)]
    pub bundle_max: u64,
    /// The most files in one bundle.
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub bundle_items: usize,
    /// How many bundles and large files move at once.
    #[arg(long, value_name = "N", default_value_t = 4)]
    pub parallel: usize,
    /// Compress what travels: `ALG` or `ALG:LEVEL`, where ALG is zstd, lz4, deflate or auto (the host's default).
    #[arg(long, value_name = "ALG[:LEVEL]")]
    pub compress: Option<String>,
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
    /// Whether and how the link is made again when it is lost.
    #[command(flatten)]
    pub persist: KeepArgs,
}

/// What a `bohio remote` command that stays up does when its link is lost: it makes a new one, with the waits of the peer's
/// `[dial]` table (or the dial defaults), and the listeners it holds stay up meanwhile. A running bridged connection still
/// ends with the link.
#[derive(Debug, Args)]
pub struct KeepArgs {
    /// End when the link is lost (status 1) instead of making a new one. The first link is always made once: a host that
    /// cannot be reached, or that does not trust this key, ends the command at once.
    #[arg(long)]
    pub once: bool,
    /// The first wait before making the link again, as `retry_min` of `[peer."<id>".dial]` (`1s`).
    #[arg(long, value_name = "TIME")]
    pub retry_min: Option<String>,
    /// The longest wait, as `retry_max` (`60s`).
    #[arg(long, value_name = "TIME")]
    pub retry_max: Option<String>,
    /// What each further failure multiplies the wait by, as `retry_factor` (`2`).
    #[arg(long, value_name = "FACTOR")]
    pub retry_factor: Option<f64>,
    /// How far a wait may move, as a fraction of itself from 0 to 1, as `retry_jitter` (`0.1`).
    #[arg(long, value_name = "FRACTION")]
    pub retry_jitter: Option<f64>,
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
        required_unless_present_any = ["remote", "dynamic"]
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
        required_unless_present_any = ["local", "dynamic"]
    )]
    pub remote: Vec<String>,
    /// Serve SOCKS5 here and have the host connect to whatever each client asks for (`ssh -D`): `[BIND:]PORT`. BIND is an address
    /// and defaults to `127.0.0.1`; port 0 picks a free one. Clients use CONNECT with IPv4, IPv6 or a domain name, which the host
    /// resolves, with no authentication, and the peer's grant decides every target: whoever can reach the port has what the
    /// peer may connect to, so another address is used as named and reported as `non-default: bind ADDRESS`. BIND and UDP
    /// ASSOCIATE are answered "command not supported". Prints `socks5 BOUND`. The peer needs `forward.connect` and rules for the
    /// targets (repeat; mix with `-L` and `-R`).
    #[arg(
        short = 'D',
        long = "dynamic",
        value_name = "SPEC",
        required_unless_present_any = ["local", "remote"]
    )]
    pub dynamic: Vec<String>,
    /// How long a SOCKS5 client has to finish its handshake before it is closed (`10s`; a bare number is seconds). Default
    /// `[forward] socks_timeout`.
    #[arg(long, value_name = "TIME", value_parser = parse_span)]
    pub socks_timeout: Option<std::time::Duration>,
    /// Whether and how the link is made again when it is lost.
    #[command(flatten)]
    pub persist: KeepArgs,
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
    /// Whether and how the link is made again when it is lost.
    #[command(flatten)]
    pub persist: KeepArgs,
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
    /// How soon a second Ctrl-C after the first (`1s`, `500ms`; a bare number is seconds) ends the shell's whole tree
    /// instead of interrupting it again. Only when this terminal does not deliver Ctrl-C as typing (input is not a
    /// terminal); on a terminal the typed Ctrl-C goes to the shell as it is.
    #[arg(long, value_name = "SPAN", default_value = "2s", value_parser = parse_span)]
    pub kill_after: std::time::Duration,
    /// A detached session instead of one that ends with this command.
    #[command(flatten)]
    pub session: SessionArgs,
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
    output::set_json(cli.json);
    install_subscriber();
    match commands::run(&cli.command, environment) {
        Ok(code) => code,
        Err(error) => {
            output::fail(&error);
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

/// `bohio hosts`.
#[derive(Debug, Args)]
pub struct HostsArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
}

/// `bohio links`.
#[derive(Debug, Args)]
pub struct LinksArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// How long to wait for each peer to say what it offers. Default `[client] links_wait` (5s).
    #[arg(long, value_name = "TIME")]
    pub wait: Option<String>,
}

/// `bohio mcp`.
#[derive(Debug, Args)]
pub struct McpArgs {
    /// The configuration: this node's identity, and the keys of the hosts it trusts. Give each agent its own data directory, so
    /// each agent's key gets its own grant on a host.
    #[command(flatten)]
    pub config: ConfigOption,
    /// A host the agent may use: `[NAME=]ENDPOINT`, or the name of a host in `[remote]` (`bohio hosts`), followed by any flags of `bohio remote` (`--server ID`, `--jump ENDPOINT`,
    /// `--ca FILE`, ...), the whole text in quotes. Repeat for several hosts: a call then names its host. The link is made
    /// when the host is first used, and once more after a loss.
    #[arg(long, value_name = "[NAME=]ENDPOINT|NAME [FLAGS]")]
    pub host: Vec<String>,
    /// Bytes of one output stream a call returns by default (`bohio_client::tools::Settings::output`). Default 65536.
    #[arg(long, value_name = "BYTES")]
    pub max_output: Option<u64>,
    /// A directory of this machine the `put` and `get` tools may use (repeat). With none, those tools are unavailable.
    #[arg(long, value_name = "DIR")]
    pub local_root: Vec<PathBuf>,
    /// A target of this machine (`HOST:PORT`) that `forward_remote` may bridge the host's listener to (repeat). With none, that
    /// tool is unavailable.
    #[arg(long, value_name = "HOST:PORT")]
    pub forward_local_target: Vec<String>,
    /// Let `forward_local` listen beyond loopback: whoever reaches the listener has this key's grant on the host.
    #[arg(long)]
    pub forward_allow_remote_bind: bool,
    /// Calls that run at once; one more is answered as busy. Default 8.
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub max_calls: usize,
}
