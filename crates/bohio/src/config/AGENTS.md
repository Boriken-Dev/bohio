# `bohio::config` and `bohio::policy` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Configuration (`bohio::config`)

Profiles (`bohio::profile`): `pub const fn table(profile: bohio_client::profile::Profile) -> &'static str` is the TOML a profile lays under the files (empty for `default`; `lan`, `weak-host`, `slow-link` and `fat-link` set only keys of `[filesystem]`, `[link]`, `[compression]`, `[hash]`, `[walk]` and `[delta]`, and no `[peer.*]`); `NodeConfig::with_profile(profile) -> NodeConfig` is the defaults with that table over them, for an embedding application; `Config::transfer_keys(&self) -> Vec<(&'static str, String)>` is every key a profile or a transfer depends on with its effective value as text, in the order `config check` prints it. `Layered` (from `Layers::load`/`parse`) holds the `profile` that `[profile] name` chose; `origin("link.bulk_window") -> Origin` says where a key's value came from (`File(label)`, `Profile(profile)` or `Default`; `Display` is `file LABEL`, `profile NAME`, `default`) and `sets(key) -> bool` whether a layer, not the profile, sets it. Merge order, lowest first: the profile's table, then each file layer; an error in a value only the profile supplied names the layer `profile NAME`.

```rust
pub struct Environment { pub windows: bool, pub local_app_data: Option<PathBuf>, pub xdg_data_home: Option<PathBuf>, pub home: Option<PathBuf>,
    pub path: Option<OsString> }                                  // Default; the binary fills it from the process (cli::capture_environment)
impl Environment { pub fn default_data_dir(&self) -> Result<PathBuf, NoDataDir>; }   // NoDataDir: neither HOME nor XDG_DATA_HOME (LOCALAPPDATA on Windows) is set; never the working directory
#[derive(Copy)] pub struct NoDataDir;                        // std::error::Error; set `[service] data_dir`

pub struct Config { pub service: ServiceConfig, pub profile: ProfileConfig /*`[profile]`: name, default "default"*/, pub listen: Vec<ListenConfig>, pub link: LinkConfig, pub filesystem: FilesystemConfig,
                    pub hash: HashConfig, pub walk: WalkConfig, pub search: SearchConfig, pub delta: DeltaConfig, pub compression: CompressionConfig, pub execution: ExecutionConfig, pub shell: ShellConfig,
                    pub sessions: SessionsConfig, pub forward: ForwardConfig, pub gui: GuiConfig, pub pairing: PairingConfig, pub waiting: WaitingConfig, pub client: ClientConfig, pub mcp: McpConfig, pub control: ControlConfig,
                    pub power: PowerConfig, pub media: MediaConfig, pub secrets: SecretsConfig, pub providers: ProvidersConfig, pub source: SourceTables, pub access: AccessConfig,
                    pub peer: Peers, pub remote: Hosts }     // Default; unknown keys refused; `peer` is `[peer.<id>]`, `remote` is `[remote.<name>]`
pub struct NodeConfig { pub name: String, pub link: LinkConfig, pub filesystem: FilesystemConfig, pub hash: HashConfig, pub walk: WalkConfig, pub search: SearchConfig, pub delta: DeltaConfig,
                        pub compression: CompressionConfig, pub execution: ExecutionConfig, pub shell: ShellConfig, pub sessions: SessionsConfig, pub forward: ForwardConfig, pub gui: GuiConfig,
                        pub pairing: PairingConfig, pub power: PowerConfig, pub media: MediaConfig, pub secrets: SecretsConfig, pub providers: ProvidersConfig, pub source: SourceTables,
                        pub access: AccessConfig }   // Default, Deserialize, unknown keys refused: what a Node runs on, read by an app from any section of its own settings
impl NodeConfig { pub fn validate(&self, label: &str) -> Result<(), ConfigError>; }   // every range check; `Node::new` calls it
impl Config {
    pub fn node(&self) -> NodeConfig;                                            // `[service] name` and every `NodeConfig` section
    pub fn from_toml(text: &str, path: &str) -> Result<Self, ConfigError>;       // one layer of text; `path` only labels errors
    pub fn load(path: &Path) -> Result<Self, ConfigError>;                       // one layer, a file
    pub fn data_dir(&self, environment: &Environment) -> Result<PathBuf, NoDataDir>;
    pub fn non_default(&self, listeners: &[ListenConfig]) -> Vec<String>;         // one `non-default: [section] key = value: cost` line for each weakened link or forward check (`[forward] socket_mode`, `ipc_same_user`, `follow_links`, `[pairing] max_attempts = 0`, `[[listen]] socket_mode`, `remote_clients`, `origins` with `*`); `bohio serve` logs them at start
}
pub struct Layers;                                                // `load` reads the files it is given and nothing else; `discover` below finds them
impl Layers {
    pub fn load(paths: &[PathBuf]) -> Result<Layered, ConfigError>;             // lowest first, every file must exist; none: the defaults
    pub fn parse(text: &str, label: &str) -> Result<Layered, ConfigError>;      // one layer of text
    pub fn peers(paths: &[PathBuf]) -> Result<Peers, ConfigError>;              // the live check: a missing file is skipped, only `[peer.*]` is read
}
pub struct Layered { pub config: Config, pub peers: Peers, pub files: Vec<PathBuf>, pub profile: Profile }   // `peers` is `config.peer`; `files` the layers read; origin(key), sets(key) above
pub fn discover(named: &[PathBuf]) -> Result<Found, DiscoverError>;     // the layers named (alone), else the system file with the user's merged over it (`[service] user_layer = false` leaves it out); a system file another account owns or may change is refused
pub fn discover_trusting(named: &[PathBuf], trust: &[String]) -> Result<Found, DiscoverError>;   // as `discover`, the accounts whose ownership of the lowest layer, its directory and the system file counts given (`bohio serve --trust-owner`); `discover` takes `BOHIO_TRUST_OWNER`
pub fn discover_in(named: &[PathBuf], system: PathBuf, user: Option<PathBuf>, trust: &[String]) -> Result<Found, DiscoverError>;   // the same with the two files given; `default_files()` is their usual places
pub struct Found { pub layered: Layered, pub watch: Vec<PathBuf>, pub create: Option<PathBuf>, pub trust: Vec<String> }   // `config()`; `guard(&Environment) -> Result<Vec<PathBuf>, String>` is the owner check `bohio serve` applies (makes the data directory; answers the layers to watch); `watched()`
// `bohio::ownership`: `check(path, Guarded) -> Result<(), Problem>`, `layer_problems`, `library_problems`, `program_problems`, `may_appear`, `decide(&[Problem], check_access, trusted) -> Result<(), Refused>` (`Refused { problems: Vec<Problem> }`, Display joins the lines), `trusted_from_environment()`
pub struct Span(pub Duration);                                    // a number of seconds, or text: 250ms, 5s, 2m, 1h; Span::parse(&str) -> Option<Span>
#[non_exhaustive] pub enum ConfigError { Read { path, reason }, Parse { path, line, column, key: Option<String>, message }, Invalid { path, key, message } }
```

`bohio.cfg` is TOML (a `.cfg` first line `#!...` is a comment); every key is optional and an unknown key or section is `ConfigError::Parse` naming it,
with file, line, column and key (a value of the file is never quoted). Defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `[service] name` | `"bohio"` | name shown in `info` |
| `[service] check_access` | `true` | refuse to start when another account may change a layer, the directory that holds it or the data directory (or, on Unix, its key file `identity` is readable by the group or others); `false` logs it and serves, except for a path another account owns, which is refused either way; lowest layer only |
| `[service] user_layer` | `true` | whether the user's file is read over the system's when no layers are named; lowest layer only |
| `[service] trusted_owners` | none | accounts (a user id or name on Unix, a security id on Windows) whose ownership of a higher layer or the data directory is accepted as the operator's, logged `non-default:`; the lowest layer and its directory are judged by `bohio serve --trust-owner ACCOUNT` (or `BOHIO_TRUST_OWNER`) alone; lowest layer only |
| `[service] upper_layer_keys` | none | `section.key` names (such as `secrets.check_access`) a higher layer may set although only the lowest layer otherwise may: its value for any other such key is ignored and logged; logged `non-default:`; lowest layer only |
| `[service] stop_wait` | `10s` | Windows: how long the service manager is told a start or a stop may take, and how long `service uninstall` waits for a running service to stop (`--wait` overrides); not zero |
| `[service] data_dir` | `bohio` under `%LOCALAPPDATA%` / `$XDG_DATA_HOME` (else `~/.local/share`) | identity and the waiting list |
| `[[listen]] endpoint` | none | `tcp://host:port`, `pipe://name` or `pipe:///path.sock`, `ws://host:port`, `wss://host:port`, `webtransport://host:port`, `serial://port?baud=N`; required in each entry |
| `[[listen]] socket_mode` | `"0o600"` | `pipe`, Unix: the socket's mode, octal text with the `0o` prefix or a TOML octal integer (`Mode`, as `[forward]` and `[control]` take it) |
| `[[listen]] remote_clients` | `false` | `pipe`, Windows: clients on other hosts through file sharing |
| `[[listen]] origins` | none | `ws`, `wss`, `webtransport`: the origins a browser may connect from; none (or empty) refuses every request that names an origin and serves a client that names none; `["*"]` allows any, logged `non-default:` |
| `[[listen]] tls_cert`, `tls_key` | none | `wss`, `webtransport`: PEM files, the certificate (leaf first, then its intermediates) and its private key; both or neither; `wss` needs both; read at start, never logged; a `webtransport` entry with neither makes its own certificate |
| `[[listen]] tls_valid_days` | `13` | `webtransport` without `tls_cert`: the made certificate's validity in days, at least 1 (a browser pins at most 14: a longer one is logged once as not browser-pinnable); it is kept in `certificates/` of the data directory (a restart serves the same one); a new one is made halfway through, its hash logged and printed, and `NodeEvent::CertificateRenewed` sent |
| `[[listen]] tls_backdate` | `1h` | `webtransport` without `tls_cert`: how long before now the made certificate starts to be valid, so a client whose clock is behind the host's accepts it at once (`0s` starts it now; a client further behind is refused until its clock reaches the start, or allows for it with `--clock-skew`); a time as in `[link]` |
| `[[listen]] mtu_initial`, `mtu_min`, `mtu_discovery`, `mtu_upper_bound` | `1200`, `1200`, `true`, `1452` | `webtransport`: the UDP payload sizes of each QUIC connection and whether it searches for a larger one (RFC 8899). A path with a smaller MTU (a tunnel) loses each probe above it, which the OS may report as a send error: set `mtu_upper_bound` to the path's size (MTU less 28) or `mtu_discovery = false`. A larger `mtu_initial` than the path carries stalls the connection until black-hole detection falls to `mtu_min`; `mtu_min` above the path's size opens none. `mtu_initial` or `mtu_min` below 1200 (QUIC's least) is refused naming the value; `mtu_upper_bound` is capped at 65527 by QUIC |
| `[[listen]] baud`, `data_bits`, `parity`, `stop_bits`, `flow_control` | `115200`, `8`, `none`, `1`, `none` | `serial`: the line; the endpoint's query (`?baud=9600`) wins; `flow_control` is `none` or `hardware`; a key for another scheme is refused naming it |
| `[profile] name` | `default` | the host's named defaults, laid under every file layer (a key any layer sets wins, key by key); `default`, `lan`, `weak-host`, `slow-link` or `fat-link`; the highest layer that sets it decides; another name is `ConfigError::Invalid` naming `profile.name`; see `bohio::profile` |
| `[link] recheck` | `5s` | how often a running stream's permission is decided again; not zero |
| `[link] key_recheck` | unset (`2 x recheck`) | how often the link itself asks whether the peer's key is still accepted; when it is not, each running stream is closed with a revoke and the link ends; not zero |
| `[link] max_streams` | `64` | streams per peer; at least 1 |
| `[link] max_connections` | `128` | links open at once across peers, those still proving included; one past it is closed unread; at least 1 |
| `[link] ipv6_prefix` | `64` | the IPv6 prefix length (1 to 128) the connections still proving themselves are grouped by; `[pairing]` and `[waiting]` have their own |
| `[link] max_unproven_per_source` | `16` | connections from one source (an IPv4 address or an IPv6 prefix of `ipv6_prefix` bits) still proving themselves; one past it is closed unread; a pipe, serial line or stdio link has no source; at least 1 |
| `[link] max_links_per_peer` | `16` | links one proven peer may have open; one past it is closed after its proof; at least 1 |
| `[link] window` | `1048576` | bytes a peer may send on one stream ahead of what the host has read (the credit window); at least 1. Not negotiated: each sender starts a stream with its own as credit, so keep it equal on both sides (a side whose window is larger can end the other's streams as broken; each side logs a difference in `window` or `max_frame` that the peer's `link/1` shows, once per link, and refuses nothing). One stream moves at most `window` per round trip |
| `[link] bulk_window` | unset (`window`) | the receive window this side grants a bulk stream it receives: a client's `bohio-read/1`, a host's `bohio-write/1`; widens a stream by granting credit, so it may differ between sides and needs no change at the peer; at most `max_window`; a value below `window` changes nothing; costs that much memory for each bulk stream whose reader falls behind |
| `[link] max_window` | unset (the larger of `window` and `bulk_window`) | the most a stream's receive window may grow to, by `bulk_window` or `auto_window`; at least 1; at least `window`, and at least `bulk_window` |
| `[link] auto_window` | `false` | grows a stream's window by itself while data arrives (one `ping` per round trip; a stream that received over two thirds of its window before the `pong` doubles it, up to `max_window`); never shrinks; does nothing unless `max_window` is above `window` |
| `[link] proof_timeout` | `10s` | how long a peer has to prove itself, or to pair; not zero |
| `[pairing] timeout` | `5m` | how long an armed PIN lasts when its request names no time; not zero |
| `[pairing] max_attempts` | `10` | wrong guesses a PIN takes before it is used up (the last allowed one uses it up); `0` is no limit, the PIN then lasts until `timeout`; the odds with the defaults are `10 / 10^6` per armed PIN (`docs/security.md`) |
| `[pairing] attempt_delay` | `1s` | how long a source waits after a wrong guess; doubles at each further one from it up to `attempt_delay_max`; a source that tries sooner is refused without a try; `0` is no wait |
| `[pairing] attempt_delay_max` | `30s` | the longest wait; at least `attempt_delay` |
| `[pairing] pin_length` | `6` | digits in a PIN; at least 1 |
| `[pairing] pending` | `32` | pairing exchanges in progress at once while a PIN is armed; a joiner that sends the marker and then nothing holds a place until `exchange_timeout`; at least 1 |
| `[pairing] pending_per_source` | `4` | exchanges in progress at once from one source (an IPv4 address, or an IPv6 prefix of `ipv6_prefix` bits), which also bounds one source's simultaneous guesses; at least 1 |
| `[pairing] ipv6_prefix` | `64` | the IPv6 prefix length sources are grouped by, 1 to 128 |
| `[pairing] exchange_timeout` | `5s` | how long a joiner has to finish a pairing once it sent the marker (a proof has `[link] proof_timeout`); a silent joiner frees its place after it; a longer time tolerates a slower path and lets silent joiners hold places longer; not zero |
| `[waiting] per_source` | `16` | entries one source (an IPv4 address, or an IPv6 prefix of `ipv6_prefix` bits) may hold in the waiting list; at least 1 |
| `[waiting] total` | `1000` | entries the whole waiting list may hold; past it nothing is recorded; at least 1 |
| `[waiting] rerecord` | `60s` | how long the same peer from the same source is not recorded again |
| `[waiting] record_interval` | `1s` | the least time between two new records from one source; `0` is no limit |
| `[waiting] ipv6_prefix` | `64` | the IPv6 prefix length waiting sources are grouped by, 1 to 128 |
| `[waiting] when_full` | `refuse` | what a source at `per_source` does with a newcomer: `refuse`, or `replace-oldest` (peers sharing a source can then flush each other) |
| `[client] reply_timeout` | `30s` | the `bohio` command's wait for the host's first answer to a question (`info`, the start of a read, a shell or a desktop session); `remote --reply-timeout`; not zero |
| `[client] transfer_timeout` | `5m` | one step of a `put`, `get` or `write` may wait this for the host; `remote --transfer-timeout`; `0` waits for good |
| `[client] gui_timeout` | `2m` | a `gui` action's wait for the host, which may wait for the person at the desktop; not zero |
| `[client] input_chunk` | `16384` | bytes of standard input read at a time by `exec --stdin`, `shell`, `write` and `listen`; at least 1 |
| `[client] read_size` | `1048576` | bytes of a local file read at a time by `put`, `get` and a comparison; at least 1 |
| `[client] input_queue` | `4` | chunks of standard input `listen --keep` holds ahead of a connection; at least 1 |
| `[client] probe_timeout` | `1s` | how long `status` waits for an endpoint to answer before it reports it unreachable; not zero |
| `[client] power_wait` | `90s` | the least `remote power` and `remote media` wait for the host's answer; not zero |
| `[client] kill_after` | `2s` | a second Ctrl-C within this of the first ends the program's whole tree (`--kill-after`); not zero |
| `[client] sync_bundle_max` | `1048576` | the largest file `put` and `get` of a tree move in a bundle (`--bundle-max`); at least 1 |
| `[client] sync_bundle_items` | `1000` | the most files in one bundle (`--bundle-items`); at least 1 |
| `[mcp] max_calls` | `8` | the tool calls `bohio mcp` runs at once (`--max-calls`); at least 1 |
| `[mcp] gui_max_image_bytes` | `1048576` | where a `gui` tool's `format: "auto"` turns from PNG to JPEG 80 (`--gui-max-image-bytes`) |
| `[client] links_wait` | `5s` | how long `links` (`--wait`) and `remote --via` wait for each peer to say what it offers; not zero |
| `[client] parallel_streams` | `1` | streams `get`, `put` and `read` run at once for one file; `remote ... --parallel N`; `1` is the ordinary transfer (a delta where it applies), more moves the whole file in ranges; at least 1 |
| `[client] parallel_links` | `1` | links those streams are spread over, each a connection of its own with its own proof; `--links M`; through `--via` there is one; at least 1 |
| `[client] profile` | unset (`default`) | the profile whose client defaults (compression, delta, comparison, streams, bulk window, when to prepare) `bohio remote` uses for what a flag leaves unset; `remote --profile NAME` overrides it; a key the file sets itself (`parallel_streams`, `[link] bulk_window`) wins over it; one of `default`, `lan`, `weak-host`, `slow-link`, `fat-link` |
| `[client] parallel_range` | `0` | bytes of one range; `--range BYTES`; `0` is one range per stream for `get` and `put`, 8 MiB for `read` (which holds up to two per stream in memory) |
| `[link] max_frame` | `65536` | most bytes of one message in one frame (a longer message is sent in parts); at least 1 |
| `[link] max_metadata` | `4096` | most metadata one open may carry; an open with more is refused; at least 1 |
| `[link] max_pending_opens` | `8` | streams a peer may have opened that nothing has accepted yet; at least 1 |
| `[link] max_opens_per_minute` | `600` | streams a peer may open in a minute (ten a second); a sync that opens a stream for each small file needs more; at least 1 |
| `[link] tcp_send_buffer`, `tcp_receive_buffer` | unset (the system's automatic sizing) | a TCP socket's buffer sizes in bytes, set before connecting (the client) and on the listener and each accepted socket (the host); at least 1; the system may clamp them; they matter only where a path's bandwidth-delay product exceeds what the system reaches |
| `[link] max_message` | `1048576` | largest message on a stream; more than the largest `chunk_size` |
| `[link] queue_bytes` | `262144` | bytes a stream holds each way between the link and its engine; a larger message passes alone; at least 1 |
| `[link] read_ahead` | `8` | messages the socket's reader holds ahead of the link once a peer is proven (one before); at least 1 |
| `[link] queued_bytes` | `1048576` | bytes the socket's writer may have queued; at least 1 |
| `[link] send_limit` | `10s` | how long a frame the socket takes nothing of ends the link, and a finished stream's queue is given to drain; not zero |
| `[link] send_wait` | `250ms` | how long a send waits for credit before it reports no progress and is tried again; not zero |
| `[link] open_wait` | `250ms` | how long an open waits for one of the link's own streams to end when it holds `max_streams`, before it fails as no progress; `0` fails at once |
| `[link] poll_interval` | `50ms` | how often a stream's threads look at whether it was closed; not zero |
| `[link] heartbeat` | `25s` | how often the link asks its peer whether it is there (both sides ask; also keeps a quiet link from a proxy's or NAT's idle timeout); `0` is off |
| `[link] heartbeat_misses` | `3` | heartbeat intervals with nothing heard after which the link is lost; at least 1 |
| `[link] accept_wait` | `200ms` | how long a listener (the node's, and the local endpoint's) waits for a connection before it looks at whether it was stopped, so a stop is noticed within it; not zero |
| `[link] accept_poll` | `20ms` | how long a listener that cannot block sleeps between looks for a connection, and after one it could not take; not zero |
| `[link] serial_reopen` | `250ms` | how long a serial host leaves the port closed between links; a shorter wait can lose a reconnecting peer's first message |
| `[filesystem] enabled` | `true` | whether filesystem calls are served |
| `[filesystem] max_transfers` | `16` | concurrent `bohio-read/1` and `bohio-write/1` streams across peers; at least 1 |
| `[filesystem] allow_id_bits` | `false` | lowest layer only: whether a peer with `fs.write` may set the set-user-ID and set-group-ID bits (`mkdir`/`set_mode` mode, a write's `file_mode`); `true` is logged `non-default:`; a copy or replacement keeps neither bit either way |
| `[filesystem] max_transfers_per_peer` | `16` | the most transfers one peer has running of the `max_transfers` across peers; at least 1 |
| `[execution] max_processes_per_peer` | `16` | the most processes one peer has running of `max_processes`; at least 1 |
| `[shell] max_sessions_per_peer` | `8` | the most shell sessions one peer has open of `max_sessions`; at least 1 |
| `[link] max_streams_per_peer` | `256` | the most streams one peer has open over all its links; one past it is refused; at least 1 |
| `[walk] max_visited` | `10000000` | the most names one walk looks at over all its roots, sent or not (an `include` that matches nothing still ends); `0` is no limit |
| `[walk] recheck_entries` | `1000` | names a walk looks at between looks at the peer's grant; 0: once for each root |
| `[filesystem] chunk_size` | `65536` | most file bytes in one frame, and the buffer a copy reads with; at least 1; a smaller message bound of the stream wins |
| `[filesystem] max_tree_entries` | `1000000` | entries a recursive `copy`, or a `move` across filesystems, goes through before it stops with `busy`; `0` is no limit |
| `[filesystem] max_bundle_items` | `1000000` | files one bundle (`bohio-read/1` or `bohio-write/1` with `items`) carries before it stops with `busy`; `0` is no limit |
| `[filesystem] resume_ttl` | `0` | how long a resumable `replace` keeps its temporary file after its stream ended early; `0` keeps none, and such a request is `unsupported`; a kept file takes the disk space of what was received |
| `[filesystem] follow_interval` | `250ms` | how often a followed read looks for appended bytes when its request names no interval; not zero |
| `[filesystem] max_follows` | `16` | followed reads running at once across peers (a follow holds one of these and no transfer slot; `idle_timeout` does not end it); one past it is `busy`; at least 1 |
| `[filesystem] write_hashes` | `["sha256"]` | the digests a write answers when its request names none; each must be in `[hash] algorithms`; empty answers none |
| `[hash] max_streams` | `4` | concurrent `bohio-hash/1` streams across peers (`busy` past it); at least 1 |
| `[hash] rate` | `0` | most bytes a second one hash stream reads; `0` is unlimited |
| `[hash] read_size` | `1048576` | the buffer a hash reads with; at least 1 (a `rate` below twenty times it reads smaller pieces) |
| `[hash] algorithms` | every one | the digests the host computes, for hash streams, reads and writes; at least one, each known to this build |
| `[delta] min_block` | `2048` | the smallest block a signature the host makes uses; at least 1 |
| `[delta] max_block` | `1048576` | the largest block a signature uses or a peer may ask for (held in memory twice per stream); at least `min_block`, at most 2^30 |
| `[delta] max_signature_bytes` | `67108864` | the most signature a delta read takes from the client, decided from `base_size` before a batch is read (`busy` past it); at least 1 |
| `[delta] min_size` | `1048576` | advertised in `info.transfer.delta`: the size of file from which a client's `auto` uses a delta |
| `[delta] max_expansion` | `1024` | a delta write's output, once past 1 MiB, may be this many times the target's size plus the bytes received (`bad_request` past it); `0` is off |
| `[compression] allowed` | every algorithm this build has | which of `zstd`, `lz4`, `deflate` a peer may ask for (`unsupported` otherwise); empty allows none |
| `[compression] default` | `{algorithm = "zstd", level = 3}` (`lz4` without zstd) | what `auto` means; one of `allowed` |
| `[compression] max_level` / `deflate_max_level` | `19` / `9` | the highest zstd / deflate level; a higher request is clamped and the header says so |
| `[compression] window_log_max` | `23` | the largest zstd window (power of two, 10 to 31) the host uses and accepts; a decoder holds about one window per stream |
| `[compression] workers` | `0` | zstd worker threads per stream; above 0 needs the `zstdmt` feature |
| `[compression] skip_extensions` | archives, images, audio, video, documents, packages | extensions a read never compresses |
| `[compression] sample_bytes` / `sample_ratio` | `65536` / `0.9` | a read compresses its first `sample_bytes` as a trial and goes raw when it saves less than `1 - sample_ratio`; `0` bytes is no trial |
| `[compression] exclude` | `*.key *.pem *.p12 *.pfx id_* .env *.kdbx` | globs (of the path and of the name) of files never compressed |
| `[compression] max_expansion` | `1024` | decompressed over compressed bytes a write or standard input may reach (checked past 1 MiB); `0` is off |
| `[compression] bundle_context` | `per_item` | `per_item`: each file of a bundle starts a compression context (the host restarts its encoder and sends an empty raw frame; a client's write does the same unless `BundleOptions::shared_context`); `shared`: one context for the bundle, logged `non-default:` |
| `[compression] prepare_dir` / `prepare_max_bytes` / `prepare_ttl` | `prepared` in the data directory (the command; none when embedded) / `1073741824` / `10m` | where prepared reads are kept, the most one holds (`busy` past it) and how long they live |
| `[filesystem] list_page` | `1000` | most entries in one listing page, whatever the client asks for; at least 1 |
| `[filesystem] idle_timeout` | `5m` | how long a transfer may make no progress before the host abandons it; not zero |
| `[filesystem] poll_interval` | `20ms` | how often a transfer looks at its stream while it waits; not zero |
| `[filesystem] sync_writes` | `true` | whether a write is flushed to storage before it is put in place |
| `[execution] enabled` | `true` | whether processes are run |
| `[execution] max_processes` | `16` | concurrent processes across peers; at least 1 |
| `[execution] chunk_size` | `16384` | most output bytes in one frame; at least 1; a smaller message bound of the stream wins |
| `[execution] stdin_queue` | `4194304` | bytes of standard input held for a process that has not read them; the host keeps reading frames until they are held, so a kill or a signal reaches a program that reads nothing; at least 1 |
| `[execution] poll_interval` | `20ms` | how often a process, its deadline and its stream are looked at; not zero |
| `[execution] drain_timeout` | `500ms` | wait for a process's remaining output after it exits, before the rest of its tree is ended |
| `[execution] host_death` | `kill` | Unix: what happens to a process's group when the host dies without ending it: `kill` (SIGKILL), `term` (SIGTERM) or `leave` |
| `[execution] reset_signals` | `true` | Unix: a program starts with SIGINT, SIGQUIT and SIGHUP at their default action; `false` keeps what the host inherited and is logged `non-default:` |
| `[execution] reap_orphans` | `auto` | Linux: the host collects the guardians it adopts when its programs end; `auto` does so when it is process 1, `always` and `never` decide whatever its id is (`never` as process 1 is logged `non-default:`). A program's own orphans still need an init |
| `[execution] reap_interval` | `1s` | how often the host looks for ended guardians to collect; not zero |
| `[shell] host_death` | `kill` | Unix: what happens to a session's tree when the host dies without ending it: `kill`, `term` or `leave` |
| `[sessions] enabled` | `true` | whether detached sessions run; a peer still needs `session` |
| `[sessions] max_detached` | `16` | sessions across peers, the exited ones kept included; at least 1 |
| `[sessions] max_per_peer` | `8` | sessions one peer holds; at least 1 |
| `[sessions] scrollback` | `1048576` | output bytes kept when a request names no size; at least 1 |
| `[sessions] max_scrollback` | `16777216` | the most a request may ask to keep (more is `bad_request`); at least `scrollback` |
| `[sessions] linger` | `24h` | how long an unattached detached session is kept; not zero |
| `[sessions] keep_exited` | `1h` | how long an exited session is kept for a reattach; not zero |
| `[sessions] check_every` | `5s` | how often sessions are looked at for linger, kept time and a revoked grant; not zero |
| `[power] enabled`, `action_timeout`, `max_streams` | `false`, `60s`, `4` | whether power is served (a peer needs `power` too), how long an action may take before `timeout`, concurrent requests; not zero / at least 1 |
| `[media] enabled`, `action_timeout`, `max_streams` | `false`, `60s`, `4` | as `[power]`, for virtual media (a peer needs `media`) |
| `[secrets] max_bytes` | `65536` | the most bytes one secret reference may read; at least 1 |
| `[secrets] check_access` | `true` | a `file:` secret must be the node's account's and closed to the group and others; `false` reads any and logs `non-default:`; lowest layer only |
| `[secrets] group_readable` | `false` | accept a `file:` secret of mode `0640` owned by the node's account or root (Unix), logged `non-default:`; lowest layer only |
| `[secrets] allow_literal` | `false` | let a sensitive setting hold the secret itself instead of a reference, logged `non-default:`; lowest layer only |
| `[providers] paths` | empty | provider libraries mapped at start: files, or directories of `.so`/`.dll`/`.dylib`; only a library that declares a Bohío kind (`bohio_terminal`, `bohio_desktop`, `bohio_power`, `bohio_media`, `bohio_exec`, `bohio_files`) is mapped. Empty: nothing is read or loaded. A statically linked Linux build (x86_64, aarch64) maps them through the system's own `ld.so` and glibc, started inside the node (glibc and its dynamic linker must exist; one foreign C library per process, which takes signal 33); an empty path is refused |
| `[providers] dlopen` | `auto` | which loader maps a library: `auto` (the system's `ld.so` and glibc on a static Linux build, the platform's loader elsewhere), `system` (never the foreign one: a static build then loads nothing) or `foreign` (always it); not `auto` logs `non-default:` |
| `[providers] ld_so` | unset | the dynamic linker the foreign loader enters when it is not at the usual path (`/lib64/ld-linux-x86-64.so.2`, `/lib/ld-linux-aarch64.so.1`); mapped and run, so only the lowest layer sets it; the first start's value holds for the process |
| `[providers] allow_reserved_signal_handlers` | `false` | start the foreign loader though the node handles signal 32 or 33, which it takes; `true` logs `non-default:`; only the lowest layer sets it |
| `[[providers.program]]` | none | provider programs started as children over their standard input and output (guatiao's remote protocol; any language, any build): `command` (the program's file, then its arguments; no `PATH` lookup; required), `name` (default `program-N`), `env`, `cwd`, `restart_limit` (`5`; `0` never), `restart_window` (`1m`), `restart_delay` (`1s`), `restart_if_changed` (`false`: a restart re-runs the owner check and refuses a file whose SHA-256 changed), `start_timeout` (`10s`, not zero), `stop_grace` (`500ms`), `call_timeout` (`30s`; `0` waits), `max_calls` (`64`), `max_frame` (`67108864`). A provider it offers is one instance, so a `[source.<name>]` naming it takes `provider` alone. A program that exits is `failed` with how it ended and is started again at the next source lookup, within the limit |
| `[providers] isolate` | `false` | run every library of `paths` in a process of its own, through `library_host` (`HOST --stdio --load LIBRARY`, guatiao's remote protocol), instead of mapping it; `false` in every build, static ones included |
| `[providers] isolated` | empty | libraries (files, or directories) run in a process of their own whatever `isolate` says |
| `[providers] library_host` | `guatiao-remote-server` beside the node | the host program for an isolated library; no `PATH` lookup; its file is checked as a program is. A provider it serves is one instance (a source naming it takes `provider` alone) |
| `[providers] check_access` | `true` | refuse a library or a provider program, or the directory holding it, that another account owns or may write; `false` loads it and logs `non-default:`; only the lowest layer sets it |
| `[sessions] queue_frames` | `16` | frames of a program's output that may wait before the session takes them; at least 1 |
| `[shell] enabled` | `true` | whether shell sessions are opened |
| `[shell] max_sessions` | `8` | concurrent sessions across peers; at least 1 |
| `[shell] chunk_size` | `16384` | most terminal bytes in one frame; at least 1; a smaller message bound of the stream wins |
| `[shell] poll_interval` | `20ms` | how often a session looks at its program and its stream; not zero |
| `[shell] drain_quiet` | `200ms` | how long the terminal must stay quiet after the program exited before the session ends |
| `[shell] kill_wait` | `2s` | how long the host waits for the output reader after the tree was ended before it reports the exit regardless |

**Layers.** `Layers::load` reads each file alone (syntax, unknown keys, types: the error names the file, line, column and key) and then merges them with
`anycfg` over `toml::Value`: tables merge by key, arrays and scalars replace (an array is one setting, so a higher layer can narrow a root list or `[[listen]]`;
merging elements would only widen). Out-of-range values, checked on the merged value, are `ConfigError::Invalid` naming the key (`link.max_streams`, ...) and the
highest layer that sets it, as is a `[peer.<name>]` whose name is not a peer id. A key only the lowest layer may set (`[service] check_access`, `user_layer`, `trusted_owners`, `upper_layer_keys`; `[providers] check_access`, `ld_so`, `allow_reserved_signal_handlers`; `[secrets]`'s three checks; the thirteen `[gui]` helper keys of `LOWEST_ONLY` in `config/layers.rs`: `helper_accounts`, `helper_endpoint`, `node_account`, `source`, `source_retry` and the `helper_upload*` and `helper_socket_dir_check` ones) is ignored, and logged, in a higher layer unless the lowest layer's `[service] upper_layer_keys` names it as `section.key`. The command
finds the layers (the library reads no environment variable): `--config FILE` (repeatable, lowest first) or else `BOHIO_CONFIG` name them, alone; else the system
file (`%ProgramData%\bohio\bohio.cfg`, `/etc/bohio/bohio.cfg`) then the user's (`%APPDATA%\bohio\bohio.cfg`, `$XDG_CONFIG_HOME/bohio/bohio.cfg`, else
`~/.config/bohio/bohio.cfg`), each if it exists; none: the defaults. The `[peer.*]` tables are read again from the files at every question (`peer::LayeredPeers`);
every other section is read when the host starts. A default layer that does not exist when `serve` starts is read once made only when the nearest directory above it
that exists passes the access check below (so `%ProgramData%\bohio\bohio.cfg`, which every Windows user may create, is read only after a restart); every command
checks the system file when it exists, as `serve` checks every layer.

## Peer grants (`bohio::policy`)

```rust
#[non_exhaustive] pub enum Permission { Info, FsRead, FsWrite, Exec, Shell, Admin, ForwardConnect, GuiView, GuiInput, GuiClipboard, ForwardListen, Session, Power, Media }   // Ord; names "info","fs.read","fs.write","exec","shell","admin","forward.connect","gui.view","gui.input","gui.clipboard","forward.listen","session","power","media"
impl Permission { pub const fn name(self) -> &'static str; }
pub struct Grant;                                                // Default = nothing
impl Grant { pub fn none() -> Self; pub fn allowing(permissions) -> Self; pub fn with_fs_roots(self, roots) -> Self; pub fn with_programs(self, programs) -> Self; pub fn restricts_programs(&self) -> bool;
             pub fn allows(&self, Permission) -> bool; pub fn permissions(&self) -> impl Iterator<Item = Permission>; pub fn fs_roots(&self) -> &[PathBuf];
             pub fn permits_program(&self, program: &str) -> bool; pub fn permits_shell_script(&self) -> bool;
             pub fn with_env(self, names) -> Self; pub fn env_names(&self) -> &[String]; pub fn permits_env(&self, env: &bohio_proto::exec::Env) -> bool;
             pub fn with_sources(self, names) -> Self; pub fn with_default_sources(self, names) -> Self; pub fn sources(&self) -> Option<&[String]>; pub fn permits_source(&self, name: &str) -> bool;
             pub fn with_connect(self, rules) -> Self; pub fn forward(&self) -> &ForwardGrant; pub fn permits_connect(&self, target: &bohio_proto::forward::Target) -> bool;
             pub fn with_listen(self, rules) -> Self; pub fn permits_listen(&self, bind: &bohio_proto::forward::Bind) -> bool; }   // permits_connect: `forward.connect` and a `connect` rule that matches; permits_listen: `forward.listen` and a `listen` rule that matches
pub struct ForwardGrant;                                         // `[peer.<id>.forward]`: connect() and listen() -> &[String], the rule texts; an empty list allows nothing
pub struct Peers;                                                // Default = no peer; Deserialize (the `[peer]` table of `bohio.cfg`)
pub struct PeerTable;                                            // one `[peer.<id>]`: grant(), endpoint() -> Option<&str>, dial() -> &DialConfig, forward_local() and forward_remote() -> &[String]
impl Peers { pub fn grant_for(&self, peer: &str) -> Grant; pub fn table(&self, peer: &str) -> Option<&PeerTable>; pub fn dials(&self) -> impl Iterator<Item = (&str, &str, &DialConfig)>; pub fn has(&self, peer: &str) -> bool; pub fn ids(&self) -> impl Iterator<Item = &str>; pub fn all_roots(&self) -> Vec<PathBuf>; pub fn declared(&self) -> impl Iterator<Item = (&str, &PeerTable)>;
             pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool; }
```

`[peer."<id>"]` (the id is `sha256:` and 64 hex digits, so it is quoted as a key) with `allow = [..]`, `fs_roots = [..]`, `sources = [..]` (the sources beyond each capability's default the peer may use: `Grant::with_sources`, `with_default_sources`, `permits_source`; absent: the defaults alone), `[peer."<id>".exec]` with
`programs = [..]` and `env = [..]`, and `[peer."<id>".forward]` with `connect = [RULE..]`. A table, even an empty one, accepts the key. An unknown key or permission is `ConfigError::Parse`. A peer with no table, or
no `allow`, holds nothing. `programs` empty means any program and
explicit shell scripts; non-empty means only those programs, each named exactly as the peer asks for it, and no shell script
(a shell runs anything). Under a non-empty list each name is resolved on the host's own `PATH` (a name with a directory
must be absolute; a relative one is `not_found`) and that absolute path is started, never what the request's `PATH` or working
directory would find, and a request may set or remove only the variables `env` names (exactly; without case on Windows), `PATH` too only when `env`
names it (the program still starts from the host's own `PATH`), and may not clear the environment, else it is `denied`: a variable is as strong as an argument (`LD_PRELOAD`,
`DYLD_INSERT_LIBRARIES`, `BASH_ENV` load or run code the peer chose). `permits_env` is that check; without a program list it always holds. The list
limits which program starts, not what it does: `git -c alias.x=!cmd`, `make`, `find -exec` and interpreters run anything they are
given, so list only programs that are safe with every argument. `fs_roots` empty means no file operation is allowed.

`[peer."<id>"] endpoint = "<any endpoint>"` makes this node dial that peer (the table's name pins the key that must answer there); `bohio serve` and `bohio service run`
run a dialer for each, beside the listeners, read at start (a restart applies a new endpoint). `[peer."<id>".dial]` is `bohio::config::DialConfig` (Deserialize,
usable by an embedder; unknown keys refused):

| Key | Default | Meaning |
| --- | --- | --- |
| `ca` | none | PEM files trusted instead of the system roots, for `wss` and `webtransport` |
| `cert_hash` | none | certificate hashes (`sha256:` and 64 hex digits) trusted as they are |
| `fallback` | none | for a `webtransport://` endpoint, a `ws://` or `wss://` one tried when the QUIC path fails before anything crossed |
| `mtu_initial`, `mtu_min`, `mtu_discovery`, `mtu_upper_bound` | `1200`, `1200`, `true`, `1452` | for a `webtransport://` endpoint, as `[[listen]]`'s: the QUIC payload sizes and path-MTU discovery of this dial; one set for another endpoint is `ConfigError::Invalid` naming `peer.<id>.dial`, a size below 1200 names its key and value |
| `timeout` | `10s` | opening the path (the proof has `[link] proof_timeout` besides); not zero |
| `retry_min` | `1s` | the first wait after a failure; not zero |
| `retry_max` | `60s` | the longest wait; at least `retry_min` |
| `retry_factor` | `2` | what each further failure multiplies the wait by; at least 1 |
| `retry_jitter` | `0.1` | a wait moves by up to this fraction of itself, either way; 0 to 1 |

A shorter `retry_min` and a smaller factor retry faster and ask the peer, and a watching network, more often; a larger `retry_max` notices a peer that came back later. A
link that stayed up at least `retry_min` starts the sequence again, and one that ends sooner counts as a failure (the peer most likely refused the key after its proof).
A bad endpoint, a fallback that does not follow a `webtransport://` one, or a bad `cert_hash` is `ConfigError::Invalid`; a name in a `tcp://` endpoint is resolved at each attempt, never when the file is read.
