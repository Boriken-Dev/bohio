# `bohio` — API header

Header-file-style reference for the `bohio` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

| Feature | Adds | Needed for | Default |
| --- | --- | --- | --- |
| `cli` | module `cli`, the `bohio` binary; `clap`, `tracing-subscriber`, `serde_json` | the command | on |

MSRV Rust 1.89. A library-only build (`default-features = false`) depends on `bohio-proto`, `coqui-transport` (git revision),
`serde`, `toml`, `thiserror`, `tracing`, `cap-std` and `cap-fs-ext` (confined file access), `sha2`, and `rustix` (Unix) or
`win32job` (Windows). Version `0.0.0-alpha.0`, not published.

Imports: the root holds `Config`, `ConfigError`, `Environment`, `Grant`, `Permission`, `Policies`, `PolicyError`. Public modules:
`config`, `control`, `exec`, `fs`, `info`, `limit`, `policy`, `cli` (feature `cli`). No `unsafe` code (`#![forbid(unsafe_code)]`). The library
prints nothing, reads no environment variable and installs no logger; it starts threads only inside `exec::serve`.

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
| `[service] data_dir` | `bohio` under `%LOCALAPPDATA%` / `$XDG_DATA_HOME` (else `~/.local/share`) | identity, trust store, `peers.cfg` |
| `[[listen]] endpoint` | none | `scheme://address`; required in each entry |
| `[link] recheck` | `5s` | how often a running stream's permission is decided again; not zero |
| `[link] max_streams` | `64` | streams per peer; at least 1 |
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

`locate`: `explicit` alone when given (must exist); else the first existing of `BOHIO_CONFIG` (must exist when set), the user's file
(`%APPDATA%\bohio\bohio.cfg`, `$XDG_CONFIG_HOME/bohio/bohio.cfg`, else `~/.config/bohio/bohio.cfg`), the system's
(`%ProgramData%\bohio\bohio.cfg`, `/etc/bohio/bohio.cfg`); with none, the defaults and `None`. Out-of-range values are
`ConfigError::Invalid` naming the key (`link.max_streams`, ...).

## Peer grants (`bohio::policy`)

```rust
#[non_exhaustive] pub enum Permission { Info, FsRead, FsWrite, Exec, Shell, Admin }   // Ord; names "info","fs.read","fs.write","exec","shell","admin"
impl Permission { pub const fn name(self) -> &'static str; }
pub struct Grant;                                                // Default = nothing
impl Grant { pub fn none() -> Self; pub fn allowing(permissions) -> Self; pub fn with_fs_roots(self, roots) -> Self; pub fn with_programs(self, programs) -> Self;
             pub fn allows(&self, Permission) -> bool; pub fn permissions(&self) -> impl Iterator<Item = Permission>; pub fn fs_roots(&self) -> &[PathBuf];
             pub fn permits_program(&self, program: &str) -> bool; pub fn permits_shell_script(&self) -> bool; }
pub struct Policies;                                             // Default = no peer
impl Policies { pub fn from_toml(text, path: &str) -> Result<Self, PolicyError>; pub fn load(path: &Path) -> Result<Self, PolicyError>;   // missing file: empty
                pub fn grant_for(&self, peer: &str) -> Grant; pub fn peers(&self) -> impl Iterator<Item = &str>; }
#[non_exhaustive] pub enum PolicyError { Read { path, reason }, Parse { path, line, column, message } }
```

`peers.cfg`: `[peer.<id>]` with `allow = [..]`, `fs_roots = [..]`, and `[peer.<id>.exec]` with `programs = [..]`. An unknown key or
permission is `PolicyError::Parse`. A peer with no entry, or no `allow`, holds nothing. `programs` empty means any program and
explicit shell scripts; non-empty means only those programs, each named exactly as the peer asks for it, and no shell script
(a shell runs anything). `fs_roots` empty means no file operation is allowed.

## Execution (`bohio::exec`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a ExecutionConfig, pub slots: &'a Slots }   // Copy
pub fn serve(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<bohio_proto::exec::Exit, ErrorBody>;
```

Serves one `bohio-exec/1` stream: `metadata` is the open's CBOR request. Order of checks: decode (`bad_request`), execution enabled
(`unsupported`), the grant holds `exec` (`denied`), the command (empty argv or a NUL: `bad_request`; unlisted program or a shell
script under a program list: `denied`), the working directory exists (`not_found`), a free slot (`busy`), the start
(`not_found`, `denied`, else `io`). A refused request gets one `error` frame, the same error is returned, and the stream stays
open for the caller to close.

A started process runs in a tree of its own (a job object on Windows; a process group on Unix, plus on Linux a walk of `/proc`
for processes that left it). Frames out: `started`, then `stdout` and `stderr` chunks as the process writes them, then `exit`
after every output byte. The tree is ended by a `kill` frame, the stream closing or failing, the request's `timeout_ms`
(`Exit::timed_out` and `killed`), and always after the process exits: output is awaited for `drain_timeout`, then whatever the
process left running is killed, so nothing outlives a request. Sending waits while the stream has no room, so a slow client
slows the process instead of growing a buffer. `serve` does not close the channel.

`bohio::limit`: `Slots::new(limit)`, `Slots::acquire() -> Option<Permit<'_>>`, `in_use()`, `limit()`; a `Permit` frees its slot
when dropped. One `Slots` is shared by every stream that counts against a bound.

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
then the operation (`not_found`, `exists`, `denied`, `unsupported`, `io`). Messages name what failed, never a path.

**Roots and paths.** `fs_roots` is the whole of what a peer can reach. A relative path starts at the first root; an absolute one
must lie under a root (the longest match; compared without case on Windows and macOS, and `\\?\C:\x` equals `C:\x`). `.` and
`..` are folded lexically; a `..` that would leave the path's start is `outside_root`. Windows also refuses device names (`CON`,
`NUL`, `COM1`...), a colon in a name (alternate data streams), a name ending in a dot or space, `\\.\` and `\\?\GLOBALROOT`
forms and a drive-relative `C:x` (`bad_request`); both separators are accepted there.

**Symbolic links.** The rest of the path is resolved by `cap-std` against the root's directory handle, which follows a link only
while it stays under that handle: a link out of a root, at the end or in the middle of a path, is `outside_root`, and so is an
absolute link even when it points back inside. `remove` removes a link, never what it points at; `stat` without `follow` describes
the link. `symlink` refuses an absolute target or one that climbs above the roots (`outside_root`); `readlink` returns the text
as stored.

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
pub struct Context<'a> { pub filesystem: fs::Context<'a>, pub info: &'a Info }              // Copy
pub fn serve(channel: &Arc<dyn Channel>, context: &Context<'_>) -> u64;                       // requests answered
```

Serves one `bohio/1` stream until the peer closes it or it fails, answering each request in order. `hello` needs no permission
(version 0 is `bad_request`; the answer is the lower of the two versions), `info` needs `info`, filesystem calls as above. A request
that cannot be read is answered with id `0` and `bad_request`; a reply too large for the stream is replaced by an `internal` error.

## Capabilities (`bohio::info`)

```rust
pub fn collect(config: &Config, environment: &Environment) -> bohio_proto::info::Info;
```

The operating system and architecture, the shells found on `environment.path` (`sh`, `bash`, `zsh`, `cmd`, `powershell`,
`pwsh`), the filesystem features (`unix_modes` on Unix, `windows_attributes` on Windows, `case_sensitive` off on Windows and
macOS, the native separator), whether execution and shells are enabled, and the configured bounds. `host_id` is `None`.

## Command line (`bohio::cli`, feature `cli`)

```rust
pub fn main() -> ExitCode;                                       // arguments and environment from the process
pub fn run(arguments: impl IntoIterator<Item = OsString>, environment: &Environment) -> ExitCode;
pub fn capture_environment() -> Environment;
pub struct Cli { pub command: Command }                          // clap Parser; Command: Config(ConfigArgs), Info(InfoArgs)
```

`bohio config check [--config FILE] [--format text|json]` reads the configuration and prints `ok` with a summary;
`bohio info [--config FILE] [--format text|json]` prints this host's capabilities (`json` is `bohio_proto::info::Info`). Exit
status: 0 success, 1 the operation failed (one line on standard error), 2 the invocation was wrong.

## Environment variables

| Name | Effect |
| --- | --- |
| `BOHIO_CONFIG` | the configuration file when `--config` is not given; must exist when set |
| `BOHIO_LOG` | log level or filter for standard error, default `warn` |
| `NO_COLOR` | set: no colour in log lines |
| `PATH` | where shells are looked for in `info` and programs by `exec` |
| `APPDATA`, `LOCALAPPDATA`, `ProgramData` (Windows); `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `HOME` (Unix) | where the configuration and data directory are looked for |

## Gotchas

- `serve` takes `Arc<dyn Channel>` because its output threads outlive a wedged pipe: a descendant the platform cannot end that
  keeps a pipe open leaves a thread behind after `drain_timeout`, not a hung stream.
- A `kill` frame queued behind standard input the process is not reading is seen only when it reads, when the deadline passes or
  when the stream closes.
- Standard input sent to a request that asked for `Stdin::Closed` is ignored.
- The program name is looked up as the host's `PATH` finds it (on Windows also the application's directory and the current one);
  `programs` compares the name exactly as requested, so `git` and `/usr/bin/git` are different entries.
- Windows: a process starts a moment before it joins its job object; a process it starts in that instant is outside the tree.
