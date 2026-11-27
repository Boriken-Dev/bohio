# Profiles

A profile is a named starting point for the transfer settings: a table of defaults that sits under your configuration
files. It is never a constraint. A key that any file sets wins over the profile, key by key, and a key that neither sets is
the built-in default. A profile sets no `[peer.*]` table and fixes no key.

A host and a client choose separately. The host's profile (`[profile] name`) sets its defaults and caps; the client's
(`bohio remote --profile`, or `[client] profile`) sets what it asks for per request. They need not match, and a client that
asks for more than a host allows is clamped by the host, which says what it used.

| Profile | For | Host defaults | Client asks |
| --- | --- | --- | --- |
| `default` | nothing in particular | the built-in values | nothing compressed, one stream, the link's own windows |
| `lan` | a local network | larger frames, a larger bulk window and one compression context for a bundle | no compression, two streams |
| `weak-host` | a machine that cannot compress or hash much | the cheap compressor, low caps, one hash and one walk at a time | lz4, a cheap digest, one stream |
| `slow-link` | a slow or distant path | the best ratio the host can make, long windows, a growing window | zstd level 19 with a long window, a digest to compare, four streams |
| `fat-link` | a long path with room in it | a larger bulk window | an 8 MiB bulk window |

## Starting from one

Name it in the file, on the host and on the client:

```toml
[profile]
name = "slow-link"      # the host's defaults and caps

[client]
profile = "slow-link"   # what `bohio remote` asks for; `remote --profile NAME` overrides it
```

`examples/profiles/` has one file for each profile. The highest layer that sets `[profile] name` decides the profile (a user
file can name another than the system file), and an unknown name is refused naming `profile.name` and the names there are.

`bohio config check` prints every transfer setting as `key = value (source)`, where the source is `file PATH`, `profile NAME`
or `default`, so you can see what a profile changed and what you did. `--format json` prints the same as objects
(`{"key", "value", "source"}`) under `transfer`.

## Overriding one

Set the key. This keeps `slow-link` and widens only the window it grows to:

```toml
[profile]
name = "slow-link"

[link]
max_window = 33554432   # 32 MiB, in bytes
```

A value the profile supplies is checked with the rest, and when it conflicts with what your file sets, the error names the
layer: `profile slow-link` for a value only the profile supplied, otherwise your file. A weaker or stronger choice than the
profile's is never an error; only a meaningless value is. A `max_window` only the profile supplies is a default and not your
bound: a `window` or `bulk_window` your file raises past it raises it too, while a `max_window` your file sets is yours and a
larger window is refused.

A flag of `bohio remote` (`--compress`, `--delta`, `--compare`, `--parallel`, `--prepare`) wins over the client profile, and
`--compress none` turns a profile's compression off for one run. A key the file sets itself (`[client] parallel_streams`,
`[link] bulk_window`) wins over the client profile too.

## The tables

The host values below are in [`bohio::profile`](https://github.com/Boriken-Dev/bohio/blob/main/crates/bohio/src/profile.rs).
Each is a starting point, and the "why" is what it costs.

### `lan`

| Key | Value | Why |
| --- | --- | --- |
| `[filesystem] chunk_size` | 256 KiB | fewer, larger frames when a round trip costs a millisecond |
| `[link] bulk_window` | 4 MiB | one bulk stream is no longer held to the 1 MiB window |
| `[link] max_window` | 8 MiB | the most a stream's window may grow to |
| `[compression] bundle_context` | `shared` | the files of a bundle compress against each other, so many small similar files shrink more; on a trusted network the size one file leaks about the one before it is not a risk (the host logs `non-default:`; see [Security](security.md)) |

Compression is otherwise left as it is: nothing is compressed unless a client asks, and a LAN client asks for none. The host's
`[compression] default` (what a request for `auto` means) stays zstd level 3, because it must name an algorithm.

### `weak-host`

| Key | Value | Why |
| --- | --- | --- |
| `[compression] allowed` | `["lz4", "zstd"]` | no deflate: a host that cannot afford much does not offer the expensive choices |
| `[compression] default` | lz4 | the cheap one is what `auto` means |
| `[compression] max_level` | 3 | a higher level takes more CPU and memory than this host has |
| `[compression] window_log_max` | 20 (1 MiB) | a decoder holds about one window per stream |
| `[compression] workers` | 0 | compress in the stream's own thread |
| `[hash] max_streams`, `[walk] max_streams` | 1 | one pass over the disk at a time |
| `[filesystem] max_transfers` | 4 | fewer transfers at once |
| `[link] max_window`, `auto_window` | 1 MiB, off | no stream's window grows past the default |
| `[delta] max_signature_bytes` | 8 MiB | a smaller signature may be held per delta read |

`allowed` names zstd and lz4, so a build without either refuses this profile, naming `compression.allowed` and the layer
`profile weak-host`.

### `slow-link`

| Key | Value | Why |
| --- | --- | --- |
| `[compression] default` | zstd level 19 | the best ratio, at tens of MiB and a fraction of a core's speed |
| `[compression] window_log_max` | 27 (128 MiB) | repeats far apart are found; a decoder holds that much for each stream |
| `[compression] prepare_max_bytes` | 4 GiB | a prepared read of a large file fits |
| `[link] bulk_window`, `max_window` | 4 MiB, 16 MiB | a bulk stream moves more than a window per round trip |
| `[link] auto_window` | on | the window grows to fill the path |
| `[link] max_opens_per_minute` | 6000 | a sync that opens a stream per file is not held to ten a second |
| `[filesystem] resume_ttl` | 1 hour | an interrupted replace can continue |

The client asks `zstd` level 19 with `window_log` 27 (the host clamps it to its own `window_log_max`), prepares a compressed
read of a file over 16 MiB in full first, uses a delta where it applies, compares by blake3 and carries a file on four
streams. The memory is the cost: a 128 MiB window on each stream, on both ends.

### `fat-link`

| Key | Value | Why |
| --- | --- | --- |
| `[link] bulk_window` | 8 MiB | one bulk stream moves at most its window per round trip |

A path with a round trip of 267 ms and 200 Mbit/s holds about 6.7 MB in flight, which is the window a stream needs to fill
it; the default 1 MiB window limits one stream to about 4 MB/s on such a path. Nothing else changes, and nothing is compressed
unless asked. A path with a larger bandwidth-delay product wants a larger `bulk_window`, set in your file.

`bulk_window` is what a side grants a bulk stream it *receives*: a client's reads and a host's writes. Set the profile on
both ends, or the same key on both, for transfers in both directions. It costs that much memory for each bulk stream whose
reader falls behind.

## Where each was measured

| Profile | Link | Result |
| --- | --- | --- |
| `fat-link` | an in-process emulation of the `fat` path (133 ms each way, 200 Mbit/s), Windows ARM64, a loaded machine, 3 samples | a 64 MiB warm read took a median 17.9 s at the default settings and 3.0 s with `link.bulk_window` 8 MiB (`benchmarks/results/latency-781323cd-fat.json`, `latency-781323cd-fat-bulk8MiB.json`); the harness's own sanity check, not a measurement of record. The same read with `bohio-bench local --profile fat-link` and with `--profile default`, one after the other on the same machine, took 3.0 s and 17.8 s (`benchmarks/results/profiles-564c570e-fat-fat-link.json`, `profiles-564c570e-fat-default.json`) |
| `lan`, `weak-host`, `slow-link` | not yet measured on their links | the values are starting points; the bench workflow's runs of `lan`, `weak` and `slow`/`wan` are what will confirm or move them |

`bohio-bench local ... --profile NAME` runs the host under the profile and takes this side's link bounds and compression
request from it; a `--set` key wins over the profile.
