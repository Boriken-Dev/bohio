# `bohio-proto` — API header

Header-file-style reference for the `bohio-proto` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `serde`, `ciborium` and `thiserror`. Version `0.0.0-alpha.0`, not published.

Imports: the root holds `decode`, `decode_with_depth`, `encode`, `DEFAULT_DEPTH`, `Error`, `ErrorBody`, `ErrorCode` and
`PROTOCOL_VERSION`. Public modules: `codec`, `control`, `error`, `exec`, `forward`, `fs`, `gui`, `hash`, `info`, `local`, `protocol`, `shell`. No `unsafe` code
(`#![forbid(unsafe_code)]`); no I/O; no process-wide state.

## Protocols (`bohio_proto::protocol`)

```rust
pub const CONTROL: &str = "bohio/1";   pub const EXEC: &str = "bohio-exec/1";   pub const SHELL: &str = "bohio-shell/1";
pub const READ: &str = "bohio-read/1"; pub const WRITE: &str = "bohio-write/1"; pub const HASH: &str = "bohio-hash/1";
pub const CONNECT: &str = "bohio-connect/1"; pub const TCP: &str = "tcp/1"; pub const LISTEN: &str = "bohio-listen/1"; pub const ACCEPT: &str = "bohio-accept/1";   // `tcp/1` is coqui-link's: metadata is the text HOST:PORT, raw bytes at once, no answer
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
#[non_exhaustive] pub enum ErrorCode { BadRequest, Denied, NotFound, Exists, Unsupported, Busy, OutsideRoot, Io, Refused, Unreachable, Timeout, Unavailable, InUse, Changed, Internal, Other }   // `Unavailable`: a desktop that exists and cannot be used now (locked for input, not at the screen); `Refused`, `Unreachable` and `Timeout` are a forward's connect failing
pub struct ErrorBody { pub code: ErrorCode, pub message: String }          // Error; ErrorBody::new(code, message)
```

`ErrorCode` is `snake_case` on the wire (`bad_request`, `outside_root`, ...). A code this version does not know decodes as `Other`.

## Control (`bohio_proto::control`)

```rust
pub struct Request { pub id: u64, pub call: Call }
#[non_exhaustive] pub enum Call {
    Hello { version: u32, client: String }, Info,
    Stat { path, follow }, List { path, after: Option<String>, limit: Option<u64> }, Mkdir { path, parents, mode: Option<u32> },
    Remove { path, recursive }, Rename { from, to, overwrite }, Copy { from, to, overwrite }, Symlink { target, link }, Readlink { path },
    SetTimes { path, accessed: Option<Timestamp>, modified: Option<Timestamp> }, SetMode { path, mode: u32 },
    SetAttributes { path, readonly: Option<bool> } }   // `bool`s default false
pub struct Response { pub id: u64, pub reply: Option<Reply>, pub error: Option<ErrorBody> }   // exactly one is present
impl Response { pub const fn ok(id, reply) -> Self; pub const fn failed(id, error) -> Self; pub fn into_result(self) -> Result<Reply, ErrorBody>; }
#[non_exhaustive] pub enum Reply { Hello { version: u32, transfer: TransferInfo }, Info(Box<Info>), Stat(Box<Metadata>), List(Listing), Done,
                                    Link { target: String } }
```

The control stream's messages are bare CBOR, one request or response per stream message (no tag byte). `Done` answers every call
that returns nothing. The `bohio/1` open carries no metadata.

## Filesystem (`bohio_proto::fs`)

```rust
pub struct Timestamp { pub secs: i64, pub nanos: u32 }                                   // Copy, Default; secs since 1970, negative before
#[non_exhaustive] pub enum FileKind { File, Directory, Symlink, Other }                  // snake_case; unknown kinds decode as Other
pub struct Metadata { kind, size: u64, modified/accessed/created: Option<Timestamp>, mode: Option<u32> /* unix bits, 0o7777 */,
                      readonly: bool, attributes: Option<u32> /* windows */ }
pub struct Entry { pub name: String, pub metadata: Metadata }                            // not following a link
pub struct Listing { pub entries: Vec<Entry>, pub next: Option<String>, pub skipped: u64 }   // ascending by name; `next` = pass as `after`
pub struct ReadRequest { path, offset: u64 /* 0 */, length: Option<u64>, hashes: Vec<Algorithm>, compression: Option<Compression>, prepare: bool, resume: Option<Resume>, delta: Option<ReadDelta> }   // open metadata of bohio-read/1; ReadRequest::whole(path)
pub struct ReadHeader { size: u64, length: u64 /* decompressed */, compression: Option<Compression> /* what the host used, never auto */, compressed_length: Option<u64>, token: Option<String>, delta: bool /* copy frames and data add up to length */ }   // ReadHeader::raw(size, length)
pub const WEAK_RSYNC: &str = "rsync";
pub struct DeltaCopy { pub block: u64, pub count: u64 }                              // consecutive blocks of the old file; the last block may be shorter
pub struct ReadDelta { block_size: u64, weak: String /* "rsync" */, strong: Algorithm, strong_len: u64, base_size: u64 }   // `delta` of a ReadRequest: the client sends its signature after the open
pub struct SignatureBatch { first: u64, weak: Vec<u32>, strong: Vec<u8> /* strong_len bytes per block, joined */ }
pub struct DeltaBase { size: u64, modified: Option<Timestamp>, hashes: Digests }   // what a delta write was made against
#[non_exhaustive] pub struct ReadDone { pub hashes: Digests }                        // the digests the request named, of the bytes sent
#[non_exhaustive] pub enum ReadOutput { Header(ReadHeader), Data(Vec<u8>), Compressed(Vec<u8>) /* 0x06 */, Progress(Progress) /* 0x08 */, Copy(DeltaCopy) /* 0x09 */, Done(ReadDone), Error(ErrorBody) }
#[non_exhaustive] pub enum ReadInput { Signature(SignatureBatch) /* 0x12 */, SignatureEnd /* 0x13 */ }   // client to host, a delta read only
#[non_exhaustive] pub enum WriteMode { Replace /* default */, Create, Append, At { offset: u64 }, Delta { block_size: u64, base: DeltaBase } }   // tagged by "type"; no longer Copy (Delta holds digests); a delta needs fs.read as well
pub struct WriteRequest { path, mode: WriteMode, parents: bool, file_mode: Option<u32>, expect: Digests, hashes: Option<Vec<Algorithm>>, compression: Option<Compression> }   // WriteRequest::replace(path); hashes None: the host's write_hashes; compression: what the client sends, never Auto
pub struct WriteDone { pub written: u64, pub hashes: Digests }
#[non_exhaustive] pub enum WriteOutput { Ready, Done(WriteDone), Error(ErrorBody) }
#[non_exhaustive] pub enum WriteInput { Data(Vec<u8>), Compressed(Vec<u8>) /* 0x14 */, Copy(DeltaCopy) /* 0x15 */, End }
// each Request: to_metadata() / from_metadata(&[u8]); each frame enum: encode() / decode(&[u8]) (Error::Tag for an unknown tag)
```

Paths are the host's native text (`info.filesystem.separator`); a Windows host also accepts `/`. A relative path starts at the
host's first granted root. `bohio-read/1` frames: host to client `0x01` header (CBOR), `0x02` data (raw), `0x04` done (CBOR `ReadDone`), `0x05` error
(CBOR); `done` follows exactly `header.length` data bytes. `bohio-write/1`: host to client `0x01` ready, `0x04` done (CBOR
`WriteDone`), `0x05` error; client to host `0x10` data (raw), `0x11` end. A write is in place only after the host's `done`.

## Hash (`bohio_proto::hash`)

```rust
#[non_exhaustive] pub enum Algorithm { Md5, Sha1, Sha256, Sha384, Sha512, Blake3, Xxh3, Xxh128, Xxh64, Crc32c, Other(String) }   // Ord; a string on the wire
impl Algorithm { pub const KNOWN: [Algorithm; 10]; pub fn name(&self) -> &str; pub fn from_name(&str) -> Self; pub const fn is_known(&self) -> bool; pub const fn hex_len(&self) -> Option<usize>; }
pub type Digests = BTreeMap<Algorithm, String>;                // lowercase hex in the byte order md5sum, b3sum, xxhsum print
pub struct HashRequest { algorithms: Vec<Algorithm>, blocks: Option<BlockOptions> }   // open metadata of bohio-hash/1; HashRequest::new(algorithms)
pub struct HashItem { path, offset: u64, length: Option<u64>, if_size: Option<u64>, if_modified: Option<Timestamp> }   // HashItem::whole(path)
pub struct HashResult { index: u64, size: u64, modified: Option<Timestamp>, length: u64, unchanged: bool, hashes: Digests, block_size: Option<u64>, strong_len: Option<u64> }   // the last two with `blocks` asked for
pub struct BlockOptions { size: Option<u64>, weak: Option<String>, strong_len: Option<u64> }   // the signature of each item: strong hash = the request's first algorithm cut to strong_len
pub struct HashBlocks { index, first, size: u64, weak: Vec<u32>, strong: Vec<u8> }   // 0x02: the next blocks of an item's signature, before that item's result
pub struct ItemError { index: u64, code: ErrorCode, message: String }               // ItemError::new(index, body), .body()
pub struct HashDone { pub items: u64 }
#[non_exhaustive] pub enum HashOutput { Result(HashResult), Blocks(HashBlocks), ItemError(ItemError), Done(HashDone), Error(ErrorBody) }   // tags 0x01..0x05
#[non_exhaustive] pub enum HashInput { Item(HashItem), End }                         // tags 0x10, 0x11
```

`bohio-hash/1`: one stream of items, answered in order; an item that fails is an `ItemError` and the stream goes on. See `docs/protocol.md`.

## Forward (`bohio_proto::forward`)

```rust
#[non_exhaustive] pub enum Target { Tcp { host: String, port: u16 }, Unix { path: String }, Pipe { name: String } }   // tagged by "type", snake_case; Hash, Display, FromStr (BadTarget): HOST:PORT (an IPv6 address in brackets), unix:PATH, pipe:NAME
impl Target { pub const fn kind(&self) -> &'static str }                                // "tcp", "unix" or "pipe"
pub struct BadTarget;                                                                   // Display, Error
pub struct ConnectRequest { pub target: Target }                                        // the open's metadata of `bohio-connect/1`
impl ConnectRequest { pub const fn new(Target) -> Self; pub fn to_metadata(&self) -> Result<Vec<u8>, Error>; pub fn from_metadata(&[u8]) -> Result<Self, Error> }   // an unknown kind is Error::Decode
pub struct Connected { pub address: Option<String> }                                    // Default; the address a TCP connection reached
#[non_exhaustive] pub enum Answer { Connected(Connected), Error(ErrorBody) }            // tags 0x01 and 0x05, the host's first frame; encode() -> Result<Vec<u8>, Error>, decode(&[u8])
pub struct ForwardInfo { pub enabled: bool, pub connect: Vec<String>, pub listen: Vec<String> }   // `info.forward`: the target kinds the host connects to and the listener kinds it binds ("tcp", "unix" or "pipe")
#[non_exhaustive] pub enum Bind { Tcp { host: String, port: u16 /* 0 = the host chooses */ }, Unix { path: String }, Pipe { name: String } }   // Target's shape and spelling (FromStr/Display, BadTarget); a TCP host is an IP address
pub struct ListenRequest { pub bind: Bind, pub backlog: Option<u32> }                  // the open's metadata of `bohio-listen/1`; new(Bind), to_metadata(), from_metadata()
pub struct Listening { pub address: String, pub token: String }                        // `0x01`: what was bound (a TCP address with the chosen port, `unix:PATH`, `pipe:NAME`); a 128-bit hex token
pub struct Arrival { pub id: u64, pub source: Option<String> }                         // `0x02`
#[non_exhaustive] pub enum ListenFrame { Listening(Listening), Arrival(Arrival), Error(ErrorBody) }   // tags 0x01, 0x02, 0x05; encode(), decode(&[u8])
pub struct AcceptRequest { pub token: String, pub id: u64 }                            // the open's metadata of `bohio-accept/1`; the host answers with `Answer`
```

`bohio-connect/1`: after `Connected` the stream is raw bytes both ways, untagged, a `finish` a half-close; an `Error` is the last frame. See `docs/protocol.md`.

## Gui (`bohio_proto::gui`)

```rust
pub const GUI: &str = "bohio-gui/1";   // in `protocol`
pub struct GuiRequest { pub session: Option<String>, pub want: Option<Vec<Want>> }   // `new(want)`, `wants()` (default [View], sorted), to_metadata / from_metadata
pub enum Want { View, Input, Clipboard }                          // "view", "input", "clipboard"
pub struct DisplayId(pub String);   pub struct Rect { x, y, width, height: u32 }  // `fits(w, h)`, `area()`
pub struct Display { id, name, primary, x, y: i32, width, height, scale: f64, rotation }   pub struct State { active, locked: Option<bool>, displays }
pub struct Ready { session, granted: Vec<Want>, state: State }
pub enum ImageFormat { Png, Jpeg, Webp }                          // ALL, name(), from_name()
pub struct Snapshot { seq, display, region, format, quality, max_width, max_height, cursor: bool, settle: Option<SettleOptions> }   // Default
pub struct ImageHeader { seq, display, format, width, height, region, scale: f64, cursor: Option<Point>, length: u64, settled: Option<Settled> }
pub enum InputEvent { Move, Button, Scroll, Key, Text, Wait }     // tagged by `type`; `InputEvent::combo("ctrl+shift+t") -> Result<Vec<_>, BadCombo>` (xdotool names)
pub const KEY_NAMES: &[&str];   pub fn is_key(key: &str) -> bool;  // a listed name or exactly one character
pub struct Input { seq, events }   pub struct InputDone { seq, applied }   pub struct Settle { seq, display, region, quiet_ms, timeout_ms }   pub struct SettledFrame { seq, settled, waited_ms }
pub struct ClipboardGet { seq }   pub struct ClipboardSet { seq, text }   pub struct ClipboardText { seq, text }   pub struct RequestError { seq, code, message }   // `new(seq, body)`, `body()`
#[non_exhaustive] pub enum GuiOutput { Ready, Image, Data(Vec<u8>), Error, InputDone, Settled, Clipboard, State, RequestError }   // encode / decode; tags 0x01 0x02 0x03 0x05 0x06 0x07 0x08 0x09 0x0b
#[non_exhaustive] pub enum GuiInput { Snapshot, Input, Settle, ClipboardGet, ClipboardSet }                                       // tags 0x10 0x14 0x15 0x16 0x17
pub struct GuiInfo { enabled, available, reason, backend, input, clipboard, lock_detection, formats, sessions: Vec<GuiSession> }   // `info.gui`
pub enum ApproveOutput { Pending { requests: Vec<PendingRequest> }, Decided { id, found } }   pub enum ApproveInput { Decide { id, allow } }   // `bohio-gui-approve/1` (`local::GUI_APPROVE`)
```

`Info` is no longer `Eq` (a scale is a float). Tags `0x04`, `0x0a`, `0x11` to `0x13` are reserved for a screen stream.

## Info (`bohio_proto::info`)

```rust
pub const PROTOCOL_VERSION: u32 = 1;
pub struct Info { protocol, service: Service{name, version}, host_id: Option<String>, os, arch, shells: Vec<ShellInfo{kind, path}>,
                  filesystem: FilesystemInfo{enabled, symlinks, unix_modes, windows_attributes, case_sensitive, separator},
                  exec: ExecInfo{enabled}, shell: SessionInfo{enabled}, limits: Limits{max_streams, max_processes, max_sessions: Option<u64>},
                  transfer: TransferInfo{hashes: Vec<Algorithm>, hash_limits: HashLimits{max_streams: u64, rate: u64 /* 0 unlimited */},
                         compression: Vec<CompressionInfo{algorithm: Codec, levels: Levels{min, max}, default_level: i32, window_log_max: u32}>,
                         compression_default: Option<Compression>, prepared: bool,
                         delta: DeltaInfo{weak: Vec<String>, min_block, max_block, min_size, max_signature_bytes: u64}},
                  forward: ForwardInfo{enabled, connect, listen}, gui: GuiInfo }
```

Every field has a default and unknown keys are ignored: a client works with a host that reports less or more. `os` and `arch` are
`std::env::consts` names. `separator` is the host's native path separator.

## Shells (`bohio_proto::shell`)

```rust
#[non_exhaustive] pub enum ShellKind { Sh, Bash, Zsh, Cmd, Powershell, Pwsh, Other }   // snake_case on the wire
impl ShellKind { pub const KNOWN: [Self; 6]; pub const fn program(self) -> &'static str; }  // the PATH name; "" for Other
pub const DEFAULT_COLS: u16 = 80;  pub const DEFAULT_ROWS: u16 = 24;  pub const MAX_DIMENSION: u16 = 32_767;   // ConPTY holds sizes as i16

pub struct Request { pub program: Program, pub cwd: Option<String>, pub env: exec::Env, pub cols: u16 /* 80 */, pub rows: u16 /* 24 */ }
impl Request { pub fn kind(ShellKind) -> Self; pub fn argv(argv) -> Self;
               pub fn to_metadata(&self) -> Result<Vec<u8>, Error>; pub fn from_metadata(&[u8]) -> Result<Self, Error>; }
#[non_exhaustive] pub enum Program { Kind { kind: ShellKind }, Argv { argv: Vec<String> } }       // tagged by "type"
pub struct Started { pub pid: u32 }
pub struct Exit { pub code: Option<i32>, pub signal: Option<i32>, pub killed: bool }              // Default
pub struct Size { pub cols: u16, pub rows: u16 }
#[non_exhaustive] pub enum Output { Started(Started), Data(Vec<u8>), Exit(Exit), Error(ErrorBody) }   // host to client
#[non_exhaustive] pub enum Input { Data(Vec<u8>), Resize(Size), Kill }                                  // client to host
impl Output { pub fn encode(&self) -> Result<Vec<u8>, Error>; pub fn decode(&[u8]) -> Result<Self, Error>; }
impl Input  { pub fn encode(&self) -> Result<Vec<u8>, Error>; pub fn decode(&[u8]) -> Result<Self, Error>; }
```

`bohio-shell/1`: one stream is one session; the open's metadata is the `Request`. Tags: host to client `0x01` started (CBOR),
`0x02` data (raw terminal bytes, escape sequences included), `0x04` exit (CBOR), `0x05` error (CBOR); client to host `0x10` data
(raw; Enter is `\r`), `0x11` resize (CBOR `Size`), `0x12` kill. `Started` is the first frame; `Exit` and `Error` are the last
the host sends, and `Exit` follows every output byte. `Output::decode` and `Input::decode` return `Error::Tag` for a tag this
version does not know, which a reader may skip.

## Exec (`bohio_proto::exec`)

```rust
pub struct Request { pub command: Command, pub cwd: Option<String>, pub env: Env, pub stdin: Stdin, pub timeout_ms: Option<u64>, pub output_compression: Option<Compression>, pub stdin_compression: Option<Compression> }
impl Request { pub fn argv(argv) -> Self; pub fn shell(kind: ShellKind, script) -> Self;
               pub fn to_metadata(&self) -> Result<Vec<u8>, Error>; pub fn from_metadata(&[u8]) -> Result<Self, Error>; }
#[non_exhaustive] pub enum Command { Argv { argv: Vec<String> }, Shell { kind: ShellKind, script: String } }
pub struct Started { pub pid: u32, pub output_compression: Option<Compression>, pub stdin_compression: Option<Compression> }   // Started::new(pid)
#[non_exhaustive] pub enum Stdin { Closed /* default */, Open }
#[non_exhaustive] pub enum Output { Started(Started), Stdout(Vec<u8>), Stderr(Vec<u8>), StdoutCompressed(Vec<u8>) /* 0x06 */, StderrCompressed(Vec<u8>) /* 0x07 */, Exit(Exit), Error(ErrorBody) }   // host to client
#[non_exhaustive] pub enum Input { Stdin(Vec<u8>), StdinCompressed(Vec<u8>) /* 0x13 */, StdinEnd, Kill }                              // client to host
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

## Local control (`bohio_proto::local`)

Spoken only on a running node's local endpoint, never on a peer's link (CDDL in `docs/protocol.md`, "Local control").

```rust
pub const VIA: &str = "bohio-via/1";   pub const LINKS: &str = "bohio-links/1";   pub const PAIR: &str = "bohio-pair/1";
pub struct PairRequest { lifetime_ms: Option<u64> }   // Default; to_metadata(), from_metadata (empty is the default)
pub enum PairReply { Armed { pin: String, lifetime_ms: u64 }, Paired { peer: String }, Ended { reason: String } }   // encode(), decode(&[u8]); first Armed, then Paired or Ended
pub struct ViaRequest { peer: String, protocol: String, metadata: Vec<u8> /* a CBOR byte string */ }   // to_metadata(), from_metadata(&[u8])
pub struct LinksRequest { wait_ms: Option<u64> }   // Default; to_metadata(), from_metadata (empty is the default)
pub struct LinkEntry { peer, side: String /* dialed|answered */, endpoint: Option<String>, source: Option<String>, since: u64, mine: Vec<String>, theirs: Option<Vec<String>> }
pub struct LinksReply { links: Vec<LinkEntry> }    // Default; encode(), decode(&[u8])
```

## Compression (`bohio_proto::compress`)

```rust
#[non_exhaustive] pub enum Codec { Zstd, Lz4, Deflate, None, Auto, Other(String) }   // Ord; a string on the wire; Codec::KNOWN = the three that compress
impl Codec { pub fn name(&self) -> &str; pub fn from_name(&str) -> Self; pub const fn compresses(&self) -> bool; }
pub struct Compression { pub algorithm: Codec, pub level: Option<i32>, pub window_log: Option<u32> }   // the request, or what the host used
impl Compression { const fn none() / auto() / new(Codec); fn level(self, i32) / window_log(self, u32); const fn is_requested(&self) -> bool; fn parse("zstd:19") -> Result<Self, String>; }
pub struct Resume { pub token: String, pub offset: u64 }      // a prepared read, from a compressed offset
pub struct Progress { pub prepared: u64 }                     // `0x08` frame: bytes of the file compressed so far
```

Compressed bytes travel in a tag of their own beside the raw one (read host to client `0x06`, write client to host `0x14`, exec `0x06` stdout and `0x07`
stderr from the host, `0x13` stdin from the client). The new fields of requests and headers are left out of the CBOR when absent, so a peer that never asks
sends the bytes it always did. **A raw frame ends the compression context**; the rules and bounds are in `docs/protocol.md` ("Compression").
