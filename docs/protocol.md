# The protocol

This page is the wire, for an implementer in any language. Nothing here needs Rust. `vectors/*.json` in the repository holds every message below as the hex of its bytes, and each implementation decodes them.

A Bohío host serves six protocols over one proven, sealed link. A client opens one stream per operation, names the protocol, and sends the request as the stream's open metadata.

## Layers

| Layer | What it does | Where it is specified |
| --- | --- | --- |
| Carrier | moves whole messages between two endpoints | [Carriers](#carriers) |
| Proof | shows who holds a key, then seals every message after it | the proof's method, by [`coqui-link`'s page](https://github.com/Boriken-Dev/coqui-link/blob/main/docs/link.md) |
| Link | named streams over the proven connection, with credit | [`docs/link.md` of coqui-link](https://github.com/Boriken-Dev/coqui-link/blob/main/docs/link.md) |
| Bohío | six protocols, each its own messages | this page |

The proof and the link are not repeated here. An implementation takes them from coqui-link's page, which has the proof's methods (`noise` by default, `webcrypto` for a browser), the frame format (`open`, `data`, `more`, `credit`, `finish`, `close`, `ping`, `pong`), stream ids, flow control and limits, and its own vectors in `vectors/mux.json`. Everything below starts after a stream is open.

A Bohío host answers `open` for a protocol token this page names, if the peer's grant allows it, and answers `close` with `refused` otherwise. A proof admits a peer to the link; it grants no protocol.

## Carriers

The proof and the link run on any channel that delivers whole messages in order, whichever end opened it: the end that opened it sends the proof's first message and the other answers, and once the proof holds the link is the same in both directions (see [Roles](#roles)). The first message of a connection is the proof's, bounded to the proof's maximum size until the proof completes; after it the bound is the link's largest record, which is the largest message plus 5 bytes for the link's frame header plus the seal.

| Endpoint | A message is |
| --- | --- |
| `tcp://host:port` | a 4-byte big-endian length, then the message |
| `pipe://name` | the same, on a Windows named pipe or a Unix domain socket |
| standard input and output (`bohio serve --stdio`) | the same, on the process's standard streams |
| `ws://host:port/path`, `wss://host:port/path` | one binary WebSocket message (TLS for `wss`) |
| `webtransport://host:port/path` | one message on a WebTransport bidirectional stream, behind a 4-byte little-endian length (QUIC, always TLS) |
| `serial://port?baud=N` | a COBS-delimited frame with a CRC-32C, as `coqui-transport-serial` defines |

TLS, where a carrier has it, decides whether the carrier's own handshake succeeds and keeps a network observer from the message sizes; it never says who the peer is. The proof does that for every carrier, and a peer that arrives over TLS is refused or admitted by it exactly as one over TCP. A client may open WebTransport first and fall back to a WebSocket endpoint of the same host when the QUIC path fails before anything crossed (a blocked UDP port); a refused certificate never falls back, and the proof runs on whichever opened. Datagrams are outside the proof and Bohío does not use them.

A host serves a serial line to one peer at a time. A message never exceeds the link's `max_message` setting (1 MiB by default); a longer one is the link's job to cut into `more` frames of at most `max_frame` bytes (64 KiB by default).

## Flow control

Each direction of each stream starts with a credit window of `[link] window` bytes (1 MiB by default), and a sender may be at most that far ahead of the reader, so one stream moves at most a window per round trip: about 4 MB/s over a 250 ms path. The initial window is not negotiated, so both sides keep it equal; a side whose window is larger can end the other's streams as broken. A receiver widens one stream by granting credit beyond what it has read, which every sender already accepts, so no peer changes: `[link] bulk_window` is what a side grants a bulk stream it receives (a client's `bohio-read/1`, a host's `bohio-write/1`), `max_window` bounds every widening and `auto_window` grows a stream from the link's round trip. Each side's `link/1` document carries its `window` and `max_frame`, and a side that sees them differ from its own logs it once.

## Protocol names

A stream's `open` carries a protocol token and metadata. A client ignores a token it does not know and treats an unknown `offers` entry the same way.

| Token | Purpose | Open metadata | A peer needs |
| --- | --- | --- | --- |
| `bohio/1` | hello, info and the small filesystem calls | none | one of `info`, `fs.read`, `fs.write` |
| `bohio-exec/1` | one process, run to its exit | CBOR [`exec-request`](#exec-bohio-exec1) | `exec` |
| `bohio-shell/1` | one interactive shell on a terminal | CBOR [`shell-request`](#shells) | `shell` |
| `bohio-read/1` | one file read, streamed | CBOR [`read-request`](#reading-a-file) | `fs.read` |
| `bohio-write/1` | one file written, streamed | CBOR [`write-request`](#writing-a-file) | `fs.write`, and `fs.read` as well for a [delta](#deltas) write |
| `bohio-hash/1` | digests of many files, computed on the host | CBOR [`hash-request`](#hashing-bohio-hash1) | `fs.read` |
| `bohio-connect/1` | one connection from the host to a target, then its bytes | CBOR [`connect-request`](#forwarding-bohio-connect1-and-tcp1) | `forward.connect` and a rule that matches the target |
| `tcp/1` | the same, as [coqui-link](https://github.com/Boriken-Dev/coqui-link) defines it | the text `HOST:PORT` | `forward.connect` and a rule that matches the target |
| `bohio-gui/1` | a view of the host's desktop: snapshots, keyboard and mouse, the clipboard's text | CBOR [`gui-request`](#desktop-bohio-gui1) | one of `gui.view`, `gui.input`, and each thing the request wants |
| `bohio-listen/1` | one listener the host binds for the peer, each arrival announced on the stream | CBOR [`listen-request`](#remote-forwards-bohio-listen1-and-bohio-accept1) | `forward.listen` and a `listen` rule that matches the bind |
| `bohio-accept/1` | one arrival of the peer's own listener, then its bytes | CBOR [`accept-request`](#remote-forwards-bohio-listen1-and-bohio-accept1) | `forward.listen` |

Each `open` is decided against the peer's grant, and each running stream again at the host's recheck interval. Removing a grant closes the stream with reason `revoked`; the reason text reaches the peer.

## Conventions

- **CBOR.** Control messages and open metadata are CBOR ([RFC 8949](https://www.rfc-editor.org/rfc/rfc8949)), always a map with text keys. A decoder reads any valid CBOR encoding of these values (definite or indefinite lengths, any integer width); the vectors show what the Rust implementation writes.
- **Enums carry a `type` key.** A choice among shapes is a map whose `type` key names the shape in `snake_case`, with the shape's own keys beside it. A shape that wraps another message (`reply` of `info`, `stat`, `list`) holds that message's keys in the same map, beside `type`.
- **Optional keys.** A writer writes every key, an absent value as `null`. A reader accepts a key that is missing, or `null`, as absent and uses the default the CDDL shows. Written `opt<T>` below.
- **Unknown keys are ignored.** A reader never fails on a key it does not know, and a reader never writes one. An unknown error code, file kind or shell kind decodes as `other`.
- **Trailing bytes.** A message is exactly one CBOR item: bytes after it are a decoding failure. A decoder bounds nesting (the Rust one stops at 32 levels).
- **Frames.** A stream message of a streaming protocol (`exec`, `shell`, `read`, `write`) is one **frame**: a tag byte, then the body. The body is CBOR, raw bytes, or empty, as the table of each protocol says. A tag a reader does not know is skipped, except where the protocol says otherwise. Raw bytes are never put inside CBOR or JSON. A frame is at most the stream's message size, so a host splits output into frames of at most that size less one.
- **Integers.** `uint` is an unsigned integer up to 2^64-1, `int` a signed one in 64 bits, `u32`-ranged values are marked.
- **Text.** UTF-8. A path is the host's native text, with the separator `info.filesystem.separator` reports; a Windows host also accepts `/`.
- **The stream's end.** A host closes a stream in order after its last frame. A client that closes a stream first ends what it started: a process's tree is killed, a shell's tree is killed and a write is abandoned.

## Errors

Every error is `{code, message}`:

```text
error-body = { code: error-code, ? message: text }
error-code = "bad_request" / "denied" / "not_found" / "exists" / "unsupported"
           / "busy" / "outside_root" / "io" / "refused" / "unreachable" / "timeout" / "in_use"
           / "changed" / "unavailable" / "internal" / "other"   ; unknown text decodes as "other"
```

| Code | Means |
| --- | --- |
| `bad_request` | the request cannot be read, or is not valid (an empty `argv`, a NUL in a string, a path with `..` that escapes) |
| `denied` | the peer's grant, or the operating system, refuses |
| `not_found` | no such file, directory or program |
| `exists` | the target exists and the call would not replace it |
| `unsupported` | the host cannot do it: the capability is off, or the platform has no such call (`set_mode` on Windows) |
| `busy` | a configured bound is full (`max_processes`, `max_sessions`, `max_transfers`) |
| `outside_root` | a path or a symbolic link leaves the peer's filesystem roots |
| `io` | the operating system failed the call for another reason |
| `refused` | a forward's target refused the connection |
| `unreachable` | a forward's target cannot be reached: no route, or a network that is down |
| `timeout` | a forward's target did not answer within `[forward] connect_timeout` |
| `unavailable` | the thing exists but cannot be used now: a desktop that is locked for input, a session that is not at the screen, a permission of the operating system that is missing (`unsupported` is a host with no desktop backend, or `[gui] enabled = false`) |
| `in_use` | a remote forward's address, socket path or pipe name is already bound |
| `changed` | a precondition no longer holds: the file a [delta](#deltas) write was made against is not as the client saw it |
| `internal` | a bug or a reply that does not fit the stream |
| `other` | a code this version does not know |

A streaming protocol sends an error as its `0x05` frame, always the last the host sends. A control call answers with the error in its `response`.

## Control: `bohio/1`

The stream carries any number of calls. Each message is one CBOR `request` from the client or one `response` from the host, with no tag byte. A host answers in the order the requests arrive; `id` lets a client match them, and any `id` is accepted (a `request` that cannot be read is answered with id `0` and `bad_request`). The stream stays open until the client closes it.

```text
request  = { id: uint, call: call }
response = { id: uint, ? reply: opt<reply>, ? error: opt<error-body> }   ; exactly one of reply, error is not null

call = hello / info / stat / list / mkdir / remove / rename / copy / symlink
     / readlink / set-times / set-mode / set-attributes

hello          = { type: "hello", ? version: uint, ? client: text }       ; version at least 1
info           = { type: "info" }
stat           = { type: "stat", path: text, ? follow: bool }            ; follow defaults to false
list           = { type: "list", path: text, ? after: opt<text>, ? limit: opt<uint> }
mkdir          = { type: "mkdir", path: text, ? parents: bool, ? mode: opt<uint> }
remove         = { type: "remove", path: text, ? recursive: bool }
rename         = { type: "rename", from: text, to: text, ? overwrite: bool }
copy           = { type: "copy", from: text, to: text, ? overwrite: bool }
symlink        = { type: "symlink", target: text, link: text }
readlink       = { type: "readlink", path: text }
set-times      = { type: "set_times", path: text, ? accessed: opt<timestamp>, ? modified: opt<timestamp> }
set-mode       = { type: "set_mode", path: text, mode: uint }
set-attributes = { type: "set_attributes", path: text, ? readonly: opt<bool> }

reply = { type: "hello", version: uint, ? transfer: transfer-info }   ; transfer-info as in info

      / { type: "info", * info-field }          ; the keys of info, beside type
      / { type: "stat", * metadata-field }      ; the keys of metadata, beside type
      / { type: "list", * listing-field }       ; the keys of listing, beside type
      / { type: "done" }                        ; every call that returns nothing
      / { type: "link", target: text }

timestamp = { secs: int, ? nanos: uint }        ; seconds since 1970-01-01T00:00:00Z, nanos below 10^9
file-kind = "file" / "directory" / "symlink" / "other"
metadata  = { kind: file-kind, ? size: uint, ? modified: opt<timestamp>, ? accessed: opt<timestamp>,
              ? created: opt<timestamp>, ? mode: opt<uint>,     ; Unix permission bits, at most 0o7777
              ? readonly: bool, ? attributes: opt<uint> }       ; Windows attribute bits
listing   = { ? entries: [* entry], ? next: opt<text>, ? skipped: uint }
entry     = { name: text, metadata: metadata }                  ; a link is described, not followed
```

What each call needs: `info` needs `info`; `hello` needs nothing; `stat`, `list` and `readlink` need `fs.read`; `mkdir`, `remove`, `rename`, `symlink`, `set_times`, `set_mode` and `set_attributes` need `fs.write`; `copy` needs both. Each needs the peer to hold a filesystem root, and resolves every path against the roots.

- `hello`: the host answers with `min(version, its own)`. Both sides then speak that version. A `version` of 0 is `bad_request`.
- `list`: entries are in ascending order by name. A reply holds as many entries as fit the stream's message size; `next` is then the name to send as `after`, and `skipped` counts names that are not valid text and so are left out. `limit` caps the count.
- Digests are not a control call: a long hash would hold every later call on the stream, so they have their own stream, [`bohio-hash/1`](#hashing-bohio-hash1).
- `mkdir` with `parents` creates what is missing; `remove` of a link removes the link; `rename` and `copy` without `overwrite` answer `exists` for an existing target.
- Calls a platform lacks answer `unsupported`: `set_mode` and `mode` on Windows, `set_attributes` anywhere else.
- A mode (`set_mode`, `mkdir`'s `mode`, a write's `file_mode`) above `0o7777` is `bad_request`, and one with the set-user-ID or set-group-ID bit (`0o6000`) is `denied`: such a file would run as the host's account for whoever starts it. A `copy`, and a `replace` write over an existing file, keep the source's or the target's mode without those two bits.

```text
info = { ? protocol: uint, ? service: { ? name: text, ? version: text }, ? host_id: opt<text>,
         ? os: text, ? arch: text,                 ; the names Rust's std::env::consts uses: "linux", "windows", "macos"; "x86_64", "aarch64"
         ? shells: [* { kind: shell-kind, path: text }],
         ? filesystem: { ? enabled: bool, ? symlinks: bool, ? unix_modes: bool, ? windows_attributes: bool,
                         ? case_sensitive: bool, ? separator: text },
         ? exec: { ? enabled: bool }, ? shell: { ? enabled: bool },
         ? limits: { ? max_streams: opt<uint>, ? max_processes: opt<uint>, ? max_sessions: opt<uint> },
         ? transfer: transfer-info,
         ? forward: forward-info, ? gui: gui-info }
gui-info = { ... }   ; see "Desktop: bohio-gui/1"
forward-info = { ? enabled: bool, ? connect: [* text], ? listen: [* text] }   ; the target kinds `bohio-connect/1` reaches and the listener kinds `bohio-listen/1` binds: "tcp", and "unix" (Unix hosts) or "pipe" (Windows hosts)
transfer-info = { ? hashes: [* text],                        ; the algorithms the host computes, empty when none
                  ? hash_limits: { ? max_streams: uint, ? rate: uint },    ; hash streams at once; bytes a second one stream reads, 0 unlimited
                  ? compression: [* compression-info],                     ; the algorithms the host compresses with, empty when none
                  ? compression_default: opt<compression>,                 ; what "auto" means on this host
                  ? prepared: bool,                                        ; whether a read may say `prepare`
                  ? delta: delta-info }                                    ; what the host offers for deltas
delta-info = { ? weak: [* text],                                           ; the weak checksums it computes: "rsync"; empty when it serves no deltas
               ? min_block: uint, ? max_block: uint,                       ; the block sizes it chooses between, and the largest it accepts
               ? min_size: uint,                                           ; the size of file from which a client's "auto" uses a delta
               ? max_signature_bytes: uint }                               ; the most bytes of signature a delta read takes from the client
compression-info = { algorithm: text, ? levels: { min: int, max: int },    ; the levels after the host's own cap; 0 and 0 for lz4
                     ? default_level: int, ? window_log_max: uint }        ; the level used when a request names none; the largest zstd window, 0 where there is none
shell-kind = "sh" / "bash" / "zsh" / "cmd" / "powershell" / "pwsh" / "other"
```

## Reading a file

`bohio-read/1`. The open's metadata is a CBOR `read-request`; the client sends nothing more on the stream.

```text
read-request = { path: text, ? offset: uint, ? length: opt<uint>, ? hashes: [* text],   ; length null: to the end; hashes: algorithm names
                 ? compression: compression,                                           ; how the host sends the bytes; absent or "none": raw
                 ? prepare: bool,                                                      ; compress the whole range first, then send it
                 ? resume: { token: text, offset: uint },                              ; continue a prepared read at a compressed offset
                 ? delta: read-delta }                                                 ; send only what differs from the client's old copy (see Deltas)
read-header  = { size: uint, length: uint,                                             ; the file's size; the bytes that follow, decompressed
                 ? compression: compression,                                           ; what the host used, never "auto"; absent: raw
                 ? compressed_length: uint, ? token: text,                             ; prepared reads: the compressed size, the name to resume by
                 ? delta: bool }                                                       ; a delta read: copy frames and data add up to `length`
read-done    = { ? hashes: { * text => text } }                                        ; the digests asked for, of the bytes sent
progress     = { prepared: uint }                                                      ; the bytes of the file compressed so far
```

| Direction | Tag | Body |
| --- | --- | --- |
| host to client | `0x01` | CBOR `read-header` |
| host to client | `0x02` | raw file bytes, in order |
| host to client | `0x04` | CBOR `read-done`: all `length` bytes were sent |
| host to client | `0x05` | CBOR `error-body`: the last frame |
| host to client | `0x06` | a piece of the compressed stream (see [Compression](#compression)) |
| host to client | `0x08` | CBOR `progress`: a prepared read is still being compressed |
| host to client | `0x09` | CBOR `delta-copy`: a delta read, these blocks of the client's old copy stand here |
| client to host | `0x12` | CBOR `signature-batch`: a delta read, the next blocks of the client's signature |
| client to host | `0x13` | empty: a delta read, the signature is complete |

The host sends the header first, then data frames whose bytes add up to `header.length` (after decompression), then `0x04`. A stream that ends with no `0x04` and no `0x05` is a read cut short. A refused request is one `0x05` frame. A client closing the stream stops the read.

## Writing a file

`bohio-write/1`. The open's metadata is a CBOR `write-request`.

```text
write-request = { path: text, ? mode: write-mode, ? parents: bool, ? file_mode: opt<uint>,
                  ? expect: { * text => text },          ; algorithm name => the digest the data must have, in hex
                  ? hashes: opt<[* text]>,                ; digests to answer; null: the host's [filesystem] write_hashes (sha256 by default)
                  ? compression: compression }            ; what the client sends: a concrete algorithm, never "auto"; absent or "none": raw
write-mode    = { type: "replace" } / { type: "create" } / { type: "append" } / { type: "at", offset: uint }   ; replace by default
              / { type: "delta", block_size: uint, base: delta-base }   ; build the new file from the target and what follows (see Deltas)
write-done    = { written: uint, ? hashes: { * text => text } }            ; the bytes written, and the digests `hashes` asked for
```

| Direction | Tag | Body |
| --- | --- | --- |
| host to client | `0x01` | empty: ready, the client may send data |
| host to client | `0x04` | CBOR `write-done`: the write is complete |
| host to client | `0x05` | CBOR `error-body`: the last frame |
| client to host | `0x10` | raw file bytes, in order |
| client to host | `0x11` | empty: the end of the data |
| client to host | `0x14` | a piece of the compressed stream (see [Compression](#compression)) |
| client to host | `0x15` | CBOR `delta-copy`: a delta write, these blocks of the target stand here |

The client waits for `0x01`, sends `0x10` frames, then `0x11`, then waits for `0x04`. A `replace` write goes to a temporary sibling and is renamed over the target after `0x11` and a matching `expect` (every named digest is checked, a mismatch is `io`, and nothing is replaced; `expect` is for `replace`, `create` and `delta` only, and a value that is not the algorithm's lowercase hex is `bad_request`); until the host's `0x04` the target is as it was. `create` fails with `exists` when the file is there. `append` and `at` write in place, and what was written stays when the stream is abandoned. `parents` creates missing directories; `file_mode` sets Unix permission bits on a new file. A write the client abandons (closing the stream) leaves no temporary file.

A host computes the union of `hashes` and `expect` in one pass and answers only `hashes`. An algorithm the host does not offer (`[hash] algorithms`), in a read, a write or a hash stream, is `unsupported` before any data moves.

## Hashing: `bohio-hash/1`

Digests of many files on one stream, computed on the host, so a client compares files without downloading them and a long hash never holds the control stream. The open's metadata is a CBOR `hash-request`; the client then sends one `0x10` frame per file and a `0x11`, and the host answers each item, in order, on one thread.

```text
hash-request = { algorithms: [+ text], ? blocks: opt<block-options> }
block-options = { ? size: opt<uint>, ? weak: opt<text>, ? strong_len: opt<uint> }   ; the signature of each item: see Deltas
hash-item    = { path: text, ? offset: uint, ? length: opt<uint>, ? if_size: opt<uint>, ? if_modified: opt<timestamp> }
hash-result  = { index: uint, size: uint, ? modified: opt<timestamp>, length: uint, ? unchanged: bool, hashes: { * text => text },
                 ? block_size: uint, ? strong_len: uint }       ; with blocks asked for: what they were made with
hash-blocks  = { index: uint, first: uint, size: uint, ? weak: [* uint], strong: bstr }
item-error   = { index: uint, code: error-code, ? message: text }
hash-done    = { items: uint }
```

| Direction | Tag | Body |
| --- | --- | --- |
| client to host | `0x10` | CBOR `hash-item` |
| client to host | `0x11` | empty: no more items |
| host to client | `0x01` | CBOR `hash-result` |
| host to client | `0x02` | CBOR `hash-blocks`: the next blocks of an item's signature, before that item's `0x01` |
| host to client | `0x03` | CBOR `item-error`: that item failed, the stream goes on |
| host to client | `0x04` | CBOR `hash-done`: every item was answered |
| host to client | `0x05` | CBOR `error-body`: the stream failed or was refused; the last frame |

`index` is the item's position among the client's items, from 0. `length` bytes from `offset` are hashed (to the end of the file when `length` is `null`; an `offset` past the end is that item's `bad_request`), and `size` and `modified` describe the file as it was opened. With `if_size` and `if_modified` both equal to the file's, nothing is read: the result has `unchanged` true, `length` 0 and no `hashes`. Each item's path is resolved against the peer's grant as it is when the item is reached, so a grant narrowed while the stream is open refuses the next item (`outside_root`, `denied`), and a missing file, a directory or a path outside the roots is that item's error, not the stream's. A request with no algorithm is `bad_request`; an algorithm the host does not offer, or a weak checksum it does not know, is `unsupported`; a peer without `fs.read` or a root is `denied`; past the host's hash-stream limit the stream is `busy` (`[hash] max_streams`, default 4). A stream that gets no item for `[filesystem] idle_timeout` ends with `io`. The host may hold one stream to `[hash] rate` bytes a second.

Names and digests. An algorithm is one of `md5`, `sha1`, `sha256`, `sha384`, `sha512`, `blake3`, `xxh3` (XXH3 with 64 bits), `xxh128`, `xxh64` (seed 0) and `crc32c`; an unknown name is `other` to a reader and `unsupported` to a host. A digest is lowercase hex in the byte order the reference tool prints (`md5sum`, `sha256sum`, `b3sum`, `xxhsum`): the digest's bytes for the cryptographic ones, and the value in big-endian for the others, `xxh128` high half first and `crc32c` as 8 digits. So the digest has 32 (`md5`, `xxh128`), 40, 64 (`sha256`, `blake3`), 96, 128, 16 (`xxh3`, `xxh64`) or 8 hex digits. Test vectors of the empty input and `abc` are in `crates/bohio-transfer/tests/vectors.rs`; `crc32c` of `123456789` is `e3069283`. `md5` and `sha1` are for comparing with sums that still use them, not for trust.

## Deltas

A client that has an older copy of a file, or a host that has one, moves only what differs: **rsync's algorithm** (Tridgell and Mackerras, "The rsync algorithm", 1996), streamed. The side that has the *old* file sends its **signature**, the weak and strong hash of each block; the side that has the *new* file slides a window over it, finds the blocks the old file already has and sends **steps**: a copy of blocks of the old file, or the bytes the old file lacks. The receiver applies the steps in order. Three uses share it: a **pull** (`bohio-read/1` with `delta`: the client has the old copy and sends its signature, the host sends steps), a **push** (`bohio-write/1` in mode `delta`: the host has the old copy, the client asks for its signature with `bohio-hash/1` and sends steps), and a **comparison** (the signatures of both sides equal). Memory on each side is a signature, a window of one block and one frame of literal bytes, never the file.

**Blocks.** The old file of `S` bytes is cut into blocks of `L` bytes, the last one shorter when `S` is not a multiple of `L`; block `k` is bytes `kL` to `kL + L - 1`. A signature has one entry per block: the *weak* hash and the *strong* hash.

- **The block size** is chosen by the side that makes the signature, unless the request names one: `clamp(next_power_of_two(floor(sqrt(S))), min_block, max_block)`, where the bounds are `info.transfer.delta` (`[delta] min_block`, 2048, and `max_block`, 1 MiB, on a host). A request may name any size from 1 to the host's `max_block`.
- **The weak hash** is `rsync`, a checksum of two 16-bit sums over a window `x[0..L]`: `a = (sum of x[i]) mod 2^16` and `b = (sum of (L - i) * x[i]) mod 2^16`, and the value is `a + 2^16 * b` (32 bits; no offset is added to the bytes; an empty block is `0`). It rolls one byte forward in constant time: dropping `out = x[0]` and taking in `in = x[L]`, `a' = (a - out + in) mod 2^16` and `b' = (b - L * out + a') mod 2^16`. `vectors/delta.json` holds the sums of the empty input, `abc` (`024a0126`) and others, and of a 64 KiB window and the same window rolled one byte, each computed from the definition.
- **The strong hash** is one of the [hash algorithms](#hashing-bohio-hash1) (the signature's first, `blake3` by default) of the block's bytes, cut to its first `strong_len` bytes (the digest's length up to 16 by default, from 1 to the digest's length). A weak match counts only when the strong hash matches too.
- **Wire form.** A signature travels as batches: `{first: uint, weak: [* uint], strong: bstr}` carries the blocks from number `first`, each block's weak value in `weak` and its strong hash, `strong_len` bytes, joined in `strong` (so `strong` is `strong_len` times as long as `weak` has entries). Batches come in order with no gap, and are cut to fit a frame.

**Steps.** `delta-copy = { block: uint, count: uint }` says that `count` (at least 1) consecutive blocks of the old file, from block `block`, stand here; the old file's last block may be shorter, so a copy that includes it is shorter than `count * L`. A copy that names a block the old file does not have is `bad_request` on a host and a protocol error on a client. Any other data is carried in the stream's data frames, as it is in a plain transfer, and is compressed as the stream's `compression` says: a copy frame neither ends nor disturbs a compression context, since every compressed piece decodes completely. The steps are not unique: a receiver applies whatever steps rebuild the file. What the sender does is this: it takes the weak hash of the window at its position, looks for blocks of the old file with that weak value, confirms with the strong hash, and on a match emits a copy (merged with the previous copy when the blocks are consecutive, and preferring the block that continues the previous copy when several equal blocks match) and moves the window on by `L`; with no match it adds the window's first byte to the literal bytes and rolls the window by one. At the end of the new file, bytes left (fewer than `L`) are matched against the old file's short last block as a suffix, and are literal otherwise. Literal bytes are cut into frames of at most the stream's chunk size.

```text
read-delta      = { block_size: uint, weak: "rsync", strong: text, strong_len: uint,
                    base_size: uint }                       ; the size of the client's old copy: what tells a block from the short last one
signature-batch = { first: uint, ? weak: [* uint], ? strong: bstr }
delta-copy      = { block: uint, count: uint }
delta-base      = { size: uint, ? modified: opt<timestamp>, ? hashes: { * text => text } }
```

**A pull.** The request is a `read-request` with `delta` and no `offset`, `length`, `prepare` or `resume`. After the open the client sends its signature, `0x12` batches then `0x13`, and the host answers the header (`delta` true, `length` the file's size) once it holds the whole signature; then `0x09` copies and `0x02` or `0x06` data frames whose bytes (a copy counts the bytes of its blocks) add up to `length`, then `0x04` with the `hashes` the request named, of the whole new file. The host holds a signature of at most `[delta] max_signature_bytes` (64 MiB; a larger one, known from `base_size` and `strong_len`, is `busy` before any batch is read) and a block size up to `[delta] max_block`; the strong hash must be an algorithm `[hash] algorithms` allows. A client rebuilds into a temporary file beside its old copy, checks the digest, and renames it over the old copy; a digest that does not match leaves the old copy as it was.

**A push.** The client asks the host for the signature of the target: a `bohio-hash/1` stream whose request has `blocks`, the strong hash taken from the request's first algorithm. The host answers, for each item, `0x02` batches of the item's blocks (numbered from the item's `offset`, so a range has a signature of its own) and then the item's `0x01` result, whose `block_size` and `strong_len` say how they were made and whose `size` and `modified` are what the client then names as the `base`. The client opens `bohio-write/1` with `mode` `{type: "delta", block_size, base}` and `expect` the digest the new file must have, matches the new file against the signature and sends `0x15` copies, data frames and `0x11`. The host checks the permissions before anything else: **a delta write needs `fs.read` as well as `fs.write`** (`denied` otherwise, and a revoke of either ends a running one), because a copy reads the target and a mismatching `expect` would tell a peer that may only write whether its guess about the target's bytes was right. It opens the target, and answers `changed` before `ready` when its size is not `base.size`, its modification time is not `base.modified` (when given) or a digest in `base.hashes` differs; it builds the new file in a temporary sibling from the open handle of the target, so the target is never modified in place; after `0x11` it checks `expect`, checks once more that the target's size and time are what they were when it opened it (else `changed`), and renames the sibling over the target as `replace` does. Nothing is replaced on any failure. `written` in `write-done` is the new file's size. A host bounds what a delta may write: past 1 MiB of output, the output may not exceed `[delta] max_expansion` (1024; `0` turns it off) times the target's size plus the bytes received, or the write ends with `bad_request`.

**Comparing without downloading.** A client decides in tiers, each dearer than the last: the size and modification time from a `stat`; then one digest of each side (`bohio-hash/1`, with `if_size` and `if_modified` to skip files that did not change); then the signatures of both sides. The client library does this (`bohio_client::sync::compare_file`) and `put` and `get` use a delta by default (`auto`) when the old copy at the destination is at least `info.transfer.delta.min_size` (a host's `[delta] min_size`, 1 MiB), because a delta of a small file saves less than its round trips cost.

## Exec: `bohio-exec/1`

One process, run to its exit. The open's metadata is a CBOR `exec-request`.

```text
exec-request = { command: command, ? cwd: opt<text>, ? env: env, ? stdin: stdin, ? timeout_ms: opt<uint>,
                 ? output_compression: compression,             ; how the host sends standard output and standard error
                 ? stdin_compression: compression }             ; how the client sends standard input: a concrete algorithm
command      = { type: "argv", argv: [+ text] }                 ; the first element runs directly, never through a shell
             / { type: "shell", kind: shell-kind, script: text } ; only for a caller that chose a shell
env          = { ? clear: bool, ? set: { * text => text }, ? remove: [* text] }   ; applied in that order
stdin        = "closed" / "open"                                ; closed by default
started      = { pid: uint, ? output_compression: compression, ? stdin_compression: compression }   ; what the host uses and accepts, never "auto"
exit         = { ? code: opt<int>, ? signal: opt<int>, ? timed_out: bool, ? killed: bool }
```

| Direction | Tag | Body |
| --- | --- | --- |
| host to client | `0x01` | CBOR `started` |
| host to client | `0x02` | raw standard output |
| host to client | `0x03` | raw standard error |
| host to client | `0x04` | CBOR `exit`: after every output byte |
| host to client | `0x05` | CBOR `error-body` |
| host to client | `0x06` | a piece of the compressed standard output |
| host to client | `0x07` | a piece of the compressed standard error |
| client to host | `0x10` | raw standard input (only with `stdin: "open"`) |
| client to host | `0x11` | empty: end of standard input |
| client to host | `0x12` | empty: kill the process tree |
| client to host | `0x13` | a piece of the compressed standard input |

Standard output and standard error have a compression context each, and standard input a third; a `0x02` frame ends the output context, `0x03` the error one and `0x10` the input one. The host sends `started`, then output as the process writes it, then `exit`. `exit` and `error` are the last frame the host sends. `killed` is true whenever the host ended the tree: a `kill` frame, a closed stream, or the deadline; `timed_out` says it was the deadline (`timeout_ms`). `code` is absent when a signal ended the process. A request the host refuses is one `error` frame (`bad_request`, `unsupported`, `denied`, `not_found`, `busy`). Standard input sent for `stdin: "closed"` is ignored. The tree is ended when the stream closes, on every path.

## Compression

Compression is per stream and per direction, asked for in the request, and only on `bohio-read/1` (what the host sends), `bohio-write/1` (what the client sends) and `bohio-exec/1` (output and standard input). The control stream and shells are never compressed, and a peer that never asks never sees a compressed frame (a reader skips a tag it does not know).

```text
compression = { algorithm: "zstd" / "lz4" / "deflate" / "none" / "auto" / text,
                ? level: opt<int>,          ; the algorithm's own scale; its default when absent; clamped by the host
                ? window_log: opt<uint> }   ; zstd: the window as a power of two; clamped by the host
```

**What a request gets.** What the host *sends* (read data, exec output): the client asks; the host replaces `auto` by its `[compression] default`, clamps `level` to its `max_level` and `window_log` to its `window_log_max`, and reports what it used in the first frame (`read-header.compression`, `started.output_compression`; never `auto`). It refuses an algorithm it does not allow, or that its build lacks, as `unsupported`. What the client *sends* (write data, standard input): the client names a concrete algorithm, which the host must allow (`unsupported` otherwise; `auto` is `bad_request`), and compresses within the host's `info.transfer.compression` levels (a host does not need the level: it decodes). A client reads `info.transfer.compression` and `compression_default` to choose.

**The algorithms** are the standard formats, so any language decodes them with its own library:

| Name | Stream | Notes |
| --- | --- | --- |
| `zstd` | zstd frames (RFC 8878): the pieces of one context are the flushed blocks of one frame, so a piece alone is not a frame | levels from the negative fast ones to 22; `window_log` sets the window; a decoder refuses a frame whose window is above the bound it runs under |
| `lz4` | LZ4 frame format (independent blocks, no checksums): the pieces of one context are the blocks of one frame, the header on the first | no levels; the cheapest, for hosts whose CPU cannot afford more |
| `deflate` | a zlib stream (RFC 1950), a sync flush at the end of each piece | levels 0 to 9; what a browser's `DecompressionStream("deflate")` and Python's `zlib` decode |

**Pieces and the raw frame.** A sender compresses each frame of data with the context of its stream direction and flushes, so every piece decodes completely as it arrives. When a piece is not smaller than its input the sender sends the input as a *raw* frame (`0x02` for read data, `0x10` for write data, `0x02`/`0x03`/`0x10` for exec) instead. **A raw frame ends the compression context:** the next compressed piece begins a new zstd frame, a new LZ4 frame (header included) or a new zlib stream, and a receiver resets its decoder on every raw frame it receives. Every implementation must follow this, since a decoder in the middle of an old stream cannot read a new one. A stream is therefore any mix of raw and compressed frames, and data that does not compress (an archive, a photograph) costs the sender one failed attempt per frame and the link nothing.

A host also decides per file for reads: a file whose extension is in `[compression] skip_extensions`, whose path or name matches `exclude` (key and certificate patterns by default), or whose first `sample_bytes` compress to more than `sample_ratio` of their size is sent raw from the first frame (the header then has no `compression`).

**Bounds.** A reader (the client of a read) stops with a protocol error when the decompressed bytes would pass `read-header.length`, and refuses a stream asking for a window above its own bound. A host decoding what a peer sends (a write, standard input) holds it to `[compression] window_log_max` and to `max_expansion` (the decompressed bytes may not pass that many times the compressed bytes received, checked once more than 1 MiB has been produced; `0` turns it off); a stream past either ends with `bad_request`. A write is bounded by the disk and the expansion check alone, so a host that accepts files of zeros sets `max_expansion = 0` knowingly.

**One context per direction, never shared.** The context of one stream direction is never reused for another stream or direction, so what a client sends and what it reads never mix in one dictionary window.

**Prepared reads.** With `prepare: true` the host compresses the whole range into a file first, in one context at the requested level, and sends `progress` frames (`0x08`) at least every `idle_timeout / 4` while it works, so neither side's idle timer runs out. It then sends the header with `compressed_length` and `token`, and the compressed stream in `0x06` frames cut anywhere (a decoder takes it in any pieces), then `0x04`. The token names the file, which only the peer that made it can open and which expires after `[compression] prepare_ttl`; `resume: {token, offset}` (with the same `path`) from the same peer sends the same header and the compressed stream from that compressed offset. A client that resumes holds the first part itself and decodes the whole stream once it has it, since a decoder cannot start in the middle. A result past `prepare_max_bytes` is `busy`, and a host without a prepare directory answers `unsupported`.

## Shells

`bohio-shell/1`. One stream is one terminal session. The open's metadata is a CBOR `shell-request`.

```text
shell-request = { program: program, ? cwd: opt<text>, ? env: env, ? cols: uint, ? rows: uint }   ; 80 by 24 by default, at most 32767
program       = { type: "kind", kind: shell-kind } / { type: "argv", argv: [+ text] }
size          = { cols: uint, rows: uint }
shell-exit    = { ? code: opt<int>, ? signal: opt<int>, ? killed: bool }
```

`started` is `{pid: uint}` as in exec, and `env` is the exec `env`.

| Direction | Tag | Body |
| --- | --- | --- |
| host to client | `0x01` | CBOR `started` |
| host to client | `0x02` | raw terminal output, escape sequences included |
| host to client | `0x04` | CBOR `shell-exit`: after every output byte |
| host to client | `0x05` | CBOR `error-body` |
| client to host | `0x10` | raw terminal input (Enter is `\r`) |
| client to host | `0x11` | CBOR `size`: resize the terminal |
| client to host | `0x12` | empty: kill the shell's tree |

`started` is the first frame; `shell-exit` and `error` are the last the host sends. The terminal merges standard output and error. The session's tree is ended when the stream closes.

## Forwarding: `bohio-connect/1` and `tcp/1`

One stream is one connection that the host makes, to a target the peer names, from the host's own network position. The open's metadata is a CBOR `connect-request`; the host answers with one frame, and after a `connected` the stream carries the connection's bytes both ways.

```text
connect-request = { target: target }
target          = { type: "tcp", host: text, port: uint }     ; a name, or an address (an IPv6 one without brackets); port 1 to 65535
                / { type: "unix", path: text }                 ; a Unix domain socket on the host, an absolute path
                / { type: "pipe", name: text }                 ; a Windows named pipe, NAME of \\.\pipe\NAME
connected       = { ? address: opt<text> }                     ; the address a TCP connection reached, as "ADDR:PORT"; null for a socket or a pipe
```

| Direction | Tag | Body |
| --- | --- | --- |
| host to client | `0x01` | CBOR `connected`: the connection is made |
| host to client | `0x05` | CBOR `error-body`: nothing was connected; the last frame |
| either way, after `0x01` | none | raw bytes of the connection, one message per chunk |

The client sends nothing before `0x01`. After it both sides send raw bytes with no tag: a `finish` of the stream is a half-close (the host shuts down its write side of the connection, or the client's input ends), the target's end becomes the host's `finish`, and a stream that ends any other way (a close, a revoke, a lost link) resets the connection, so a transfer that was cut short never reads as finished. A message is at most `[forward] chunk_size` bytes. A `serial` target kind is reserved; a request that names an unknown kind is `bad_request`. On a Windows named pipe there is no half-close: a `finish` ends both directions, so a reply the pipe's server would send after the client's end is lost, which a server that answers each message does not mind.

The error codes of a refused connect: `denied` (the grant stopped covering the target between the open and the answer, or the operating system refused the connection), `unsupported` (`[forward] enabled = false`, or a kind this host does not offer: `unix` on Windows, `pipe` on Unix), `busy` (`[forward] max_connections` or `max_connections_per_peer`), `refused` (the target refused the connection, or a Unix socket exists and nothing listens on it), `unreachable`, `timeout` (each address its name resolves to is tried for `[forward] connect_timeout`), `not_found` (a name that does not resolve, a socket path or a pipe that does not exist) and `bad_request`.

**The grant decides before anything connects.** The peer needs `forward.connect` and a rule of its `[peer.<id>.forward] connect` list that matches the target. A TCP rule is `HOST:PORT`, `HOST` an exact name (lowercase), `*.suffix` (names under it, not the suffix itself), `*` (any target), an address or a CIDR network (an IPv6 one in brackets) and `PORT` a number, a range `A-B` or `*`; a `unix:PATH` or `pipe:NAME` rule matches the text exactly, or with `*` and `?`, which match within one path component. Nothing is resolved to decide: an address or network rule matches only an address target and a name rule only a name target, so `localhost:80` and `127.0.0.1:80` are different targets, and `*` is what covers both. A path with a `..` part never matches. A target the grant does not cover is refused as the stream opens, as any protocol the peer may not use is: the client sees the stream refused (a `Permission` failure) and the host connects to nothing. The grant is asked again at every `[link] recheck`; a peer that loses it has its running connections reset and hears `revoked`.

`tcp/1` is `coqui-link`'s own forward and the host serves it as that crate specifies, under the same grant and `[forward]` bounds, so a `coqui-link` peer (`coqui_link::forward::connect` or `listen`) uses a Bohío host unchanged: the open's metadata is the text `HOST:PORT`, there is no answer before the bytes, and a refusal or a failed connect is a close with a reason. `bohio-connect/1` is what Bohío's client opens, because its `connected` answer is what a SOCKS or `--check` client needs and `tcp/1` has none.

## Desktop: `bohio-gui/1`

One stream is one view of one desktop session. The open's metadata is a `gui-request`; the host answers with `ready`, or with an `error` frame and the end. A client then sends requests, each with a `seq` of its own, and the host answers each with frames that carry the same `seq`. A request that fails is a `request_error` and the stream goes on. Coordinates are physical pixels of one display with its top-left corner as the origin; `display` defaults to the primary one.

```text
gui-request = { ? session: opt<text>, ? want: [+ ("view" / "input" / "clipboard")] }   ; want defaults to ["view"]; a wanted item the grant lacks is "denied"
display     = { id: text, ? name: opt<text>, ? primary: bool, x: int, y: int, width: uint, height: uint, ? scale: float, ? rotation: opt<uint> }
state       = { ? active: bool, ? locked: opt<bool>, ? displays: [* display] }
ready       = { session: text, granted: [* ("view" / "input" / "clipboard")], state: state }
rect        = { x: uint, y: uint, width: uint, height: uint }
snapshot    = { seq: uint, ? display: opt<text>, ? region: opt<rect>, ? format: opt<"png" / "jpeg" / "webp">, ? quality: opt<uint>,
                ? max_width: opt<uint>, ? max_height: opt<uint>, ? cursor: bool, ? settle: opt<{ ? quiet_ms: opt<uint>, ? timeout_ms: opt<uint> }> }
image       = { seq: uint, display: text, format: text, width: uint, height: uint, region: rect, scale: float,
                ? cursor: opt<{ x: int, y: int }>, length: uint, ? settled: opt<{ settled: bool, waited_ms: uint }> }
input       = { seq: uint, events: [+ input-event] }
input-event = { type: "move", ? display: opt<text>, x: int, y: int }
            / { type: "button", button: "left" / "right" / "middle" / "back" / "forward", ? action: "press" / "release" / "click",
                ? count: opt<uint>, ? display: opt<text>, ? x: opt<int>, ? y: opt<int> }
            / { type: "scroll", ? display: opt<text>, ? x: opt<int>, ? y: opt<int>, ? dx: int, ? dy: int }   ; wheel notches; positive dy scrolls the content down
            / { type: "key", key: text, ? action: "press" / "release" / "click" }   ; a name of the list below, or exactly one character
            / { type: "text", text: text }
            / { type: "wait", ms: uint }
input-done  = { seq: uint, applied: uint }
settle      = { seq: uint, ? display: opt<text>, ? region: opt<rect>, ? quiet_ms: opt<uint>, ? timeout_ms: opt<uint> }
settled     = { seq: uint, settled: bool, waited_ms: uint }
clipboard   = { seq: uint, text: text }   ; the host's answer to both clipboard requests (the text is empty for a write)
request-error = { seq: uint, code: error-code, ? message: text }
```

Key names, snake case: `enter`, `tab`, `escape`, `backspace`, `delete`, `insert`, `home`, `end`, `page_up`, `page_down`, `up`, `down`, `left`, `right`, `space`, `shift`, `control`, `alt`, `meta`, `caps_lock`, `print_screen`, `menu` and `f1` to `f24`; any other key is one character. An unknown name is `bad_request`.

| Direction | Tag | Frame | Body |
| --- | --- | --- | --- |
| host to client | `0x01` | `ready` | CBOR `ready`, first |
| host to client | `0x02` | `image` | CBOR `image`; the image's `length` bytes follow in `0x03` frames |
| host to client | `0x03` | `data` | raw bytes of the image |
| host to client | `0x05` | `error` | CBOR `error-body`; the last frame |
| host to client | `0x06` | `input_done` | CBOR `input-done` |
| host to client | `0x07` | `settled` | CBOR `settled` |
| host to client | `0x08` | `clipboard` | CBOR `clipboard` |
| host to client | `0x09` | `state` | CBOR `state`, whenever it changes |
| host to client | `0x0b` | `request_error` | CBOR `request-error`; the stream goes on |
| client to host | `0x10` | `snapshot` | CBOR `snapshot` |
| client to host | `0x14` | `input` | CBOR `input` |
| client to host | `0x15` | `settle` | CBOR `settle` |
| client to host | `0x16` | `clipboard_get` | CBOR `{ seq: uint }` |
| client to host | `0x17` | `clipboard_set` | CBOR `{ seq: uint, text: text }` |

`0x04`, `0x0a`, `0x11` to `0x13` are reserved for a screen stream. A frame with another tag is skipped.

The host decides the open in this order: the request (`bad_request`), `[gui] enabled` and a desktop (`unsupported`), each wanted item against the grant (`denied`; the link refuses an open the grant does not cover before the engine sees it), the session (`not_found`), a free slot (`busy`), and, with `[gui] approval = "local"`, the person at the desk (`denied`, "not approved"). Each request is then decided by the grant as it is at that moment, so a narrowed grant refuses the next frame at once and ends the stream at the next `[link] recheck`.

- **Snapshot.** The host captures the display or the region, scales it down with the same proportions to fit `max_width` and `max_height` (never up) and encodes it. `scale` is image pixels per display pixel. `png` and `webp` (lossless) reproduce the pixels exactly; `jpeg` takes a `quality`. A region outside the display, or with more pixels than `[gui] max_snapshot_pixels`, is `bad_request`. With `settle` the host waits first, as below.
- **Input.** The events are applied in order and `applied` counts them. A frame holds at most `max_batch_events` events, its `wait` events add up to at most `max_batch_wait`, a `text` event holds at most `max_text` characters, and a stream sends at most `max_input_rate` events a second: a frame that would pass the rate is `busy` and none of it is applied. A click counts as its `count`. While the session is not at the screen, or the screen is locked and `input_when_locked` is false, input is `unavailable`.
- **Settle.** The host captures every `settle_interval`, compares the 64 by 64 pixel tiles (XXH3) and answers when no more than `settle_ignore_tiles` tiles changed between captures for `quiet_ms`, or with `settled: false` at `timeout_ms`: one round trip, not one per poll.
- **Clipboard.** Text only, at most `clipboard_max_bytes`; each read is an event the owner hears.

`info` reports the desktop in `gui`:

```text
gui-info = { ? enabled: bool, ? available: bool, ? reason: opt<text>, ? backend: opt<text>, ? input: bool, ? clipboard: bool,
             ? lock_detection: bool, ? formats: [* text], ? sessions: [* gui-session] }
gui-session = { id: text, ? user: opt<text>, ? active: bool, ? locked: opt<bool>, ? displays: [* display] }
```

It is asked again at every `info`. `sessions` and `user` are sent only to a peer that holds `gui.view` or `gui.input`. A host that already runs a VNC server is reached with `forward -L 5900=127.0.0.1:5900` under a `forward.connect` rule for `127.0.0.1:5900`; nothing here builds on it.
## Remote forwards: `bohio-listen/1` and `bohio-accept/1`

A remote forward has the host listen for the peer and hand over each connection it takes: the peer's own side connects it to
something there (`ssh -R`), or to its standard input and output. Two protocols make it. The peer opens `bohio-listen/1` once,
and that stream is the listener: it lives exactly as long as the stream does. The peer then opens one `bohio-accept/1` per
connection the host announces, and that stream carries the connection's bytes. The peer is always the one that opens streams, so
a controller that serves nothing and grants nothing can use it.

```text
listen-request = { bind: bind, ? backlog: opt<uint> }
bind           = { type: "tcp", host: text, port: uint }       ; an IP address (an IPv6 one without brackets); port 0 lets the host choose
               / { type: "unix", path: text }                   ; a Unix domain socket the host creates
               / { type: "pipe", name: text }                   ; a Windows named pipe, NAME of \\.\pipe\NAME
listening      = { address: text, token: text }                 ; `address`: what was bound, "ADDR:PORT", "unix:PATH" or "pipe:NAME"; `token`: 128 random bits in hex
arrival        = { id: uint, ? source: opt<text> }              ; `source`: "ADDR:PORT" of a TCP connection, null for a socket or a pipe
accept-request = { token: text, id: uint }
```

| Stream | Direction | Tag | Body |
| --- | --- | --- | --- |
| `bohio-listen/1` | host to client | `0x01` | CBOR `listening`: the listener is bound; the first frame |
| `bohio-listen/1` | host to client | `0x02` | CBOR `arrival`: a connection arrived and waits to be claimed |
| `bohio-listen/1` | host to client | `0x05` | CBOR `error-body`: nothing was bound, or the listener failed; the last frame |
| `bohio-accept/1` | host to client | `0x01` | CBOR `connected` (as `bohio-connect/1`; `address` is the arrival's `source`) |
| `bohio-accept/1` | host to client | `0x05` | CBOR `error-body`; the last frame |
| `bohio-accept/1` | either way, after `0x01` | none | the connection's bytes, raw, a `finish` a half-close, any other end a reset |

The client sends nothing on a listen stream and nothing on an accept stream before `0x01`. The host decides the bind from the
peer's grant before it binds anything: the peer needs `forward.listen` and a rule of its `[peer.<id>.forward] listen` list that
matches. A TCP rule is `BIND:PORT`, `BIND` an address (an IPv6 one in brackets; `0.0.0.0` and `::` too), a CIDR network or `*`,
and `PORT` a number, a range `A-B` or `*`; the port `0`, a host-chosen one, matches only a rule that names `0` or `*`, so
`127.0.0.1:18090-18099` allows those ports and no host-chosen one. A `unix:PATH` or `pipe:NAME` rule matches the text exactly,
or with `*` and `?`, which match within one path component, and a path with a `..` part never matches. Nothing is resolved: a
bind is an address, and a name there is `bad_request`. A bind no rule covers is refused as the stream opens, as a protocol the
peer may not use is, so the client sees the stream refused and nothing listens. The grant is asked again at every
`[link] recheck`: when it stops covering the bind, the host closes the listener, closes the connections still waiting and
resets the ones already handed over, and the peer hears `revoked` on every stream.

**Binding.** A TCP listener binds the address the request names, so a request for `127.0.0.1` is loopback and one for `0.0.0.0` is
every interface of the host; the bind is the peer's, within its rule. A Unix socket is created with `[forward] socket_mode`
(`0o600`: the host's own account) and a Windows pipe with the access list `coqui-transport-pipe` gives its creator alone;
whichever the mode, `[forward] ipc_same_user` (default on) closes, unread, a connection to a socket or pipe listener from
another account, as the operating system names it. An address, socket or pipe that is taken is `in_use`; a bind past
`[forward] max_listeners` or `max_listeners_per_peer` is `busy`; a kind the host does not offer is `unsupported`.

**Arrivals.** Each connection the listener takes is held and announced with `0x02`, its `id` counting up from 1 within the
listener. The peer claims it with `bohio-accept/1` and the listener's token, and the host answers `connected` and carries the
bytes as `bohio-connect/1` does. At most `[forward] max_pending` arrivals wait for one listener (a request's `backlog` may lower
that for its own); a connection past it is closed at once, and one nobody claims within `[forward] accept_timeout` is closed. A
claim is answered `not_found` for a token that is unknown, an id that is unknown, already claimed or expired, and a token that
belongs to another peer, in the same words each time, so a peer learns nothing of another's listeners. A claim is a forwarded
connection like a connect and holds one of `max_connections` and `max_connections_per_peer` while it runs. A Windows named pipe
has no half-close, so a `finish` there ends both directions, as it does for `bohio-connect/1`.

A client that cannot bridge an arrival (its local target refused) still claims it and closes it, so the connection to the host
ends at once instead of waiting out `accept_timeout`. The listener's end by the peer (closing the listen stream) or the host (a
revoke, or a failure it reports with `0x05`) ends the connections it handed over: a stream of `bohio-accept/1` that is still
running ends with the same reason.

### Retrying an open

Nothing on the wire says "try again", and the client does not need it to: a host that is full answers in the words it always had, and a client that was built to wait tells them apart. A host over `[link] max_pending_opens`, `max_streams`, `max_per_protocol` or `max_opens_per_minute` closes the new stream with the reason `refused` and the text `over a limit` (`coqui-link`'s words; a grant's refusal has no such text). A client that sees that, a link whose queue is full when the open is sent, or an answer `busy` to `bohio-connect/1` or `bohio-accept/1`, opens the same stream again every `[forward] open_retry_interval` for `[forward] open_retry`, and only then fails the connection it was opening. A `denied`, a revoke or any other error is final at once, and so is a `busy` answer to `bohio-listen/1` (a kept `remote` entry asks again for the port the host chose, with the `[dial]` waits).

## Capability negotiation

Nothing is negotiated before a stream opens, and nothing falls back. A client learns what a host does in this order:

1. **The link's `link/1` stream** answers what this peer may open now (`offers`), the authoritative list of protocol tokens, as coqui-link specifies: a token is offered when the answering side's grant for this peer holds a permission it needs. A client does not open a token that is not offered. Either side of a link may ask, and see [Roles](#roles).
2. **`bohio/1` `hello`** with the client's protocol version; the reply is the version both speak. This page is version 1.
3. **`bohio/1` `info`** (a peer that holds `info`) reports the host's operating system and architecture, its shells, what its filesystem supports (`symlinks`, `unix_modes`, `windows_attributes`, `case_sensitive`, `separator`), whether `exec` and `shell` are enabled, its limits, and `forward` (whether it forwards and the kinds of target it connects to).

A client reads `info` and adapts: it takes the path separator from `filesystem.separator`, does not send `set_mode` to a host without `unix_modes`, and picks a shell from `shells`. Every field has a default and unknown fields are ignored, so a client works with a host that reports less or more than it expects. A new protocol is a new token with its own version (`bohio-exec/2`); a host serving both offers both.

## Roles

A link has two ends and no fixed roles: either side may open streams on it, and either may answer them. Which side dialed (the carrier's direction) decides nothing; a firewalled host can dial out and be controlled over the link it made. Each side hosts what it offers and controls what it opens:

- **What a side offers a peer** is the `offers` of the `link/1` document that side answers: the protocol tokens for which that peer's current grant holds a permission (`bohio/1` for `info`, `fs.read` or `fs.write`; `bohio-exec/1` for `exec`; `bohio-shell/1` for `shell`; `bohio-read/1` and `bohio-hash/1` for `fs.read`; `bohio-write/1` for `fs.write`; `bohio-connect/1` and `tcp/1` for `forward.connect`; `bohio-gui/1` for `gui.view` or `gui.input`). A node that grants a peer nothing offers it nothing, and may still open streams on that peer.
- **How a side learns its role** is by opening `link/1` on the link and reading the peer's `offers`. The intersection of those tokens and what the side wants to do is what it can do on that link; opening a token that is not offered is refused (a `Permission` failure), as it always was.
- **Re-evaluation**: `offers` is computed each time the document is read, and admission is decided at every open and every `[link] recheck`, both from the peer's grants as they are then, so a grant added or removed on a live link changes the next document and ends what it no longer covers. Nothing is pushed to the peer.
- **Nothing is stated on the wire** beyond `link/1`: there is no request for a role, and what a side asks for is only what it opens. A peer that is only a controller offers nothing and opens streams; a peer that is only a host offers its tokens and opens none; either can do both on one link.

## Local control

A running node holds links, including links a peer dialed, and only the node can open streams on them. It therefore listens on a local endpoint (`[control] endpoint`, a `pipe://` one, by default `pipe://bohio-` and the first 16 hex digits of the node's id, this user alone) and serves three protocols there: `bohio-via/1` and `bohio-links/1` reach its links, and `bohio-pair/1` (below) arms the node's pairing PIN. They are never served on a peer's link and are not offered in a peer's `link/1` document. The link on this endpoint is a mux without the proof or the seal: who can connect is the boundary, as for the key file, and with no `socket_mode`, `socket_group` or `security_descriptor` set a client that the system does not say is this user is closed unread. A client checks the other way before it sends anything: with no access settings in its `[control]`, an endpoint the system does not say is served by the client's own account is refused, since another account could have bound the name first.

```text
via-request   = { peer: text, protocol: text, ? metadata: bstr }   ; metadata defaults to empty
pair-request  = { ? lifetime_ms: opt<uint> }                       ; empty metadata is the same as {}
pair-reply    = { armed: { pin: text, lifetime_ms: uint } }        ; first, always
              / { paired: { peer: text } }                         ; then one of these two, then the end
              / { ended: { reason: text } }
links-request = { ? wait_ms: opt<uint> }                           ; empty metadata is the same as {}
links-reply   = { links: [* link-entry] }                          ; oldest first
link-entry    = { peer: text, side: "dialed" / "answered", ? endpoint: opt<text>, ? source: opt<text>,
                  since: uint, ? mine: [* text], ? theirs: opt<[* text]> }
```

`bohio-via/1`: the open's metadata is a `via-request`. The node opens `protocol` with `metadata` unchanged on its oldest link that is up to `peer` and passes every message both ways unchanged, so the local client speaks that protocol exactly as it would over a direct link. What the peer grants this node decides the stream: a refusal or a revoke by the peer closes the local stream with the same reason (`refused`, `revoked`, `superseded`) and text. A peer the node has no link to is `refused` with the text "no link to that peer".

`bohio-links/1`: the open's metadata is a `links-request`; the node sends one `links-reply`, then finishes the stream. `mine` is what this node offers that peer now, `theirs` what the peer's `link/1` document offered when read just now (absent when it did not answer within `wait_ms`, default the node's `[link] proof_timeout`), `since` is in seconds since the Unix epoch, and `side` says which end dialed.

`bohio-pair/1`: the open's metadata is a `pair-request`. The node arms one PIN, in memory, replacing the one armed before (that request then ends as `replaced`), and sends `armed` with the PIN and how long it lasts (`lifetime_ms` of the request, else `[pairing] timeout`). A joiner on any of the node's listeners may then try it; when a peer pairs the node writes its `[peer.<id>]` table, holding nothing, and sends `paired`; otherwise it sends `ended` with why (expired, used up by `max_attempts` wrong guesses, replaced, or withdrawn) and finishes the stream. A client that closes the stream withdraws the PIN. A node that cannot pair answers `ended` at once. The PIN is in no log or event.

`bohio-gui-approve/1`: the node holds the requests of peers waiting for the person at the desk (`[gui] approval = "local"`). The stream opens with no metadata; the node sends `pending` at once, and again whenever the list changes, and answers each `decide` with `decided`.

```text
pending-request = { id: uint, peer: text, session: text, want: [* ("view" / "input" / "clipboard")], since: uint }
approve-output  = { pending: { requests: [* pending-request] } } / { decided: { id: uint, found: bool } }
approve-input   = { decide: { id: uint, allow: bool } }
```

`found` is false for a request that timed out or was answered already. An allowed peer is not asked again on that session for `[gui] approval_ttl`; a denied or unanswered request ends the peer's open with `denied` and the text "not approved". `bohio gui pending`, `allow ID` and `deny ID` speak it.

A joiner on a listener sends one message first, `coqui-link/pairing/1`, which is neither a Noise first message (32 bytes) nor a WebCrypto one (JSON): the node reads the first message of every connection and runs a pairing on that marker and a proof on anything else.

## Vectors

`vectors/control.json`, `exec.json`, `shell.json`, `read.json`, `write.json`, `hash.json`, `forward.json`, `gui.json`, `listen.json` and `accept.json` hold one entry per message; `vectors/delta.json` holds the [weak sums, a signature and a stream of steps](#deltas) in its own shape. Each file is:

```json
{ "protocol": "bohio-exec/1", "encoding": "...", "vectors": [ { "name": "stdout", "direction": "host_to_client", "kind": "frame",
    "type": "exec.Output", "hex": "0268656c6c6f0a", "value": { "tag": 2, "data_hex": "68656c6c6f0a" } } ] }
```

| Field | Meaning |
| --- | --- |
| `name` | what the vector shows |
| `direction` | `client_to_host` or `host_to_client`; an open's metadata is `client_to_host` |
| `kind` | `open` (the open's metadata), `message` (a control message) or `frame` (a tag byte, then the body) |
| `type` | the message's name in this page: `control.Request`, `control.Response`, `exec.Request`, `exec.Output`, `exec.Input`, `shell.Request`, `shell.Output`, `shell.Input`, `fs.ReadRequest`, `fs.ReadOutput`, `fs.ReadInput`, `fs.WriteRequest`, `fs.WriteOutput`, `fs.WriteInput`, `hash.Request`, `hash.Output`, `hash.Input`, `forward.ConnectRequest`, `forward.Answer`, `forward.ListenRequest`, `forward.ListenFrame`, `forward.AcceptRequest` |
| `hex` | the stream message exactly as it travels |
| `value` | its meaning: the decoded CBOR as JSON for a message; for a frame, `tag`, and `message` (the CBOR body as JSON) or `data_hex` (the raw body) |
| `decode_only` | the bytes are not what a writer produces (a message with a key this version does not know): a decoder accepts them and finds `value`, and a test does not compare a re-encoding |
| `error` | a decoder refuses these bytes: `tag` (an unknown tag byte) or `short` (an empty frame) |

An implementation decodes every `hex`, compares the result with `value` (a map compares without regard to key order, and an absent optional key equals `null`), and, for a vector that is not `decode_only`, may encode `value` again and compare the bytes: the Rust writer is deterministic, so a writer that wants the same bytes writes keys in the order the vectors show. A reader must not depend on the order.

The Rust tests (`crates/bohio-proto/tests/vectors.rs`) build the vectors from the types, rewrite a file that differs, then decode every entry from its hex. A clean working tree after `cargo test` means the vectors are current.
