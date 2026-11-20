# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `bohio::node::Node`, an embeddable host: it is made from a `NodeConfig`, an identity, `Keys` (who is accepted) and `Grants`
  (what each may do) the caller supplies, and reads no file, environment variable or process-wide state, so an application runs
  one from settings kept anywhere. `MemoryKeys` and `MemoryGrants` are in-memory implementations whose changes apply at the next
  open or `[link] recheck`; `NodeEvents` (default `LogEvents`) reports link and stream events. `Config::node()` and
  `NodeConfig::validate` give and check the settings a node runs on.
- Either side may open streams on any link. `Node::connect` and `Node::connect_channel` dial a peer (its key pinned and proven before this
  node sends its own) and serve the link as an answered one is; every link a node serves runs with an opener, so `Node::peer(&id)` gives a
  `PeerLink` (an `Open`, cheap to clone) and `Node::links()` lists each link's peer, side, endpoint, source and start. `bohio_client::connect::{Opener, Opens}`
  are the open loop `Connection` already ran, now reusable, and `Connection::opener()` hands one out. Who dialed decides nothing: each side's own grants
  decide what the other may open, and a dialed key the node does not accept ends the link after the proof and is reported as a stranger.
- Roles are read from the link. A node's `link/1` `offers` now lists only the protocols its grant for that peer holds a permission for (it listed all
  five for any accepted key), so the document says what the node would serve that peer now. `PeerLink::offers(wait)` reads the peer's document,
  `LinkInfo::mine` gives this node's side, and `Opener::capabilities(wait)` reads it on the link's own thread. Admission still decides every open. Nothing
  is added to the wire; `docs/protocol.md` has a "Roles" section.
- A node dials out. A peer's table with an `endpoint` (`[peer."<id>"] endpoint = "tcp://host.example:7620"`, any endpoint `bohio remote` takes) makes
  `bohio serve` keep a link to that key, which the table's name pins and which must prove itself before this node sends its own; `[peer."<id>".dial]`
  (`DialConfig`) sets `ca`, `cert_hash`, `fallback`, `timeout` (`10s`), `retry_min` (`1s`), `retry_max` (`60s`), `retry_factor` (`2`) and
  `retry_jitter` (`0.1`). `Node::run_dial` is the loop, reporting `NodeEvent::Dialing` and `DialFailed`; a node with no `[[listen]]` may only dial.
  `[link] heartbeat` (`25s`, `0` off) and `heartbeat_misses` (`3`) make both sides of every link ask whether the other is there, so a path that goes silent
  without closing ends the link. `bohio peer approve --endpoint` writes the endpoint, `peer list` and `status` show it, `config check` counts the dials.

### Changed

- One layered `bohio.cfg` holds the peers; `peers.cfg` is gone (no reader kept). A peer is a `[peer."<id>"]` table, which
  accepts its key (even an empty one); `allow`, `fs_roots` and `[peer."<id>".exec]` say what it may do, and a table name that
  is not a peer id is refused. Without `--config` the system file is read and the user's merged over it, through `anycfg`
  (tables merge by key; arrays and scalars replace, so a higher layer can narrow a list); `--config FILE` (repeatable) or
  `BOHIO_CONFIG` name the layers instead, alone. `[service] check_access` and the new `user_layer` are read from the lowest
  layer only. Each layer is checked alone (an error names the file, line, column and key, never a value) and the merged value
  for ranges, naming the highest layer that sets the key. The `[peer.*]` tables are read again at every decision, so a hand edit
  of any layer applies at the next open or `[link] recheck`; a layer that cannot be read refuses every peer until it is fixed.
  The data directory keeps only the identity and the waiting list; the trust store's `accepted/` is no longer read.
  `bohio peer approve` takes any id and writes its table in place with `toml_edit` (the file's comments and layout stay) into the
  layer that holds it, else the last layer it can write, else a new user file; `peer revoke` removes it from every layer and names
  one it cannot write; `bohio pair` and `bohio join` write the other side's key into the configuration. `bohio remote` trusts the
  `[peer.<id>]` tables (or `--server`). On Unix a layer or its directory owned by another account than the running one or root, or
  writable by its group or others, is refused (`check_access = false` starts anyway); on Windows each layer and its directory get the
  rule the file and the data directory did. `config::Layers`, `Layered` and `policy::Peers` replace `Config::locate`,
  `Environment::config_candidates` and its configuration fields, and `Policies` and `PolicyError`; `ConfigError::Parse` gains `key`.
- `serve::Server` is replaced by `node::Node` (no alias), `serve::ServeError` by `node::NodeError`, `link::TrustPeers` by
  `link::NodePeers`, and `link::admission` and `link::grant_of` take `Grants` where they took the path of `peers.cfg`. The command
  supplies the two traits from its configuration layers (`peer::LayeredPeers`). `info::collect` takes a `NodeConfig` and the
  host's `PATH`.
- Choices the operator may make are no longer refusals; each keeps its strict default and says in the docs what it costs.
  `--fallback ENDPOINT` takes a `ws://` endpoint on any address (it used to be loopback only) and one on another host than the
  `webtransport://` endpoint; naming it is the choice. `tls_valid_days` takes any value from 1 (it was 1 to 14): a certificate valid
  longer than 14 days, or from files with a key that is not ECDSA P-256, is logged once at start as one a browser will not accept
  pinned by its hash, and other clients pin it by hash whatever its key or validity (`--cert-hash` no longer holds a WebTransport
  hash to the browser rule). A grant that lists `PATH` under `exec.env` lets a request set it (it was refused even when named).
  `[service] check_access = false` starts the host on Windows although another account may change its configuration file or data
  directory, and logs what was found; the default is still to refuse. `bohio-client`'s `Endpoint::with_fallback` no longer checks
  the host. Pins `coqui-transport-web` at the revision whose `Endpoints` takes these choices.

### Fixed

- A grant narrowed while a stream runs now reaches it: a `bohio/1` stream reads the peer's grant again for every call, so
  taking away `fs.write` or a root applies to its next call (it used to keep the grant it opened with for as long as it
  stayed open), and the admission of an exec, a shell or a transfer checks that the program is still listed and the file
  still under a root, so such a stream ends at the next `[link] recheck`. `link::admission` takes the protocol, `link::covers`
  is the check, and `control::Context::current` is where the control stream reads the grant.
- On Windows `bohio serve` and the service refuse to start from a configuration file another account owns or every local
  user may change, and from a data directory every local user may change or read. Every user may create files and folders
  under `%ProgramData%`, where the service looks for `bohio\bohio.cfg`, so a local user could plant a configuration that
  chose the data directory, and with it the trusted keys and grants (running code as `LocalService`), or read the host's
  key from a data directory made there.
- One source, or one peer, can no longer hold every connection: `[link] max_unproven_per_source` (default 16) closes a
  connection unread when its source (an IPv4 address or an IPv6 /64) already has that many proving themselves, and
  `[link] max_links_per_peer` (default 16) closes a proven peer's link past that many. Before, anyone who could reach a
  listener could keep it from serving everyone by opening `max_connections` connections and sending nothing.
  `listen::Arrival::source` names a connection's source.
- A peer can no longer make a set-user-ID or set-group-ID file: a mode with `0o6000` in `set_mode`, `mkdir` or a write's
  `file_mode` is `denied`, and a `copy` or a `replace` drops those bits from the mode it keeps. Such a file, written by a
  peer that may only write under one root, ran as the service's account for any local user who started it.
- Under a program list a request may set or remove only the environment variables the grant names (`[peer.<id>.exec] env`,
  `bohio peer approve --env NAME`), never `PATH`: a peer allowed one harmless program could make it load a library of its own
  with `LD_PRELOAD` (or `DYLD_INSERT_LIBRARIES`, or run a file with `BASH_ENV`). `Grant::with_env`, `env_names` and
  `permits_env` are the grant's side.
- A program list (`exec.programs`) is now checked against what starts: each listed name is resolved on the host's own
  `PATH` to an absolute path and that path is spawned, and a request that sets or removes `PATH`, or clears the
  environment, is refused. A peer allowed `git` can no longer run its own `git` by pointing `PATH` at a directory it
  controls. `exec::Context` and `shell::Context` carry the host's `PATH` (`exec::Context::path`).

### Added

- `wss://` and `webtransport://` endpoints, to listen and to dial, for browsers and TLS-only proxies. The proof still decides who the
  host is; TLS only has to be accepted. A `[[listen]]` entry takes `tls_cert` and `tls_key` (PEM, read at start, never logged; `wss`
  needs both), and a `webtransport://` entry without them makes its own ECDSA P-256 certificate (`tls_valid_days`, 1 to 14, default
  13), prints `certificate sha256:... for ...` and makes a new one halfway through. `bohio remote` takes `--ca FILE` (roots instead of
  the system's) and `--cert-hash HASH` (a certificate trusted as it is) and, for a `webtransport://` host, `--fallback ENDPOINT`: a
  `ws://` (loopback only) or `wss://` endpoint of the same host, dialled when the QUIC path fails before anything crossed; a refused
  certificate never falls back. `bohio-client` has `dial::ServerTrust`, `Endpoint::dial_with`, `Endpoint::with_fallback`,
  `Endpoint::WebTransport` and `Connection::connect_with`; `bohio::listen` has `server_config` and `MadeCertificate`.
- `docs/protocol.md` is the wire for an implementer in another language: the carriers, the open metadata and CBOR messages of
  each protocol in CDDL, the one-byte frame tags, the error codes and the capability negotiation. `vectors/*.json` holds every
  message as hex, and `crates/bohio-proto/tests/vectors.rs` writes and checks them. `docs/security.md` covers the service's privileges
  against a peer's grants, container isolation, privileged deployments and the known limits; `docs/hostctl.md` maps a hostctl
  provider onto Bohío's calls.
- `packaging/systemd/bohio.service` runs the host as the unprivileged user `bohio` (`NoNewPrivileges`, `ProtectSystem=strict`, a
  writable data directory only, `KillMode=control-group`), and `packaging/container/Containerfile` builds a non-root image whose
  default command is `bohio serve --stdio`.
- `bohio service install|uninstall|run` runs the host as a Windows service (`windows-service`): `install` registers it to start
  automatically as `NT AUTHORITY\LocalService` (or `--account`), `uninstall` stops and removes it, and the service serves the configured
  `[[listen]]` endpoints until the manager stops it. The account must be able to write `[service] data_dir`.
- Every coqui transport carries the link: `pipe://` (Windows named pipes and Unix domain sockets, `coqui-transport-pipe`), `ws://`
  (WebSocket binary messages, `coqui-transport-web`), `serial://port?baud=N` (COBS frames with a CRC-32C, `coqui-transport-serial`) and
  standard input and output. `bohio serve --stdio` serves one link on standard input and output (for `docker exec -i`), and
  `bohio remote --command "program args"` or `stdio://program args` starts a program and uses its standard streams as the link.
  `[[listen]]` takes the settings of its scheme (`socket_mode`, `remote_clients`, `origins`, `baud`, `data_bits`, `parity`,
  `stop_bits`, `flow_control`), `bohio status` reports every configured endpoint, and `bohio-client` has `dial::Endpoint`.
  The proof and the grants are unchanged.
- The authenticated link over TCP, on `coqui-link`: `bohio serve` proves each peer (Noise by default) within `[link] proof_timeout` and
  serves its streams; a key that proves itself and is not accepted is recorded as waiting under its source address and refused. Each
  stream is decided from `peers.cfg` when it opens and every `[link] recheck` while it runs, so a revoke or a narrowed grant ends
  running processes, shells and transfers (the peer sees a `Permission` failure).
- Memory per link is bounded by `[link]` settings with documented defaults: `window`, `max_frame`, `max_message`, `queue_bytes`,
  `read_ahead`, `queued_bytes`, `send_limit`, `send_wait`, `poll_interval`, plus `max_connections` and `proof_timeout`. A 64 MiB file
  crosses the link in both directions with the host's resident memory growing by about 1 MiB.
- `bohio-client`: `connect::Connection` (a proven, owned `Open` over TCP or any coqui channel), `relay::Relay` (an owned, bounded channel over
  one stream of a link) and `link::Settings`.
- Commands: `bohio serve`, `status`, `id`, `peer list|approve|revoke`, `pair` (a PIN), `join` and `remote info|exec|read|shell`.
- Interactive shells (`bohio::shell`, `bohio-shell/1`): a shell kind from `info` or an explicit program on a
  pseudo-terminal (`portable-pty`: `openpty`, ConPTY), terminal output streamed raw, input, resize and kill frames, a
  bound on concurrent sessions (`[shell] max_sessions`), and the program's whole tree ended when the stream closes.
  `bohio-proto` has the shell request and frames; `bohio-client` has `shell::Shell`.

- `bohio-proto`: the wire types of protocol version 1 and their CBOR codec: control calls
  (`hello`, `info`), structured errors with a closed code list, the host's capabilities, shell kinds
  and the `bohio-exec/1` request and frames.
- `bohio` library: `bohio.cfg` configuration (unknown keys refused, every setting documented with
  its default, searched in `$BOHIO_CONFIG`, then the user's, then the system's location),
  `peers.cfg` per-peer grants (`info`, `fs.read`, `fs.write`, `exec`, `shell`, `admin`, filesystem
  roots, a program list), and the host's capabilities.
- `bohio config check` and `bohio info` (text or `--format json`); exit status 0, 1 or 2.
- Process execution (`bohio::exec::serve`): argv run directly or an explicit shell script, working
  directory and environment changes, standard input, streamed output and error, a deadline, kill and
  closed-stream handling that end the whole process tree, and a bound on concurrent processes
  (`[execution] max_processes`).
- `bohio-client`: the `Open` trait for a link and `exec::Exec` to run a process.
- Filesystem access confined to a peer's `fs_roots` (`bohio::fs`, `bohio::control`): `stat`, `list` (paged), `mkdir`,
  `remove`, `rename`, `copy`, `symlink`, `readlink`, `set_times`, `set_mode`, `set_attributes` and `checksum` on the control
  stream, and file streams `bohio-read/1` and `bohio-write/1` (atomic replace through a temporary sibling, or create, append
  and write at an offset). A `..` or a symbolic link that leaves a root is refused as `outside_root`, with every component
  resolved against a directory handle; the header states what is race-free on each operating system. New `[filesystem]`
  settings bound transfers, chunks, listing pages and idle time.
- `bohio-proto`: the filesystem calls, metadata and the transfer frames. `bohio-client`: `control::Control`, `fs::Fs`,
  `fs::Reader` and `fs::Writer`.
