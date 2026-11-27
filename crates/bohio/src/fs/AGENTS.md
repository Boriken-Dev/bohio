# `bohio::fs` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Filesystem (`bohio::fs`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a FilesystemConfig, pub hash: &'a HashConfig, pub delta: &'a DeltaConfig, pub compression: &'a CompressionConfig, pub peer: &'a str, pub source: FilesRef<'a> /*the files source the roots open from*/, pub slots: &'a Slots, pub follows: &'a Slots /*the bound of `[filesystem] max_follows`*/,
                    pub current: Option<control::Current<'a>> /*where the grant is read before each item of a bundle and root of a walk; the grant is used when absent*/ }   // Copy
pub fn call(context: &Context<'_>, call: &bohio_proto::control::Call, budget: usize) -> Result<Reply, ErrorBody>;
pub fn serve_read(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<u64, ErrorBody>;     // bytes sent
pub fn serve_write(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<WriteDone, ErrorBody>;
```

`call` answers one filesystem call of the control stream; `budget` is the largest reply the stream carries (a listing page is cut
to fit it). `serve_read` and `serve_write` serve one `bohio-read/1` and `bohio-write/1` stream whose open carried `metadata`; `serve_hash(channel, metadata, &HashContext)` serves one `bohio-hash/1` stream (`HashContext { files: Context, slots: &Slots, current: Option<control::Current> }`: `current` is read before each item, so a narrowed grant refuses the next one; `Context` carries `hash: &HashConfig`). A
refused request gets one `error` frame, the same error is returned, and the stream stays open for the caller to close. Order of
checks: decode (`bad_request`), `[filesystem] enabled` (`unsupported`), the permission (`denied`: `fs.read` for `stat`, `list`,
`readlink`, hashes and reads; `fs.write` for `mkdir`, `remove`, `rename`, `symlink`, `set_*` and writes; both for `copy`), at
least one root in the grant (`denied`), a free transfer slot (`busy`, transfers only), the path (`bad_request`, `outside_root`),
then the operation (`not_found`, `exists`, `denied`, `unsupported`, `io`). Messages name what failed, never a path. A mode a peer
names (`set_mode`, `mkdir`, a write's `file_mode`) above `0o7777` is `bad_request` and one with the set-user-ID or set-group-ID bit
(`0o6000`) is `denied`; a `copy` and a `replace` over an existing file keep the mode without those bits.

**Roots and paths.** `fs_roots` is the whole of what a peer can reach. A relative path starts at the first root; an absolute one
must lie under a root (the longest match; compared without case on Windows and macOS, and `\\?\C:\x` equals `C:\x`). `.` and
`..` are folded lexically; a `..` that would leave the path's start is `outside_root`. Windows also refuses device names (`CON`,
`NUL`, `COM1`...), a colon in a name (alternate data streams), a name ending in a dot or space, `\\.\` and `\\?\GLOBALROOT`
forms and a drive-relative `C:x` (`bad_request`); both separators are accepted there.

**Which source.** A stream or control call names a files source (`source` of its request); `fs::pick(sources, grant, name)` answers the source, whether it names its files as POSIX (`posix_paths` in `describe`) and why the request may not use it (`Picked`, `FilesRef`); every engine's `authorize` answers that refusal first, and `fs::path::locate_in` reads the path in the source's syntax. `covers_read`/`covers_write` hold the path to the roots in either syntax (they know no source).

**Handles.** The engines reach files only through `fs/vfs.rs` (`Dir`, `File`, `Metadata`, shaped like the `cap-std` types) over the `FilesSource` kind; `access/native_files.rs` is the `native` provider, where `cap-std` and `fs/native.rs` (winfs on Windows) live. A `Dir` is a handle for a root or for a directory opened below one, so a caller that keeps one (`Staged`, a walk level) is not redirected by a rename above it. A rename between two roots is `FilesSource::rename_across` (the source opens both). A name still resolves beneath its handle on every call, but a sequence the engine runs in several calls (temporary file, then rename) names the parent through a handle it opened once, so the guarantee holds with one exception: the attributes of a listed entry (`DirEntry::metadata`) are asked by name below the directory the listing was opened on, not through the listing's handle. Confinement to the root holds, and a directory swapped for a link between the listing and that question can answer for another directory inside the root.

**Symbolic links.** The rest of the path is resolved by `cap-std` against the root's directory handle, which follows a link only
while it stays under that handle: a link out of a root, at the end or in the middle of a path, is `outside_root`, and so is an
absolute link even when it points back inside. `remove` removes a link, never what it points at; `stat` without `follow` describes
the link. `symlink` refuses an absolute target or one that climbs above the roots (`outside_root`); `readlink` returns the text
as stored. The target is judged by its words, so on Unix a chain of links can still name a place outside (`sub/d` -> `..`, `esc` ->
`sub/d/..`): the host's calls refuse it, a program following links without a handle does not (Windows reads a target's `..` by its words).

**What is race-free** (no gap between checking a component and using it, so a link swapped in meanwhile cannot lead out):

| OS | Race-free | Not race-free |
| --- | --- | --- |
| Linux | every lookup: one `openat2` with `RESOLVE_BENEATH` (kernel 5.6+), else `openat` + `O_NOFOLLOW` per component; changes and removals are `*at` calls on the parent's handle | the configured root itself is opened by its path once per call |
| macOS and other Unix | the same `openat` + `O_NOFOLLOW` walk, `*at` calls on the parent's handle | the root's own path |
| Windows | every lookup and open relative to the root's handle, reparse points not followed; `list`, `readlink`, `mkdir`, `remove` (a tree too), `rename`, `symlink` and `set_attributes` act on the last name through the handle of the directory holding it (`NtCreateFile` with `RootDirectory`, then `SetFileInformationByHandle`; `platform/winfs.rs`), so no path is built and a directory swapped for a junction cannot redirect the call | the configured root itself is opened by its path once per call |

On every OS the root's owner is trusted: a hard link another local user makes to a file outside a root exposes that file, and a
bind mount or junction an administrator places inside a root is part of the root. Windows has no POSIX modes (`set_mode` and a
`mode` are `unsupported`, `Metadata::mode` is absent); `set_attributes` sets the read-only attribute there and is `unsupported`
elsewhere.

**Compression** (`fs::compress`, `fs::prepare`, `exec`): `fs::Context` carries the `[compression]` settings and the peer's id; `exec::Context` the settings. A read with
`compression` sends each chunk as a `compressed` frame when that is smaller, else a raw `data` frame (the encoder then starts a new context); a file skipped by
extension, `exclude` or the sample is sent raw and the header has no `compression`. `prepare` makes `<token>.bin` and `<token>.meta` in `prepare_dir` (a random
128-bit token, the peer's id in the record), sends `progress` at least every `idle_timeout / 4`, and `resume` re-opens it for the same peer and path. A write
decodes `0x14` frames within `window_log_max` and `max_expansion` (`bad_request` past either) and resets the decoder on a raw frame; exec output has an encoder per pipe and
standard input a decoder. Cargo feature `zstd` (default) builds the C library; `zstdmt` adds worker threads. `fs::sweep_prepared(dir, ttl)` removes expired files (`Node::new` calls it).

**Transfers.** Reads send `header`, `data` frames of at most `chunk_size` bytes, `done`; sending waits while the stream has no room.
A `replace` write goes to a temporary sibling (`.bohio-<pid>-<n>.tmp`) and is renamed over the target after `end` (and a matching
`expect` digest matched), keeping the target's mode on Unix; a stream that closes, fails or stays idle for `idle_timeout` leaves the
target as it was and removes the temporary file. `create` removes the file it made when abandoned; `append` and `at` write in
place and keep what was written. A read and a write hash the bytes they move for `hashes` and `expect` (a write: the union, one pass; only `hashes`, else `[filesystem] write_hashes`, is answered). A hash stream holds one `[hash] max_streams` slot, a transfer one `max_transfers` slot, until it ends; a hash stream reads at most `[hash] rate` bytes a second. Memory is one chunk plus what the
link's channel queues and reads ahead: a channel that queues `n` bytes can hold `n` more.

## Trees, bundles, moves and resumes (`bohio::fs`, `[walk]`, `[filesystem]`)

```rust
pub fn fs::serve_walk(channel: Arc<dyn Channel>, metadata: &[u8], context: &WalkContext<'_>) -> Result<u64, ErrorBody>;   // entries sent; one thread; each root decided by the grant as it is then (`Context::current`)
pub struct fs::WalkContext<'a> { pub files: fs::Context<'a>, pub walk: &'a WalkConfig, pub slots: &'a Slots }   // Debug, Clone, Copy
pub struct config::WalkConfig { pub max_streams: usize /*4*/, pub max_entries: u64 /*1000000; 0: no limit*/, pub max_visited: u64 /*10000000; names looked at, sent or not; 0: no limit*/, pub recheck_entries: u64 /*1000; entries between looks at the peer's grant; 0: once a root*/ }   // `[walk]`; Config.walk, NodeConfig.walk
```

A bundle (`items: true`) holds one transfer slot and serves `bohio-read/1` or `bohio-write/1` from `fs/bundle.rs`; each item is decided alone and fails alone, and its data is read and dropped when it failed so the compression context spans the items. `move` (`fs/tree.rs`) renames, and on `CrossesDevices` copies with mode and time then removes; a rename without `overwrite` is `renameat2(RENAME_NOREPLACE)` on Linux and a hard link plus an unlink for a file elsewhere (a directory is checked and renamed). A resumable `replace` keeps `.bohio-resume-<peer-and-path hash>-<token>.tmp` beside the target when its stream ends early; `Staged::{open_resumable, resume, keep_for_resume}` hold it, a process-wide set refuses a second stream under one token, and expired files are swept from the directory of the next resumable write or resume (not at start). Metadata has `device`, `inode` and `links` on Unix only (cap-std reports none on Windows), so `walk --follow` does not enter a link to a directory there.
## Search, follow, stats and which (`bohio::fs`, `bohio::stats`, `[search]`, `[filesystem]`)

```rust
pub fn fs::serve_search(channel: Arc<dyn Channel>, metadata: &[u8], context: &SearchContext<'_>) -> Result<u64, ErrorBody>;   // matches sent; one thread; each root decided by the grant as it is then; links never followed; the pattern is never logged
pub struct fs::SearchContext<'a> { pub files: fs::Context<'a>, pub search: &'a SearchConfig, pub slots: &'a Slots }   // Debug, Clone, Copy
pub struct config::SearchConfig { max_searches: usize /*4*/, max_file_bytes: u64 /*16777216: skipped and counted; also the memory one search holds*/, max_line_bytes: usize /*4096*/, max_matches: u64 /*10000*/, max_files: u64 /*100000*/, max_context: u64 /*50*/, regex_size_limit: usize /*10485760*/, binary_probe: usize /*8192; 0: none*/, deadline: Span /*60s*/ }   // `[search]`; Config.search, NodeConfig.search; zero values other than binary_probe and max_context are ConfigError::Invalid
// A followed read (bohio-read/1 with `follow`) is `fs/follow.rs`: the file from `offset` or `tail`, then each look every interval; `truncated` when the file got shorter or (by name) another file took the path, by device and inode (Unix) or volume and file index (Windows).
// `control::Context.path: Option<&OsStr>` is the host's own PATH, for `which` (`unsupported` when None). `bohio::stats::collect() -> Stats` reads the system through `sysinfo` (`system`, `disk`).
```

`bohio-search/1` is served for `fs.read` (in `link::PROTOCOLS`); `Info.transfer` reports `search`, `search_limits` and `follow` when `[filesystem] enabled`.
