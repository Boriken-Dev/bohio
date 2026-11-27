# `bohio` single-file modules — API header

Part of the `bohio` API header (`../AGENTS.md`): the modules that are one file each.

## Control stream (`bohio::control`)

```rust
pub struct Context<'a> { pub filesystem: fs::Context<'a>, pub info: &'a Info, pub current: Option<Current<'a>>, pub gui: Option<GuiProbe<'a>>, pub sessions: Option<SessionsView<'a>> }   // Copy
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
macOS, the native separator), whether execution and shells are enabled, the configured bounds, and `forward` (`enabled`, and the target kinds `connect` reaches and the listener kinds `listen` binds). `host_id` is `None`.

## The node (`bohio::node`, `bohio::link`, `bohio::peer`)

```rust
pub use bohio_client::dial::Endpoint;                              // the endpoint text of every transport: see bohio-client's header
pub struct Node;                                                   // Debug; no path, CLI or process-wide state is needed
impl Node {
    pub fn new(config: NodeConfig, identity: Identity, keys: Arc<dyn Keys>, grants: Arc<dyn Grants>) -> Result<Self, NodeError>;   // validates `config`; the proof is built once over `keys`
    pub fn with_events(self, events: Arc<dyn NodeEvents>) -> Self;  // default `LogEvents`
    pub fn with_path(self, path: OsString) -> Self;                 // the host's own PATH: programs, shells and `info`; default empty
    pub fn with_pairing(self, store: coqui_peer::TrustStore) -> Result<Self, NodeError>;   // lets the node pair on its own listeners (the store `respond` accepts a joiner in); without it every joiner is closed unanswered
    pub fn arm_pairing(&self, lifetime: Option<Duration>) -> Result<Armed, NodeError>;   // a new PIN (replacing the armed one), `[pairing] timeout` when None; Pairing error without a store or for a zero lifetime
    pub const fn host(&self) -> coqui_peer::PeerId;
    pub fn serve_listening(&self, listening: &Listening, stop: &AtomicBool) -> io::Result<()>;   // accepts until `stop`; a thread per connection (a serial line: one link at a time, the port reopened after each)
    pub fn serve_stdio(&self) -> io::Result<()>;                   // the one link on standard input and output; nothing else may write to standard output
    pub fn serve_channel<C: Channel>(&self, channel: &C, source: Option<SocketAddr>);   // proves the peer on any unproven channel, serves its link to the end; refuses (logs, returns) past `max_connections` and, for a known `source`, `max_unproven_per_source`; at most max_links_per_peer per peer; the caller closes it
    pub fn connect_channel<C: Channel>(&self, channel: &C, expect: &PeerId, source: Option<SocketAddr>) -> Result<(), NodeError>;   // dials over an unproven channel (framed as `bohio_client::link::framed`): proves `expect` before sending its own key, serves the link as an answered one is, returns when it ends
    pub fn connect(&self, endpoint: &Endpoint, expect: &PeerId, trust: &ServerTrust, timeout: Duration) -> Result<(), NodeError>;   // dials any endpoint, then connect_channel; the link lists `endpoint`
    pub fn peer(&self, peer: &PeerId) -> Option<PeerLink>;          // the oldest link that is up to `peer`, whichever side made it
    pub fn links(&self) -> Vec<LinkInfo>;                           // every link that is up, oldest first
    pub fn run_forwards(&self, declarations: &dyn forward::Declarations, stop: &AtomicBool);   // keeps the `[peer."<id>".forward] local` / `remote` entries up until `stop`: see Forwarding
    pub fn run_dial(&self, peer: &PeerId, endpoint: &str, dial: &DialConfig, stop: &AtomicBool);   // keeps a link to `peer` at `endpoint` (text, read again at each attempt) until `stop`: dials, serves, waits (retry_min x retry_factor per failure, up to retry_max, with jitter), repeats; no dial while an up link to the peer exists
}
#[non_exhaustive] pub enum NodeError { Config(ConfigError), Proof(String), Pairing(String), Listen { endpoint, reason }, Dial { endpoint, reason }, Stranger { peer } }   // Dial: the path or the proof failed; Stranger: the dialed key proved itself and Keys do not accept it
#[non_exhaustive] pub struct LinkInfo { pub peer: PeerId, pub side: Side, pub endpoint: Option<String>, pub source: Option<SocketAddr>, pub since: SystemTime, pub mine: Vec<String> }   // `mine`: the tokens this node offers the peer now, computed when asked   // `side` (Dialed or Answered) decides nothing
#[derive(Clone)] pub struct PeerLink;                              // Debug; bohio_client::Open (Error::Closed once the link ended); peer(), is_up(), offers(wait) -> Result<Vec<String>, bohio_client::Error> (the peer's `link/1` offers now)

pub trait Keys: Send + Sync {
    fn is_accepted(&self, peer: &PeerId) -> bool;                  // the proof, every stream decision and the link's check every `[link] key_recheck`
    fn stranger(&self, peer: &PeerId, key: &KeyMaterial, source: Option<SocketAddr>) {}   // a proven key that was refused; default ignores
    fn paired(&self, peer: &PeerId, key: &KeyMaterial) -> Result<(), KeysError> {}   // a key that paired by the armed PIN: record it as accepted, holding nothing; the default errors, so such keys pair nobody (`MemoryKeys` accepts it, `LayeredPeers` writes `[peer.<id>]`)
}
pub struct KeysError;                                              // KeysError::new(reason)
pub trait Grants: Send + Sync {
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError>;  // asked at every open, every recheck and every `bohio/1` call; an error refuses
}
pub struct GrantsError;                                            // GrantsError::new(reason); Display is the reason
pub struct MemoryKeys;                                             // new(), accept(PeerId), remove(&PeerId) -> bool; FromIterator<PeerId>
pub struct MemoryGrants;                                           // new(), set(PeerId, Grant), remove(&PeerId) -> bool; FromIterator<(PeerId, Grant)>; no entry: Grant::none()

pub trait NodeEvents: Send + Sync { fn event(&self, event: &NodeEvent); }   // runs on the link's thread: return quickly
#[non_exhaustive] pub enum NodeEvent { LinkUp { peer, side }, LinkDown { peer }, Opened { peer, protocol }, Refused { peer, protocol }, Revoked { peer, protocol }, Dialing { peer, endpoint }, DialFailed { peer, endpoint, reason, retry_in }, Kept { peer, direction, spec, state }, Paired { peer }, PairingRefused { source, attempts, reason }, Stranger { peer, source }, Forwarded { peer, kind, target, outcome }, Gui { peer, session, change: GuiChange }, Session { peer, session: String, change: SessionChange } }   // an id, a protocol, an address: never a key, PIN or request; SessionChange { Started, Attached, Detached, Ended { reason } }
pub struct LogEvents;                                              // the default: log lines

pub mod pair;   // Armed { pin(), lifetime(), wait(up_to) -> Option<PairEnd>, withdraw() } (Debug hides the PIN); #[non_exhaustive] PairEnd { Paired(PeerId), Expired, UsedUp, Replaced, Withdrawn, Failed(String) }
// PairingConfig { timeout, max_attempts, attempt_delay, attempt_delay_max, pin_length, pending, pending_per_source, ipv6_prefix, exchange_timeout } is `[pairing]`; ClientConfig { reply_timeout, transfer_timeout, gui_timeout, input_chunk, read_size, input_queue, probe_timeout, links_wait, parallel_streams, parallel_links, parallel_range, profile: Option<String> } is `[client]` (`transfer()` is `None` for `0`); one PIN at a time, in memory; a connection's first message `cabuya::pairing::MARKER` makes it a pairing (else a proof)

pub const PROTOCOLS: [(&str, &[Permission]); 9];                   // each protocol with the permissions of which a peer needs one to open it (`bohio-connect/1` and `tcp/1` need `forward.connect`, `bohio-gui/1` `gui.view` or `gui.input`)
pub const PROTOCOLS: [(&str, &[Permission]); 12];                  // each protocol with the permissions of which a peer needs one to open it (`bohio-connect/1` and `tcp/1` need `forward.connect`, `bohio-listen/1` and `bohio-accept/1` `forward.listen`, `bohio-attach/1` `session`; `bohio/1` also opens for `session`)
pub struct NodePeers;                                              // cabuya::peers::PeerStore over `Keys` and `Grants`: NodePeers::new(keys, grants); an accepted key may open each protocol its grant holds a permission for, which is what `link/1` offers it
pub fn offered(grant: &Grant) -> Vec<&'static str>;                       // the PROTOCOLS tokens `grant` holds a permission for: what a node offers that peer
pub fn admission(grants: Arc<dyn Grants>, protocol: &'static str) -> cabuya::link::Admission;   // asks `grants` afresh each call
pub fn covers(grant: &Grant, protocol: &str, metadata: &[u8]) -> bool;   // the grant still covers what the open asked for
pub fn grant_of(grants: &dyn Grants, peer: &str) -> Result<Grant, GrantsError>;

pub struct Data;                                                   // Data::new(dir): dir(), identity_path(), identity(), store() (the waiting list), with_waiting(&WaitingConfig) (`[waiting]` bounds for it)
pub struct LayeredPeers;                                           // Keys + Grants over config layers read again at every question: LayeredPeers::new(files: Vec<PathBuf>, &Data) -> Result<Self, PeerError>; a stranger is recorded as waiting in the Data directory
pub fn list(files: &[PathBuf], data: &Data) -> Result<Listing, PeerError>;   // Listing { accepted: Vec<Accepted{id, grant, endpoint}>, waiting: Vec<Waiting{id, source}> }
pub fn approve(files: &[PathBuf], create: Option<&Path>, data: &Data, id: &PeerId, grant: Option<&Grant>, endpoint: Option<&str>) -> Result<Approved, PeerError>;   // Approved { file, made }
pub fn revoke(files: &[PathBuf], data: &Data, id: &PeerId) -> Result<Revoked, PeerError>;   // Revoked { files, unwritable, key }; NoSuchPeer when nothing was removed and no layer holds it
#[non_exhaustive] pub enum PeerError { Identity(String), Store(String), Config(ConfigError), Write(String), NoLayer, NoSuchPeer }
```

A `Node` is made from what its caller supplies and reads no file or environment variable: settings (`NodeConfig`), an identity, `Keys` and `Grants`. An app
keeps them wherever it likes (its own settings, a database record, memory) and implements the two traits over them; `bohio serve` implements them over files.
A connection proves itself first, within `[link] proof_timeout`, with an unproven peer held to one proof-sized message. A key that proves it holds the key it
presents and that `Keys::is_accepted` refuses is passed to `Keys::stranger` (with the connection's source address) and the connection ends: the peer sees only
that. An accepted peer's link is served by `cabuya::link::serve`: each stream's open is decided by `Keys` (`NodePeers`, accepted keys only) and then by
`admission` (the peer's `Grants` entry: it must hold a permission the protocol needs, `bohio/1` needs one of `info`, `fs.read`, `fs.write`, and `covers` must
hold: an exec's or a shell's explicit program is one the program list holds, a transfer's file lies under a root), and each running stream is decided again
every `[link] recheck`, so removing a key or a grant, or narrowing a program list or the roots under a running stream, closes the stream with a revoke whose
reason reaches the peer (a `Permission` failure). A `bohio/1` stream reads the grant again at every call. A `Grants` that cannot be read refuses. The link itself
checks that the key is still accepted every `[link] key_recheck` (`2 x recheck` unset) and, when it is not, closes each running stream with a revoke before it ends. The stream's engine then runs behind a `Relay` with the grant the peer has at that moment.

Either side opens streams on any link. `serve_channel` and `serve_listening` answer a link; `connect` and `connect_channel` dial one (`prove_out` with the
responder's key pinned to `expect`, so a path that leads elsewhere gets nothing, then served exactly as an answered link, `Side::Dialed`). Every link, whichever
side made it, runs `cabuya::link::serve_with` with a `bohio_client::connect::Opens` as the node's opener, so the node answers the peer's streams under the
peer's grants and `Node::peer(&id)` opens its own under the peer's grants for this node. Who dialed decides nothing: `Side` reaches neither `Keys` nor `Grants`, and
a link serves only a key whose `Keys` accept it, so a dialed key that proved itself and is not accepted ends the link after the proof (`NodeError::Stranger`,
`Keys::stranger`, `NodeEvent::Stranger`). A dialed link counts against `[link] max_links_per_peer`. Both sides dialing at once make two links; both serve.

`run_dial` is the keep-alive of a dialed link: after a failure it waits `retry_min`, then that times `retry_factor` per further failure up to `retry_max` (with `retry_jitter`),
and starts again from `retry_min` after a link that stayed up at least `retry_min`; it never dials while an up link to the peer exists, nor while `Keys` do not accept the
peer (asked again every `retry_min`, reported once as a `DialFailed`), so a peer revoked while the dialer runs is not reached again; the node's `[link] heartbeat`
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
instant can lose one change. `LayeredPeers::paired` (`Keys::paired`) is how a node records a peer that paired: it calls `approve` (no grant), which writes a `[peer.<id>]` table (`LayeredPeers::with_create` names the file to make). The joiner does the
same with the host's key, so it trusts the host through its own `[peer.<host>]` table.
## Listeners (`bohio::listen`)

```rust
#[non_exhaustive] pub enum Listening { Tcp(TcpListener), Pipe(coqui_transport_pipe::Listener), WebSocket(coqui_transport_web::websocket::Listener), WebTransport { listener: coqui_transport_web::webtransport::Listener, certificate: Certificate }, Serial { path: String, options: SerialOptions } }
impl Listening {
    pub fn bind(config: &ListenConfig, settings: &Settings) -> Result<(Listening, Endpoint), ServeError>;   // the endpoint as bound: port 0 becomes the port chosen
    pub fn bind_with(config: &ListenConfig, settings: &Settings, store: Option<&CertificateStore>) -> Result<(Listening, Endpoint), ServeError>;   // `bind`, with the made WebTransport certificate kept in the store (`CertificateStore::new(dir).with_events(events)`: a restart serves the same hash; a renewal is `NodeEvent::CertificateRenewed { endpoint, hash }`); `bind` keeps none
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
## Local control (`bohio::local`, `[control]`)

```rust
pub struct Local;                                                  // Debug; the bound endpoint: Local::bind(&ControlConfig, &PeerId) -> Result<Self, NodeError>, endpoint() -> &Endpoint
impl Node {
    pub fn serve_local(&self, local: &Local, stop: &AtomicBool) -> io::Result<()>;   // accepts local clients until `stop`; a client that is not this user is closed unread unless the operator set socket_mode, socket_group or security_descriptor
    pub fn serve_local_channel<C: Channel>(&self, channel: &C, stop: Option<&AtomicBool>);   // one client on an unproven, framed channel, unsealed
}
#[non_exhaustive] pub enum Server { ThisAccount, Any }           // who a client accepts as the endpoint's server; Server::for_control(&ControlConfig): Any when it sets socket_mode, socket_group or security_descriptor
pub const ANOTHER_ACCOUNT: &str;                                   // the Error::Link text of an endpoint another account serves
pub struct LocalLink;                                              // Debug; Drop ends it. LocalLink::connect(&Endpoint, &Settings) -> Result<Self, Error> (Server::ThisAccount); connect_to(&Endpoint, &Settings, Server); pair(lifetime: Option<Duration>) -> Result<bohio::local::Pairing, Error> (pin(), lifetime(), wait(self) -> Result<PairReply, Error>; dropping it withdraws the PIN); links(wait: Duration) -> Result<bohio_proto::local::LinksReply, Error>
pub struct Via;                                                    // Debug; bohio_client::Open. Via::connect(&Endpoint, &PeerId, &Settings) -> Result<Self, Error>; connect_to(&Endpoint, &PeerId, &Settings, Server); links(wait)
pub fn since(entry: &LinkEntry) -> SystemTime;
pub struct ControlConfig { pub enabled: Option<bool> /*true*/, pub endpoint: Option<String>, pub socket_mode: Option<String> /*"0o600"*/, pub socket_group: Option<String>, pub security_descriptor: Option<String> }   // `[control]`; endpoint_for(&PeerId) is `pipe://bohio-` and the first 16 hex digits of the id
```

The node serves `bohio-via/1`, `bohio-links/1`, `bohio-providers/1` (the libraries `[providers] paths` loaded, one `ProvidersReply`; `bohio providers`) and `bohio-pair/1` (CDDL in `docs/protocol.md`, types in `bohio_proto::local`) on this endpoint only, never on a peer's link. `bohio-via/1`
opens the named protocol on the node's oldest up link to the peer (`Node::peer`) and passes messages both ways; the peer's refusal or revoke closes the local stream with the
same reason and text (`bohio_client::connect::Opened::end`, `Relay::end`); no link is `refused` with "no link to that peer". The endpoint holds what every peer grants this node:
it is as sensitive as the key file, and the same-user check is defence in depth, not a replacement for the socket's own access. `bohio serve` binds it unless `[control] enabled = false`
and prints `control <endpoint>`; a bind failure is logged and the node serves without it. The endpoint is not sealed, so a client checks the other way too: `LocalLink::connect`
and `Via::connect` refuse a `pipe://` endpoint the system does not report as served by this account (`Error::Link(ANOTHER_ACCOUNT)`, before anything is sent), since
another account could bind the name first; `remote --via`, `links` and `pair` say so, and `pair` does not fall back to serving alone.
`bohio::limit`: `Slots::new(limit)`, `Slots::acquire() -> Option<Permit<'_>>`, `Slots::with_share(limit, per_key)` and `acquire_for(key)` (at most `per_key` permits for one key, a peer), `in_use()`, `limit()`; a `Permit` frees its slot
when dropped. One `Slots` is shared by every stream that counts against a bound.
