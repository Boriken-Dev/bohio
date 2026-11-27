# `bohio-client` single-file modules — API header

Part of the `bohio-client` API header (`../AGENTS.md`): the modules that are one file each.

## Keeping a link (`bohio_client::keep`, `connect::Cause`)

```rust
#[non_exhaustive] pub struct Backoff { pub min: Duration /*1 s*/, pub max: Duration /*60 s*/, pub factor: f64 /*2*/, pub jitter: f64 /*0.1*/ }   // Default, Copy; wait(failures, random) -> Duration: min x factor^failures up to max, moved by jitter x random (-1 to 1)
pub struct Jitter;   impl Jitter { pub fn new() -> Self; pub fn sample(&mut self) -> f64 }   // numbers from -1 to 1, seeded by the process's hasher
pub trait Links: Send + Sync { fn current(&self) -> Option<Arc<dyn Open>>; }   // the link to use now; implemented for `Fn() -> Option<Arc<dyn Open>>`
pub struct Relink;   impl Relink { pub fn new(links: Arc<dyn Links>, wait: Duration, poll: Duration) -> Self }   // an Open over Links: an open uses the link that is up and, with none (or one that ended as it opened), waits up to `wait` looking every `poll`, then Error::Link; is_up is always true; relink_wait is Some(wait)
pub struct Reconnecting;   impl Reconnecting { pub fn start(first: Arc<dyn Open>, make: Make, backoff: Backoff, poll: Duration, report: Box<dyn Fn(&Kept) + Send + Sync>) -> Self; pub fn current(&self) -> Option<Arc<dyn Open>> }   // Links; a thread makes the next link with `make` after each Backoff wait once the held one is not up; Drop stops it
#[non_exhaustive] pub enum Kept { Lost, Failed { reason: String, retry_in: Duration }, Restored }   // what `report` hears, on the thread
pub type Make = Box<dyn Fn() -> Result<Arc<dyn Open>, String> + Send + Sync>;
pub struct connect::Cause;   // Clone, Default: Cause::unknown(); end() -> Option<StreamEnd>; over_a_limit() -> bool (the host refused the open with the text `connect::OVER_A_LIMIT`, "over a limit", which cabuya writes for max_pending_opens, max_streams, max_per_protocol and max_opens_per_minute); Opened::cause() gives one
```

A listener of `-L` over a `Relink` stays up while the link is away, and a connection that arrives then waits for it; `RemoteForward` and a `listen` over a `Reconnecting` are asked for again by whoever holds them (`bohio::forward::keep_remote`).

## Execution (`bohio_client::exec`)

```rust
pub struct Exec;                                                  // Debug; every method takes &self; Drop closes the stream
impl Exec {
    pub fn start(link: &dyn Open, request: &bohio_proto::exec::Request) -> Result<Self, Error>;
    pub fn next(&self, timeout: Option<Duration>) -> Result<Option<Event>, Error>;    // Ok(None): the host closed in order
    pub fn write_stdin(&self, bytes: &[u8]) -> Result<(), Error>;                     // split to the channel's message size, waits for room
    pub fn finish_stdin(&self) -> Result<(), Error>;
    pub fn kill(&self) -> Result<(), Error>;
    pub fn signal(&self, signal: Signal) -> Result<(), Error>;      // the host answers with Event::Signalled (delivered or not)
    pub fn session(&self) -> Option<String>;                          // the id, once `started` said the request made a session, or when attached
    pub fn attach(link: &dyn Open, session: &str, from: Option<u64>) -> Result<(Self, Attached), Error>;   // `bohio-attach/1`; Error::Protocol for a shell session
    pub fn wait(&self, timeout: Option<Duration>) -> Result<Completed, Error>;
    pub fn close(&self);
}
#[non_exhaustive] pub enum Event { Started { pid: u32 }, Stdout(Vec<u8>), Stderr(Vec<u8>), Signalled(Signalled), Exit(Exit) }
pub struct Completed { pub pid: Option<u32>, pub stdout: Vec<u8>, pub stderr: Vec<u8>, pub exit: Exit }
```

`next` skips a frame whose tag it does not know, returns the host's error frame as `Error::Remote`, and `Error::Timeout` when
nothing arrived in `timeout` (the stream goes on). `wait` collects output until the exit; `Error::Closed` when the stream ends
first, `Error::Timeout` when `timeout` passes (the process keeps running). `write_stdin` and `finish_stdin` only mean something
for a request with `Stdin::Open`. Dropping an `Exec` before the exit closes the stream, and the host ends the process tree.

## Shells (`bohio_client::shell`)

```rust
pub struct Shell;                                                 // Debug; every method takes &self; Drop closes the stream: the host ends the tree
impl Shell {
    pub fn start(link: &dyn Open, request: &bohio_proto::shell::Request, timeout: Option<Duration>) -> Result<Self, Error>;   // waits for `started`
    pub const fn pid(&self) -> u32;                               // the host's process id of the shell; 0 for a stream that attached
    pub fn session(&self) -> Option<&str>;                        // the id, when the request made a session or the stream attached to one
    pub fn attach(link: &dyn Open, session: &str, from: Option<u64>) -> Result<(Self, Attached), Error>;   // `bohio-attach/1`; Error::Protocol for an exec session
    pub fn signal(&self, signal: Signal) -> Result<(), Error>;    // the host answers with Event::Signalled
    pub fn next(&self, timeout: Option<Duration>) -> Result<Option<Event>, Error>;          // Ok(None): the host closed in order
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error>;       // typed input, split to the channel's message size; Enter is \r
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error>;
    pub fn kill(&self) -> Result<(), Error>;
    pub fn read_until(&self, needle: &[u8], timeout: Option<Duration>) -> Result<Vec<u8>, Error>;   // everything read, up to and past the match
    pub fn wait(&self, timeout: Option<Duration>) -> Result<(Vec<u8>, bohio_proto::shell::Exit), Error>;
    pub fn close(&self);
}
#[non_exhaustive] pub enum Event { Data(Vec<u8>), Signalled(Signalled), Exit(Exit) }
```

A refusal (`denied`, `not_found`, `busy`, `bad_request`, ...) comes from `Shell::start` as `Error::Remote`. `read_until` is
`Error::Closed` when the shell ends before the text appears; the typed text is echoed by the terminal, so match something the
echo does not hold. Output is a terminal's rendering with escape sequences, not the program's bytes.

## Power and media (`bohio_client::power`, `bohio_client::media`)

```rust
power::act(link: &dyn Open, source: Option<&str>, action: Action, timeout: Option<Duration>) -> Result<State, Error>;  power::status(link, source, timeout)
media::call(link, &Request, timeout) -> Result<Vec<Slot>, Error>;  media::{status, insert(link, source, slot, Image, timeout), eject(link, source, slot, timeout)}
shell::Shell::start(link, &Request::console(Some("name")), timeout)   // the node's console of a terminal source; Shell::pid() is 0 for it
```

The agent tools `power_status` and `media_status` (needs `power`/`media` and a source in `info.access`) only read.

## Sessions (`bohio_client::session`)

```rust
#[non_exhaustive] pub enum Attachment { Exec(Exec), Shell(Shell) }
pub fn attach(link: &dyn Open, session: &str, from: Option<u64>) -> Result<(Attachment, Attached), Error>;   // opens `bohio-attach/1` for a session of either kind
```

A session starts with a request whose `session` is set (`Exec::start`, `Shell::start`; the id is `Exec::session()` or `Shell::session()`) and needs the peer's `session` permission. `Attached { kind, from, lost }`: `from` is where the output starts (the offset asked for, or the oldest the host still keeps), `lost` the bytes between. Dropping the returned stream detaches: the program runs on.

## Parallel ranges (`bohio_client::parallel`)

```rust
pub fn get_parallel(links: &[&dyn Open], remote: &str, local: &Path, &ParallelOptions) -> Result<Moved, Error>;   // stat, then `streams` workers each take the next range (ReadRequest offset/length) and write it at its offset into a temporary sibling `.NAME.bohio-PID-N.part`; the whole file's digests (`hashes`) are asked of the host at the same time and compared with the part's; then renamed over `local` (keeping its permissions). Any failure removes the part and leaves `local`
pub fn put_parallel(links: &[&dyn Open], local: &Path, remote: &str, &ParallelOptions) -> Result<Moved, Error>;   // stats the target (its mode is kept), `create`s `.NAME.bohio-part-<16 hex>` beside it (the host's own path text: the last `/` or `\` ends the directory; `parents` is not made), `at` streams per range, `bohio-hash/1` of the staged file against the local digest, `move` with overwrite; a failure removes the staged file
pub fn read_parallel(links: &[&dyn Open], remote: &str, out: &mut dyn Write, &ParallelOptions) -> Result<u64, Error>;   // ranges are handed to `out` in order; at most 2 x streams ranges are fetched ahead (memory = that many ranges); the digest check comes after the last byte
pub struct ParallelOptions { pub streams: usize /*4*/, pub range: Option<u64> /*None: one per stream (get, put); DEFAULT_READ_RANGE (8 MiB) for read*/, pub compression: Option<Compression> /*each range its own stream; a write names the algorithm*/, pub caps: Caps, pub hashes: Vec<Algorithm> /*[Sha256]; empty skips the check and the host's second read*/, pub timeout: Option<Duration> /*5 min*/, pub chunk: usize /*64 KiB*/, pub read_size: usize /*1 MiB*/ }   // Default
pub const DEFAULT_READ_RANGE: u64 = 8 << 20;
```

`links` may hold one `Open` (the streams share its connection and its congestion window: `n` streams carry `n` receive windows) or several `Connection`s to the same host (stream `i` uses link `i % links.len()`; each link proves itself, about two round trips). The first link carries the stat, the digests, the `create` and the `move`. Moves the whole file: no delta, no comparison. The host's `[hash] max_streams` limits the digest streams and `max_pending_opens` / `max_opens_per_minute` the opens (one open per range).

## Control stream (`bohio_client::control`)

```rust
pub struct Control;                                               // Debug; every method takes &self; the host answers in the order calls arrive; Drop closes the stream
impl Control {
    pub fn open(link: &dyn Open) -> Result<Self, Error>;          // `bohio/1`, no metadata
    pub fn call(&self, call: bohio_proto::control::Call, timeout: Option<Duration>) -> Result<Reply, Error>;   // submit + collect; a Timeout abandons the call (its late answer is dropped)
    pub fn submit(&self, call: Call, timeout: Option<Duration>) -> Result<u64, Error>;     // sends, returns the id; the timeout is for room to send. Many may be outstanding; a thread that submits more than the stream's window carries must collect meanwhile
    pub fn collect(&self, id: u64, timeout: Option<Duration>) -> Result<Reply, Error>;     // that call's answer; others that arrive are kept for their own collect; Timeout leaves the call outstanding; an id with nothing outstanding is Protocol
    pub fn abandon(&self, id: u64);                               // stop waiting: the answer is dropped when it comes
    pub fn call_many(&self, calls: Vec<Call>, timeout: Option<Duration>) -> Vec<Result<Reply, Error>>;   // all sent (on one thread) while the answers are read (on the caller's), in order, each its own result; `timeout` is per wait, so a call the host does not answer reports Timeout and the ones the host holds behind it do too, and the stream goes on
    pub fn is_closed(&self) -> bool;
    pub fn hello(&self, client: &str, timeout: Option<Duration>) -> Result<u32, Error>;    // the version the host will speak
    pub fn info(&self, timeout: Option<Duration>) -> Result<Info, Error>;
    pub fn sessions(&self, timeout: Option<Duration>) -> Result<Vec<bohio_proto::session::Listed>, Error>;   // the detached sessions this peer holds (an admin: all, with `peer`)
    pub fn end_session(&self, id: &str, signal: Option<Signal>, timeout: Option<Duration>) -> Result<(), Error>;   // `kill` when None; `not_found` for another peer's id
    pub fn close(&self);
}
```

`call` returns the host's `{code, message}` as `Error::Remote`, `Error::Timeout` when no answer came in time (a late answer is
skipped by the next call), `Error::Closed` when the host closed the stream.

## Gui (`bohio_client::gui`)

```rust
pub struct Gui;                                                    // Debug; Drop closes. Every method takes &self; calls take turns
impl Gui {
    pub fn open(link: &dyn Open, request: &GuiRequest) -> Result<Gui, Error>;   // waits up to OPEN_WAIT (120 s: the host may wait for its owner); `open_within(link, request, wait: Option<Duration>)` takes the wait (the command's `[client] gui_timeout`); Error::Remote for a refusal the engine made, Transport(Permission) when the link's grant does not cover the request
    pub fn granted(&self) -> &[Want];   pub fn session(&self) -> &str;   pub fn state(&self) -> State;   // state: the last the host said (`state` frames are kept as they pass)
    pub fn snapshot(&self, options: &SnapshotOptions, timeout: Option<Duration>) -> Result<Image, Error>;   // Image { header: ImageHeader, bytes }
    pub fn input(&self, events: &[InputEvent], timeout: Option<Duration>) -> Result<u32, Error>;            // applied; `denied`, `busy`, `unavailable`, `bad_request` are Error::Remote and the stream goes on
    pub fn settle(&self, request: &Settle, timeout: Option<Duration>) -> Result<SettleResult, Error>;
    pub fn clipboard(&self, timeout: Option<Duration>) -> Result<String, Error>;   pub fn set_clipboard(&self, text: &str, timeout: Option<Duration>) -> Result<(), Error>;
    pub fn watch(&self, options: &WatchOptions) -> Result<(), Error>;   pub fn unwatch(&self) -> Result<(), Error>;   // a watch replaces the settings of a running one; the host refusal is an Error::Remote from the next `next_event`
    pub fn next_event(&self, timeout: Option<Duration>) -> Result<WatchEvent, Error>;   // WatchEvent::{Update(UpdateInfo { header, bytes, acknowledged }), Cursor(CursorFrame)}: an update is applied to `screen()` (and acknowledged, unless `set_auto_ack(false)`) before it is returned; Error::Timeout when nothing came (call again), Error::Screen for an update the limits or the decoders refuse
    pub fn screen(&self) -> Option<Screen>;   // Screen { width, height, rgb }: the frame the updates built (feature `screen`, on with the host crate `gui`; None without it)
    pub fn ack(&self, frame: u64) -> Result<(), Error>;   pub fn set_auto_ack(&self, on: bool);   pub fn updates(&self) -> u64;
    pub fn set_screen_limits(&self, max_rect_bytes: u64, max_frame_pixels: u64);   // feature `screen`; defaults 32 MiB each (`[gui] max_rect_bytes`, `max_snapshot_pixels`)
    pub fn set_image_limit(&self, max_bytes: u64);   // a snapshot whose announced length is over it is Error::Screen at the header; default 32 MiB (`[gui] max_rect_bytes`); `bohio remote gui` sets it from that key
    pub fn close(&self);
}
pub struct SnapshotOptions { display, region, format, quality, max_width, max_height, cursor, settle }   // Default
pub struct WatchOptions { display, region, fps, encoding: Option<WatchEncoding>, quality, max_width, max_height, max_rate, cursor, refine, key }   // Default
```

`Error::Screen(String)` is an update that cannot be used (a rectangle outside the frame, bytes that do not decode, a size past the limits); the text never carries pixels. A request made during a watch (`snapshot`, `input`...) reads, applies and acknowledges the updates that arrive meanwhile.

## Profiles (`bohio_client::profile`)

```rust
#[non_exhaustive] pub enum Profile { Default, Lan, WeakHost, SlowLink, FatLink }   // Copy, Default (= Default), Eq, Hash
impl Profile { pub const ALL: [Self; 5]; pub const fn name(self) -> &'static str /*"default", "lan", "weak-host", "slow-link", "fat-link"*/;
    pub fn parse(name: &str) -> Option<Self>; pub fn names() -> String /*comma-separated*/; pub fn defaults(self) -> ClientDefaults; }
#[non_exhaustive] pub struct ClientDefaults { pub compression: Option<Compression>, pub delta: sync::DeltaMode, pub compare: Option<sync::Compare>, pub parallel: usize /*>= 1*/,
    pub bulk_window: Option<u32>, pub prepare_min: Option<u64> }   // Default: nothing compressed, Auto, no comparison, 1, None, None
```

What a client asks for where the caller names nothing; never a limit. `lan`: two streams, a 4 MiB bulk window. `weak-host`: lz4, `Compare::Hash(Xxh128)`. `slow-link`: zstd level 19 with `window_log` 27, `Compare::Hash(Blake3)`, four streams, a 4 MiB bulk window, `prepare_min` 16 MiB (a compressed read of a file at least that large is prepared in full first). `fat-link`: an 8 MiB bulk window. `bulk_window` is for `link::Settings::bulk_window` (the receive window this side grants a bulk stream it receives); the caller applies each field where it chooses.

## Sync (`bohio_client::sync`)

A local file and a file of a host, compared and moved without sending what the other side has (rsync's algorithm; `docs/protocol.md` "Deltas").

```rust
pub enum Compare { Meta, Hash(Algorithm), Blocks }          // size and time; size then a digest of each side; size then the signatures
pub enum Verdict { Same, Differs, RemoteMissing, LocalMissing }
pub fn compare_file(link: &dyn Open, local: &Path, remote: &str, how: Compare, options: &SyncOptions) -> Result<Verdict, Error>;   // differing sizes differ at every depth; Meta compares times to the nanosecond; `options` gives `timeout`, `read_size` and `strong`
pub enum DeltaMode { Auto /* default */, Always, Never }    // Auto: a delta when the destination's copy is at least the host's [delta] min_size
pub struct SyncOptions { delta, compression: Option<Compression>, compare: Option<Compare> /* skip a file that is the same */, caps: Caps, block_size: Option<u32>,
                         strong: Algorithm /* blake3 */, strong_len: Option<usize>, verify: Option<Algorithm> /* blake3, else sha256 */, chunk: usize /* 64 KiB */,
                         timeout: Option<Duration> /* 5 min */, read_size: usize /* 1 MiB: local reads and the downloaded file's buffer */, transfer: Option<TransferInfo> /* the host's, when the caller has it */, window_log_max: Option<u32>, max_expansion: Option<u32> /* 1024 */ }   // Default; PutOptions and GetOptions are this type
pub struct Moved { size, literal_bytes, copied_bytes, wire_bytes: u64, delta: bool, skipped: bool }   // wire_bytes: frames in both directions, signature included, as they travelled
pub fn put(link: &dyn Open, local: &Path, remote: &str, options: &PutOptions) -> Result<Moved, Error>;   // replaces the host's file atomically; a delta needs fs.read as well as fs.write; `changed` when the host's copy moved meanwhile
pub fn get(link: &dyn Open, remote: &str, local: &Path, options: &GetOptions) -> Result<Moved, Error>;   // rebuilds into a temporary sibling, checks the host's digest (Error::Mismatch), renames
```

What the host offers is read from its `hello` reply (`Control::transfer`), which needs no `info` permission; a host that serves no deltas gets a whole transfer. A delta `put` asks the host for the target's signature (`hash_blocks`), hashes the local file for
`expect`, and sends `copy` and literal frames (`Writer::copy`, `Writer::write`); a delta `get` streams the signature of the local file while it reads it and
rebuilds as the host's copies and data arrive. `Error::Local` is a local file failure (no path in the text); `Error::Mismatch` a rebuilt file that does not match.

## Trees and bundles (`bohio_client::walk`, `bundle`, `range`, `sync::sync_tree`)

```rust
pub fn walk(link: &dyn Open, roots: &[&str], options: &WalkOptions) -> Result<WalkIter, Error>;   // walk_with(.., timeout) too; roots are sent while entries come back; one stream
#[derive(Default)] pub struct WalkOptions { pub depth: Option<u64>, pub follow: bool, pub include: Vec<String>, pub exclude: Vec<String>, pub hashes: Vec<Algorithm>, pub compression: Option<Compression>, pub window_log_max: Option<u32> /* 27 */, pub max_expansion: Option<u32> /* 1024; 0 off */ }
pub enum WalkItem { Entry { root: u64, entry: Box<WalkEntry> }, Failure(WalkItemError) }
pub struct WalkIter;   // Iterator<Item = Result<WalkItem, Error>>; done() -> Option<WalkDone>; wire_bytes(); compression(); dropping it closes the stream
pub fn read_bundle(link, items: &[ReadItem], &BundleOptions, on: &mut dyn FnMut(Fetch<'_>) -> Result<(), Error>) -> Result<BundleStats, Error>;   // Fetch::{Begin(&ReadHeader), Data(&[u8]), End { index, hashes }, Failed { index, error }}: a Failed after a Begin discards what came
pub fn get_many(link, items: &[ReadItem], &BundleOptions) -> Result<(Vec<Result<Fetched, ErrorBody>>, BundleStats), Error>;   // Fetched { header, data, hashes }; all in memory
pub fn put_many(link, files: &mut [BundleFile<'_>], &BundleOptions) -> Result<Put, Error>;   // BundleFile { item: WriteItem, source: &mut (dyn Read + Send) } (BundleFile::replace(path, source)); Put { results: Vec<Result<WriteDone, ErrorBody>>, stats }
pub struct BundleOptions { pub compression: Option<Compression>, pub hashes: Option<Vec<Algorithm>>, pub caps: Caps, pub chunk: usize /*64 KiB*/, pub window_log_max: Option<u32>, pub max_expansion: Option<u32> /* 1024 */, pub shared_context: bool /* false: each file of a write starts a compression context */, pub timeout: Option<Duration> /*5 min*/ }   // Default
pub struct BundleStats { pub items: u64, pub wire_bytes: u64 }
pub struct RangeReader<'a>;   // RangeReader::open(link, path, &RangeOptions) -> Read + Seek; len(), position(); each range is one read stream of `range` bytes with `prefetch` more open ahead; a seek inside the range held or to the start of one open is free, any other closes what is ahead
pub struct RangeOptions { pub range: u64 /*1 MiB*/, pub prefetch: usize /*2*/, pub compression: Option<Compression>, pub timeout: Option<Duration> }   // Default
pub fn sync::sync_tree(link, local: &Path, remote: &str, &TreeOptions) -> Result<TreeReport, Error>;   // walks both sides (the host's in one stream), compares, moves files up to bundle_max in bundles of bundle_items and larger ones by delta, `parallel` streams at once; delete, dry_run; links are skipped; a path the host lists that is not relative, holds `..`, an empty part or a name the local system cannot have, or leads out of the local folder through a link, ends the sync with `Error::Local` before anything is touched (no option)
pub struct TreeOptions { pub direction: Direction /*Push*/, pub compare: Compare /*Meta*/, pub delete: bool, pub dry_run: bool, pub bundle_max: u64 /*1 MiB*/, pub bundle_items: usize /*1000*/, pub parallel: usize /*4*/, pub modify_window: Duration /*1 us*/, pub sync: SyncOptions, pub bundle: BundleOptions }
pub struct TreeReport { pub actions: Vec<Action { step: Step, path, size }>, unchanged, bundled, bundles, singles, deltas, deleted, skipped, bytes, wire_bytes, walk_entries, walk_wire_bytes: u64, pub failures: Vec<(String, String)> }   // is_clean(); Step::{MakeDirectory, Create, Update, Delete}
// Fs gains stat_many(&[&str], follow) -> Vec<Result<Metadata, Error>> and list_many(&[&str]) -> Vec<Result<Vec<Entry>, Error>> (calls pipelined: about one round trip for the batch; list_many asks each directory's next page together with the others', never one directory's pages ahead of each other), and Connection::fs() -> Result<Arc<Fs>, Error> (one control stream per connection, opened on first use and opened again when it has ended; calls wait without limit).
// Fs gains move_to(from, to, overwrite), truncate(path, length), copy_with(from, to, &CopyOptions { overwrite, recursive, preserve }), spawn_control(link) -> Fs (a second control stream: a copy or move of a tree holds its stream).
// Writer gains token() (a resumable write's), resumed_at() (a resumed write's offset) and flush_remote(timeout) -> Result<u64, Error> (append and at: the bytes the host has written, made safe).
```

A pushed file takes the local modification time (a bundle item's `modified`, `set_times` after a single write), a pulled one the host's, so the next sync by `Compare::Meta` finds it unchanged; `Compare::Hash(alg)` has the host hash during the walk and this side read its files; `Compare::Blocks` compares file by file. A push does not set permission bits (a Windows host has none); a pull keeps them on Unix.

## Hash (`bohio_client::hash`)

```rust
pub fn hash_file(link: &dyn Open, path: &str, algorithms: &[Algorithm]) -> Result<Digests, Error>;
pub fn hash_range(link: &dyn Open, item: HashItem, algorithms: &[Algorithm]) -> Result<Digests, Error>;
pub fn hash_many(link: &dyn Open, items: &[HashItem], algorithms: &[Algorithm]) -> Result<Vec<Result<Digests, ErrorBody>>, Error>;   // one stream; a file that fails fails alone
pub fn hash_items(link: &dyn Open, request: &HashRequest, items: &[HashItem]) -> Result<Vec<Result<HashResult, ErrorBody>>, Error>;   // keeps size, modified, unchanged
```

```rust
pub struct Blocks { pub result: HashResult, pub signature: Signature, pub wire_bytes: u64 }   // a file's signature as the host made it, and what it cost on the wire
pub fn hash_blocks(link: &dyn Open, item: HashItem, algorithms: &[Algorithm], options: &BlockOptions) -> Result<Blocks, Error>;   // the strong hash is algorithms[0] cut to options.strong_len; the host chooses the block size when options.size is None
pub fn hash_blocks_within(link: &dyn Open, item: HashItem, algorithms: &[Algorithm], options: &BlockOptions, max_signature_bytes: u64) -> Result<Blocks, Error>;   // hash_blocks takes fs::DEFAULT_MAX_SIGNATURE_BYTES (64 MiB); past the bound Error::Local; 0 takes any
```

One `bohio-hash/1` stream each. Items are sent on one scoped thread and results read on another, so a long batch cannot stop on a full stream. A refusal of
the stream (`unsupported`, `busy`, `denied`) is `Error::Remote`; for `hash_file` and `hash_range` a failed item is too. Calls wait without limit.

## Files (`bohio_client::fs`)

```rust
pub struct Fs;                                                    // Debug; one control stream; calls wait without limit until with_timeout
impl Fs {
    pub fn connect(link: &dyn Open) -> Result<Self, Error>;       pub fn with_timeout(self, Option<Duration>) -> Self;     pub fn control(&self) -> &Control;
    pub fn stat(&self, path: &str, follow: bool) -> Result<Metadata, Error>;
    pub fn list(&self, path: &str, after: Option<&str>, limit: Option<u64>) -> Result<Listing, Error>;     // one page
    pub fn list_all(&self, path: &str) -> Result<Vec<Entry>, Error>;                                       // every page
    pub fn mkdir(&self, path: &str, parents: bool) -> Result<(), Error>;      pub fn remove(&self, path: &str, recursive: bool) -> Result<(), Error>;
    pub fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error>;  pub fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<(), Error>;
    pub fn symlink(&self, target: &str, link: &str) -> Result<(), Error>;     pub fn readlink(&self, path: &str) -> Result<String, Error>;
    pub fn set_times(&self, path: &str, accessed: Option<Timestamp>, modified: Option<Timestamp>) -> Result<(), Error>;
    pub fn set_mode(&self, path: &str, mode: u32) -> Result<(), Error>;       pub fn set_readonly(&self, path: &str, readonly: bool) -> Result<(), Error>;
}
pub struct Reader;                                                // Debug; Drop closes the stream: the host stops reading
impl Reader {
    pub fn start(link: &dyn Open, request: &ReadRequest, timeout: Option<Duration>) -> Result<Self, Error>;   // waits for the host to open the file
    pub fn start_with(link: &dyn Open, request: &ReadRequest, timeout: Option<Duration>, options: ReadOptions) -> Result<Self, Error>;   // progress frames restart `timeout`
    pub fn header(&self) -> ReadHeader;                           // size, how many bytes will arrive (decompressed), what the host used
    pub const fn compression(&self) -> Option<&Compression>;      // what the host compresses with (the level it clamped to); None: raw
    pub fn wire_bytes(&self) -> u64;                              // the data bytes that travelled so far, compressed where they were
    pub fn digests(&self) -> Option<Digests>;                     // the request's `hashes` of what was sent; Some once the last chunk was read
    pub fn next_chunk(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>, Error>;                  // Ok(None): all arrived
    pub fn copy_to(&self, out: &mut impl Write, timeout: Option<Duration>) -> Result<u64, Error>;           // one chunk in memory
    pub fn read_to_end(&self, timeout: Option<Duration>) -> Result<Vec<u8>, Error>;     pub fn close(&self);
}
pub struct Writer;                                                // Debug; Drop before finish closes the stream: the host abandons the write
impl Writer {
    pub const DEFAULT_CHUNK: usize = 64 * 1024;
    pub fn start(link: &dyn Open, request: &WriteRequest, timeout: Option<Duration>) -> Result<Self, Error>;  // waits for `ready`; compresses when request.compression names an algorithm
    pub fn start_with(link: &dyn Open, request: &WriteRequest, timeout: Option<Duration>, caps: &Caps) -> Result<Self, Error>;   // levels and window this side may use
    pub fn write_compressed(&self, bytes: &[u8]) -> Result<(), Error>;   // a piece of a stream compressed elsewhere (prepared), cut anywhere
    pub fn with_chunk(self, bytes: usize) -> Self;                // most file bytes in one frame; keep it below what the link queues
    pub fn write(&self, bytes: &[u8]) -> Result<(), Error>;       // waits for room
    pub fn finish(&self, timeout: Option<Duration>) -> Result<WriteDone, Error>;   // sends `end`, waits for the host to put the data in place
    pub fn abort(&self);
}
```

```rust
pub struct ReadOptions { pub window_log_max: Option<u32> /* 27 */, pub max_expansion: Option<u32> /* fs::DEFAULT_MAX_EXPANSION, 1024: the decoded bytes may not pass that many times the compressed ones received once past 1 MiB (Error::Compression); Some(0) off */, pub raw: bool /* hand out the compressed stream undecoded */, pub on_progress: Option<Box<dyn FnMut(u64) + Send>> }   // Default
pub struct PutOptions { pub compression: Option<Compression>, pub prepare: bool, pub caps: Caps, pub temp_dir: Option<PathBuf>, pub chunk: Option<usize> }   // Default
pub fn put(link: &dyn Open, request: &WriteRequest, source: &mut dyn Read, options: &PutOptions, timeout: Option<Duration>) -> Result<WriteDone, Error>;   // prepare: one context into a temp file, then sent
pub fn resolve_auto(info: &Info, wanted: &Compression) -> Option<Compression>;        // `auto` -> the host's compression_default (a client names what it sends)
```

`Reader` decodes `compressed` frames as they arrive (cargo feature `zstd`, on in `bohio`, decodes zstd; without it a zstd header is `Error::Compression`) and stops with
`Error::Protocol` when the output would pass `header.length` and with `Error::Compression` when it grows past `max_expansion` times the compressed bytes received; a read that `resume`s, or `raw: true`, hands out the compressed bytes as they travelled. `Writer`
compresses with the same per-frame raw fallback (a raw frame ends the context). `Exec::start_with(link, request, caps)` does the same for standard input and
decodes both outputs, each held to `Exec::max_expansion(times)` (default 1024; 0 off); `Exec::compression()` is what `started` reported. `Error::Compression(text)`: an algorithm this build lacks, or `auto` where the client names one.

A refusal (`denied`, `outside_root`, `not_found`, `exists`, `busy`, ...) comes from `Reader::start` and `Writer::start` as
`Error::Remote`, before any data moves. `next_chunk` ends with `Error::Closed` when the stream finishes with a different number of
bytes than the header announced. `finish` is the only proof a write is in place: `Error::Remote` there means nothing was put in
place for `replace` and `create`.

## Endpoints and dialing (`bohio_client::dial`)

```rust
pub use coqui_transport_web::webtransport::{MIN_MTU, Mtu};   // Mtu { initial, min, discovery, upper_bound } (non_exhaustive, Copy, Default = quinn's), MIN_MTU = 1200
#[non_exhaustive] pub enum Endpoint { Tcp(SocketAddr), Pipe(PipeName), WebSocket(Url), WebTransport { url: Url, fallback: Option<Url>, mtu: Mtu }, Serial { path: String, options: SerialOptions }, Command(Vec<String>) }   // Clone, Eq, Display; WebSocket is `ws` or `wss`
impl Endpoint {
    pub fn parse(text: &str) -> Result<Endpoint, String>;     // the reason, for an unknown scheme or a bad address; `webtransport://` needs a UDP port
    pub fn dial(&self, timeout: Duration, settings: &Settings) -> Result<Box<dyn Channel>, Error>;   // an unproven channel; Error::Transport(why); dial_with and the default trust
    pub fn dial_with(&self, timeout: Duration, settings: &Settings, trust: &ServerTrust) -> Result<Box<dyn Channel>, Error>;   // a refused certificate is Transport(Failure::Certificate); Error::Link when the trust cannot be set up (an unreadable CA file)
    pub fn with_fallback(self, fallback: Endpoint) -> Result<Endpoint, String>;   // a `ws://` or `wss://` endpoint, on any host, after a `webtransport://` one; the caller naming it is the choice (a plain one is readable until the proof, one elsewhere sends the proof there)
    pub fn with_mtu(self, mtu: Mtu) -> Result<Endpoint, String>;   // the UDP payload sizes and path-MTU discovery of a `webtransport://` endpoint's QUIC connection, default quinn's (1200, 1200, discovery on, up to 1452); Err for another endpoint or an `initial` or `min` below 1200, naming the value. A tunnel with a smaller MTU loses each probe above it (a send error in the host's log): lower `upper_bound` to its size or turn `discovery` off
}
#[non_exhaustive] pub struct ServerTrust { pub ca_files: Vec<PathBuf>, pub pins: Vec<PeerId>, pub clock_skew: Duration /* default zero */ }   // Default, Clone, Eq; who a TLS client accepts as the host's certificate; a pinned or chained certificate is held to its dates, `clock_skew` is how far from this clock a start or end is still accepted (a certificate made just now by a host whose clock is ahead is refused until this clock reaches its start)
impl ServerTrust { pub fn pin(&mut self, text: &str) -> Result<(), String>; pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, String>; pub fn refusal(&self) -> Option<String>; }   // pin: `sha256:` and 64 hex digits; refusal: why the last certificate a config from this trust refused was refused (the presented certificate's hash, and for its dates the start or end and how far this clock is from it), for the message of a `Failure::Certificate`
#[non_exhaustive] pub struct SerialOptions { pub baud: Option<u32>, pub data_bits: Option<DataBits>, pub parity: Option<Parity>, pub stop_bits: Option<StopBits>, pub flow_control: Option<FlowControl> }   // Default, Copy, Eq
impl SerialOptions { pub fn set(&mut self, key: &str, value: &str) -> Result<(), String>; pub fn or(self, fallback: Self) -> Self; }
pub fn serial(path: &str, options: &SerialOptions, settings: &Settings) -> io::Result<StreamChannel>;   // opens the port: COBS + CRC-32C frames
pub fn command(argv: &[String], settings: &Settings) -> io::Result<StreamChannel>;                       // starts the program; stderr is this process's; its tree ends on close
pub fn web_limits(settings: &Settings) -> coqui_transport_web::Limits;                                    // one proof-sized message, read ahead one at a time, until the proof
pub fn split_command(line: &str) -> Result<Vec<String>, String>;                                         // spaces separate, quotes group, nothing escapes
```

| Endpoint | Transport |
| --- | --- |
| `tcp://host:port` | a socket; a name is resolved and its first address taken; a socket that connected to itself is refused |
| `pipe://NAME`, `pipe:///path/to.sock`, `pipe://\\.\pipe\NAME` | a Windows named pipe, a Unix domain socket (`NAME` in `$XDG_RUNTIME_DIR`, else `$TMPDIR`, else the first directory of `$RUNTIME_DIRECTORY`); an endpoint never names another host's pipe |
| `ws://host:port[/path]` | a WebSocket, binary messages, no TLS (plain beyond loopback too: the proof seals what follows) |
| `wss://host:port[/path]` | the same over TLS, the host's certificate checked by the `ServerTrust`: the system's roots by default (`rustls-platform-verifier`), `ca_files` instead of them, `pins` accepted as they are whatever their name and issuer, either when both are set |
| `webtransport://host:port[/path]` | WebTransport over QUIC, always TLS, the same trust; a hash pin is the certificate's SHA-256 whatever its key and validity (a browser's pin is stricter: ECDSA P-256, at most 14 days). With a fallback: a failure of the path (not a certificate, permission or protocol failure) before anything crossed dials the WebSocket endpoint, plain or on another host when it was named so |
| `serial://PORT[?baud=N&data_bits=N&parity=P&stop_bits=N&flow_control=F]` | `PORT` is a device path or `COM3`; `parity` is `none`, `odd`, `even`; `flow_control` is `none` or `hardware` |
| `stdio://PROGRAM ARG...` | the program is run directly with its standard streams as the channel |

Every channel `dial` returns holds the peer to one message of `cabuya::auth::MAX_PROOF_MESSAGE` until the proof raises the bound to
`Settings::record_length` (`max_frame + 5` plus the seal), so a carrier's own message bound must allow that: a stream channel, a WebSocket and
a COBS channel all follow the proof's `set_max_message`. A serial line has no end of stream: closing it sends nothing, so the host learns the
link ended from the mux's close frames, and a first message sent to a host that is reopening its port is lost (try again).

## The link (`bohio_client::connect`, `link`, `relay`)

```rust
pub struct Connection;                                            // Debug; Open; Drop ends the link and every stream
impl Connection {
    pub fn connect(endpoint: &Endpoint, timeout: Duration, proof: &cabuya::auth::Proof, settings: &Settings) -> Result<Self, Error>;   // any transport
    pub fn connect_with(endpoint: &Endpoint, timeout: Duration, proof: &cabuya::auth::Proof, settings: &Settings, trust: &ServerTrust) -> Result<Self, Error>;   // connect() with a TLS trust
    pub fn connect_tcp(address: SocketAddr, timeout: Duration, proof: &cabuya::auth::Proof, settings: &Settings) -> Result<Self, Error>;   // connect() for tcp
    pub fn over<C: Channel + 'static>(channel: C, proof: &Proof, settings: &Settings) -> Result<Self, Error>;   // an unproven channel to the host
    pub fn through(link: Arc<dyn Open>, endpoint: &Endpoint, proof: &Proof, settings: &Settings, forward: &ForwardSettings) -> Result<Self, Error>;   // a link over a stream the host `link` reaches carries (`ssh -J`): `Tcp` and `Pipe` (a Unix socket, or a Windows pipe named `\.\pipe\NAME`) through `forward::connect`, `Command` as a program run by `bohio-exec/1` with its standard input open (waited until it started, so a refusal is the error); any other endpoint is `Error::Link` naming it. `proof` proves the host at the far end, never the jump: a jump that answers is refused. Keeps `link` up as long as it lives, closed before it
    pub const fn host(&self) -> coqui_peer::PeerId;                // the host's proven id
    pub fn is_up(&self) -> bool;                                   // false once the host or the network ended the link
    pub fn close(&self);                                           // closes every stream and joins the link's thread
    pub fn opener(&self) -> Opener;                                // an Open that is Clone and does not keep the connection alive
}

#[derive(Clone)] pub struct Opener;                               // Debug; Open (Error::Closed once the link ended or `close` was called)
pub struct Opens;                                                 // Debug; the serving half of an Opener
impl Opener {
    pub fn pair(settings: &Settings) -> (Opener, Opens);           // nothing opens until `Opens::run` runs on a link; `settings.bulk_window` is what `Open::bulk_window` answers
    pub fn open_stream(&self, protocol: &str, metadata: &[u8]) -> Result<Opened, Error>;   // as Open::open, keeping the type that says why a stream ended
    pub fn open_stream_with(&self, protocol: &str, metadata: &[u8], options: &OpenOptions) -> Result<Opened, Error>;   // a `receive_window` is granted before the stream is returned
    pub fn is_up(&self) -> bool;                                   // false once the link ended or `close` was called
    pub fn close(&self);                                           // closes every stream this opener opened and makes `Opens::run` return
    pub fn capabilities(&self, wait: Duration) -> Result<cabuya::link::Capabilities, Error>;   // reads the peer's `link/1` document now, on a thread of the link's
}
impl Opens {
    pub fn run<C: Channel>(self, mux: &cabuya::mux::Mux<'_, C>, settings: &Settings);   // serves open requests, each stream through a bounded relay, until the link leaves Up, `close`, or every Opener is gone; when the link ends, a stream's reader still gets what the stream held and how it ended (a revoke as `Permission`, `Opened::end` the text) for at most `send_limit`, where `close` ends it at once
}

#[non_exhaustive] pub struct Settings { pub max_streams: usize /*64*/, pub proof_timeout: Duration /*10 s*/, pub window: u32 /*1 MiB*/, pub max_window: u32 /*1 MiB*/, pub auto_window: bool /*false*/,
    pub bulk_window: u32 /*1 MiB*/, pub max_frame: usize /*64 KiB*/, pub max_metadata: usize /*4096*/, pub max_pending_opens: usize /*8*/, pub max_opens_per_minute: u32 /*600*/,
    pub tcp_send_buffer: Option<usize> /*None: system*/, pub tcp_receive_buffer: Option<usize> /*None: system*/,
    pub max_message: usize /*1 MiB*/, pub queue_bytes: usize /*256 KiB*/, pub read_ahead: usize /*8*/, pub queued_bytes: usize /*1 MiB*/,
    pub send_limit: Duration /*10 s*/, pub send_wait: Duration /*250 ms*/, pub open_wait: Duration /*250 ms; zero: refuse at once*/, pub poll_interval: Duration /*50 ms*/,
    pub heartbeat: Duration /*25 s; zero is off*/, pub heartbeat_misses: u32 /*3; at least 1*/,
    pub accept_wait: Duration /*200 ms: a listener's wait before it looks at its stop flag*/, pub accept_poll: Duration /*20 ms: a non-blocking listener's sleep*/, pub serial_reopen: Duration /*250 ms*/ }   // Default; the host's `[link]` section
impl Settings { pub fn limits(&self) -> cabuya::mux::Limits; pub fn record_length(&self, method: cabuya::auth::Method) -> usize; }
pub fn framed<S: Split, C: Codec>(stream: S, codec: C, settings: &Settings) -> io::Result<StreamChannel>;   // an unproven peer held to one proof-sized message
pub fn stream<S: Split>(stream: S, settings: &Settings) -> io::Result<StreamChannel>;   // framed with a 4-byte big-endian length: a pipe, a child's streams
pub fn socket(stream: TcpStream, settings: &Settings) -> io::Result<StreamChannel>;   // stream() with TCP_NODELAY and the TCP buffers `settings` name
pub fn connect(address: &SocketAddr, timeout: Duration, settings: &Settings) -> io::Result<TcpStream>;   // the buffers set before connecting (`TcpStream::connect_timeout` when none is set)
pub fn tune_listener(listener: &TcpListener, settings: &Settings) -> io::Result<()>;   // the buffers on a listening socket
pub fn stdio(settings: &Settings) -> io::Result<StreamChannel>;                       // stream() over this process's standard input and output

pub struct Relay;                                                 // Debug; Channel and LinkStream: an owned, bounded channel over one stream of a link; `finish` is queued behind what is sent, `finished_by_peer` and `life` follow the stream (a peer that finished leaves this side's direction open), `close_with` closes the stream with a reason once what is queued is delivered
#[derive(Debug)] pub struct Plain<C>(pub C);                      // a Channel as a LinkStream with no half-close: open until closed, finish is Unsupported, close_with closes
pub fn relay<'scope, 'env, P: Deref + Clone + Send + 'scope>(scope: &'scope thread::Scope<'scope, 'env>, stream: P, settings: &Settings) -> Arc<Relay>
pub fn relay_with<'scope, 'env, P: Deref + Clone + Send + 'scope, E: Fn(&P::Target) -> Option<cabuya::mux::StreamEnd> + Send + 'scope>(scope: &'scope thread::Scope<'scope, 'env>, stream: P, settings: &Settings, ended: E) -> Arc<Relay>;   // relay, asking `ended` why the stream ended; Relay::end() -> Option<StreamEnd> gives it
#[derive(Debug)] pub struct Opened;                                // an Open'ed stream: Channel and LinkStream; end() -> Option<cabuya::mux::StreamEnd> (a refusal or revoke with the peer's text)
    where P::Target: LinkStream;
```

`connect_tcp` and `over` prove this side to the host and the host to this side inside `settings.proof_timeout`, with the keys `proof` holds
(`Proof::builder(identity).responders(Trust::Store(store) | Trust::pinned([id]))`; the identity and store are the caller's, `bohio::peer::Data`
makes both). A host whose key this side does not trust, or that does not accept this side's key yet (it records it as waiting), ends the
connection without a word: both are `Error::Transport`, a failed proof of another kind is `Error::Link(text)`. A socket that connected to itself
is refused. A thread runs the link and two more run per open stream; `Open::open` returns a `Relay` handle, so a stream is `'static` and
outlives the call. A stream the host refused (no grant, no such protocol) or ended because the peer lost its grant fails with
`Error::Transport(Failure::Permission)` from the call that reads it; a lost link is `Failure::PathLost`.

A `Relay` holds at most `queue_bytes` in each direction (a message larger than that passes alone), so a slow reader or writer makes the other side
wait; closing it delivers what is queued for at most `send_limit`. What one stream can buffer is `window` plus `2 x queue_bytes` (`bulk_window` instead of
`window` for a stream widened by it, up to `max_window`); what a link buffers besides is `read_ahead` messages and `queued_bytes` at the socket. Nothing grows with a
file's size.

One stream moves at most its receive window per round trip. `window` is what each sender starts every stream with and is not negotiated, so keep it equal on both
sides: a side whose window is larger can make the other end its streams as broken. `Open::open_with` and `Settings::bulk_window` widen one stream instead, by
granting credit the sender already accepts, which needs no setting at the peer; a link logs once, as a warning and not a refusal, a `window` or `max_frame` its
peer's `link/1` document shows differing from its own.

## Gotchas

- One thread that writes a large input before reading can deadlock against a process that echoes it; write on one thread and
  read on another (every method takes `&self`).
- A refused request surfaces from `next` or `wait` as `Error::Remote`, not from `start`: the host answers on the stream.


## Search, follow, stats and which (`bohio_client::search`, `follow`, `Control`)

Each call is a plain function or method over `&dyn Open`, so a tool layer can wrap it and hold the link itself.

```rust
// search: the lines of files on the host that match a pattern, found there (`bohio-search/1`; needs fs.read, no exec). Links are never followed.
pub fn search::search(link: &dyn Open, roots: &[&str], pattern: &str, options: &SearchOptions) -> Result<SearchIter, Error>;   // search_with(.., timeout: Option<Duration>) too (default 300 s per frame)
#[derive(Default)] pub struct SearchOptions { pub depth: Option<u64>, pub include: Vec<String>, pub exclude: Vec<String>, pub hidden: bool, pub literal: bool, pub case_insensitive: bool, pub context: u64, pub max_matches: Option<u64>, pub max_files: Option<u64> }   // the host's [search] bounds win
pub enum SearchItem { Match(Box<Found>), Failure(SearchItemError) }   pub struct Found { pub root: u64, pub path: String /* below the root; empty when the root is the file */, pub size: u64, pub hit: SearchMatch }
pub struct SearchIter;   // Iterator<Item = Result<SearchItem, Error>>; done() -> Option<SearchDone>; finish(self) -> Result<SearchResults { matches, failures, done }, Error>; dropping it closes the stream, which stops the host
// A host's refusal (denied, busy, bad_request for an empty pattern, one that does not compile or is too large, a bad glob, a context over max_context) is the first item, Error::Remote. A root outside the peer's roots is a Failure (outside_root).

// follow: a file of the host followed as it grows (`bohio-read/1` with `follow`; needs fs.read)
pub fn follow::follow(link: &dyn Open, path: &str, options: &FollowOptions, timeout: Option<Duration>) -> Result<Follow, Error>;   // timeout: the wait for the host to accept it
#[derive(Default)] pub struct FollowOptions { pub tail: Option<u64>, pub offset: u64, pub by_name: bool, pub interval: Option<Duration> /* at least 1 ms; None: the host's follow_interval */ }
pub enum FollowEvent { Data(Vec<u8>), Truncated(Truncated) }
pub struct Follow;   // size(), initial_length(); next_within(Option<Duration>) -> Result<Option<FollowEvent>, Error> (Ok(None): nothing in that time, the follow goes on); collect_for(Duration) -> Result<Vec<FollowEvent>, Error>; Iterator<Item = Result<FollowEvent, Error>> waits without limit; dropping it closes the stream

// control: what the host runs on, and where its programs are (both need `info`)
pub fn Control::stats(&self, timeout: Option<Duration>) -> Result<bohio_proto::control::Stats, Error>;
pub fn Control::which(&self, names: &[&str], timeout: Option<Duration>) -> Result<BTreeMap<String, Option<String>>, Error>;   // the absolute path or None; a name with a directory is not looked up
```

`bohio remote HOST search|glob|stats|which|read --follow` are these calls (`search` exits 0 on a match, 1 on none, 2 when something could not be read). A follow ends with `Error::Remote` (`denied`) when the grant is revoked.
