# `bohio` — API header

Header-file-style reference for the `bohio` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

| Feature | Adds | Needed for | Default |
| --- | --- | --- | --- |
| `cli` | module `cli`, the `bohio` binary; `clap`, `tracing-subscriber`, `serde_json`, `crossterm` | the command | on |

MSRV Rust 1.89. A library-only build (`default-features = false`) depends on `bohio-proto`, `bohio-client` (the link and its relay),
`coqui-transport`, `coqui-link` (with `sealed`), `coqui-peer`, `coqui-transport-pipe` and `coqui-transport-web` (each a git revision; one copy of each builds), `serde`, `toml`,
`thiserror`, `tracing`, `anycfg` (the layers) and `toml_edit` (in-place edits of a peer table), `portable-pty` (terminals), `cap-std` and `cap-fs-ext` (confined file access), `sha2`, and `rustix` (Unix) or
`win32job` (Windows). Version `0.0.0-alpha.0`, not published.

Imports: the root holds `Config`, `ConfigError`, `Environment`, `Grant`, `Peers`, `Permission`. Public modules:
`config`, `control`, `exec`, `fs`, `info`, `limit`, `link`, `listen`, `node`, `peer`, `policy`, `shell`, `cli` (feature `cli`). No `unsafe` code
(`#![forbid(unsafe_code)]`). The library prints nothing, reads no environment variable and installs no logger; it starts threads only inside
`exec::serve`, `shell::serve`, the transfer and control engines, `node::Node` (a thread per connection, per stream and per relay) and
`peer::pair`.

## Configuration (`bohio::config`)

```rust
pub struct Environment { pub windows: bool, pub local_app_data: Option<PathBuf>, pub xdg_data_home: Option<PathBuf>, pub home: Option<PathBuf>,
    pub path: Option<OsString> }                                  // Default; the binary fills it from the process (cli::capture_environment)
impl Environment { pub fn default_data_dir(&self) -> PathBuf; }

pub struct Config { pub service: ServiceConfig, pub listen: Vec<ListenConfig>, pub link: LinkConfig, pub filesystem: FilesystemConfig,
                    pub execution: ExecutionConfig, pub shell: ShellConfig, pub peer: Peers }     // Default; unknown keys refused; `peer` is `[peer.<id>]`
pub struct NodeConfig { pub name: String, pub link: LinkConfig, pub filesystem: FilesystemConfig,
                        pub execution: ExecutionConfig, pub shell: ShellConfig }   // Default, Deserialize, unknown keys refused: what a Node runs on, read by an app from any section of its own settings
impl NodeConfig { pub fn validate(&self, label: &str) -> Result<(), ConfigError>; }   // every range check; `Node::new` calls it
impl Config {
    pub fn node(&self) -> NodeConfig;                                            // `[service] name`, `[link]`, `[filesystem]`, `[execution]`, `[shell]`
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError>;       // one layer of text; `path` only labels errors
    pub fn load(path: &Path) -> Result<Self, ConfigError>;                       // one layer, a file
    pub fn data_dir(&self, environment: &Environment) -> PathBuf;
}
pub struct Layers;                                                // the library reads the files it is given and nothing else; the command finds them
impl Layers {
    pub fn load(paths: &[PathBuf]) -> Result<Layered, ConfigError>;             // lowest first, every file must exist; none: the defaults
    pub fn parse(text: &str, label: &str) -> Result<Layered, ConfigError>;      // one layer of text
    pub fn peers(paths: &[PathBuf]) -> Result<Peers, ConfigError>;              // the live check: a missing file is skipped, only `[peer.*]` is read
}
pub struct Layered { pub config: Config, pub peers: Peers, pub files: Vec<PathBuf> }   // `peers` is `config.peer`; `files` the layers read
pub struct Span(pub Duration);                                    // a number of seconds, or text: 250ms, 5s, 2m, 1h; Span::parse(&str) -> Option<Span>
#[non_exhaustive] pub enum ConfigError { Read { path, reason }, Parse { path, line, column, key: Option<String>, message }, Invalid { path, key, message } }
```

`bohio.cfg` is TOML (a `.cfg` first line `#!...` is a comment); every key is optional and an unknown key or section is `ConfigError::Parse` naming it,
with file, line, column and key (a value of the file is never quoted). Defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `[service] name` | `"bohio"` | name shown in `info` |
| `[service] check_access` | `true` | refuse to start when another account may change a layer, the directory that holds it or (Windows) the data directory; `false` logs it and serves; lowest layer only |
| `[service] user_layer` | `true` | whether the user's file is read over the system's when no layers are named; lowest layer only |
| `[service] data_dir` | `bohio` under `%LOCALAPPDATA%` / `$XDG_DATA_HOME` (else `~/.local/share`) | identity and the waiting list |
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
| `[link] heartbeat` | `25s` | how often the link asks its peer whether it is there (both sides ask; also keeps a quiet link from a proxy's or NAT's idle timeout); `0` is off |
| `[link] heartbeat_misses` | `3` | heartbeat intervals with nothing heard after which the link is lost; at least 1 |
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

**Layers.** `Layers::load` reads each file alone (syntax, unknown keys, types: the error names the file, line, column and key) and then merges them with
`anycfg` over `toml::Value`: tables merge by key, arrays and scalars replace (an array is one setting, so a higher layer can narrow a root list or `[[listen]]`;
merging elements would only widen). Out-of-range values, checked on the merged value, are `ConfigError::Invalid` naming the key (`link.max_streams`, ...) and the
highest layer that sets it, as are a `[peer.<name>]` whose name is not a peer id and `[service] check_access` or `user_layer` set above the lowest layer. The command
finds the layers (the library reads no environment variable): `--config FILE` (repeatable, lowest first) or else `BOHIO_CONFIG` name them, alone; else the system
file (`%ProgramData%\bohio\bohio.cfg`, `/etc/bohio/bohio.cfg`) then the user's (`%APPDATA%\bohio\bohio.cfg`, `$XDG_CONFIG_HOME/bohio/bohio.cfg`, else
`~/.config/bohio/bohio.cfg`), each if it exists; none: the defaults. The `[peer.*]` tables are read again from the files at every question (`peer::LayeredPeers`);
every other section is read when the host starts.

## Peer grants (`bohio::policy`)

```rust
#[non_exhaustive] pub enum Permission { Info, FsRead, FsWrite, Exec, Shell, Admin }   // Ord; names "info","fs.read","fs.write","exec","shell","admin"
impl Permission { pub const fn name(self) -> &'static str; }
pub struct Grant;                                                // Default = nothing
impl Grant { pub fn none() -> Self; pub fn allowing(permissions) -> Self; pub fn with_fs_roots(self, roots) -> Self; pub fn with_programs(self, programs) -> Self; pub fn restricts_programs(&self) -> bool;
             pub fn allows(&self, Permission) -> bool; pub fn permissions(&self) -> impl Iterator<Item = Permission>; pub fn fs_roots(&self) -> &[PathBuf];
             pub fn permits_program(&self, program: &str) -> bool; pub fn permits_shell_script(&self) -> bool;
             pub fn with_env(self, names) -> Self; pub fn env_names(&self) -> &[String]; pub fn permits_env(&self, env: &bohio_proto::exec::Env) -> bool; }
pub struct Peers;                                                // Default = no peer; Deserialize (the `[peer]` table of `bohio.cfg`)
pub struct PeerTable;                                            // one `[peer.<id>]`: grant(), endpoint() -> Option<&str>, dial() -> &DialConfig
impl Peers { pub fn grant_for(&self, peer: &str) -> Grant; pub fn table(&self, peer: &str) -> Option<&PeerTable>; pub fn dials(&self) -> impl Iterator<Item = (&str, &str, &DialConfig)>; pub fn has(&self, peer: &str) -> bool; pub fn ids(&self) -> impl Iterator<Item = &str>;
             pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool; }
```

`[peer."<id>"]` (the id is `sha256:` and 64 hex digits, so it is quoted as a key) with `allow = [..]`, `fs_roots = [..]`, and `[peer."<id>".exec]` with
`programs = [..]` and `env = [..]`. A table, even an empty one, accepts the key. An unknown key or permission is `ConfigError::Parse`. A peer with no table, or
no `allow`, holds nothing. `programs` empty means any program and
explicit shell scripts; non-empty means only those programs, each named exactly as the peer asks for it, and no shell script
(a shell runs anything). Under a non-empty list each name is resolved on the host's own `PATH` (a name with a directory
must be absolute; a relative one is `not_found`) and that absolute path is started, never what the request's `PATH` or working
directory would find, and a request may set or remove only the variables `env` names (exactly; without case on Windows), `PATH` too only when `env`
names it (the program still starts from the host's own `PATH`), and may not clear the environment, else it is `denied`: a variable is as strong as an argument (`LD_PRELOAD`,
`DYLD_INSERT_LIBRARIES`, `BASH_ENV` load or run code the peer chose). `permits_env` is that check; without a program list it always holds. The list
limits which program starts, not what it does: `git -c alias.x=!cmd`, `make`, `find -exec` and interpreters run anything they are
given, so list only programs that are safe with every argument. `fs_roots` empty means no file operation is allowed.

`[peer."<id>"] endpoint = "<any endpoint>"` makes this node dial that peer (the table's name pins the key that must answer there); `bohio serve` and `bohio service run`
run a dialer for each, beside the listeners, read at start (a restart applies a new endpoint). `[peer."<id>".dial]` is `bohio::config::DialConfig` (Deserialize,
usable by an embedder; unknown keys refused):

| Key | Default | Meaning |
| --- | --- | --- |
| `ca` | none | PEM files trusted instead of the system roots, for `wss` and `webtransport` |
| `cert_hash` | none | certificate hashes (`sha256:` and 64 hex digits) trusted as they are |
| `fallback` | none | for a `webtransport://` endpoint, a `ws://` or `wss://` one tried when the QUIC path fails before anything crossed |
| `timeout` | `10s` | opening the path (the proof has `[link] proof_timeout` besides); not zero |
| `retry_min` | `1s` | the first wait after a failure; not zero |
| `retry_max` | `60s` | the longest wait; at least `retry_min` |
| `retry_factor` | `2` | what each further failure multiplies the wait by; at least 1 |
| `retry_jitter` | `0.1` | a wait moves by up to this fraction of itself, either way; 0 to 1 |

A shorter `retry_min` and a smaller factor retry faster and ask the peer, and a watching network, more often; a larger `retry_max` notices a peer that came back later. A
link that stayed up at least `retry_min` starts the sequence again, and one that ends sooner counts as a failure (the peer most likely refused the key after its proof).
A bad endpoint, a fallback that does not follow a `webtransport://` one, or a bad `cert_hash` is `ConfigError::Invalid`; a name in a `tcp://` endpoint is resolved at each attempt, never when the file is read.

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
stream is open applies to the next call; without it `filesystem.grant` is used throughout. `node::Node` always passes `current`.

## Capabilities (`bohio::info`)

```rust
pub fn collect(config: &NodeConfig, path: &OsStr) -> bohio_proto::info::Info;
```

The operating system and architecture, the shells found on `path` (`sh`, `bash`, `zsh`, `cmd`, `powershell`,
`pwsh`), the filesystem features (`unix_modes` on Unix, `windows_attributes` on Windows, `case_sensitive` off on Windows and
macOS, the native separator), whether execution and shells are enabled, and the configured bounds. `host_id` is `None`.

## The node (`bohio::node`, `bohio::link`, `bohio::peer`)

```rust
pub use bohio_client::dial::Endpoint;                              // the endpoint text of every transport: see bohio-client's header
pub struct Node;                                                   // Debug; no path, CLI or process-wide state is needed
impl Node {
    pub fn new(config: NodeConfig, identity: Identity, keys: Arc<dyn Keys>, grants: Arc<dyn Grants>) -> Result<Self, NodeError>;   // validates `config`; the proof is built once over `keys`
    pub fn with_events(self, events: Arc<dyn NodeEvents>) -> Self;  // default `LogEvents`
    pub fn with_path(self, path: OsString) -> Self;                 // the host's own PATH: programs, shells and `info`; default empty
    pub const fn host(&self) -> coqui_peer::PeerId;
    pub fn serve_listening(&self, listening: &Listening, stop: &AtomicBool) -> io::Result<()>;   // accepts until `stop`; a thread per connection (a serial line: one link at a time, the port reopened after each)
    pub fn serve_stdio(&self) -> io::Result<()>;                   // the one link on standard input and output; nothing else may write to standard output
    pub fn serve_channel<C: Channel>(&self, channel: &C, source: Option<SocketAddr>);   // proves the peer on any unproven channel, serves its link to the end (at most max_links_per_peer per peer); the caller closes it
    pub fn connect_channel<C: Channel>(&self, channel: &C, expect: &PeerId, source: Option<SocketAddr>) -> Result<(), NodeError>;   // dials over an unproven channel (framed as `bohio_client::link::framed`): proves `expect` before sending its own key, serves the link as an answered one is, returns when it ends
    pub fn connect(&self, endpoint: &Endpoint, expect: &PeerId, trust: &ServerTrust, timeout: Duration) -> Result<(), NodeError>;   // dials any endpoint, then connect_channel; the link lists `endpoint`
    pub fn peer(&self, peer: &PeerId) -> Option<PeerLink>;          // the oldest link that is up to `peer`, whichever side made it
    pub fn links(&self) -> Vec<LinkInfo>;                           // every link that is up, oldest first
    pub fn run_dial(&self, peer: &PeerId, endpoint: &str, dial: &DialConfig, stop: &AtomicBool);   // keeps a link to `peer` at `endpoint` (text, read again at each attempt) until `stop`: dials, serves, waits (retry_min x retry_factor per failure, up to retry_max, with jitter), repeats; no dial while an up link to the peer exists
}
#[non_exhaustive] pub enum NodeError { Config(ConfigError), Proof(String), Listen { endpoint, reason }, Dial { endpoint, reason }, Stranger { peer } }   // Dial: the path or the proof failed; Stranger: the dialed key proved itself and Keys do not accept it
#[non_exhaustive] pub struct LinkInfo { pub peer: PeerId, pub side: Side, pub endpoint: Option<String>, pub source: Option<SocketAddr>, pub since: SystemTime, pub mine: Vec<String> }   // `mine`: the tokens this node offers the peer now, computed when asked   // `side` (Dialed or Answered) decides nothing
#[derive(Clone)] pub struct PeerLink;                              // Debug; bohio_client::Open (Error::Closed once the link ended); peer(), is_up(), offers(wait) -> Result<Vec<String>, bohio_client::Error> (the peer's `link/1` offers now)

pub trait Keys: Send + Sync {
    fn is_accepted(&self, peer: &PeerId) -> bool;                  // the proof, every stream decision and the link's check every `2 x recheck`
    fn stranger(&self, peer: &PeerId, key: &KeyMaterial, source: Option<SocketAddr>) {}   // a proven key that was refused; default ignores
}
pub trait Grants: Send + Sync {
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError>;  // asked at every open, every recheck and every `bohio/1` call; an error refuses
}
pub struct GrantsError;                                            // GrantsError::new(reason); Display is the reason
pub struct MemoryKeys;                                             // new(), accept(PeerId), remove(&PeerId) -> bool; FromIterator<PeerId>
pub struct MemoryGrants;                                           // new(), set(PeerId, Grant), remove(&PeerId) -> bool; FromIterator<(PeerId, Grant)>; no entry: Grant::none()

pub trait NodeEvents: Send + Sync { fn event(&self, event: &NodeEvent); }   // runs on the link's thread: return quickly
#[non_exhaustive] pub enum NodeEvent { LinkUp { peer, side }, LinkDown { peer }, Opened { peer, protocol }, Refused { peer, protocol }, Revoked { peer, protocol }, Dialing { peer, endpoint }, DialFailed { peer, endpoint, reason, retry_in }, Stranger { peer, source } }   // an id, a protocol, an address: never a key, PIN or request
pub struct LogEvents;                                              // the default: log lines

pub const PROTOCOLS: [(&str, &[Permission]); 5];                   // each protocol with the permissions of which a peer needs one to open it
pub struct NodePeers;                                              // coqui_link::peers::PeerStore over `Keys` and `Grants`: NodePeers::new(keys, grants); an accepted key may open each protocol its grant holds a permission for, which is what `link/1` offers it
pub fn offered(grant: &Grant) -> Vec<&'static str>;                       // the PROTOCOLS tokens `grant` holds a permission for: what a node offers that peer
pub fn admission(grants: Arc<dyn Grants>, protocol: &'static str) -> coqui_link::link::Admission;   // asks `grants` afresh each call
pub fn covers(grant: &Grant, protocol: &str, metadata: &[u8]) -> bool;   // the grant still covers what the open asked for
pub fn grant_of(grants: &dyn Grants, peer: &str) -> Result<Grant, GrantsError>;

pub struct Data;                                                   // Data::new(dir): dir(), identity_path(), identity(), store() (the waiting list)
pub struct LayeredPeers;                                           // Keys + Grants over config layers read again at every question: LayeredPeers::new(files: Vec<PathBuf>, &Data) -> Result<Self, PeerError>; a stranger is recorded as waiting in the Data directory
pub fn list(files: &[PathBuf], data: &Data) -> Result<Listing, PeerError>;   // Listing { accepted: Vec<Accepted{id, grant, endpoint}>, waiting: Vec<Waiting{id, source}> }
pub fn approve(files: &[PathBuf], create: Option<&Path>, data: &Data, id: &PeerId, grant: Option<&Grant>, endpoint: Option<&str>) -> Result<Approved, PeerError>;   // Approved { file, made }
pub fn revoke(files: &[PathBuf], data: &Data, id: &PeerId) -> Result<Revoked, PeerError>;   // Revoked { files, unwritable, key }; NoSuchPeer when nothing was removed and no layer holds it
pub fn pair(data: &Data, listener: &TcpListener, settings: &Settings, timeout: Duration, shown: impl FnOnce(&str)) -> Result<PeerId, PeerError>;
#[non_exhaustive] pub enum PeerError { Identity(String), Store(String), Config(ConfigError), Write(String), NoLayer, NoSuchPeer, Pairing(String) }
```

A `Node` is made from what its caller supplies and reads no file or environment variable: settings (`NodeConfig`), an identity, `Keys` and `Grants`. An app
keeps them wherever it likes (its own settings, a database record, memory) and implements the two traits over them; `bohio serve` implements them over files.
A connection proves itself first, within `[link] proof_timeout`, with an unproven peer held to one proof-sized message. A key that proves it holds the key it
presents and that `Keys::is_accepted` refuses is passed to `Keys::stranger` (with the connection's source address) and the connection ends: the peer sees only
that. An accepted peer's link is served by `coqui_link::link::serve`: each stream's open is decided by `Keys` (`NodePeers`, accepted keys only) and then by
`admission` (the peer's `Grants` entry: it must hold a permission the protocol needs, `bohio/1` needs one of `info`, `fs.read`, `fs.write`, and `covers` must
hold: an exec's or a shell's explicit program is one the program list holds, a transfer's file lies under a root), and each running stream is decided again
every `[link] recheck`, so removing a key or a grant, or narrowing a program list or the roots under a running stream, closes the stream with a revoke whose
reason reaches the peer (a `Permission` failure). A `bohio/1` stream reads the grant again at every call. A `Grants` that cannot be read refuses. The link itself
checks that the key is still accepted every `2 x recheck`. The stream's engine then runs behind a `Relay` with the grant the peer has at that moment.

Either side opens streams on any link. `serve_channel` and `serve_listening` answer a link; `connect` and `connect_channel` dial one (`prove_out` with the
responder's key pinned to `expect`, so a path that leads elsewhere gets nothing, then served exactly as an answered link, `Side::Dialed`). Every link, whichever
side made it, runs `coqui_link::link::serve_with` with a `bohio_client::connect::Opens` as the node's opener, so the node answers the peer's streams under the
peer's grants and `Node::peer(&id)` opens its own under the peer's grants for this node. Who dialed decides nothing: `Side` reaches neither `Keys` nor `Grants`, and
a link serves only a key whose `Keys` accept it, so a dialed key that proved itself and is not accepted ends the link after the proof (`NodeError::Stranger`,
`Keys::stranger`, `NodeEvent::Stranger`). A dialed link counts against `[link] max_links_per_peer`. Both sides dialing at once make two links; both serve.

`run_dial` is the keep-alive of a dialed link: after a failure it waits `retry_min`, then that times `retry_factor` per further failure up to `retry_max` (with `retry_jitter`),
and starts again from `retry_min` after a link that stayed up at least `retry_min`; it never dials while an up link to the peer exists, and the node's `[link] heartbeat`
(25 s, 3 misses by default, both sides) ends a link whose path went silent without closing. A peer that does not prove itself as the table's key is a `DialFailed`
(its reason, once per change); a peer that proves itself and then ends the link at once (it does not accept this node's key) is a failure too, with the same
backoff, and sees this node's key as waiting. `NodeEvent::DialFailed` and `Dialing` carry an id, an endpoint and a reason, never a key.

Roles are read from the link, not configured: a node offers a peer the tokens its `Grants` entry for that peer holds a permission for (`offered`), which is the
`offers` of the `link/1` document it answers on every link, so the document is what this node would serve that peer now, recomputed at each read; admission and
`covers` still decide every open and recheck. `PeerLink::offers(wait)` reads the peer's document and `LinkInfo::mine` is this node's side. A node that grants a peer
nothing offers nothing and still opens streams on it. Nothing is added to the wire.

`LayeredPeers` is the command's pair: `[peer.<id>]` of the layer files (what accepts a key and what it may do) and, in the data directory, the waiting list
(`peers/waiting/`; `peers/accepted/` is no longer read). A layer that cannot be read or parsed refuses everything, logged once per distinct error.
`approve` writes the peer's table with `toml_edit`, keeping every other byte of the file (comments, layout): into the layer that already holds the table (an
error when it cannot be written), else the last of `files` this account can write, else `create` (made with its parents, readable by its owner alone); it removes
the id's waiting entry and never changes an existing table unless a grant is given. `revoke` removes the table from every layer that holds it and its waiting entry,
and names (in `Revoked::unwritable`) a layer it cannot write: the key stays accepted there. A running stream ends at the next re-check. A file is replaced
atomically through a temporary sibling, keeping its mode on Unix and taking its directory's inherited access list on Windows; two commands run at the same
instant can lose one change. `pair` shows one PIN, once (`shown` is called with it; it is never logged), and stores the one peer that joins with it; the command
then calls `approve` (no grant), which moves the key into the configuration as a `[peer.<id>]` table. The joiner does the same with the host's key, so it trusts the
host through its own `[peer.<host>]` table. A wrong PIN uses the PIN up.

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

Every command takes `--config FILE`, repeatable, lowest layer first (otherwise `BOHIO_CONFIG`, one file; otherwise the system file with the user's merged over it, unless the
system file sets `[service] user_layer = false`); the layers named are the only ones read, and the data directory is the merged configuration's. A line that
a person or a script reads is printed to standard output and flushed; diagnostics go to standard error.

| Command | What it does |
| --- | --- |
| `config check [--format text|json]` | reads the configuration and prints `ok` with a summary (including `dial: <count>`) |\|json]` | reads the configuration and prints `ok` with a summary |
| `info [--format text\|json]` | prints this host's capabilities (`json` is `bohio_proto::info::Info`) |
| `serve [--listen ENDPOINT]...` | also dials each peer whose `[peer.<id>]` table has an `endpoint` (printing `dialing <endpoint> peer <id>`; with no `[[listen]]` and no `--listen`, a node that only dials is fine); serves each `[[listen]]` endpoint and `--listen` (`tcp://host:port`, port 0 picks one; `pipe://name`, `ws://host:port`, `wss://host:port` (needs a `[[listen]]` entry with `tls_cert` and `tls_key`), `webtransport://host:port`, `serial://port?baud=N`) until killed; prints `listening <endpoint>` for each, then `host <id>` |
| `serve --stdio` | serves one link on standard input and output until it ends; prints nothing to standard output, logs to standard error; excludes `--listen` and binds no `[[listen]]` |
| `status` | the host's id, key method, data directory, one `config: <file>` line per layer read, counts of accepted and waiting peers, one `dial: <endpoint> peer <id>` line per peer dialed, and one `listen: <endpoint> <state>` line per configured endpoint: `reachable` / `not reachable` (a connect is made; a TCP, WebSocket or pipe host sees it), `device present` / `device missing` (serial, Unix) or `device not checked` (Windows), `UDP not checked` (WebTransport), `not served by this build` |
| `id` | the id of this machine's key, made on first use |
| `peer list` | `accepted <id> allow: ...` (then ` dial: <endpoint>` when the table has one) and `waiting <id> from <ip>` lines |
| `peer approve <id> [--allow P,P] [--fs-root DIR]... [--program NAME]... [--env VAR]... [--endpoint ENDPOINT]` | `--endpoint` writes the table's `endpoint` (this node then dials the peer; a table that is replaced keeps its `endpoint` and `[dial]`); accepts a peer by id (waiting, paired or new): writes its `[peer.<id>]` table to a layer and prints `wrote <file>`; any of `--allow`, `--fs-root` and `--program` replaces its whole table, none leaves an existing one; `--env` (with `--program`) names the variables its requests may change |
| `peer revoke <id>` | removes the table from every layer that holds it (naming any layer it cannot write) and its waiting entry; running streams end within `[link] recheck` |
| `pair [--listen ENDPOINT] [--timeout TIME]` | prints `pairing on <endpoint>`, `host <id>`, `pin <PIN>` (default `tcp://127.0.0.1:0`, `5m`), waits for one peer, prints `paired <id>` and `wrote <file>`: the joiner's key is accepted as a `[peer.<id>]` table, holding nothing |
| `join <endpoint> [--pin PIN]` | pairs with a host that shows a PIN (the PIN is read from standard input when absent); each side writes the other's key as a `[peer.<id>]` table into its layers; the joiner holds nothing until approved |
| `service install [--config FILE] [--name NAME] [--account ACCOUNT]` | Windows: registers the service `NAME` (default `bohio`), automatic start, as `ACCOUNT` (default `NT AUTHORITY\LocalService`), running `bohio service run --name NAME [--config ABSOLUTE FILE]`; needs an elevated prompt (`access denied: run this from an elevated prompt` otherwise); elsewhere an error naming the systemd unit |
| `service uninstall [--name NAME]` | Windows: stops and deletes the service |
| `service run [--config FILE] [--name NAME]` | Windows: the service's entry point, started by the service manager (a person running it gets a one-line explanation); serves the configured `[[listen]]` endpoints until the manager sends stop or shutdown, then reports stopped and the process exits, which closes the job objects that end every process it started |
| `remote <endpoint> [--server ID] [--ca FILE]... [--cert-hash HASH]... [--fallback ENDPOINT] info [--format text\|json]` | the host's capabilities over the link; the endpoint is any scheme above, or `stdio://program args`; `--ca`/`--cert-hash` check a `wss://` or `webtransport://` host's certificate (the system's roots without them), `--fallback` is a `ws://` (loopback) or `wss://` endpoint of the same host for a blocked UDP path |
| `remote --command "program args" [--server ID] info` | the same over a program the command starts, its standard streams the link (for example `docker exec -i NAME bohio serve --stdio`); the program is run directly, never through a shell, and its tree ends when the command does |
| `remote <endpoint> exec [--cwd D] [--timeout-ms N] [--env K=V]... [--stdin] -- <argv>` | runs the program, streams its output, and exits with its status (124 on a deadline, `128+signal` when signalled) |
| `remote <endpoint> read <path>` | writes the host's file to standard output |
| `remote <endpoint> shell [--kind sh\|bash\|zsh\|cmd\|powershell\|pwsh] [--cwd D]` | an interactive shell; the local terminal is put in raw mode when standard input is one, and its size is followed |

`serve`, `serve --stdio` and the service check every layer file and its directory, and the data directory (Windows only). On Unix a layer or its directory owned by an
account other than the running one or root, or writable by its group or others, is refused (exit 1, one line naming the path); `[service] check_access = false`
in the lowest layer logs each finding and starts. On Windows they refuse to start (exit 1, one line naming the path and the security id) when a
configuration layer they read is owned by another account than the one running `bohio`, SYSTEM or the administrators, or when Everyone,
Authenticated Users, Users, INTERACTIVE, Guests, ANONYMOUS LOGON or Domain Users may change it; and when the data directory is so owned or
any of those may change it or read what is made in it. A folder made under `%ProgramData%` is open to Users that way unless its access list
is narrowed. The check reads the access list through Windows PowerShell (`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`); a
host that cannot run it does not start.

`remote` trusts the `[peer.<id>]` tables of the layers (the keys `join` and `peer approve` wrote), or only `--server ID`. A host that ends the link before the proof is reported as "may not be listening, may not
trust this key yet, or this side does not trust the host"; a stream the host refuses or ends for lost access is "the host refused or ended the stream".
Exit status: 0 success, 1 the operation failed (one line on standard error), 2 the invocation was wrong; `remote exec` exits with the program's.

## Environment variables

| Name | Effect |
| --- | --- |
| `BOHIO_CONFIG` | the one configuration layer when `--config` is not given; must exist when set |
| `BOHIO_LOG` | log level or filter for standard error, default `warn` (`trace` shows every link event; none holds a secret) |
| `NO_COLOR` | set: no colour in log lines |
| `PATH` | where shells are looked for in `info`, and the programs a grant lists by `exec` and `shell` |
| `APPDATA`, `LOCALAPPDATA`, `ProgramData` (Windows); `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `HOME` (Unix) | where the user's and the system's configuration files and the data directory are looked for |

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
