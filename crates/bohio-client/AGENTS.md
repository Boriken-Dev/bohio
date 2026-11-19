# `bohio-client` — API header

Header-file-style reference for the `bohio-client` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `bohio-proto`, `coqui-transport` (git revision) and `thiserror`. Version
`0.0.0-alpha.0`, not published.

Imports: the root holds `Open` and `Error`. Public module: `exec`. No `unsafe` code (`#![forbid(unsafe_code)]`); no threads, no
I/O of its own: every call blocks on the channel it was given.

## Opening streams (crate root)

```rust
pub trait Open: Send + Sync {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn Channel>, Error>;   // a stream named `protocol` whose open carries `metadata`
}
#[non_exhaustive] pub enum Error { Remote(ErrorBody), Transport(Failure), Protocol(bohio_proto::Error), Timeout, Closed }   // Clone, Eq, std::error::Error
```

`Open` is the link: the caller implements it over its authenticated connection (the protocol names are `bohio_proto::protocol::*`).
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

## Gotchas

- One thread that writes a large input before reading can deadlock against a process that echoes it; write on one thread and
  read on another (every method takes `&self`).
- A refused request surfaces from `next` or `wait` as `Error::Remote`, not from `start`: the host answers on the stream.
