# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Forwards that survive a lost link, as a WireGuard peer does: nothing is run again when the other side comes back. `[peer."<id>".forward]` gains `local = ["127.0.0.1:5432=db.lan:5432", "unix:/run/app.sock=pipe:app"]` and `remote = ["127.0.0.1:0=127.0.0.1:8080"]`, the `-L` and `-R` grammar of `bohio remote ... forward` (a bad entry is named by `bohio config check`; they are what this node does toward the peer, beside the grant's `connect` and `listen`, which are what the peer may do here, and `bohio peer approve` keeps them). The node owns them: a `local` listener is bound once and stays up while the link is down, a connection that arrives then waits up to `[forward] wait_for_link` (`30s`; `0` closes it at once) for the link and is closed after, and each `remote` entry is asked of the host again whenever a link to the peer comes up, for the port the host chose the first time (so the peer's `listen` rule must cover that port, for example `127.0.0.1:*`, and AI6's `reopen_forward` finds its port again). The entries are read again every `[link] recheck`: an entry added is started, an entry removed stops and its port closes. `bohio serve` prints `local ADDRESS -> TARGET peer ID` and `remote ADDRESS -> LOCAL peer ID` each time one listens, `local|remote down ...` and `local|remote failed ...`; `NodeEvent::Kept` tells the same, `Node::run_forwards` runs them for an embedding application (`forward::Declarations`, implemented by `LayeredPeers`), and `forward::{LocalSpec, RemoteSpec}` parse an entry. `bohio remote ... forward`, `connect` and `listen` make the link again by default after one that was up is lost, with the waits of the peer's `[dial]` table (`retry_min`, `retry_max`, `retry_factor`, `retry_jitter`, found by `--server`, `--via` or the endpoint, else the dial defaults) or `--retry-min`, `--retry-max`, `--retry-factor` and `--retry-jitter`; `-L` listeners stay bound meanwhile, `-R` and `listen` ask the host again for the same port, `connect` waits for a link that ended before it was answered, and `--once` keeps the old exit with status 1 on loss. The first link is made once either way, and a bridged connection that is running still ends with its link. Over `--via` only the link to the node is kept: a connection that arrives while the node has no link to the peer is refused, which a declared forward in the node's own `bohio.cfg` waits out instead. `bohio_client::keep` has `Backoff`, `Jitter`, `Links`, `Relink` and `Reconnecting`; `DialConfig::backoff` gives a table's waits.
- A client waits instead of failing when the host is full for the moment. An open the host refuses because it is over a limit (`[link] max_pending_opens`, `max_streams`, `max_per_protocol` or `max_opens_per_minute`, which the host closes with the text `over a limit`) an open the link could not queue behind the others of a burst (`NoProgress`), and a `connect` or `accept` it answers `busy` (`[forward] max_connections`, `max_connections_per_peer`) are tried again every `[forward] open_retry_interval` (`100ms`, each moved by up to a quarter of itself) for `[forward] open_retry` (`10s`; `0` never retries) before the connection fails, so a burst of 64 connections through one `-L` is served as the host drains instead of losing most of them to a pending-open bound of 8 and a per-peer bound of 16. Those bounds keep their defaults: the retry is the client's, so a host's bound still caps what a peer costs it. A refusal for a grant, a revoke or any other answer is still final at once. `ForwardSettings` gains `open_retry`, `open_retry_interval` and `wait_for_link`; `Open` gains `open_cause` (with `connect::Cause`) and `relink_wait`. No message changes.
- Deltas: move only what changed (rsync's algorithm, streamed, specified in `docs/protocol.md` "Deltas" so another language can implement it). `bohio-hash/1` takes
  `blocks` and sends each item's block signature (a rolling weak checksum and a strong hash of each block, in `hash-blocks` frames); `bohio-read/1` takes `delta`
  (the client sends the signature of its old copy and the host answers `copy` and data frames), and `bohio-write/1` has the mode `delta` (the client sends `copy`
  and data frames against the target the host holds), applied atomically as `replace` is and refused with the new error `changed` when the target is not as the
  client saw it. A delta write reads the target, so it needs `fs.read` as well as `fs.write`. Compression composes: literal bytes are compressed as the stream says.
  The block size is about the square root of the file's size as a power of two, between `[delta] min_block` and `max_block`; `[delta]` also has `max_signature_bytes`,
  `min_size` and `max_expansion`, and `info.transfer.delta` says what a host offers. `bohio_transfer::{rolling, signature, delta, apply}` (no I/O; the signature, a
  window of one block and one frame of literals are the memory, never the file), `bohio_client::sync::{compare_file, put, get}` (a quick compare by size and time,
  then a digest, then blocks; `auto`, `always` or `never` for a delta; `Moved` says the wire bytes), `bohio_client::hash::hash_blocks`, `Writer::copy`,
  `Control::transfer` (what the host offers, from its `hello` reply, with no `info` permission), `bohio remote ... put` and `get` (`--delta`, `--compare`,
  `--compress`, `--block-size`; one line `sent W of S (copied C, literal L)`), and `vectors/delta.json` (weak sums, a signature and steps).
- A Windows program of `exec` starts inside its job object (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so a child it starts in its first instructions is in the tree and ended with it. A shell session's terminal still starts its child itself and joins the job just after.
- A killed host ends the programs it started (Unix). Each `exec` program and each shell session gets a guardian process that signals the program's process group (and a shell's session, on Linux) when the host's end of a pipe closes, so a `SIGKILL`ed `serve` no longer leaves its trees running on Linux, FreeBSD and macOS. `[execution] host_death` and `[shell] host_death` choose `kill` (the default), `term` or `leave`.
- WebTransport's UDP payload sizes and path-MTU discovery are settings: `[[listen]] mtu_initial`, `mtu_min`, `mtu_discovery` and `mtu_upper_bound` for a `webtransport` entry, the same four keys in `[peer."<id>".dial]`, `--mtu-initial`, `--mtu-min`, `--mtu-discovery` and `--mtu-upper-bound` on `bohio remote` and `bohio join`, and `Endpoint::with_mtu` in `bohio-client`. The defaults are QUIC's (`1200`, `1200`, on, `1452`), so nothing changes unless set. A path with a smaller MTU than 1452, such as a site tunnel, loses every probe above it, which the OS may report as a send error (Windows 10040, a 1389-byte datagram): set `mtu_upper_bound` to the path's size or `mtu_discovery = false`. A size below 1200 is refused naming the value.
- A host's desktop over `bohio-gui/1`, and the person at the screen told. Three new permissions, all off by default: `gui.view` (snapshots and the wait for the screen to settle), `gui.input` (keyboard, mouse and scroll, with the desktop user's whole authority) and `gui.clipboard` (text, beside one of the others); `[gui] enabled = false` also gates the host. One stream is one view: `ready`, then `snapshot` (PNG, JPEG or lossless WebP, a display or a region, scaled to fit `max_width` and `max_height`, with the pointer and an optional settle), `input` (move, button, scroll, key, text and wait, in order), `settle` (one round trip until the screen is quiet), the clipboard's text and `state` whenever the desktop changes. `[gui]` holds `max_streams` (`4`), `max_streams_per_peer` (`2`), `snapshots_per_minute` (`120`), `max_snapshot_pixels`, `formats`, `jpeg_quality` (`80`), `max_input_rate` (`1000`), `max_batch_events` (`256`), `max_text` (`4096`), `max_batch_wait` (`5s`), `input_when_locked` (`false`), the settle times, `clipboard_max_bytes`, `indicator` (`notify`), `indicator_linger` (`30s`), `approval` (`none`), `approval_timeout` (`60s`) and `approval_ttl` (`15m`). A new error code, `unavailable`, says a desktop exists but cannot be used now (locked, not at the screen); `info.gui` reports the backend, the displays and the state, and the sessions only to a peer with a GUI permission. `Node::with_desktop` takes any `gui::Desktop` (a backend, or an application's own); `NodeEvent::Gui` tells each start, end, clipboard use and refusal as counts, never pixels or typed text. While a peer views or controls the screen the host shows a desktop notice (`notify-rust`) naming it by twelve digits of its id, one per engagement however many short streams it opens; with `approval = "local"` a stream waits for `bohio gui allow ID` (or the notice's Allow action where the notification service has actions) on the node's local endpoint (`bohio-gui-approve/1`). `bohio_client::gui::Gui` and `bohio remote ... gui info|snapshot|click|move|scroll|type|key|clipboard`. This version has no platform backend, so `bohio serve` has no desktop to serve yet.
- Remote forwards: the host listens for a peer and hands it each connection (`ssh -R`), and every kind of end pairs with every kind on the other side. `bohio-listen/1` (a new `forward.listen` permission) has the host bind a TCP address (port 0 lets it choose), a Unix socket or a Windows named pipe, answers `listening` (what was bound, and a random 128-bit token) and then announces each connection that arrives (`arrival`: an id and its source); the listener lives exactly as long as that stream, so closing it, losing the link or a revoke ends the listener, closes what still waits and resets what was handed over. `bohio-accept/1` claims one arrival by the token and its id, answers `connected` or `not_found` (the same words for an unknown, expired, claimed or another peer's arrival) and then carries the connection's bytes as `bohio-connect/1` does. A peer binds only where its `[peer."<id>".forward] listen` list says (`bohio peer approve --listen RULE`, which also grants `forward.listen`): `BIND:PORT` with an address (`0.0.0.0` too), a CIDR network or `*` and a port, a range or `*` (the host-chosen port `0` only where a rule names it or `*`), and `unix:PATH` or `pipe:NAME` exactly or with `*` and `?`; a bind no rule covers is refused before anything listens. A socket the host creates is `0o600` and a pipe is for its own account, and `ipc_same_user` closes a connection from another account unread. `[forward]` gains `max_listeners` (`16`), `max_listeners_per_peer` (`8`), `max_pending` (`16`), `accept_timeout` (`10s`), `socket_mode` (`"0o600"`) and `ipc_same_user` (`true`); a new error code `in_use`; `info.forward.listen` lists the kinds. `bohio remote ... forward -R [BIND:]PORT=LOCAL | unix:PATH=LOCAL | pipe:NAME=LOCAL` (mixed freely with `-L`) brings each arrival to a TCP, Unix socket or pipe target on this machine, and `bohio remote ... listen [--keep] BIND` bridges one arrival (or each, with `--keep`) to standard input and output, with the address on standard error; both work over `--via`. `bohio_client::forward` has `listen`, `RemoteListener`, `RemoteForward`, `connect_local` and `Tunnel::pump_local`; `NodeEvent::Forwarded` tells each bind and claim. A Windows named pipe still has no half-close. `docs/protocol.md` has the CDDL and `vectors/listen.json` and `vectors/accept.json` the messages.
- Connections the host makes for a peer: local forwards, `connect` and `ProxyCommand`. `bohio-connect/1` (a new `forward.connect` permission) opens one connection from the host to a target (`tcp` host and port, a Unix socket, a Windows named pipe), answers once with `connected` (the address reached) or an error that says why nothing connected (`refused`, `unreachable`, `timeout`, `not_found`, `busy`, `unsupported`), and then carries the connection's bytes raw in both directions, a `finish` as a half-close and any other end as a reset. The host also serves `coqui-link`'s `tcp/1` under the same grant, so a `coqui-link` peer uses it unchanged. A peer reaches only what its `[peer."<id>".forward] connect` list names (`bohio peer approve --connect RULE`, which also grants `forward.connect`): `HOST:PORT` with an exact name, `*.suffix`, `*`, an address or a CIDR network and a port, a range or `*`, and `unix:PATH` or `pipe:NAME` exactly or with `*` and `?` that do not cross a separator; nothing is resolved to decide, so an address rule never matches a name. A target the grant does not cover is refused before the host connects, and the grant is asked again at every `[link] recheck`, so a revoke resets running connections. `[forward]` holds `enabled`, `connect_timeout` (`10s`), `max_connections` (`64`), `max_connections_per_peer` (`16`), `chunk_size`, `stalled`, the pump's `idle_min`, `idle_max` and `watch`, and the client's `listen_poll`, `answer_timeout` and `max_local_connections`; `info` reports `forward`. `bohio remote ... connect [--check] TARGET` bridges standard input and output (netcat, `ssh -o ProxyCommand="bohio remote ... connect %h:%p"`) and `bohio remote ... forward -L [BIND:]PORT=TARGET | unix:PATH=TARGET | pipe:NAME=TARGET` listens locally (loopback unless an address is named, which is reported as `non-default`); both work over `--via`. `bohio_client::forward` has `connect`, `Tunnel`, `listen_tcp`, `listen_ipc` and `ipc`; `NodeEvent::Forwarded` tells each connect (a target's text, never a byte). `bohio_client::Open::open` returns a `coqui_link::mux::LinkStream` (a `Relay` now finishes, says whether its peer finished and closes with a reason), so a half-close crosses the relay and the node's `--via` pass; an `Open` over plain channels wraps them in `relay::Plain`, and `Open::is_up` is new. A Windows named pipe has no half-close. `docs/protocol.md` has the CDDL and `vectors/forward.json` the messages.
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
- Compression, per stream and per direction, for a slow link or a weak host. A client names it in the request (`compression` on a read and a write,
  `output_compression` and `stdin_compression` on exec; `zstd`, `lz4`, `deflate`, `none` or `auto`, with a `level` and for zstd a `window_log`); the host clamps it to its
  `[compression]` caps and reports what it used in the first frame, and `info.transfer` lists the algorithms with their levels, `compression_default` and `prepared`.
  Compressed bytes travel in their own frame tags beside the raw ones (`0x06` host to client, `0x14` write, `0x06`/`0x07`/`0x13` exec), so a sender switches per
  frame: a piece that does not shrink goes raw, and **a raw frame ends the compression context**, which every implementation follows (`docs/protocol.md`). A file
  whose extension is already compressed, whose name is key material (`exclude`) or whose first `sample_bytes` do not compress is read raw from the first frame.
  `prepare: true` compresses the whole range into a file on the host first (progress frames keep the stream alive), reports `compressed_length` and a `token`, and
  resumes from a compressed offset. What a peer sends is decoded within `window_log_max` and `max_expansion`, and a reader stops at the header's `length`.
  `[compression]` has `allowed`, `default`, `max_level`, `deflate_max_level`, `window_log_max`, `workers`, `skip_extensions`, `sample_bytes`, `sample_ratio`,
  `exclude`, `max_expansion`, `prepare_dir`, `prepare_max_bytes` and `prepare_ttl`. zstd is the C library behind the default-on Cargo feature `zstd` (`zstdmt` adds
  worker threads) of `bohio`, `bohio-client` and `bohio-transfer`; without it `lz4` and `deflate` remain and a request for zstd is `unsupported`.
  `bohio_transfer::compress` (`Encoder`, `Decoder`, `Caps`, `Bounds`), `bohio_client::fs::{put, PutOptions, ReadOptions, resolve_auto}`, `Writer::start_with`,
  `Reader::{compression, wire_bytes}`, `Exec::{start_with, compression}`, `bohio remote ... read --compress ALG[:LEVEL] [--prepare]`, `write PATH` (standard input to a
  file) and `exec --compress`, and the bench's `read_<corpus>_<size>[_<algorithm>]`, `write_...` and `exec_text_...` scenarios (feature `compress`).
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

- Windows: a process `exec` starts, and the PowerShell the access check starts, hold only their three standard streams of the host. It used to inherit every inheritable handle, among them the sockets `TcpStream::try_clone` makes, so a connection the host had dropped stayed open until the process ended and then read as a reset (os error 10054) on the other side. A batch file is no longer started directly (its arguments would be read by `cmd`); run `cmd /C` on it.
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
