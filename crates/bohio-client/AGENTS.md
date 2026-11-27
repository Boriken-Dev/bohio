# `bohio-client` — API header

Header-file-style reference for the `bohio-client` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

Cargo features: `zstd` (decode zstd), `screen` (the compositor of a watch: `bohio-transfer/screen`). MSRV Rust 1.89. Depends on `bohio-proto`, `coqui-transport` (with `process`), `cabuya` (with `sealed`), `coqui-peer`, `coqui-transport-pipe` (`channel`), `coqui-transport-web` (`websocket`, `webtransport`), `coqui-peer-tls`, `rustls` and
`coqui-transport-serial` (each a git revision, one copy), `url`, `socket2`, `serde`, `serde_json`, `tracing` and `thiserror`. Version
`0.0.0-alpha.0`, not published.

Imports: the root holds `Open`, `OpenOptions`, `Sourced` and `Error`. Public modules: `bundle`, `connect`, `control`, `dial`, `exec`, `follow`, `forward`, `fs`, `gui`, `hash`, `keep`, `link`, `media`, `parallel`, `power`, `profile`, `range`, `relay`, `search`, `session`, `shell`, `socks`, `sourced`, `sync`, `tools`, `walk`. No `unsafe` code
(`#![forbid(unsafe_code)]`). A call blocks on the channel it was given and, unless it is listed here, starts no thread and touches nothing outside the link. Threads: `connect` and
`relay` (the link), and for the length of a call `bundle`, `control` (`call_many`), `fs`, `hash`, `keep`, `parallel`, `walk`, `socks`, `forward` (with `forward::ipc` and `forward::remote`), `sync` (the tree walks) and `tools` (`net`, `process`).
Local sockets: `connect` and `link` dial, and `forward`, `forward::remote`, `socks` and `tools::net` bind or connect them. Local files: `fs` (`put`, `get`), `parallel`, `sync` (with its tree walk), `dial` (the CA files of a TLS trust)
and `tools::files` read and write them.

## Opening streams (crate root)

```rust
pub trait Open: Send + Sync {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error>;   // a stream named `protocol` whose open carries `metadata`; cabuya::mux::LinkStream is a Channel that can also `finish` (half-close), say how much of it is left (`life`) and `close_with` a reason
    fn open_with(&self, protocol: &str, metadata: &[u8], options: &OpenOptions) -> Result<Box<dyn LinkStream>, Error>;   // provided: `open`, ignoring the options
    fn bulk_window(&self) -> Option<u32>;                          // provided: None; what `Reader::start` asks for, `Some` only when above the link's `window`
    fn is_up(&self) -> bool;                                       // provided: true; whether the link can still open streams (a local listener of a forward ends when it cannot)
    fn open_cause(&self, protocol: &str, metadata: &[u8]) -> Result<(Box<dyn LinkStream>, connect::Cause), Error>;   // provided: `open` and `Cause::unknown()`; `Opener`, `Connection` and `PeerLink` give the cause that says, once the stream ended, whether the host refused it as over a limit
    fn files_source(&self) -> Option<&str>;                        // provided: None; the files source this link names (`Sourced`): a `Control` opened through it puts it on its calls
    fn relink_wait(&self) -> Option<Duration>;                     // provided: None; Some for a `keep::Relink`: a forward retries a connect whose link was lost before the answer for that long
}
#[derive(Default)] #[non_exhaustive] pub struct OpenOptions { pub receive_window: Option<u32> }   // None: the link's window; `OpenOptions::receive_window(bytes)`. A link grants the difference on the new stream, up to its `max_window`
#[non_exhaustive] pub enum Error { Remote(ErrorBody), Transport(Failure), Protocol(bohio_proto::Error), Link(String), Timeout, Closed,
                                   Compression(String) /* a codec this build lacks */, Local(String) /* a local file; never a path */, Screen(String) /* an unusable update */, Mismatch /* a rebuilt file's digest differs */ }   // Clone, Eq, std::error::Error
```

`Sourced::new(&link, "name")` / `Sourced::shared(arc, "name")` is an `Open` that names the host's files source on every filesystem stream (`bohio-read/1`, `-write/1`, `-hash/1`, `-walk/1`, `-search/1`) and, through `Open::files_source`, on the calls of a `Control` opened through it; `Tools::call` does the same for a file tool's `source` argument.

`Open` is the link: the caller implements it over its authenticated connection (or uses `connect::Connection`) (the protocol names are `bohio_proto::protocol::*`).
An implementation over plain channels (a test's memory pair) wraps each in `relay::Plain`, which is open until closed and cannot finish.
`Failure::Timeout` converts to `Error::Timeout`; every other `Failure` is `Error::Transport`. `Remote` is the host's structured
`{code, message}`.

## Module headers

Each module's API header sits beside its code and ships in the package with this one.

| Header | Covers |
| --- | --- |
| `src/forward/AGENTS.md` | `socks` and `forward`: SOCKS5, local and remote forwards, the IPC pump |
| `src/tools/AGENTS.md` | `tools`: the typed agent calls over any `Open`, their settings, the desktop calls |
| `src/AGENTS.md` | the single-file modules: `keep`, `exec`, `shell`, `power`, `media`, `session`, `parallel`, `control`, `gui`, `profile`, `sync`, `walk`, `bundle`, `range`, `hash`, `fs`, `dial`, `connect`, `link`, `relay`, `search`, `follow` |
