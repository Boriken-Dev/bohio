# `bohio-client` — API header

Header-file-style reference for the `bohio-client` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `bohio-proto`, `coqui-transport` (with `process`), `coqui-link` (with `sealed`), `coqui-peer`, `coqui-transport-pipe` (`channel`), `coqui-transport-web` (`websocket`, `webtransport`), `coqui-peer-tls`, `rustls` and
`coqui-transport-serial` (each a git revision, one copy), `url`, `socket2`, `serde_json`, `tracing` and `thiserror`. Version
`0.0.0-alpha.0`, not published.

Imports: the root holds `Open` and `Error`. Public modules: `connect`, `control`, `dial`, `exec`, `forward`, `fs`, `link`, `relay`, `shell`. No `unsafe` code
(`#![forbid(unsafe_code)]`). The operations start no threads and do no I/O of their own: every call blocks on the channel it was given
(only `connect` and `relay` start threads, and `connect` opens a socket).

## Opening streams (crate root)

```rust
pub trait Open: Send + Sync {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error>;   // a stream named `protocol` whose open carries `metadata`; coqui_link::mux::LinkStream is a Channel that can also `finish` (half-close), say how much of it is left (`life`) and `close_with` a reason
    fn open_with(&self, protocol: &str, metadata: &[u8], options: &OpenOptions) -> Result<Box<dyn LinkStream>, Error>;   // provided: `open`, ignoring the options
    fn bulk_window(&self) -> Option<u32>;                          // provided: None; what `Reader::start` asks for, `Some` only when above the link's `window`
    fn is_up(&self) -> bool;                                       // provided: true; whether the link can still open streams (a local listener of a forward ends when it cannot)
}
#[derive(Default)] #[non_exhaustive] pub struct OpenOptions { pub receive_window: Option<u32> }   // None: the link's window; `OpenOptions::receive_window(bytes)`. A link grants the difference on the new stream, up to its `max_window`
#[non_exhaustive] pub enum Error { Remote(ErrorBody), Transport(Failure), Protocol(bohio_proto::Error), Link(String), Timeout, Closed }   // Clone, Eq, std::error::Error
```

`Open` is the link: the caller implements it over its authenticated connection (or uses `connect::Connection`) (the protocol names are `bohio_proto::protocol::*`).
An implementation over plain channels (a test's memory pair) wraps each in `relay::Plain`, which is open until closed and cannot finish.
`Failure::Timeout` converts to `Error::Timeout`; every other `Failure` is `Error::Transport`. `Remote` is the host's structured
`{code, message}`.

## Forward (`bohio_client::forward`)

```rust
pub use bohio_proto::forward::{ConnectRequest, Connected, Target};   pub use coqui_link::forward::Options;   // Options: chunk, stalled, idle_min/idle_max, watch, listen_poll, connect_timeout (the host's); validate() refuses zeros
#[non_exhaustive] pub struct ForwardSettings { pub options: Options, pub answer_timeout: Duration /*30 s*/, pub max_connections: usize /*64, a local listener's*/ }   // Default, Copy
pub struct Tunnel;                                                                      // Debug; one connected `bohio-connect/1`; dropping it closes the stream
pub fn connect(link: &dyn Open, target: &Target, settings: &ForwardSettings) -> Result<Tunnel, Error>;   // waits `answer_timeout` for the host's answer
impl Tunnel {
    pub fn address(&self) -> Option<&str>;                      // the address the host's TCP connection reached
    pub fn stream(&self) -> &dyn LinkStream;                    // raw messages both ways; finish() half-closes; finished_by_peer() tells the target's end from a cut
    pub fn into_stream(self) -> Box<dyn LinkStream>;
    pub fn pump_tcp(self, connection: &TcpStream);              // coqui_link::forward::pump_with over the stream, until both directions end
    pub fn pump_ipc(self, connection: coqui_transport_pipe::Stream);   // forward::ipc::pump
    pub fn bridge(self, input: impl Read + Send + 'static, output: &mut dyn Write) -> Result<(), Error>;   // netcat: input's end is a half-close; ends when the host's direction does; Ok only for a finish; input is read on a thread that may outlive the call
}
#[non_exhaustive] pub enum Ended { Stopped, LinkLost }
pub fn listen_tcp(link: &dyn Open, listener: &TcpListener, target: &Target, settings: &ForwardSettings, stop: &AtomicBool, report: &(dyn Fn(&Error) + Sync)) -> io::Result<Ended>;
pub fn listen_ipc(link: &dyn Open, listener: &coqui_transport_pipe::Listener, target: &Target, settings: &ForwardSettings, stop: &AtomicBool, report: &(dyn Fn(&Error) + Sync)) -> io::Result<Ended>;   // one connect and pump per accepted connection, until `stop` or the link ends; bind loopback
pub mod ipc {   // a Unix socket or Windows pipe on one end
    pub fn connect(name: &PipeName, timeout: Duration, options: &Options) -> Result<Stream, coqui_transport_pipe::Error>;   // reads wake every `watch`, a write may take `stalled`
    pub fn listener(name: &PipeName, options: &Options) -> Result<Listener, coqui_transport_pipe::Error>;                 // this user alone, mode 0o600
    pub fn pump<S: LinkStream + ?Sized>(stream: &S, ipc: Stream, options: &Options);   // two threads; a Windows pipe has no half-close, so a finish ends both directions
}
```

An error from `connect`: `Error::Remote(body)` when the host did not connect (`refused`, `unreachable`, `timeout`, `not_found`, `busy`, `unsupported`, `bad_request`),
`Error::Transport(Failure::Permission)` when the peer's grant does not cover the target (refused before the host connects) or was revoked, `Error::Timeout` and
`Error::Closed`. A stream that ends any way but in order resets the host's connection.

## Execution (`bohio_client::exec`)

```rust
pub struct Exec;                                                  // Debug; every method takes &self; Drop closes the stream
impl Exec {
    pub fn start(link: &dyn Open, request: &bohio_proto::exec::Request) -> Result<Self, Error>;
    pub fn next(&self, timeout: Option<Duration>) -> Result<Option<Event>, Error>;    // Ok(None): the host closed in order
    pub fn write_stdin(&self, bytes: &[u8]) -> Result<(), Error>;                     // split to the channel's message size, waits for room
    pub fn finish_stdin(&self) -> Result<(), Error>;
    pub fn kill(&self) -> Result<(), Error>;
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Completed, Error>;
    pub fn close(&self);
}
#[non_exhaustive] pub enum Event { Started { pid: u32 }, Stdout(Vec<u8>), Stderr(Vec<u8>), Exit(Exit) }
pub struct Completed { pub pid: Option<u32>, pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub exit: Exit }
```

`next` skips a frame whose tag it does not know, returns the host's error frame as `Error::Remote`, and `Error::Timeout` when
nothing arrived in `timeout` (the stream goes on). `wait` collects output until the exit; `Error::Closed` when the stream ends
first, `Error::Timeout` when `timeout` passes (the process keeps running). `write_stdin` and `finish_stdin` only mean something
for a request with `Stdin::Open`. Dropping an `Exec` before the exit closes the stream, and the host ends the process tree.

## Shells (`bohio_client::shell`)

```rust
pub struct Shell;                                                 // Debug; every method takes &self; Drop closes the stream: the host ends the tree
impl Shell {
    pub fn start(link: &dyn Open, request: &bohio_proto::shell::Request, timeout: Option<Duration>) -> Result<Self, Error>;   // waits for `started`
    pub const fn pid(&self) -> u32;                               // the host's process id of the shell
    pub fn next(&self, timeout: Option<Duration>) -> Result<Option<Event>, Error>;          // Ok(None): the host closed in order
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error>;       // typed input, split to the channel's message size; Enter is \r
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error>;
    pub fn kill(&self) -> Result<(), Error>;
    pub fn read_until(&self, needle: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, Error>;   // everything read, up to and past the match
    pub fn wait(&self, timeout: Option<Duration>) -> Result<(Vec<u8>, bohio_proto::shell::Exit), Error>;
    pub fn close(&self);
}
#[non_exhaustive] pub enum Event { Data(Vec<u8>), Exit(Exit) }
```

A refusal (`denied`, `not_found`, `busy`, `bad_request`, ...) comes from `Shell::start` as `Error::Remote`. `read_until` is
`Error::Closed` when the shell ends before the text appears; the typed text is echoed by the terminal, so match something the
echo does not hold. Output is a terminal's rendering with escape sequences, not the program's bytes.

## Control stream (`bohio_client::control`)

```rust
pub struct Control;                                               // Debug; every method takes &self, calls take turns; Drop closes the stream
impl Control {
    pub fn open(link: &dyn Open) -> Result<Self, Error>;          // `bohio/1`, no metadata
    pub fn call(&self, call: bohio_proto::control::Call, timeout: Option<Duration>) -> Result<Reply, Error>;
    pub fn hello(&self, client: &str, timeout: Option<Duration>) -> Result<u32, Error>;    // the version the host will speak
    pub fn info(&self, timeout: Option<Duration>) -> Result<Info, Error>;
    pub fn close(&self);
}
```

`call` returns the host's `{code, message}` as `Error::Remote`, `Error::Timeout` when no answer came in time (a late answer is
skipped by the next call), `Error::Closed` when the host closed the stream.

## Hash (`bohio_client::hash`)

```rust
pub fn hash_file(link: &dyn Open, path: &str, algorithms: &[Algorithm]) -> Result<Digests, Error>;
pub fn hash_range(link: &dyn Open, item: HashItem, algorithms: &[Algorithm]) -> Result<Digests, Error>;
pub fn hash_many(link: &dyn Open, items: &[HashItem], algorithms: &[Algorithm]) -> Result<Vec<Result<Digests, ErrorBody>>, Error>;   // one stream; a file that fails fails alone
pub fn hash_items(link: &dyn Open, request: &HashRequest, items: &[HashItem]) -> Result<Vec<Result<HashResult, ErrorBody>>, Error>;   // keeps size, modified, unchanged
```

One `bohio-hash/1` stream each. Items are sent on one scoped thread and results read on another, so a long batch cannot stop on a full stream. A refusal of
the stream (`unsupported`, `busy`, `denied`) is `Error::Remote`; for `hash_file` and `hash_range` a failed item is too. Calls wait without limit.

## Files (`bohio_client::fs`)

```rust
pub struct Fs;                                                    // Debug; one control stream; calls wait without limit until with_timeout
impl Fs {
    pub fn connect(link: &dyn Open) -> Result<Self, Error>;       pub fn with_timeout(self, Option<Duration>) -> Self;     pub fn control(&self) -> &Control;
    pub fn stat(&self, path: &str, follow: bool) -> Result<Metadata, Error>;
    pub fn list(&self, path: &str, after: Option<&str>, limit: Option<u64>) -> Result<Listing, Error>;     // one page
    pub fn list_all(&self, path: &str) -> Result<Vec<Entry>, Error>;                                       // every page
    pub fn mkdir(&self, path: &str, parents: bool) -> Result<(), Error>;      pub fn remove(&self, path: &str, recursive: bool) -> Result<(), Error>;
    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error>;  pub fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error>;
    pub fn symlink(&self, target: &str, link: &str) -> Result<(), Error>;     pub fn readlink(&self, path: &str) -> Result<String, Error>;
    pub fn set_times(&self, path: &str, accessed: Option<Timestamp>, modified: Option<Timestamp>) -> Result<(), Error>;
    pub fn set_mode(&self, path: &str, mode: u32) -> Result<(), Error>;       pub fn set_readonly(&self, path: &str, readonly: bool) -> Result<(), Error>;
}
pub struct Reader;                                                // Debug; Drop closes the stream: the host stops reading
impl Reader {
    pub fn start(link: &dyn Open, request: &ReadRequest, timeout: Option<Duration>) -> Result<Self, Error>;   // waits for the host to open the file
    pub fn start_with(link: &dyn Open, request: &ReadRequest, timeout: Option<Duration>, options: ReadOptions) -> Result<Self, Error>;   // progress frames restart `timeout`
    pub fn header(&self) -> ReadHeader;                           // size, how many bytes will arrive (decompressed), what the host used
    pub const fn compression(&self) -> Option<&Compression>;      // what the host compresses with (the level it clamped to); None: raw
    pub fn wire_bytes(&self) -> u64;                              // the data bytes that travelled so far, compressed where they were
    pub fn digests(&self) -> Option<Digests>;                     // the request's `hashes` of what was sent; Some once the last chunk was read
    pub fn next_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Error>;                  // Ok(None): all arrived
    pub fn copy_to(&self, out: &mut impl Write, timeout: Option<Duration>) -> Result<u64, Error>;           // one chunk in memory
    pub fn read_to_end(&self, timeout: Option<Duration>) -> Result<Vec<u8>, Error>;     pub fn close(&self);
}
pub struct Writer;                                                // Debug; Drop before finish closes the stream: the host abandons the write
impl Writer {
    pub const DEFAULT_CHUNK: usize = 64 * 1024;
    pub fn start(link: &dyn Open, request: &WriteRequest, timeout: Option<Duration>) -> Result<Self, Error>;  // waits for `ready`; compresses when request.compression names an algorithm
    pub fn start_with(link: &dyn Open, request: &WriteRequest, timeout: Option<Duration>, caps: &Caps) -> Result<Self, Error>;   // levels and window this side may use
    pub fn write_compressed(&self, bytes: &[u8]) -> Result<(), Error>;   // a piece of a stream compressed elsewhere (prepared), cut anywhere
    pub fn with_chunk(self, bytes: usize) -> Self;                // most file bytes in one frame; keep it below what the link queues
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error>;       // waits for room
    pub fn finish(&self, timeout: Option<Duration>) -> Result<WriteDone, Error>;   // sends `end`, waits for the host to put the data in place
    pub fn abort(&self);
}
```

```rust
pub struct ReadOptions { pub window_log_max: Option<u32> /* 27 */, pub raw: bool /* hand out the compressed stream undecoded */, pub on_progress: Option<Box<dyn FnMut(u64) + Send>> }   // Default
pub struct PutOptions { pub compression: Option<Compression>, pub prepare: bool, pub caps: Caps, pub temp_dir: Option<PathBuf>, pub chunk: Option<usize> }   // Default
pub fn put(link: &dyn Open, request: &WriteRequest, source: &mut dyn Read, options: &PutOptions, timeout: Option<Duration>) -> Result<WriteDone, Error>;   // prepare: one context into a temp file, then sent
pub fn resolve_auto(info: &Info, wanted: &Compression) -> Option<Compression>;        // `auto` -> the host's compression_default (a client names what it sends)
```

`Reader` decodes `compressed` frames as they arrive (cargo feature `zstd`, on in `bohio`, decodes zstd; without it a zstd header is `Error::Compression`) and stops with
`Error::Protocol` when the output would pass `header.length`; a read that `resume`s, or `raw: true`, hands out the compressed bytes as they travelled. `Writer`
compresses with the same per-frame raw fallback (a raw frame ends the context). `Exec::start_with(link, request, caps)` does the same for standard input and
decodes both outputs; `Exec::compression()` is what `started` reported. `Error::Compression(text)`: an algorithm this build lacks, or `auto` where the client names one.

A refusal (`denied`, `outside_root`, `not_found`, `exists`, `busy`, ...) comes from `Reader::start` and `Writer::start` as
`Error::Remote`, before any data moves. `next_chunk` ends with `Error::Closed` when the stream finishes with a different number of
bytes than the header announced. `finish` is the only proof a write is in place: `Error::Remote` there means nothing was put in
place for `replace` and `create`.

## Endpoints and dialing (`bohio_client::dial`)

```rust
#[non_exhaustive] pub enum Endpoint { Tcp(SocketAddr), Pipe(PipeName), WebSocket(Url), WebTransport { url: Url, fallback: Option<Url> }, Serial { path: String, options: SerialOptions }, Command(Vec<String>) }   // Clone, Eq, Display; WebSocket is `ws` or `wss`
impl Endpoint {
    pub fn parse(text: &str) -> Result<Endpoint, String>;     // the reason, for an unknown scheme or a bad address; `webtransport://` needs a UDP port
    pub fn dial(&self, timeout: Duration, settings: &Settings) -> Result<Box<dyn Channel>, Error>;   // an unproven channel; Error::Transport(why); dial_with and the default trust
    pub fn dial_with(&self, timeout: Duration, settings: &Settings, trust: &ServerTrust) -> Result<Box<dyn Channel>, Error>;   // a refused certificate is Transport(Failure::Certificate); Error::Link when the trust cannot be set up (an unreadable CA file)
    pub fn with_fallback(self, fallback: Endpoint) -> Result<Endpoint, String>;   // a `ws://` or `wss://` endpoint, on any host, after a `webtransport://` one; the caller naming it is the choice (a plain one is readable until the proof, one elsewhere sends the proof there)
}
#[non_exhaustive] pub struct ServerTrust { pub ca_files: Vec<PathBuf>, pub pins: Vec<PeerId>, pub clock_skew: Duration /* default zero */ }   // Default, Clone, Eq; who a TLS client accepts as the host's certificate; a pinned or chained certificate is held to its dates, `clock_skew` is how far from this clock a start or end is still accepted (a certificate made just now by a host whose clock is ahead is refused until this clock reaches its start)
impl ServerTrust { pub fn pin(&mut self, text: &str) -> Result<(), String>; pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, String>; pub fn refusal(&self) -> Option<String>; }   // pin: `sha256:` and 64 hex digits; refusal: why the last certificate a config from this trust refused was refused (the presented certificate's hash, and for its dates the start or end and how far this clock is from it), for the message of a `Failure::Certificate`
#[non_exhaustive] pub struct SerialOptions { pub baud: Option<u32>, pub data_bits: Option<DataBits>, pub parity: Option<Parity>, pub stop_bits: Option<StopBits>, pub flow_control: Option<FlowControl> }   // Default, Copy, Eq
impl SerialOptions { pub fn set(&mut self, key: &str, value: &str) -> Result<(), String>; pub fn or(self, fallback: Self) -> Self; }
pub fn serial(path: &str, options: &SerialOptions, settings: &Settings) -> io::Result<StreamChannel>;   // opens the port: COBS + CRC-32C frames
pub fn command(argv: &[String], settings: &Settings) -> io::Result<StreamChannel>;                       // starts the program; stderr is this process's; its tree ends on close
pub fn web_limits(settings: &Settings) -> coqui_transport_web::Limits;                                    // one proof-sized message, read ahead one at a time, until the proof
pub fn split_command(line: &str) -> Result<Vec<String>, String>;                                         // spaces separate, quotes group, nothing escapes
```

| Endpoint | Transport |
| --- | --- |
| `tcp://host:port` | a socket; a name is resolved and its first address taken; a socket that connected to itself is refused |
| `pipe://NAME`, `pipe:///path/to.sock`, `pipe://\\.\pipe\NAME` | a Windows named pipe, a Unix domain socket (`NAME` in `$XDG_RUNTIME_DIR` or `$TMPDIR`); an endpoint never names another host's pipe |
| `ws://host:port[/path]` | a WebSocket, binary messages, no TLS (plain beyond loopback too: the proof seals what follows) |
| `wss://host:port[/path]` | the same over TLS, the host's certificate checked by the `ServerTrust`: the system's roots by default (`rustls-platform-verifier`), `ca_files` instead of them, `pins` accepted as they are whatever their name and issuer, either when both are set |
| `webtransport://host:port[/path]` | WebTransport over QUIC, always TLS, the same trust; a hash pin is the certificate's SHA-256 whatever its key and validity (a browser's pin is stricter: ECDSA P-256, at most 14 days). With a fallback: a failure of the path (not a certificate, permission or protocol failure) before anything crossed dials the WebSocket endpoint, plain or on another host when it was named so |
| `serial://PORT[?baud=N&data_bits=N&parity=P&stop_bits=N&flow_control=F]` | `PORT` is a device path or `COM3`; `parity` is `none`, `odd`, `even`; `flow_control` is `none` or `hardware` |
| `stdio://PROGRAM ARG...` | the program is run directly with its standard streams as the channel |

Every channel `dial` returns holds the peer to one message of `coqui_link::auth::MAX_PROOF_MESSAGE` until the proof raises the bound to
`Settings::record_length` (`max_frame + 5` plus the seal), so a carrier's own message bound must allow that: a stream channel, a WebSocket and
a COBS channel all follow the proof's `set_max_message`. A serial line has no end of stream: closing it sends nothing, so the host learns the
link ended from the mux's close frames, and a first message sent to a host that is reopening its port is lost (try again).

## The link (`bohio_client::connect`, `link`, `relay`)

```rust
pub struct Connection;                                            // Debug; Open; Drop ends the link and every stream
impl Connection {
    pub fn connect(endpoint: &Endpoint, timeout: Duration, proof: &coqui_link::auth::Proof, settings: &Settings) -> Result<Self, Error>;   // any transport
    pub fn connect_with(endpoint: &Endpoint, timeout: Duration, proof: &coqui_link::auth::Proof, settings: &Settings, trust: &ServerTrust) -> Result<Self, Error>;   // connect() with a TLS trust
    pub fn connect_tcp(address: SocketAddr, timeout: Duration, proof: &coqui_link::auth::Proof, settings: &Settings) -> Result<Self, Error>;   // connect() for tcp
    pub fn over<C: Channel + 'static>(channel: C, proof: &Proof, settings: &Settings) -> Result<Self, Error>;   // an unproven channel to the host
    pub const fn host(&self) -> coqui_peer::PeerId;                // the host's proven id
    pub fn is_up(&self) -> bool;                                   // false once the host or the network ended the link
    pub fn close(&self);                                           // closes every stream and joins the link's thread
    pub fn opener(&self) -> Opener;                                // an Open that is Clone and does not keep the connection alive
}

#[derive(Clone)] pub struct Opener;                               // Debug; Open (Error::Closed once the link ended or `close` was called)
pub struct Opens;                                                 // Debug; the serving half of an Opener
impl Opener {
    pub fn pair(settings: &Settings) -> (Opener, Opens);           // nothing opens until `Opens::run` runs on a link; `settings.bulk_window` is what `Open::bulk_window` answers
    pub fn open_stream(&self, protocol: &str, metadata: &[u8]) -> Result<Opened, Error>;   // as Open::open, keeping the type that says why a stream ended
    pub fn open_stream_with(&self, protocol: &str, metadata: &[u8], options: &OpenOptions) -> Result<Opened, Error>;   // a `receive_window` is granted before the stream is returned
    pub fn is_up(&self) -> bool;                                   // false once the link ended or `close` was called
    pub fn close(&self);                                           // closes every stream this opener opened and makes `Opens::run` return
    pub fn capabilities(&self, wait: Duration) -> Result<coqui_link::link::Capabilities, Error>;   // reads the peer's `link/1` document now, on a thread of the link's
}
impl Opens {
    pub fn run<C: Channel>(self, mux: &coqui_link::mux::Mux<'_, C>, settings: &Settings);   // serves open requests, each stream through a bounded relay, until the link leaves Up, `close`, or every Opener is gone; when the link ends, a stream's reader still gets what the stream held and how it ended (a revoke as `Permission`, `Opened::end` the text) for at most `send_limit`, where `close` ends it at once
}

#[non_exhaustive] pub struct Settings { pub max_streams: usize /*64*/, pub proof_timeout: Duration /*10 s*/, pub window: u32 /*1 MiB*/, pub max_window: u32 /*1 MiB*/, pub auto_window: bool /*false*/,
    pub bulk_window: u32 /*1 MiB*/, pub max_frame: usize /*64 KiB*/, pub max_metadata: usize /*4096*/, pub max_pending_opens: usize /*8*/, pub max_opens_per_minute: u32 /*600*/,
    pub tcp_send_buffer: Option<usize> /*None: system*/, pub tcp_receive_buffer: Option<usize> /*None: system*/,
    pub max_message: usize /*1 MiB*/, pub queue_bytes: usize /*256 KiB*/, pub read_ahead: usize /*8*/, pub queued_bytes: usize /*1 MiB*/,
    pub send_limit: Duration /*10 s*/, pub send_wait: Duration /*250 ms*/, pub poll_interval: Duration /*50 ms*/,
    pub heartbeat: Duration /*25 s; zero is off*/, pub heartbeat_misses: u32 /*3; at least 1*/ }   // Default; the host's `[link]` section
impl Settings { pub fn limits(&self) -> coqui_link::mux::Limits; pub fn record_length(&self, method: coqui_link::auth::Method) -> usize; }
pub fn framed<S: Split, C: Codec>(stream: S, codec: C, settings: &Settings) -> io::Result<StreamChannel>;   // an unproven peer held to one proof-sized message
pub fn stream<S: Split>(stream: S, settings: &Settings) -> io::Result<StreamChannel>;   // framed with a 4-byte big-endian length: a pipe, a child's streams
pub fn socket(stream: TcpStream, settings: &Settings) -> io::Result<StreamChannel>;   // stream() with TCP_NODELAY and the TCP buffers `settings` name
pub fn connect(address: &SocketAddr, timeout: Duration, settings: &Settings) -> io::Result<TcpStream>;   // the buffers set before connecting (`TcpStream::connect_timeout` when none is set)
pub fn tune_listener(listener: &TcpListener, settings: &Settings) -> io::Result<()>;   // the buffers on a listening socket
pub fn stdio(settings: &Settings) -> io::Result<StreamChannel>;                       // stream() over this process's standard input and output

pub struct Relay;                                                 // Debug; Channel and LinkStream: an owned, bounded channel over one stream of a link; `finish` is queued behind what is sent, `finished_by_peer` and `life` follow the stream (a peer that finished leaves this side's direction open), `close_with` closes the stream with a reason once what is queued is delivered
#[derive(Debug)] pub struct Plain<C>(pub C);                      // a Channel as a LinkStream with no half-close: open until closed, finish is Unsupported, close_with closes
pub fn relay<'scope, 'env, P: Deref + Clone + Send + 'scope>(scope: &'scope thread::Scope<'scope, 'env>, stream: P, settings: &Settings) -> Arc<Relay>
pub fn relay_with<'scope, 'env, P: Deref + Clone + Send + 'scope, E: Fn(&P::Target) -> Option<coqui_link::mux::StreamEnd> + Send + 'scope>(scope: &'scope thread::Scope<'scope, 'env>, stream: P, settings: &Settings, ended: E) -> Arc<Relay>;   // relay, asking `ended` why the stream ended; Relay::end() -> Option<StreamEnd> gives it
#[derive(Debug)] pub struct Opened;                                // an Open'ed stream: Channel and LinkStream; end() -> Option<coqui_link::mux::StreamEnd> (a refusal or revoke with the peer's text)
    where P::Target: LinkStream;
```

`connect_tcp` and `over` prove this side to the host and the host to this side inside `settings.proof_timeout`, with the keys `proof` holds
(`Proof::builder(identity).responders(Trust::Store(store) | Trust::pinned([id]))`; the identity and store are the caller's, `bohio::peer::Data`
makes both). A host whose key this side does not trust, or that does not accept this side's key yet (it records it as waiting), ends the
connection without a word: both are `Error::Transport`, a failed proof of another kind is `Error::Link(text)`. A socket that connected to itself
is refused. A thread runs the link and two more run per open stream; `Open::open` returns a `Relay` handle, so a stream is `'static` and
outlives the call. A stream the host refused (no grant, no such protocol) or ended because the peer lost its grant fails with
`Error::Transport(Failure::Permission)` from the call that reads it; a lost link is `Failure::PathLost`.

A `Relay` holds at most `queue_bytes` in each direction (a message larger than that passes alone), so a slow reader or writer makes the other side
wait; closing it delivers what is queued for at most `send_limit`. What one stream can buffer is `window` plus `2 x queue_bytes` (`bulk_window` instead of
`window` for a stream widened by it, up to `max_window`); what a link buffers besides is `read_ahead` messages and `queued_bytes` at the socket. Nothing grows with a
file's size.

One stream moves at most its receive window per round trip. `window` is what each sender starts every stream with and is not negotiated, so keep it equal on both
sides: a side whose window is larger can make the other end its streams as broken. `Open::open_with` and `Settings::bulk_window` widen one stream instead, by
granting credit the sender already accepts, which needs no setting at the peer; a link logs once, as a warning and not a refusal, a `window` or `max_frame` its
peer's `link/1` document shows differing from its own.

## Gotchas

- One thread that writes a large input before reading can deadlock against a process that echoes it; write on one thread and
  read on another (every method takes `&self`).
- A refused request surfaces from `next` or `wait` as `Error::Remote`, not from `start`: the host answers on the stream.
