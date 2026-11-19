# `bohio-proto` — API header

Header-file-style reference for the `bohio-proto` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `serde`, `ciborium` and `thiserror`. Version `0.0.0-alpha.0`, not published.

Imports: the root holds `decode`, `decode_with_depth`, `encode`, `DEFAULT_DEPTH`, `Error`, `ErrorBody`, `ErrorCode` and
`PROTOCOL_VERSION`. Public modules: `codec`, `control`, `error`, `exec`, `info`, `protocol`, `shell`. No `unsafe` code
(`#![forbid(unsafe_code)]`); no I/O; no process-wide state.

## Protocols (`bohio_proto::protocol`)

```rust
pub const CONTROL: &str = "bohio/1";   pub const EXEC: &str = "bohio-exec/1";   pub const SHELL: &str = "bohio-shell/1";
pub const READ: &str = "bohio-read/1"; pub const WRITE: &str = "bohio-write/1";
```

One stream per operation; the stream's open metadata is the request (CBOR). `PROTOCOL_VERSION` is `1`.

## Codec (`bohio_proto::codec`)

```rust
pub const DEFAULT_DEPTH: usize = 32;
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error>;
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error>;                          // depth DEFAULT_DEPTH
pub fn decode_with_depth<T: DeserializeOwned>(bytes: &[u8], depth: usize) -> Result<T, Error>;
```

CBOR, structs as maps with string keys, enums with data tagged by a `"type"` key in snake case. `decode` ignores keys the type
does not name, refuses trailing bytes and nesting past `depth` (`Error::Decode`). Missing optional keys take their defaults.

## Errors (`bohio_proto::error`)

```rust
#[non_exhaustive] pub enum Error { Encode, Decode, Tag(u8), Short }        // Copy, std::error::Error; never quotes input
#[non_exhaustive] pub enum ErrorCode { BadRequest, Denied, NotFound, Exists, Unsupported, Busy, OutsideRoot, Io, Internal, Other }
pub struct ErrorBody { pub code: ErrorCode, pub message: String }          // Error; ErrorBody::new(code, message)
```

`ErrorCode` is `snake_case` on the wire (`bad_request`, `outside_root`, ...). A code this version does not know decodes as `Other`.

## Control (`bohio_proto::control`)

```rust
pub struct Request { pub id: u64, pub call: Call }
#[non_exhaustive] pub enum Call { Hello { version: u32, client: String }, Info }
pub struct Response { pub id: u64, pub reply: Option<Reply>, pub error: Option<ErrorBody> }   // exactly one is present
impl Response { pub const fn ok(id, reply) -> Self; pub const fn failed(id, error) -> Self; pub fn into_result(self) -> Result<Reply, ErrorBody>; }
#[non_exhaustive] pub enum Reply { Hello { version: u32 }, Info(Box<Info>) }
```

## Info (`bohio_proto::info`)

```rust
pub const PROTOCOL_VERSION: u32 = 1;
pub struct Info { protocol, service: Service{name, version}, host_id: Option<String>, os, arch, shells: Vec<ShellInfo{kind, path}>,
                  filesystem: FilesystemInfo{enabled, symlinks, unix_modes, windows_attributes, case_sensitive, separator},
                  exec: ExecInfo{enabled}, shell: SessionInfo{enabled}, limits: Limits{max_streams, max_processes, max_sessions: Option<u64>} }
```

Every field has a default and unknown keys are ignored: a client works with a host that reports less or more. `os` and `arch` are
`std::env::consts` names. `separator` is the host's native path separator.

## Shells (`bohio_proto::shell`)

```rust
#[non_exhaustive] pub enum ShellKind { Sh, Bash, Zsh, Cmd, Powershell, Pwsh, Other }   // snake_case on the wire
impl ShellKind { pub const KNOWN: [Self; 6]; pub const fn program(self) -> &'static str; }  // the PATH name; "" for Other
```

## Exec (`bohio_proto::exec`)

```rust
pub struct Request { pub command: Command, pub cwd: Option<String>, pub env: Env, pub stdin: Stdin, pub timeout_ms: Option<u64> }
impl Request { pub fn argv(argv) -> Self; pub fn shell(kind: ShellKind, script) -> Self;
               pub fn to_metadata(&self) -> Result<Vec<u8>, Error>; pub fn from_metadata(&[u8]) -> Result<Self, Error>; }
#[non_exhaustive] pub enum Command { Argv { argv: Vec<String> }, Shell { kind: ShellKind, script: String } }
pub struct Env { pub clear: bool, pub set: BTreeMap<String, String>, pub remove: Vec<String> }   // applied: clear, set, remove
#[non_exhaustive] pub enum Stdin { Closed /* default */, Open }
pub struct Started { pub pid: u32 }
pub struct Exit { pub code: Option<i32>, pub signal: Option<i32>, pub timed_out: bool, pub killed: bool }
#[non_exhaustive] pub enum Output { Started(Started), Stdout(Vec<u8>), Stderr(Vec<u8>), Exit(Exit), Error(ErrorBody) }   // host to client
#[non_exhaustive] pub enum Input { Stdin(Vec<u8>), StdinEnd, Kill }                                                    // client to host
impl Output { pub fn encode(&self) -> Result<Vec<u8>, Error>; pub fn decode(&[u8]) -> Result<Self, Error>; }
impl Input  { pub fn encode(&self) -> Vec<u8>;                pub fn decode(&[u8]) -> Result<Self, Error>; }
```

One stream message is one frame: a tag byte, then the body. Tags: host to client `0x01` started (CBOR), `0x02` stdout (raw),
`0x03` stderr (raw), `0x04` exit (CBOR), `0x05` error (CBOR); client to host `0x10` stdin (raw), `0x11` stdin end, `0x12` kill.
`Exit` and `Error` are the last frame the host sends; `Exit` follows every output byte. `Command::Argv` runs the first element
directly and never through a shell; `Command::Shell` is only for a caller that chose a shell. `Output::decode` and
`Input::decode` return `Error::Tag` for a tag this version does not know, which a reader may skip.

## Gotchas

- Raw output is bounded by the stream's message size, not by this crate: a host splits it into frames of at most that size less one.
- `Exit::killed` is `true` whenever the host ended the tree (kill frame, closed stream or deadline); `timed_out` says it was the deadline.
