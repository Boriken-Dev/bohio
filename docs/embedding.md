# Embedding a node

`bohio serve` is one application of the `bohio` library. Another application can run the same node in its own process, with its settings, its key and its list of peers kept wherever it keeps the rest of its state: a database record, a section of its own configuration file, memory. Nothing here needs the `bohio` binary, a file Bohío chooses, or an environment variable: the library reads none of those.

A node is made from four things the application supplies:

| What | Type | Where it comes from |
| --- | --- | --- |
| Settings | `bohio::node::NodeConfig` | any section of the application's settings (serde, `deny_unknown_fields`), or `NodeConfig::default()` |
| Identity | `cabuya::auth::Identity` | the application's own store, as a PKCS#8 PEM |
| Who is a peer | an `Arc<dyn bohio::node::Keys>` | the application's list of accepted keys |
| What each peer may do | an `Arc<dyn bohio::node::Grants>` | the application's grants, as `bohio::policy::Grant` values |

## Settings and identity

`NodeConfig` holds the `[link]`, `[filesystem]`, `[execution]`, `[shell]` and `[pairing]` settings of [`bohio.cfg`](https://github.com/Boriken-Dev/bohio/blob/main/examples/bohio.cfg), each with its default, and `Node::new` checks their ranges. The key is a Noise identity by default; `coqui-peer-noise` reads and writes it as PEM, so it can live in the application's secret store:

```rust
let config: NodeConfig = toml::from_str(&section)?;
let identity: Identity = coqui_peer_noise::Identity::from_pkcs8_pem(&pem)?.into();
let node = Node::new(config, identity, keys, grants)?.with_path(host_path);
```

`with_path` is the `PATH` programs and shells are looked for on; a node starts with an empty one.

## Keys and grants

`Keys::is_accepted` answers who is a peer and `Grants::grant` what it may do. The node asks them at every proof, every stream's open, every `[link] recheck` of a running stream and every `bohio/1` call, so a change in the application's store applies to what is running without a restart. Answer quickly: they run on the link's threads.

```rust
impl Keys for Roster {
    fn is_accepted(&self, peer: &PeerId) -> bool { self.row(peer).is_some() }
}
```

```rust
impl Grants for Roster {
    fn grant(&self, peer: &PeerId) -> Result<Grant, GrantsError> {
        self.read(peer).map_err(|_| GrantsError::new("the roster cannot be read"))
    }
}
```

`Grant::none()` gives nothing and `Grant::allowing([Permission::Info, Permission::Exec]).with_programs(["git"])` builds one. A `Grants` error refuses the stream; a question `Keys` cannot answer should be `false`. `MemoryKeys` and `MemoryGrants` are in-memory versions for tests and small tools. `Keys::stranger` hears a key that proved itself and was refused (keep a bounded number, for an operator to approve), and `Keys::paired` records a key that paired by PIN, if the application offers pairing (`Node::with_pairing`, `Node::arm_pairing`).

## Sources

A node serves its terminal, desktop, power and media from its **sources** (`bohio::access::Sources`), which `Node::new` builds
from `NodeConfig`'s `source` and `access` tables (today's node when they are empty: `native`). An application replaces or adds to
them: `Sources::unconfigured()` has none, `with_terminal`, `with_desktop`, `with_power` and `with_media` add a source as a value
of the kind's trait, `register_library` registers a `guatiao::local_providers!` library so `[source.<name>]` can name its
providers, and `with_access(Capability, names)` says which serve each capability, the first the default. `Node::with_sources`
gives the node its sources (call it first), `Node::with_desktop(desktop)` is the shortcut that adds the application's own desktop
as the source `app` and makes it the default, `Node::sources()` reaches them for `Sources::reconfigure` (a source whose table did
not change keeps its instance; one that changed is built anew, the old one retired, and what runs on it ends `unavailable`), and
`Node::with_secrets(resolver)` says how a sensitive setting's `file:` or `credential:` reference is read: the
library reads no file and no environment variable, so a source whose settings hold a reference waits for it. `[access] fs` and
`exec` are read when the node is built. [Sources and providers](providers.md) shows how to write one.

## Listening and dialing

The node runs on the application's threads, until a stop flag is set. To answer, bind an endpoint and serve it:

```rust
let entry = ListenConfig { endpoint: "tcp://0.0.0.0:7620".into(), ..ListenConfig::default() };
let (listening, _bound) = Listening::bind(&entry, &settings)?;
scope.spawn(|| node.serve_listening(&listening, &stop));
```

To keep a link to a peer that this node reaches out to, as a host behind a firewall does, dial it; the peer must prove the id given before this node sends its own key, and the dialer waits and dials again whenever the link ends:

```rust
let dial = DialConfig::default();
scope.spawn(|| node.run_dial(&station, "wss://station.example:7622", &dial, &stop));
```

`Node::connect` dials once, and `Node::serve_channel` and `Node::connect_channel` serve a channel the application opened itself. `serve_listening` and `serve_channel` bound the connections they take (`max_connections`, and `max_unproven_per_source` from the `source` a caller passes: a channel with none is counted against the first only); one past a bound is refused at once and the application closes the channel. A dial the application makes with `connect_channel` is its own choice and counts toward `max_links_per_peer` only.

## Controlling a peer

Every link a node holds, answered or dialed, can carry this node's own streams. `Node::peer` gives a `PeerLink`, which every `bohio-client` operation takes; what the peer grants this node decides what it may do:

```rust
let link = node.peer(&host).ok_or("no link to that host")?;
let info = Control::open(&link)?.info(Some(Duration::from_secs(5)))?;
```

`PeerLink::offers` reads what the peer offers this node now, and `Node::links` lists every link with its side and what this node offers each peer.

## Bohío's own files

An application may also keep its peers in files of Bohío's format and use the same loader as the command: `Layers::load` merges the files it is given (each must exist; tables merge by key, arrays and scalars replace), and `LayeredPeers` reads their `[peer.*]` tables again at every question:

```rust
let layered = Layers::load(&[system_file, user_file])?;
let peers = Arc::new(LayeredPeers::new(layered.files.clone(), &Data::new(state_dir))?);
let node = Node::new(layered.config.node(), identity, peers.clone(), peers)?;
```

`Layers::load` does not check who may change the files. Whoever may write a file the node reads decides who it accepts and what each may do, so either keep such files where only the application's account may write, or make the command's check:

```rust
let found = bohio::config::discover(&[])?;               // the system file, then the user's, as `bohio` reads them
let watch = found.guard(&environment)?;               // refuses a layer or data directory another account owns or may change
```

`discover(named)` reads the layers `named` names, or else the system file with the user's merged over it (`[service] user_layer = false` leaves the user's out), and answers a `Found`: the merged `Config`, the layers read, the layers to watch and the user file `peer approve` makes. `Found::guard` is what `bohio serve` applies before it binds: no layer, directory that holds one, data directory or identity file may be owned by another account, or changed (the data directory also read) by its group or by others, unless `[service] check_access` is `false`, which logs `non-default:` and goes on; a path another account owns is refused either way, unless `Found::trust` (the command's `--trust-owner` and `BOHIO_TRUST_OWNER`) or `[service] trusted_owners` names that account. It answers the layers `LayeredPeers` should read. The pieces are in `bohio::ownership` (`check`, `layer_problems`, `decide`, `may_appear`) for an application that guards its own files.

## Events

`NodeEvents` hears links coming up and down, streams opened, refused and revoked, dial attempts and failures, pairings and strangers; each event names a peer id, a protocol or an address and never a key, a PIN or a request. The default logs them through `tracing`.

```rust
let node = node.with_events(Arc::new(AuditTrail::new(db)));
```

The event runs on the link's thread: an event sink that blocks holds up that link.

## A helper for a node with no desktop

A node an application runs as a service can serve a desktop user's screen through that user's helper. Bind `bohio::gui::helper::HelperListener::bind(&config.gui, &node.host())` (it is `None` unless `[gui] helper_accounts` names an account, and it resolves each name to a SID or a uid) and run `Node::serve_helpers(&listener, &stop)` on a thread; a connection whose operating-system account is not listed is closed unread, and a helper that connects is listed in `info.gui.sessions` as `ACCOUNT:NAME`. Streams reach it through the node's own `gui::serve_chosen`, which an embedder that calls `gui::serve` directly does not get: such an embedder serves its own `Desktop` as before. The helper side is `bohio::gui::helper::run(&HelperSettings, sources, &stop)`, which checks who serves the endpoint before it sends anything (`HelperError::NotTheNode`), or `serve_node` over a channel the application has already checked. `Context` of `gui::serve` gains `session`, the id a stream is served under.

## A local control endpoint

`bohio::local::Local` and `Node::serve_local` give a running node the endpoint `bohio remote --via` and `bohio links` use, on a pipe this account alone may open. It lets whoever connects act with everything every peer grants this node, so serve it only there; `Node::serve_local_channel` checks nobody. [Security](security.md#embedding) lists what else the API leaves to the application.
