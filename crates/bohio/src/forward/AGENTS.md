# `bohio::forward` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Forwarding (`bohio::forward`, `[forward]`)

```rust
pub struct ForwardConfig { pub enabled: bool, pub connect_timeout: Span /*10s*/, pub max_connections: usize /*64*/, pub max_connections_per_peer: usize /*16*/, pub chunk_size: usize /*16384*/,
    pub stalled: Span /*30s*/, pub idle_min: Span /*1ms*/, pub idle_max: Span /*10ms*/, pub watch: Span /*50ms*/, pub listen_poll: Span /*50ms*/,
    pub answer_timeout: Span /*30s*/, pub max_local_connections: usize /*64*/,
    pub wait_for_link: Span /*30s; 0 closes a connection at once when there is no link*/, pub open_retry: Span /*10s; 0 never retries*/, pub open_retry_interval: Span /*100ms, not zero*/, pub socks_timeout: Span /*10s, not zero: a SOCKS5 client's handshake*/,
    pub max_listeners: usize /*16*/, pub max_listeners_per_peer: usize /*8*/, pub max_pending: usize /*16*/, pub accept_timeout: Span /*10s*/,
    pub socket_mode: Mode /*"0o600" or 0o600*/, pub ipc_same_user: bool /*true*/,
    pub follow_links: bool /*false: a `unix:` target or bind is resolved and the resolved path must match a rule too; true follows links, logged non-default*/ }   // `[forward]`; Default; options() -> cabuya::forward::Options; client() -> bohio_client::forward::ForwardSettings; zeros are errors naming the key
pub enum ConnectRule { Tcp(cabuya::forward::Rule), Unix(String), Pipe(String) }   // FromStr (BadRule), Display, matches(&Target): HOST:PORT with a name, *.suffix, *, an address or a CIDR network and a port, A-B or *; unix:PATH and pipe:NAME exactly or with `*` and `?` (never across a separator; a `..`, `.` or empty part, or NUL never matches; pipes without case)
pub enum ListenRule { Tcp { addresses: Addresses, ports: PortSet, text: String }, Unix(String), Pipe(String) }   // FromStr (BadRule), Display, matches(&Bind): BIND:PORT with an address (`0.0.0.0` and `::` too), a CIDR network or `*` and a port, A-B or `*` (port 0, the host's choice, only where a rule names 0 or `*`); no name is resolved or matched; unix:/pipe: as ConnectRule
pub struct Listeners;                                                  // Debug; the host's listener registry: Listeners::new(&ForwardConfig), len(), is_empty()
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a ForwardConfig, pub slots: &'a Slots, pub per_peer: &'a Counted<String>, pub listeners: &'a Listeners, pub peer: PeerId, pub events: &'a dyn NodeEvents }
pub fn serve_connect(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>);   // `bohio-connect/1`: decide, connect, answer once, pump raw bytes; not closed
pub fn serve_tcp(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>);       // `tcp/1` as cabuya defines it: the connect first, a refusal is a close
pub fn covers(grant: &Grant, metadata: &[u8]) -> bool;   pub fn covers_tcp(grant: &Grant, metadata: &[u8]) -> bool;   // what `link::covers` asks at an open and at every recheck
pub fn connect_kinds() -> Vec<String>;   pub fn listen_kinds() -> Vec<String>;             // "tcp" and "unix" (Unix) or "pipe" (Windows): `info.forward.connect` and `.listen`
pub fn serve_listen(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>);     // `bohio-listen/1`: decide the bind, bind, answer `listening`, announce each arrival until the stream ends or the grant stops covering the bind; not closed
pub fn serve_accept(stream: &dyn LinkStream, metadata: &[u8], context: &Context<'_>);     // `bohio-accept/1`: claim one arrival of the peer's own listener, answer `connected` or `not_found`, pump raw bytes; ends when its listener does
pub fn covers_listen(grant: &Grant, metadata: &[u8]) -> bool;                            // what `link::covers` asks for `bohio-listen/1`
pub fn tcp_target(target: &bohio_proto::forward::Target) -> Option<cabuya::forward::Target>;
pub struct LocalSpec;   pub struct RemoteSpec;   pub struct BadSpec;   pub enum Bound { Tcp(TcpListener), Ipc(Listener) }   // FromStr: the `-L` / `-R` entry `LISTEN=TARGET` (`[BIND:]PORT`, `unix:PATH`, `pipe:NAME`); text(), target() / local(), bind() (RemoteSpec), non_default(); LocalSpec::bind(&ForwardSettings) -> Result<(Bound, shown), String> (a socket or pipe is this user's alone); Bound::serve(link, target, settings, stop, report) is listen_tcp / listen_ipc
pub struct Declared { pub peer: PeerId, pub local: Vec<LocalSpec>, pub remote: Vec<RemoteSpec>, pub backoff: Backoff, pub problems: Vec<String> }   // the forwards one `[peer."<id>".forward]` table keeps up; Declared::from_peers(&Peers); `problems` are entries that did not parse (told once, the rest kept)
pub trait Declarations: Send + Sync { fn declared(&self) -> Result<Vec<Declared>, String>; }   // read again every `[link] recheck`; `LayeredPeers` implements it over the layers
pub struct RemoteKeep<'a> { pub links: &'a dyn Links, pub settings: &'a ForwardSettings, pub backoff: &'a Backoff, pub poll: Duration, pub stop: &'a AtomicBool, pub tell: &'a (dyn Fn(KeptState) + Sync), pub report: Arc<dyn Fn(&Error) + Send + Sync> }
pub fn keep_remote(keep: &RemoteKeep<'_>, spec: &RemoteSpec, first: Option<RemoteForward>);   // asks the host to listen on every link that comes up until `stop`: first for the entry's bind (or `first`, already made), then for the port the host chose; a refusal waits Backoff; one that was up at least `min` is asked for again at once
```

`[peer."<id>".forward] local` and `remote` are what this node keeps toward the peer, beside the grant's `connect` and `listen` (what the peer may do here); they are not part of `Grant`
(`PeerTable::forward_local()`, `forward_remote()`, `Peers::declared()`), a bad entry is `ConfigError::Invalid` naming `peer.<id>.forward.local` or `.remote`, and `peer approve` keeps them. `Node::run_forwards(&declarations, stop)`
runs them: every `[link] recheck` it reads the entries and starts a task for each new one and stops those that left the file; a `local` task binds once (a bind that fails is told once and tried again at the next re-check) and serves
the listener over a `Relink` of the node's link to the peer (`[forward] wait_for_link`); a `remote` task is `keep_remote` over the same links. `NodeEvent::Kept { peer, direction: KeptDirection::{Local, Remote}, spec, state: KeptState::{Up { address }, Down, Failed { reason, retry_in }} }`
tells each change; `bohio serve` prints `local ADDRESS -> TARGET peer ID` and `remote ADDRESS -> LOCAL peer ID` at each `Up`, `local|remote down SPEC peer ID` and `local|remote failed SPEC peer ID: REASON`. A `remote` bind with port 0 is asked for again as the port the host chose, so the host's `listen` rule must cover it.

Both protocols need `forward.connect` and a matching rule, decided as the stream opens (nothing connects for a target the grant does not cover: the client sees a `Permission`
failure) and again at every `[link] recheck`, when a lost grant resets the connection. A refused connect is one `error` frame with `refused`, `unreachable`, `timeout`,
`not_found`, `busy` (a `[forward]` bound), `unsupported` (`enabled = false`, or `unix` on Windows or `pipe` on Unix) or `denied`. TCP is pumped by `cabuya::forward::pump_with` and a
Unix socket or Windows pipe by `bohio_client::forward::ipc::pump`, both on the link stream itself with no relay. `NodeEvent::Forwarded { peer, kind: ForwardKind::Connect, target, outcome: ForwardOutcome::{Connected, Failed(code)} }`
tells the owner each connect (the target's text, never a byte). `limit::Counted<K>` is the per-key bound (`take(key) -> Option<Hold>`).
