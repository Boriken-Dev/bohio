# `bohio_client::socks` and `bohio_client::forward` — API header

Part of the `bohio-client` API header (`../../AGENTS.md` in the crate root).

## SOCKS5 (`bohio_client::socks`)

```rust
pub fn serve(link: &dyn Open, listener: &TcpListener, settings: &ForwardSettings, stop: &AtomicBool, report: &(dyn Fn(&Error) + Sync)) -> io::Result<Ended>;
pub const fn reply_code(error: &Error) -> u8;   // the RFC 1928 reply for why a CONNECT failed
pub mod reply { pub const SUCCEEDED: u8 = 0; FAILURE = 1; NOT_ALLOWED = 2; UNREACHABLE = 4; REFUSED = 5; TIMEOUT = 6; COMMAND_NOT_SUPPORTED = 7; ADDRESS_NOT_SUPPORTED = 8; }
```

`serve` is `listen_tcp` for SOCKS5: method 0 only (a greeting without it is answered `0xFF`), CONNECT with an IPv4, IPv6 or domain address (a domain goes to the host unresolved, lowercased, and only host-name characters are taken: it is never read as `unix:` or `pipe:`), BIND and UDP ASSOCIATE answered `0x07`, another address type `0x08`. A client has `ForwardSettings::socks_timeout` for its whole handshake; one past `max_connections` is closed at once. The reply comes after the host's `connected`: `denied` and a grant that does not cover the target are `0x02`, `unreachable` and `not_found` `0x04`, `refused` `0x05`, `timeout` `0x06`, anything else `0x01`; the bound address in a success is the host's connection's. Whoever reaches the listener has the peer's grant: bind loopback.

## Forward (`bohio_client::forward`)

```rust
pub use bohio_proto::forward::{ConnectRequest, Connected, Target};   pub use cabuya::forward::Options;   // Options: chunk, stalled, idle_min/idle_max, watch, listen_poll, connect_timeout (the host's); validate() refuses zeros
#[non_exhaustive] pub struct ForwardSettings { pub options: Options, pub answer_timeout: Duration /*30 s*/, pub max_connections: usize /*64, a local listener's*/,
    pub open_retry: Duration /*10 s; zero never retries*/, pub open_retry_interval: Duration /*100 ms, moved by up to a quarter of itself*/, pub wait_for_link: Duration /*30 s*/,
    pub socks_timeout: Duration /*10 s; a SOCKS5 client's whole handshake; not zero*/ }   // Default, Copy
// `connect`, `RemoteListener::accept` and `listen` try an open again for `open_retry` when the host refused it as over a limit (`Cause::over_a_limit`, the text `OVER_A_LIMIT`), `connect` and `accept` also when the host answered `busy`; a `Relink` link lost before the host answered is retried for `wait_for_link`; every other refusal or error is final
pub struct Tunnel;                                                                      // Debug; one connected `bohio-connect/1`; dropping it closes the stream
pub fn connect(link: &dyn Open, target: &Target, settings: &ForwardSettings) -> Result<Tunnel, Error>;   // waits `answer_timeout` for the host's answer
impl Tunnel {
    pub fn address(&self) -> Option<&str>;                      // the address the host's TCP connection reached
    pub fn stream(&self) -> &dyn LinkStream;                    // raw messages both ways; finish() half-closes; finished_by_peer() tells the target's end from a cut
    pub fn into_stream(self) -> Box<dyn LinkStream>;
    pub fn pump_tcp(self, connection: &TcpStream);              // cabuya::forward::pump_with over the stream, until both directions end
    pub fn pump_ipc(self, connection: coqui_transport_pipe::Stream);   // forward::ipc::pump
    pub fn bridge(self, input: impl Read + Send + 'static, output: &mut dyn Write) -> Result<(), Error>;   // netcat: input's end is a half-close; ends when the host's direction does; Ok only for a finish; input is read on a thread that may outlive the call
}
#[non_exhaustive] pub enum Ended { Stopped, LinkLost, Closed }   // Closed: the host ended a remote listener while the link stayed up (a revoke, or a failure it reported)
pub fn listen_tcp(link: &dyn Open, listener: &TcpListener, target: &Target, settings: &ForwardSettings, stop: &AtomicBool, report: &(dyn Fn(&Error) + Sync)) -> io::Result<Ended>;
pub fn listen_ipc(link: &dyn Open, listener: &coqui_transport_pipe::Listener, target: &Target, settings: &ForwardSettings, stop: &AtomicBool, report: &(dyn Fn(&Error) + Sync)) -> io::Result<Ended>;   // one connect and pump per accepted connection, until `stop` or the link ends; bind loopback
pub struct Bytes;   // Debug, `Read`-and-`Write` halves via `coqui_transport::Split` (BytesReader, BytesWriter): a link can run over it (`link::stream(bytes, &settings)`)
impl Bytes {
    pub fn tunnel(tunnel: Tunnel, settings: &ForwardSettings) -> Bytes;   // the bytes of a connected `bohio-connect/1`; dropping the writer half finishes it, the last half closes it
    pub fn program(exec: Exec, settings: &ForwardSettings) -> Bytes;     // a program started with `Stdin::Open`: its standard output is read, its standard input written, its standard error dropped; ended with the stream
}
pub mod ipc {   // a Unix socket or Windows pipe on one end
    pub fn connect(name: &PipeName, timeout: Duration, options: &Options) -> Result<Stream, coqui_transport_pipe::Error>;   // reads wake every `watch`, a write may take `stalled`
    pub fn listener(name: &PipeName, options: &Options, mode: Option<u32>) -> Result<Listener, coqui_transport_pipe::Error>;   // this user alone, mode 0o600 unless `mode` (Unix sockets only)
    pub fn pump<S: LinkStream + ?Sized>(stream: &S, ipc: Stream, options: &Options);   // two threads; a Windows pipe has no half-close, so a finish ends both directions
}
```

Remote forwards (`bohio-listen/1`, `bohio-accept/1`):

```rust
pub use bohio_proto::forward::{Arrival, Bind, ListenRequest};
pub fn listen(link: &dyn Open, bind: &Bind, backlog: Option<u32>, settings: &ForwardSettings) -> Result<RemoteListener, Error>;   // the host binds; waits `answer_timeout` for `listening`
impl RemoteListener {                                           // Debug; dropping it closes the stream, which ends the host's listener and resets what it handed over
    pub fn address(&self) -> &str;  pub fn token(&self) -> &str;  pub fn is_open(&self) -> bool;
    pub fn next_arrival(&self, timeout: Option<Duration>) -> Result<Option<Arrival>, Error>;   // Ok(None): the wait ended; Err: the host reported a failure, revoked it or ended it
    pub fn accept(&self, link: &dyn Open, arrival: &Arrival, settings: &ForwardSettings) -> Result<Tunnel, Error>;   // claim one arrival: `not_found` when it was claimed, expired or its listener ended
}
pub fn connect_local(target: &Target, settings: &ForwardSettings) -> io::Result<LocalConnection>;   // TCP, a Unix socket (Unix) or a named pipe (Windows) on this machine
impl Tunnel { pub fn pump_local(self, connection: LocalConnection); }                                // pump_tcp or pump_ipc, by the connection's kind
pub struct RemoteForward;                                       // `ssh -R`: Debug; a thread of its own takes each arrival, connects it to the local target and pumps it; closed on drop
impl RemoteForward {
    pub fn open(link: Arc<dyn Open>, bind: &Bind, local: &Target, settings: &ForwardSettings, report: Arc<dyn Fn(&Error) + Send + Sync>) -> Result<Self, Error>;   // an arrival that cannot be bridged is claimed and closed, its reason goes to `report`
    pub fn address(&self) -> &str;  pub fn ended(&self) -> Option<Ended>;  pub fn wait(self) -> Ended;  pub fn close(self);
}
```

An error from `listen`: `Error::Remote(body)` (`in_use`, `busy`, `unsupported`, `denied`, `bad_request`, `unreachable`, `not_found`) or `Error::Transport(Failure::Permission)` when the grant covers no such bind (refused before anything listens); a claim that finds nothing is `Remote` `not_found` and says no more.

An error from `connect`: `Error::Remote(body)` when the host did not connect (`refused`, `unreachable`, `timeout`, `not_found`, `busy`, `unsupported`, `bad_request`),
`Error::Transport(Failure::Permission)` when the peer's grant does not cover the target (refused before the host connects) or was revoked, `Error::Timeout` and
`Error::Closed`. A stream that ends any way but in order resets the host's connection.
