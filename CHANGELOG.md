# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Hashing on the host, for pathapi's compare (its default is MD5) and any other sync. `bohio-hash/1` (needs `fs.read`) takes a request of algorithms in its open
  and a stream of items (`path`, `offset`, `length`, and `if_size` with `if_modified` to skip a file that has not changed), and answers each in order with its
  digests, or with an item error that does not end the stream; a long hash no longer holds the control stream and a batch is not held to `max_metadata`.
  Ten algorithms, chosen per call and several in one pass: `md5`, `sha1`, `sha256`, `sha384`, `sha512`, `blake3`, `xxh3`, `xxh128`, `xxh64` and `crc32c`, written
  as the reference tools print them. New crate `bohio-transfer` (`hash::{available, Hasher, digest_reader}`, no I/O of its own). `[hash]` has `max_streams` (`4`),
  `rate` (`0`, unlimited), `read_size` (`1048576`) and `algorithms` (every one); `[filesystem] write_hashes` (`["sha256"]`) is what a write answers. A write
  takes `expect` (a digest per algorithm, all checked) and `hashes`; a read takes `hashes` and its closing frame carries the digests of what it sent. `info`
  and `hello` report `transfer` (`hashes`, `hash_limits`). `bohio_client::hash::{hash_file, hash_range, hash_many, hash_items}` (items and results on two
  threads, so a long batch cannot stop on a full stream), `bohio remote ... hash [--algorithm NAME]... PATH...` and the bench's `hash_*` scenarios
  (feature `hash`). `docs/protocol.md` has the CDDL and `vectors/hash.json` the messages.
- `[link] key_recheck` (unset: `2 x recheck`) is how often the link itself asks whether the peer's key is still accepted; it was always twice `recheck`. A stream whose link ended before its reader took what it held (a bulk read behind a slow reader) lost that and read as ended in order, not revoked: the opener now lets the reader take the rest and the stream's own end for at most `[link] send_limit`. A revoke found by that check, and not by the stream recheck, now reaches a running stream as a revoke: coqui-link 50c768d closes every open stream revoked before it ends the link. A `remote --via` read or exec that the check found first used to end with `the channel failed: the peer broke the wire format`.
- `tools/build.py` (a copy of the release overlay's build tool), `tools/build.cfg` and `tools/sdk.lock`: a `deps` branch of vendored
  crates kept equal to `Cargo.lock`, the lock of the toolchains and base images a release is built with, offline builds from the
  branch, and release archives per target (static Linux musl, Windows with a static CRT, a universal macOS binary) and a container
  image per architecture. `test.yml` checks the tool, builds offline with the network cut and builds every release artifact;
  `release.yml` attaches them to a GitHub release on a `v*` tag. See `docs/building.md`.
- Per-stream receive windows. One stream moves at most its window per round trip (1 MiB over a 250 ms path is about 4 MB/s), and the window could only be
  raised safely on both sides at once. `[link] bulk_window` is the receive window a side grants a bulk stream it receives (a client's `bohio-read/1`, a host's
  `bohio-write/1`): it widens the stream by granting credit, which every sender already accepts, so it may differ between sides and no peer changes. `max_window`
  (unset: the larger of `window` and `bulk_window`) bounds it and `auto_window` (`false`) grows a stream by itself from the link's round trip, up to it. `window`
  stays the credit both sides start from; each side logs once, as a warning and not a refusal, a `window` or `max_frame` its peer's `link/1` shows differing from
  its own. `bohio_client::Open` gains `open_with` and `bulk_window` (`OpenOptions { receive_window }`), `Opener::pair` takes the settings, and the host widens a
  `bohio-write/1` as it accepts it.
- `[link] max_metadata` (`4096`), `max_pending_opens` (`8`) and `max_opens_per_minute` (`600`) are settings, where coqui-link's defaults applied before: a sync
  that opens a stream for each small file is capped by the last. `tcp_send_buffer` and `tcp_receive_buffer` (unset: the system's automatic sizing) set a TCP
  socket's buffers before connecting and on the listener and each accepted socket.
- Pairing on a node's own listeners, over every transport: `bohio pair [--timeout T]` arms one PIN on the running node through its local control endpoint (`bohio-pair/1`), or, when no node runs, serves pairing alone on the configured `[[listen]]` entries (`--listen` only for a configuration with none). A joiner sends a marker message first (`coqui-link` 517c5db `pairing::join_shared`) and the node's `coqui_link::pairing::answer` runs a pairing or a proof by the first message, so a pairing needs no second port and runs over tcp, pipe, ws, wss, webtransport and serial. `bohio join` takes any endpoint `bohio remote` takes, with `--ca`, `--cert-hash`, `--clock-skew` and `--fallback`. A node pairs with `Node::with_pairing(store)` and `arm_pairing`; a paired key goes to `Keys::paired` (`LayeredPeers` writes its `[peer.<id>]` table, holding nothing); `NodeEvent::Paired` and `PairingRefused { source, attempts, reason }` never hold a PIN. The one-guess rule is now `[pairing]`: `max_attempts` (default `10`, `0` no limit), `attempt_delay` (`1s`, doubling per source to `attempt_delay_max`, `30s`), `pin_length` (`6`) and `timeout` (`5m`); `docs/security.md` has the odds. `peer::pair` and "pairing runs over tcp only" are gone.
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
- Local control. A running node serves `bohio-via/1` and `bohio-links/1` on a local endpoint (`[control]`, by default `pipe://bohio-` and 16 hex digits of its id,
  this user alone; `bohio::local::{Local, LocalLink, Via}`), never on a peer's link. `bohio remote --via ID <action>` runs `info`, `exec`, `read` and `shell` over
  the link the node holds to that peer, whichever side made it, so the controller of a host that dials in is the node itself; `bohio links` lists each link with its side,
  endpoint or source and what each end offers. `bohio_client::connect::Opened` and `Relay::end` carry a stream's end reason and text so a refusal or a revoke crosses
  `--via` unchanged. `bohio_proto::local` has the wire types.

### Changed

- **Breaking, in place:** the control call `checksum` and its reply are gone (use `bohio-hash/1`); `write-request.expect_sha256` is `expect: {algorithm: hex}`
  and `write-done.sha256` is `hashes`; a read's closing frame (`0x04`) carries a body; `Fs::checksum` and `Checksum` are gone; `sha2` is 0.11.
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

- A configuration layer another account owns can no longer turn its own check off. `[service] check_access = false` in the lowest layer
  waived every finding, so on Windows any local user who created `%ProgramData%ohioohio.cfg` (every user may, while the folder does not
  exist) could write that line beside a `[peer.<id>]` table of their own and be served by every `bohio serve` and service that reads the
  system file. It now waives only paths the operator owns that others may change; a layer or directory another account owns is refused either way.
- A default layer that did not exist when `bohio serve` started was read at every decision once it appeared, and never checked: a system file
  another user made under `%ProgramData%` while the node ran chose its peers at once. A missing layer is now read while serving only when the
  nearest directory above it that exists passes the access check; otherwise it is read after a restart, which checks it.
- Every command checks the system file when it exists, as `serve` checks every layer: it names the data directory, where a client's key is
  made, and the control endpoint, so one another account made or every user may change is refused by `remote`, `join`, `id`, `pair` and `links` too.
- A client of a node's local control endpoint checks that it is served by this account before it sends anything (`bohio::local::Server`,
  `LocalLink::connect_to`, `Via::connect_to`, `ANOTHER_ACCOUNT`), unless `[control]` sets who may connect. The endpoint is not sealed, and
  another account that bound the name first (a pipe name is visible to every Windows user) read every `remote --via` request and could answer
  `bohio pair` with its own key as the one that paired; `pair` no longer falls back to serving alone when it meets one.
- `Node::run_dial` no longer dials a peer its `Keys` no longer accept: a peer revoked while the dialer ran kept being dialed until a restart,
  learning each time that this node was up, where it was and its key. It asks the keys again every `retry_min`.
- A pinned certificate made just now was refused at first and accepted later: a certificate is held to its dates even when pinned,
  and a client whose clock is behind the host's sees it as not yet valid until its clock reaches the start, while the message
  blamed the trust. The refusal now names the certificate's hash, its start or end and the clock difference;
  `bohio remote --clock-skew SPAN` (`ServerTrust::clock_skew`) accepts a start or end within SPAN of the client's clock (default
  none); the made WebTransport certificate's one-hour backdate is the setting `[[listen]] tls_backdate`. The message no longer
  holds a run of spaces. `--cert-hash` above is released from the browser rule, not from the certificate's dates.
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
