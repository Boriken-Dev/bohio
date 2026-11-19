# `bohio-client` — API header

Header-file-style reference for the `bohio-client` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `bohio-proto`, `coqui-transport`, `coqui-link` (with `sealed`) and `coqui-peer` (each a git revision, one copy) and `thiserror`. Version
`0.0.0-alpha.0`, not published.

Imports: the root holds `Open` and `Error`. Public modules: `connect`, `control`, `exec`, `fs`, `link`, `relay`, `shell`. No `unsafe` code
(`#![forbid(unsafe_code)]`). The operations start no threads and do no I/O of their own: every call blocks on the channel it was given
(only `connect` and `relay` start threads, and `connect` opens a socket).

## Opening streams (crate root)

```rust
pub trait Open: Send + Sync {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn Channel>, Error>;   // a stream named `protocol` whose open carries `metadata`
}
#[non_exhaustive] pub enum Error { Remote(ErrorBody), Transport(Failure), Protocol(bohio_proto::Error), Link(String), Timeout, Closed }   // Clone, Eq, std::error::Error
```

`Open` is the link: the caller implements it over its authenticated connection (or uses `connect::Connection`) (the protocol names are `bohio_proto::protocol::*`).
`Failure::Timeout` converts to `Error::Timeout`; every other `Failure` is `Error::Transport`. `Remote` is the host's structured
`{code, message}`.

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
    pub fn checksum(&self, path: &str, offset: u64, length: Option<u64>) -> Result<Checksum, Error>;       // Checksum { sha256, length }
}
pub struct Reader;                                                // Debug; Drop closes the stream: the host stops reading
impl Reader {
    pub fn start(link: &dyn Open, request: &ReadRequest, timeout: Option<Duration>) -> Result<Self, Error>;   // waits for the host to open the file
    pub fn header(&self) -> ReadHeader;                           // size, and how many bytes will arrive
    pub fn next_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Error>;                  // Ok(None): all arrived
    pub fn copy_to(&self, out: &mut impl Write, timeout: Option<Duration>) -> Result<u64, Error>;           // one chunk in memory
    pub fn read_to_end(&self, timeout: Option<Duration>) -> Result<Vec<u8>, Error>;     pub fn close(&self);
}
pub struct Writer;                                                // Debug; Drop before finish closes the stream: the host abandons the write
impl Writer {
    pub const DEFAULT_CHUNK: usize = 64 * 1024;
    pub fn start(link: &dyn Open, request: &WriteRequest, timeout: Option<Duration>) -> Result<Self, Error>;  // waits for `ready`
    pub fn with_chunk(self, bytes: usize) -> Self;                // most file bytes in one frame; keep it below what the link queues
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error>;       // waits for room
    pub fn finish(&self, timeout: Option<Duration>) -> Result<WriteDone, Error>;   // sends `end`, waits for the host to put the data in place
    pub fn abort(&self);
}
```

A refusal (`denied`, `outside_root`, `not_found`, `exists`, `busy`, ...) comes from `Reader::start` and `Writer::start` as
`Error::Remote`, before any data moves. `next_chunk` ends with `Error::Closed` when the stream finishes with a different number of
bytes than the header announced. `finish` is the only proof a write is in place: `Error::Remote` there means nothing was put in
place for `replace` and `create`.

## The link (`bohio_client::connect`, `link`, `relay`)

```rust
pub struct Connection;                                            // Debug; Open; Drop ends the link and every stream
impl Connection {
    pub fn connect_tcp(address: SocketAddr, timeout: Duration, proof: &coqui_link::auth::Proof, settings: &Settings) -> Result<Self, Error>;
    pub fn over<C: Channel + 'static>(channel: C, proof: &Proof, settings: &Settings) -> Result<Self, Error>;   // an unproven channel to the host
    pub const fn host(&self) -> coqui_peer::PeerId;                // the host's proven id
    pub fn is_up(&self) -> bool;                                   // false once the host or the network ended the link
    pub fn close(&self);                                           // closes every stream and joins the link's thread
}
pub const DEFAULT_MAX_STREAMS: usize = 64;

#[non_exhaustive] pub struct Settings { pub proof_timeout: Duration /*10 s*/, pub window: u32 /*1 MiB*/, pub max_frame: usize /*64 KiB*/,
    pub max_message: usize /*1 MiB*/, pub queue_bytes: usize /*256 KiB*/, pub read_ahead: usize /*8*/, pub queued_bytes: usize /*1 MiB*/,
    pub send_limit: Duration /*10 s*/, pub send_wait: Duration /*250 ms*/, pub poll_interval: Duration /*50 ms*/ }   // Default; the host's `[link]` section
impl Settings { pub fn limits(&self, max_streams: usize) -> coqui_link::mux::Limits; pub fn record_length(&self, method: coqui_link::auth::Method) -> usize; }
pub fn socket(stream: TcpStream, settings: &Settings) -> io::Result<StreamChannel>;   // length-prefixed, an unproven peer held to one proof-sized message

pub struct Relay;                                                 // Debug; Channel: an owned, bounded channel over one stream of a link
pub fn relay<'scope, 'env, P: Deref + Clone + Send + 'scope>(scope: &'scope thread::Scope<'scope, 'env>, stream: P, settings: &Settings) -> Arc<Relay>
    where P::Target: Channel;
```

`connect_tcp` and `over` prove this side to the host and the host to this side inside `settings.proof_timeout`, with the keys `proof` holds
(`Proof::builder(identity).responders(Trust::Store(store) | Trust::pinned([id]))`; the identity and store are the caller's, `bohio::peer::Data`
makes both). A host whose key this side does not trust, or that does not accept this side's key yet (it records it as waiting), ends the
connection without a word: both are `Error::Transport`, a failed proof of another kind is `Error::Link(text)`. A socket that connected to itself
is refused. A thread runs the link and two more run per open stream; `Open::open` returns a `Relay` handle, so a stream is `'static` and
outlives the call. A stream the host refused (no grant, no such protocol) or ended because the peer lost its grant fails with
`Error::Transport(Failure::Permission)` from the call that reads it; a lost link is `Failure::PathLost`.

A `Relay` holds at most `queue_bytes` in each direction (a message larger than that passes alone), so a slow reader or writer makes the other side
wait; closing it delivers what is queued for at most `send_limit`. What one stream can buffer is `window` plus `2 x queue_bytes`; what a link
buffers besides is `read_ahead` messages and `queued_bytes` at the socket. Nothing grows with a file's size.

## Gotchas

- One thread that writes a large input before reading can deadlock against a process that echoes it; write on one thread and
  read on another (every method takes `&self`).
- A refused request surfaces from `next` or `wait` as `Error::Remote`, not from `start`: the host answers on the stream.
