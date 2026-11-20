# `bohio` — API header

Header-file-style reference for the `bohio` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

| Feature | Adds | Needed for | Default |
| --- | --- | --- | --- |
| `cli` | module `cli`, the `bohio` binary; `clap`, `tracing-subscriber`, `serde_json`, `crossterm` | the command | on |

MSRV Rust 1.89. A library-only build (`default-features = false`) depends on `bohio-proto`, `bohio-client` (the link and its relay),
`coqui-transport`, `coqui-link` (with `sealed`), `coqui-peer`, `coqui-transport-pipe` and `coqui-transport-web` (each a git revision; one copy of each builds), `serde`, `toml`,
`thiserror`, `tracing`, `portable-pty` (terminals), `cap-std` and `cap-fs-ext` (confined file access), `sha2`, and `rustix` (Unix) or
`win32job` (Windows). Version `0.0.0-alpha.0`, not published.

Imports: the root holds `Config`, `ConfigError`, `Environment`, `Grant`, `Permission`, `Policies`, `PolicyError`. Public modules:
`config`, `control`, `exec`, `fs`, `info`, `limit`, `link`, `listen`, `peer`, `policy`, `serve`, `shell`, `cli` (feature `cli`). No `unsafe` code
(`#![forbid(unsafe_code)]`). The library prints nothing, reads no environment variable and installs no logger; it starts threads only inside
`exec::serve`, `shell::serve`, the transfer and control engines, `serve::Server` (a thread per connection, per stream and per relay) and
`peer::pair`.

## Configuration (`bohio::config`)

```rust
pub struct Environment { pub windows: bool, pub config: Option<PathBuf>, pub app_data: Option<PathBuf>, pub local_app_data: Option<PathBuf>,
    pub program_data: Option<PathBuf>, pub xdg_config_home: Option<PathBuf>, pub xdg_data_home: Option<PathBuf>, pub home: Option<PathBuf>,
    pub path: Option<OsString> }                                  // Default; the binary fills it from the process (cli::capture_environment)
impl Environment { pub fn config_candidates(&self) -> Vec<PathBuf>; pub fn default_data_dir(&self) -> PathBuf; }

pub struct Config { pub service: ServiceConfig, pub listen: Vec<ListenConfig>, pub link: LinkConfig, pub filesystem: FilesystemConfig,
                    pub execution: ExecutionConfig, pub shell: ShellConfig }     // Default; unknown keys refused
impl Config {
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError>;       // `path` only labels errors
    pub fn load(path: &Path) -> Result<Self, ConfigError>;
    pub fn locate(explicit: Option<&Path>, environment: &Environment) -> Result<(Self, Option<PathBuf>), ConfigError>;
    pub fn data_dir(&self, environment: &Environment) -> PathBuf;
}
pub struct Span(pub Duration);                                    // a number of seconds, or text: 250ms, 5s, 2m, 1h; Span::parse(&str) -> Option<Span>
#[non_exhaustive] pub enum ConfigError { Read { path, reason }, Parse { path, line, column, message }, Invalid { path, key, message } }
```

`bohio.cfg` is TOML; every key is optional and an unknown key or section is `ConfigError::Parse` naming it, with line and column
(the line's content is never quoted). Defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `[service] name` | `"bohio"` | name shown in `info` |
| `[service] check_access` | `true` | Windows: refuse to start when another account may change the configuration file or data directory; `false` logs it and serves |
| `[service] data_dir` | `bohio` under `%LOCALAPPDATA%` / `$XDG_DATA_HOME` (else `~/.local/share`) | identity, trust store, `peers.cfg` |
| `[[listen]] endpoint` | none | `tcp://host:port`, `pipe://name` or `pipe:///path.sock`, `ws://host:port`, `wss://host:port`, `webtransport://host:port`, `serial://port?baud=N`; required in each entry |
| `[[listen]] socket_mode` | `"0o600"` | `pipe`, Unix: the socket's mode, octal text with the `0o` prefix |
| `[[listen]] remote_clients` | `false` | `pipe`, Windows: clients on other hosts through file sharing |
| `[[listen]] origins` | none | `ws`, `wss`, `webtransport`: the origins a browser may connect from; none allows any |
| `[[listen]] tls_cert`, `tls_key` | none | `wss`, `webtransport`: PEM files, the certificate (leaf first, then its intermediates) and its private key; both or neither; `wss` needs both; read at start, never logged; a `webtransport` entry with neither makes its own certificate |
| `[[listen]] tls_valid_days` | `13` | `webtransport` without `tls_cert`: the made certificate's validity in days, at least 1 (a browser pins at most 14: a longer one is logged once as not browser-pinnable); a new one is made halfway through and its hash logged |
| `[[listen]] baud`, `data_bits`, `parity`, `stop_bits`, `flow_control` | `115200`, `8`, `none`, `1`, `none` | `serial`: the line; the endpoint's query (`?baud=9600`) wins; `flow_control` is `none` or `hardware`; a key for another scheme is refused naming it |
| `[link] recheck` | `5s` | how often a running stream's permission is decided again; not zero |
| `[link] max_streams` | `64` | streams per peer; at least 1 |
| `[link] max_connections` | `128` | links open at once across peers, those still proving included; one past it is closed unread; at least 1 |
| `[link] max_unproven_per_source` | `16` | connections from one source (an IPv4 address or an IPv6 /64) still proving themselves; one past it is closed unread; a pipe, serial line or stdio link has no source; at least 1 |
| `[link] max_links_per_peer` | `16` | links one proven peer may have open; one past it is closed after its proof; at least 1 |
| `[link] proof_timeout` | `10s` | how long a peer has to prove itself; not zero |
| `[link] window` | `1048576` | bytes a peer may send on one stream ahead of what the host has read (the credit window); at least 1 |
| `[link] max_frame` | `65536` | most bytes of one message in one frame (a longer message is sent in parts); at least 1 |
| `[link] max_message` | `1048576` | largest message on a stream; more than the largest `chunk_size` |
| `[link] queue_bytes` | `262144` | bytes a stream holds each way between the link and its engine; a larger message passes alone; at least 1 |
| `[link] read_ahead` | `8` | messages the socket's reader holds ahead of the link once a peer is proven (one before); at least 1 |
| `[link] queued_bytes` | `1048576` | bytes the socket's writer may have queued; at least 1 |
| `[link] send_limit` | `10s` | how long a frame the socket takes nothing of ends the link, and a finished stream's queue is given to drain; not zero |
| `[link] send_wait` | `250ms` | how long a send waits for credit before it reports no progress and is tried again; not zero |
| `[link] poll_interval` | `50ms` | how often a stream's threads look at whether it was closed; not zero |
| `[filesystem] enabled` | `true` | whether filesystem calls are served |
| `[filesystem] max_transfers` | `16` | concurrent `bohio-read/1` and `bohio-write/1` streams across peers; at least 1 |
| `[filesystem] chunk_size` | `65536` | most file bytes in one frame, and the buffer a copy or checksum reads with; at least 1; a smaller message bound of the stream wins |
| `[filesystem] list_page` | `1000` | most entries in one listing page, whatever the client asks for; at least 1 |
| `[filesystem] idle_timeout` | `5m` | how long a transfer may make no progress before the host abandons it; not zero |
| `[filesystem] poll_interval` | `20ms` | how often a transfer looks at its stream while it waits; not zero |
| `[filesystem] sync_writes` | `true` | whether a write is flushed to storage before it is put in place |
| `[execution] enabled` | `true` | whether processes are run |
| `[execution] max_processes` | `16` | concurrent processes across peers; at least 1 |
| `[execution] chunk_size` | `16384` | most output bytes in one frame; at least 1; a smaller message bound of the stream wins |
| `[execution] poll_interval` | `20ms` | how often a process, its deadline and its stream are looked at; not zero |
| `[execution] drain_timeout` | `500ms` | wait for a process's remaining output after it exits, before the rest of its tree is ended |
| `[shell] enabled` | `true` | whether shell sessions are opened |
| `[shell] max_sessions` | `8` | concurrent sessions across peers; at least 1 |
| `[shell] chunk_size` | `16384` | most terminal bytes in one frame; at least 1; a smaller message bound of the stream wins |
| `[shell] poll_interval` | `20ms` | how often a session looks at its program and its stream; not zero |
| `[shell] drain_quiet` | `200ms` | how long the terminal must stay quiet after the program exited before the session ends |
| `[shell] kill_wait` | `2s` | how long the host waits for the output reader after the tree was ended before it reports the exit regardless |

`locate`: `explicit` alone when given (must exist); else the first existing of `BOHIO_CONFIG` (must exist when set), the user's file
(`%APPDATA%\bohio\bohio.cfg`, `$XDG_CONFIG_HOME/bohio/bohio.cfg`, else `~/.config/bohio/bohio.cfg`), the system's
(`%ProgramData%\bohio\bohio.cfg`, `/etc/bohio/bohio.cfg`); with none, the defaults and `None`. Out-of-range values are
`ConfigError::Invalid` naming the key (`link.max_streams`, ...).

## Peer grants (`bohio::policy`)

```rust
#[non_exhaustive] pub enum Permission { Info, FsRead, FsWrite, Exec, Shell, Admin }   // Ord; names "info","fs.read","fs.write","exec","shell","admin"
impl Permission { pub const fn name(self) -> &'static str; }
pub struct Grant;                                                // Default = nothing
impl Grant { pub fn none() -> Self; pub fn allowing(permissions) -> Self; pub fn with_fs_roots(self, roots) -> Self; pub fn with_programs(self, programs) -> Self; pub fn restricts_programs(&self) -> bool;
             pub fn allows(&self, Permission) -> bool; pub fn permissions(&self) -> impl Iterator<Item = Permission>; pub fn fs_roots(&self) -> &[PathBuf];
             pub fn permits_program(&self, program: &str) -> bool; pub fn permits_shell_script(&self) -> bool;
             pub fn with_env(self, names) -> Self; pub fn env_names(&self) -> &[String]; pub fn permits_env(&self, env: &bohio_proto::exec::Env) -> bool; }
pub struct Policies;                                             // Default = no peer
impl Policies { pub fn from_toml(text, path: &str) -> Result<Self, PolicyError>; pub fn load(path: &Path) -> Result<Self, PolicyError>;   // missing file: empty
                pub fn grant_for(&self, peer: &str) -> Grant; pub fn peers(&self) -> impl Iterator<Item = &str>; }
#[non_exhaustive] pub enum PolicyError { Read { path, reason }, Parse { path, line, column, message } }
```

`peers.cfg`: `[peer.<id>]` with `allow = [..]`, `fs_roots = [..]`, and `[peer.<id>.exec]` with `programs = [..]` and `env = [..]`. An unknown key or
permission is `PolicyError::Parse`. A peer with no entry, or no `allow`, holds nothing. `programs` empty means any program and
explicit shell scripts; non-empty means only those programs, each named exactly as the peer asks for it, and no shell script
(a shell runs anything). Under a non-empty list each name is resolved on the host's own `PATH` (a name with a directory
must be absolute; a relative one is `not_found`) and that absolute path is started, never what the request's `PATH` or working
directory would find, and a request may set or remove only the variables `env` names (exactly; without case on Windows), `PATH` too only when `env`
names it (the program still starts from the host's own `PATH`), and may not clear the environment, else it is `denied`: a variable is as strong as an argument (`LD_PRELOAD`,
`DYLD_INSERT_LIBRARIES`, `BASH_ENV` load or run code the peer chose). `permits_env` is that check; without a program list it always holds. The list
limits which program starts, not what it does: `git -c alias.x=!cmd`, `make`, `find -exec` and interpreters run anything they are
given, so list only programs that are safe with every argument. `fs_roots` empty means no file operation is allowed.

## Execution (`bohio::exec`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a ExecutionConfig, pub slots: &'a Slots, pub path: &'a OsStr }   // Copy; `path` is the host's own PATH
pub fn serve(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<bohio_proto::exec::Exit, ErrorBody>;
```

Serves one `bohio-exec/1` stream: `metadata` is the open's CBOR request. Order of checks: decode (`bad_request`), execution enabled
(`unsupported`), the grant holds `exec` (`denied`), the command (empty argv or a NUL: `bad_request`; unlisted program or a shell
script under a program list: `denied`), the working directory exists (`not_found`), a free slot (`busy`), the start
(`not_found`, `denied`, else `io`). A refused request gets one `error` frame, the same error is returned, and the stream stays
open for the caller to close.

A started process runs in a tree of its own (a job object on Windows; a process group on Unix, plus a walk of the descendants (`/proc` on Linux, `ps` elsewhere)
for processes that left it). Frames out: `started`, then `stdout` and `stderr` chunks as the process writes them, then `exit`
after every output byte. The tree is ended by a `kill` frame, the stream closing or failing, the request's `timeout_ms`
(`Exit::timed_out` and `killed`), and always after the process exits: output is awaited for `drain_timeout`, then whatever the
process left running is killed, so nothing outlives a request. Sending waits while the stream has no room, so a slow client
slows the process instead of growing a buffer. `serve` does not close the channel.

`bohio::limit`: `Slots::new(limit)`, `Slots::acquire() -> Option<Permit<'_>>`, `in_use()`, `limit()`; a `Permit` frees its slot
when dropped. One `Slots` is shared by every stream that counts against a bound.

## Shells (`bohio::shell`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a ShellConfig, pub slots: &'a Slots, pub path: &'a OsStr }   // Copy
pub fn serve(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<bohio_proto::shell::Exit, ErrorBody>;
```

Serves one `bohio-shell/1` stream: `metadata` is the open's CBOR request, `path` the host's `PATH` (where a shell kind or a program
without a directory is looked for; the library reads no environment variable). One stream is one session, bounded by
`[shell] max_sessions` across peers. Order of checks: decode (`bad_request`), shells enabled (`unsupported`), the grant holds `shell`
(`denied`), the program (`Program::Kind`: `Other` is `bad_request`, a shell not found on `path` is `not_found`; `Program::Argv`: empty
or a NUL is `bad_request`, a name not on `path` is `not_found`, and under a non-empty `exec.programs` list a name not on it is `denied`,
while a kind is always allowed: `shell` already means a shell), the size (`bad_request` outside `1..=32767`), the working
directory (`not_found`), a free slot (`busy`), the start (`not_found`, `denied`, else `io`). A refused request gets one `error` frame,
the same error is returned, and the stream stays open for the caller to close.

The program runs on a terminal (`portable-pty`: `openpty` on Unix, ConPTY on Windows) in a tree of its own (a job object on Windows; on
Unix `portable-pty` makes it a session and group leader, and the host also walks the descendants (`/proc` on Linux, `ps` elsewhere) for what left the group, such as a
job-control shell's background jobs). Frames out: `started {pid}`, `data` as the terminal produces output, then `exit` after every
output byte (the terminal must be quiet for `drain_quiet` after the program exited, since a pseudo-console can deliver the last of
the output after the exit). The tree is ended by a `kill` frame, the stream closing or failing, and always after the program exits:
nothing outlives a session. `TERM` is `xterm-256color` unless the request's environment sets it. A resize outside `1..=32767` is
ignored. Typed bytes go to the terminal as they are (Enter is `\r`). Sending waits while the stream has no room, so a client that
reads slowly slows the shell.

## Filesystem (`bohio::fs`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a FilesystemConfig, pub slots: &'a Slots }   // Copy
pub fn call(context: &Context<'_>, call: &bohio_proto::control::Call, budget: usize) -> Result<Reply, ErrorBody>;
pub fn serve_read(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<u64, ErrorBody>;     // bytes sent
pub fn serve_write(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<WriteDone, ErrorBody>;
```

`call` answers one filesystem call of the control stream; `budget` is the largest reply the stream carries (a listing page is cut
to fit it). `serve_read` and `serve_write` serve one `bohio-read/1` and `bohio-write/1` stream whose open carried `metadata`. A
refused request gets one `error` frame, the same error is returned, and the stream stays open for the caller to close. Order of
checks: decode (`bad_request`), `[filesystem] enabled` (`unsupported`), the permission (`denied`: `fs.read` for `stat`, `list`,
`readlink`, `checksum` and reads; `fs.write` for `mkdir`, `remove`, `rename`, `symlink`, `set_*` and writes; both for `copy`), at
least one root in the grant (`denied`), a free transfer slot (`busy`, transfers only), the path (`bad_request`, `outside_root`),
then the operation (`not_found`, `exists`, `denied`, `unsupported`, `io`). Messages name what failed, never a path. A mode a peer
names (`set_mode`, `mkdir`, a write's `file_mode`) above `0o7777` is `bad_request` and one with the set-user-ID or set-group-ID bit
(`0o6000`) is `denied`; a `copy` and a `replace` over an existing file keep the mode without those bits.

**Roots and paths.** `fs_roots` is the whole of what a peer can reach. A relative path starts at the first root; an absolute one
must lie under a root (the longest match; compared without case on Windows and macOS, and `\\?\C:\x` equals `C:\x`). `.` and
`..` are folded lexically; a `..` that would leave the path's start is `outside_root`. Windows also refuses device names (`CON`,
`NUL`, `COM1`...), a colon in a name (alternate data streams), a name ending in a dot or space, `\\.\` and `\\?\GLOBALROOT`
forms and a drive-relative `C:x` (`bad_request`); both separators are accepted there.

**Symbolic links.** The rest of the path is resolved by `cap-std` against the root's directory handle, which follows a link only
while it stays under that handle: a link out of a root, at the end or in the middle of a path, is `outside_root`, and so is an
absolute link even when it points back inside. `remove` removes a link, never what it points at; `stat` without `follow` describes
the link. `symlink` refuses an absolute target or one that climbs above the roots (`outside_root`); `readlink` returns the text
as stored. The target is judged by its words, so on Unix a chain of links can still name a place outside (`sub/d` -> `..`, `esc` ->
`sub/d/..`): the host's calls refuse it, a program following links without a handle does not (Windows reads a target's `..` by its words).

**What is race-free** (no gap between checking a component and using it, so a link swapped in meanwhile cannot lead out):

| OS | Race-free | Not race-free |
| --- | --- | --- |
| Linux | every lookup: one `openat2` with `RESOLVE_BENEATH` (kernel 5.6+), else `openat` + `O_NOFOLLOW` per component; changes and removals are `*at` calls on the parent's handle | the configured root itself is opened by its path once per call |
| macOS and other Unix | the same `openat` + `O_NOFOLLOW` walk, `*at` calls on the parent's handle | the root's own path |
| Windows | `stat`, `checksum`, reads and the opens of writes: relative to the root's handle, reparse points not followed | `list`, `readlink`, `mkdir`, `remove`, `rename`, `symlink` and `set_mode` or `set_attributes`: the parent's handle becomes its current path and the name is joined to it, so a user who can rename a directory inside the root during the call can redirect it; also the root's own path |

On every OS the root's owner is trusted: a hard link another local user makes to a file outside a root exposes that file, and a
bind mount or junction an administrator places inside a root is part of the root. Windows has no POSIX modes (`set_mode` and a
`mode` are `unsupported`, `Metadata::mode` is absent); `set_attributes` sets the read-only attribute there and is `unsupported`
elsewhere.

**Transfers.** Reads send `header`, `data` frames of at most `chunk_size` bytes, `done`; sending waits while the stream has no room.
A `replace` write goes to a temporary sibling (`.bohio-<pid>-<n>.tmp`) and is renamed over the target after `end` (and a matching
`expect_sha256`), keeping the target's mode on Unix; a stream that closes, fails or stays idle for `idle_timeout` leaves the
target as it was and removes the temporary file. `create` removes the file it made when abandoned; `append` and `at` write in
place and keep what was written. A transfer holds one `max_transfers` slot until it ends. Memory is one chunk plus what the
link's channel queues and reads ahead: a channel that queues `n` bytes can hold `n` more.

## Control stream (`bohio::control`)

```rust
pub struct Context<'a> { pub filesystem: fs::Context<'a>, pub info: &'a Info, pub current: Option<Current<'a>> }   // Copy
pub struct Current<'a>(pub &'a dyn Fn() -> Result<Grant, ErrorBody>);                         // Copy; the grant as it is now
pub fn serve(channel: &Arc<dyn Channel>, context: &Context<'_>) -> u64;                       // requests answered
```

Serves one `bohio/1` stream until the peer closes it or it fails, answering each request in order. `hello` needs no permission
(version 0 is `bad_request`; the answer is the lower of the two versions), `info` needs `info`, filesystem calls as above. A request
that cannot be read is answered with id `0` and `bad_request`; a reply too large for the stream is replaced by an `internal` error.
With `current`, each call is decided by the grant it returns at that call (its error answers the call), so a grant narrowed while the
stream is open applies to the next call; without it `filesystem.grant` is used throughout. `serve::Server` always passes `current`.

## Capabilities (`bohio::info`)

```rust
pub fn collect(config: &Config, environment: &Environment) -> bohio_proto::info::Info;
```

The operating system and architecture, the shells found on `environment.path` (`sh`, `bash`, `zsh`, `cmd`, `powershell`,
`pwsh`), the filesystem features (`unix_modes` on Unix, `windows_attributes` on Windows, `case_sensitive` off on Windows and
macOS, the native separator), whether execution and shells are enabled, and the configured bounds. `host_id` is `None`.

## The link (`bohio::serve`, `bohio::link`, `bohio::peer`)

```rust
pub use bohio_client::dial::Endpoint;                              // the endpoint text of every transport: see bohio-client's header
pub struct Server;                                                 // Debug
impl Server {
    pub fn new(config: &Config, environment: &Environment) -> Result<Self, ServeError>;   // opens or makes the identity and trust store in the data directory
    pub const fn host(&self) -> coqui_peer::PeerId;
    pub fn serve_listening(&self, listening: &Listening, stop: &AtomicBool) -> io::Result<()>;   // accepts until `stop`; a thread per connection (a serial line: one link at a time, the port reopened after each)
    pub fn serve_stdio(&self) -> io::Result<()>;                   // the one link on standard input and output; nothing else may write to standard output
    pub fn serve_channel<C: Channel>(&self, channel: &C, source: Option<SocketAddr>);   // proves the peer on any unproven channel, serves its link to the end (at most max_links_per_peer per peer); the caller closes it
}
#[non_exhaustive] pub enum ServeError { Peer(PeerError), Proof(String), Listen { endpoint, reason } }

pub const PROTOCOLS: [(&str, &[Permission]); 5];                   // each protocol with the permissions of which a peer needs one to open it
pub struct TrustPeers;                                             // coqui_link::peers::PeerStore over the trust store: an accepted key may open every protocol
pub fn admission(policy: PathBuf, protocol: &'static str) -> coqui_link::link::Admission;   // reads peers.cfg afresh each call
pub fn covers(grant: &Grant, protocol: &str, metadata: &[u8]) -> bool;   // the grant still covers what the open asked for
pub fn grant_of(policy: &Path, peer: &str) -> Result<Grant, String>;
pub struct LogEvents;                                              // link events as log lines: a peer id and a protocol name, nothing else

pub struct Data;                                                   // Data::new(dir): identity_path(), policy_path(), identity(), store(), policies()
pub fn list(data: &Data) -> Result<Listing, PeerError>;            // Listing { accepted: Vec<Accepted{id, grant}>, waiting: Vec<Waiting{id, source}>, orphaned: Vec<String> }
pub fn approve(data: &Data, id: &PeerId, grant: Option<Grant>) -> Result<(), PeerError>;   // waiting or accepted -> accepted; Some(grant) replaces the grant
pub fn revoke(data: &Data, id: &PeerId) -> Result<Revoked, PeerError>;   // the grant first, then the key; NoSuchPeer when neither existed
pub fn pair(data: &Data, listener: &TcpListener, settings: &Settings, timeout: Duration, shown: impl FnOnce(&str)) -> Result<PeerId, PeerError>;
#[non_exhaustive] pub enum PeerError { Identity(String), Store(String), Policy(PolicyError), Write(String), NoSuchPeer, Pairing(String) }
```

The data directory holds `identity` (this host's key, a Noise key made on first use, mode 0600 on Unix), `peers/{accepted,waiting}/` (the trust
store) and `peers.cfg` (what each accepted peer may do). A connection proves itself first, within `[link] proof_timeout`, with an unproven peer held to
one proof-sized message. A key that proves it holds the key it presents and is not accepted is recorded as waiting by the proof itself, under the
connection's source address (bounded by coqui-peer's waiting limits), and the connection ends: the peer sees only that. An accepted peer's link is
served by `coqui_link::link::serve`: each stream's open is decided by the trust store (`TrustPeers`, accepted keys only) and then by `admission`
(`peers.cfg`: the peer must hold a permission the protocol needs, `bohio/1` needs one of `info`, `fs.read`, `fs.write`, and `covers` must hold: an
exec's or a shell's explicit program is one the program list holds, a transfer's file lies under a root), and each running stream is decided again
every `[link] recheck`, so removing a key or a grant, or narrowing a program list or the roots under a running stream, closes the stream with a
revoke whose reason reaches the peer (a `Permission` failure). A `bohio/1` stream reads the grant again at every call. A store or `peers.cfg` that cannot be read refuses. The link itself checks that the key is still accepted every `2 x recheck`. The
stream's engine then runs behind a `Relay` with the grant `peers.cfg` gives the peer at that moment.

`approve` never changes a grant unless one is given; `revoke` removes the `peers.cfg` entry before the key, so a running stream ends at the next
re-check even if the link still holds. `peers.cfg` is rewritten whole (comments are not kept) by `approve` and `revoke`, atomically; two commands run
at the same instant can lose one change. `pair` shows one PIN, once (`shown` is called with it; it is never logged), accepts the one peer that joins
with it, and gives it no grant; the joiner stores the host's key, so it then trusts the host with `Trust::Store`. A wrong PIN uses the PIN up.

## Listeners (`bohio::listen`)

```rust
#[non_exhaustive] pub enum Listening { Tcp(TcpListener), Pipe(coqui_transport_pipe::Listener), WebSocket(coqui_transport_web::websocket::Listener), WebTransport { listener: coqui_transport_web::webtransport::Listener, certificate: Certificate }, Serial { path: String, options: SerialOptions } }
impl Listening {
    pub fn bind(config: &ListenConfig, settings: &Settings) -> Result<(Listening, Endpoint), ServeError>;   // the endpoint as bound: port 0 becomes the port chosen
    pub fn accept(&self, wait: Duration, settings: &Settings) -> io::Result<Option<Arrival>>;               // None when none came; not for Serial
    pub fn certificate_hash(&self) -> Option<String>;                                                        // WebTransport: `sha256:...` of the certificate now presented, for `--cert-hash`
}
pub fn server_config(config: &ListenConfig) -> Result<Arc<rustls::ServerConfig>, String>;                    // the chain and key `tls_cert` and `tls_key` name; no client certificate
pub enum Certificate { Files(String), Made(Arc<MadeCertificate>) }
impl MadeCertificate { pub fn new(validity: Duration, backdate: Duration) -> Result<Arc<Self>, String>; pub fn config(&self) -> Arc<rustls::ServerConfig>; pub fn hash(&self) -> String; }   // renewed by config() when half the validity has passed
pub enum Arrival { Ready(Box<dyn Channel>, Option<SocketAddr>), Handshake(Box<Pending>), SessionHandshake(Box<webtransport::Pending>) }
impl Arrival { pub fn source(&self) -> Option<SocketAddr>; pub fn open(self) -> io::Result<(Box<dyn Channel>, Option<SocketAddr>)>; }   // a WebSocket handshake runs here, on the connection's own thread
```

Each scheme yields an unproven channel that holds a peer to one proof-sized message until the proof raises it (`bohio_client::link::framed`);
`max_frame + 5` (plus the seal) is the bound the proof sets, which every carrier takes (a stream channel and a WebSocket's `Limits::max_message` both
follow `set_max_message`). `ws://` is served without TLS beyond loopback too: the proof seals everything after it. `wss://` and `webtransport://` add TLS without trusting
it: no client certificate is asked for, the proof says who the peer is. A `webtransport://` listener prints `certificate sha256:... for <endpoint>`
after `listening`. A serial host opens its port, proves the peer, serves the link and opens the
port again; a client whose first message arrives before the host reopened it (a second `remote` right behind the first) is dropped and must try
again. `serve --stdio` binds none of the `[[listen]]` entries.

## Command line (`bohio::cli`, feature `cli`)

```rust
pub fn main() -> ExitCode;                                       // arguments and environment from the process
pub fn run(arguments: impl IntoIterator<Item = OsString>, environment: &Environment) -> ExitCode;
pub fn capture_environment() -> Environment;
pub struct Cli { pub command: Command }                          // clap Parser; Command: Config, Info, Serve, Status, Id, Peer, Pair, Join, Remote
```

Every command takes `--config FILE` (otherwise `BOHIO_CONFIG`, the user's, the system's; the data directory is the configuration's). A line that
a person or a script reads is printed to standard output and flushed; diagnostics go to standard error.

| Command | What it does |
| --- | --- |
| `config check [--format text\|json]` | reads the configuration and prints `ok` with a summary |
| `info [--format text\|json]` | prints this host's capabilities (`json` is `bohio_proto::info::Info`) |
| `serve [--listen ENDPOINT]...` | serves each `[[listen]]` endpoint and `--listen` (`tcp://host:port`, port 0 picks one; `pipe://name`, `ws://host:port`, `wss://host:port` (needs a `[[listen]]` entry with `tls_cert` and `tls_key`), `webtransport://host:port`, `serial://port?baud=N`) until killed; prints `listening <endpoint>` for each, then `host <id>` |
| `serve --stdio` | serves one link on standard input and output until it ends; prints nothing to standard output, logs to standard error; excludes `--listen` and binds no `[[listen]]` |
| `status` | the host's id, key method, data directory, counts of accepted and waiting peers, and one `listen: <endpoint> <state>` line per configured endpoint: `reachable` / `not reachable` (a connect is made; a TCP, WebSocket or pipe host sees it), `device present` / `device missing` (serial, Unix) or `device not checked` (Windows), `UDP not checked` (WebTransport), `not served by this build` |
| `id` | the id of this machine's key, made on first use |
| `peer list` | `accepted <id> allow: ...`, `waiting <id> from <ip>` and `grant-only <id>` lines |
| `peer approve <id> [--allow P,P] [--fs-root DIR]... [--program NAME]... [--env VAR]...` | accepts a waiting or paired peer; any of `--allow`, `--fs-root` and `--program` replaces its whole grant, none leaves it; `--env` (with `--program`) names the variables its requests may change |
| `peer revoke <id>` | removes the grant and the key; running streams end within `[link] recheck` |
| `pair [--listen ENDPOINT] [--timeout TIME]` | prints `pairing on <endpoint>`, `host <id>`, `pin <PIN>` (default `tcp://127.0.0.1:0`, `5m`), waits for one peer, prints `paired <id>` |
| `join <endpoint> [--pin PIN]` | pairs with a host that shows a PIN (the PIN is read from standard input when absent); both sides keep the other's key; the joiner holds nothing until approved |
| `service install [--config FILE] [--name NAME] [--account ACCOUNT]` | Windows: registers the service `NAME` (default `bohio`), automatic start, as `ACCOUNT` (default `NT AUTHORITY\LocalService`), running `bohio service run --name NAME [--config ABSOLUTE FILE]`; needs an elevated prompt (`access denied: run this from an elevated prompt` otherwise); elsewhere an error naming the systemd unit |
| `service uninstall [--name NAME]` | Windows: stops and deletes the service |
| `service run [--config FILE] [--name NAME]` | Windows: the service's entry point, started by the service manager (a person running it gets a one-line explanation); serves the configured `[[listen]]` endpoints until the manager sends stop or shutdown, then reports stopped and the process exits, which closes the job objects that end every process it started |
| `remote <endpoint> [--server ID] [--ca FILE]... [--cert-hash HASH]... [--fallback ENDPOINT] info [--format text\|json]` | the host's capabilities over the link; the endpoint is any scheme above, or `stdio://program args`; `--ca`/`--cert-hash` check a `wss://` or `webtransport://` host's certificate (the system's roots without them), `--fallback` is a `ws://` (loopback) or `wss://` endpoint of the same host for a blocked UDP path |
| `remote --command "program args" [--server ID] info` | the same over a program the command starts, its standard streams the link (for example `docker exec -i NAME bohio serve --stdio`); the program is run directly, never through a shell, and its tree ends when the command does |
| `remote <endpoint> exec [--cwd D] [--timeout-ms N] [--env K=V]... [--stdin] -- <argv>` | runs the program, streams its output, and exits with its status (124 on a deadline, `128+signal` when signalled) |
| `remote <endpoint> read <path>` | writes the host's file to standard output |
| `remote <endpoint> shell [--kind sh\|bash\|zsh\|cmd\|powershell\|pwsh] [--cwd D]` | an interactive shell; the local terminal is put in raw mode when standard input is one, and its size is followed |

On Windows `serve`, `serve --stdio` and the service refuse to start (exit 1, one line naming the path and the security id) when the
configuration file they read is owned by another account than the one running `bohio`, SYSTEM or the administrators, or when Everyone,
Authenticated Users, Users, INTERACTIVE, Guests, ANONYMOUS LOGON or Domain Users may change it; and when the data directory is so owned or
any of those may change it or read what is made in it. A folder made under `%ProgramData%` is open to Users that way unless its access list
is narrowed. The check reads the access list through Windows PowerShell (`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`); a
host that cannot run it does not start. Elsewhere nothing is checked: the system locations are the administrator's and the user ones private.

`remote` trusts the keys `join` stored, or only `--server ID`. A host that ends the link before the proof is reported as "may not be listening, may not
trust this key yet, or this side does not trust the host"; a stream the host refuses or ends for lost access is "the host refused or ended the stream".
Exit status: 0 success, 1 the operation failed (one line on standard error), 2 the invocation was wrong; `remote exec` exits with the program's.

## Environment variables

| Name | Effect |
| --- | --- |
| `BOHIO_CONFIG` | the configuration file when `--config` is not given; must exist when set |
| `BOHIO_LOG` | log level or filter for standard error, default `warn` (`trace` shows every link event; none holds a secret) |
| `NO_COLOR` | set: no colour in log lines |
| `PATH` | where shells are looked for in `info`, and the programs a grant lists by `exec` and `shell` |
| `APPDATA`, `LOCALAPPDATA`, `ProgramData` (Windows); `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `HOME` (Unix) | where the configuration and data directory are looked for |

## Gotchas

- `serve` takes `Arc<dyn Channel>` because its output threads outlive a wedged pipe: a descendant the platform cannot end that
  keeps a pipe open leaves a thread behind after `drain_timeout`, not a hung stream.
- A `kill` frame queued behind standard input the process is not reading is seen only when it reads, when the deadline passes or
  when the stream closes.
- Standard input sent to a request that asked for `Stdin::Closed` is ignored.
- Without a program list the name is looked up as the host's `PATH` finds it, with the request's own `PATH` if it sets one (on Windows
  also the application's directory and the current one): such a peer may run anything anyway. `programs` compares the name exactly as
  requested, so `git` and `/usr/bin/git` are different entries.
- Windows: a process starts a moment before it joins its job object; a process it starts in that instant is outside the tree. A
  shell's terminal (ConPTY) starts the child itself, so the same moment applies to a shell session.
- Windows ConPTY asks the terminal for its cursor position before the program's first output and holds that output until it is
  answered: the host removes the query (`ESC[6n`) from the output and answers it with `ESC[1;1R`.
- A `kill` frame queued behind typed input the shell is not reading is seen only when it reads, or when the stream closes.
- Nothing logs a key, a PIN or a request: link events carry a peer id and a protocol name, and `bohio pair` writes the PIN to standard output
  only. `tests/link.rs` captures every process's log at `trace` and fails when a PIN or any stored key bytes appear in it. `bohio` has no
  invitation flow, so there is no invitation secret.
- A peer id is public; `bohio peer list` and the log print them. A key that proves itself and is not accepted is recorded under the connection's source
  address, so a flood from one source cannot fill the waiting list for the others.
- A revoked peer's link ends at the second re-check (`2 x recheck`) at the latest; its streams end at the first.
- `bohio serve` stops only when killed: the exec trees it started are ended when the stream drops, and on Windows by the job object when the
  process dies; on Unix a killed host leaves nothing to end them.
- A loopback connect to a closed port in the ephemeral range can connect a socket to itself; `Connection::connect_tcp` and `bohio join` refuse one.
