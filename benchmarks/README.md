# Benchmarks

Bohío's transfer benchmarks: a harness (`crates/bohio-bench`), emulated links, a raw TCP control and
committed results. They run on demand, never per push.

## Run it

```bash
cargo build --release -p bohio -p bohio-bench && cargo build --release -p bohio --example exec_helper
target/release/bohio-bench corpus --dir target/bench-corpus
target/release/bohio-bench local --corpus target/bench-corpus \
    --scenario connect --scenario read_16MiB_cold --samples 3 --save target/bench-local.json
```

`local` starts `bohio serve` on `127.0.0.1:0` with a temporary data directory, pairs its own key through
`bohio pair` and `bohio join`, approves it with `bohio peer approve`, runs the scenarios and removes
everything. It never writes a peers file or the trust store itself, so the same harness drives any layout
of the grants. `--set link.window=4MiB` puts a `[link]` or `[filesystem]` key in the host's configuration
and, for the link bounds, in this side's `Settings` too: both ends of a link must agree on `window` and
`max_frame`.

`local --wan DELAY_MS[:MBIT]` (`--wan 133:20` is `netem.sh`'s `wan`) puts an in-process proxy between this side and the host that holds every chunk for a one-way delay and sends it at a fixed rate each way, with no loss: it needs no namespaces or root, so a Windows, WSL or any machine can take a sanity measurement of a change on a long thin path. Its timing is the operating system's own; a result is not evidence (CI on the emulated links is).

`run` runs the scenarios against a host that already exists (another machine, a network namespace): the
operator starts `bohio serve`, runs `bohio pair` there and `bohio join ENDPOINT --pin PIN --config CLIENT.cfg`
here, then `bohio peer approve` with `--allow info,exec,fs.read,fs.write --fs-root ROOT --program HELPER`:

```bash
bohio-bench run --endpoint tcp://HOST:PORT --client-config CLIENT.cfg --data-dir CLIENT-DATA \
    --root ROOT --program HELPER --raw HOST:RAWPORT --link site-vpn --scenario ... --save FILE
```

The raw control's server is `bohio-bench raw-serve --listen ADDR` on the host. Host CPU metrics need
`--host-pid` for a host on the same machine.

## Scenarios

Names are metric prefixes. Sizes are `1KiB`, `16MiB`, ...; a count is `x100`.

| Scenario | What is timed |
| --- | --- |
| `connect` | dial and proof until the link is ready |
| `info` | a control stream and one `info`, on a ready link |
| `stat_xN_serial` | N `stat`s, one after the other |
| `open_close_xN` | N control streams opened and closed: an open does not wait for the host, so this is the client's cost; on a fast link 100 in a row are refused (the host's `max_streams` is 64), so `lan` runs 50 |
| `read_<size>_cold` | a new link, then a read of the whole random file: what a one-shot command pays |
| `read_<size>_warm` | the read on a link that has already carried one (past TCP slow start) |
| `write_<size>` | a `replace` of that many bytes on a ready link |
| `exec_echo`, `exec_out_<size>` | a program that prints a word, or that many bytes |
| `checksum_<size>` | the host's SHA-256 of a file, over a `bohio-hash/1` stream |
| `hash_<algorithm>_<size>`, `hash_many_<n>` | (feature `hash`) the host's digest of a file by one algorithm; SHA-256 of `n` small files on one stream |
| `read_<corpus>_<size>[_<algorithm>]`, `write_<corpus>_<size>[_<algorithm>]`, `exec_text_<size>[_<algorithm>]` | (feature `compress`) a read or a write of the `text` or `random` corpus, or a program's text output, with the compression the name gives (`zstd3`, `zstd19`, `lz4`, `deflate6`, `none`) or, with no algorithm in the name, the run's `--set bench.compress=ALG[:LEVEL]` (none when unset), on a ready link. `read_` also records `<name>_wire_bytes_...`: the bytes that travelled, in the `*_ms` fields (the ratio is that over the size) |
| `cpu_hash_<algorithm>_<size>` | (feature `hash`) the control: the same algorithm over memory, no I/O |
| `delta_push_<size>_<edit>`, `delta_pull_<size>_<edit>`, `full_push_<size>_<edit>`, `full_pull_<size>_<edit>` | (feature `delta`) a random file of `size` that differs from the other side's copy by `edit` (`1byte` in the middle, `Npct` as one run of N percent, `Npctscatter` N percent as runs of 4 KiB spread over the file), moved to the host (`push`) or from it (`pull`) with a delta or whole (`full_`, the control), on a ready link. Each sample puts the two versions back first (untimed); the file that landed is checked against the new one by BLAKE3. Also records `<name>_wire_bytes_...`: the bytes of frames that travelled in both directions, in the `*_ms` fields. The local files are under the system's temporary directory (`TMPDIR`) |
| `walk_<n>`, `bundle_read_<n>x<size>`, `bundle_write_<n>x<size>`, `file_read_<n>x<size>`, `file_write_<n>x<size>`, `sync_tree_unchanged_<n>` | (feature `walk`) a walk of a tree of `n` small files on one stream (`walk_10k`); `n` files of `size` read or written as one bundle, and the same one stream a file (`file_`, the control: set `--set link.max_opens_per_minute=...` past `n` on both sides, or the link refuses the opens); a second `sync_tree` of a local tree of `n` files already on the host, which moves no file (`sync_tree_unchanged_10k`, with `_wire_bytes`). The trees are made first, untimed, under the corpus directory's `written/` |
| `remote_read_<size>`, `remote_info` | `bohio remote ...` as a child process: process start counts |
| `timeline_read_<size>`, `timeline_info` | the parts of one cold request: `_dial`, `_proof`, `_open` (to the header), `_first` (to the first byte), `_body`, `_total`; with the bytes received by time in `curves` |
| `open_call_xN`, `stat_xN_pipelined`, `read_<size>_parallelN`, `remote_via_read_<size>` | (feature `pipeline`) `N` control streams each opened, asked one `stat` and closed (the open with the host's admission; `open_close` is the client's part only); `N` `stat`s sent before the first answer is read, to compare with `stat_xN_serial`; the same read as `read_<size>_warm` on `N` streams at once, one range each, no digest; `bohio remote --via HOST read` through a node of the client's configuration that already holds the link (`local` starts the node when a scenario starts with `remote_via_`), to compare with `remote_read_<size>` |
| `raw_connect`, `raw_down_<size>`, `raw_up_<size>` | the control: a plain TCP socket, no proof, no mux, with the connect included | 
| `raw_warm_down_<size>`, `raw_warm_up_<size>` | the control on a connection that already carried a transfer: the counterpart of `read_<size>_warm` |
| `raw_par_xN_down_<size>` | N raw downloads at once: separates a per-flow limit from the path's capacity |
| `cpu_sha256_<size>` | the control: SHA-256 of memory, no I/O |

Scenarios for features a later change adds sit behind the Cargo features `hash`, `compress`, `delta`,
`walk` and `pipeline`, off by default. Every call into the client library goes through
`crates/bohio-bench/src/compat.rs`, so one file follows an API change.

## Results

One JSON file per result in `results/`:

```json
{"name": "...", "raw": {"commit", "rustc", "os", "arch", "processor", "transport", "link", "settings", "label"},
 "metrics": {"<scenario>_<transport>_<link>": {"min_ms", "median_ms", "max_ms", "samples", "bytes"}},
 "curves": {"<scenario>_<transport>_<link>": [[[ms, bytes], ...], ...]}}
```

`bytes` is what one sample moved, so throughput is `bytes / median_ms` kB/s. `<scenario>_host_cpu_...` is
the user plus system milliseconds of the host process tree during a sample. `link` is the emulated link's
name, or `site-vpn` for the cross-site pair. No result names a host or an address. Compare two with
`dotagents compare-bench before.json after.json` (the median decides).

## Emulated links in CI

`.github/workflows/bench.yml` (`workflow_dispatch`: `ref`, `baseline_ref`, `samples`, `links`, `a_settings`, `features`, `extra_scenarios`) builds the
refs, and on each link runs the scenario set (`scenarios.sh`) three rounds, alternating A (`ref`) and B
(`baseline_ref`). `netem.sh up <link>` makes two network namespaces joined by a veth pair, with `tc netem`
on each end:

| Link | Shape |
| --- | --- |
| `lan` | none |
| `wan` | 133 ms each way, 20 mbit |
| `fat` | 133 ms each way, 200 mbit: the 1 MiB window is the limit |
| `lossy` | 50 ms each way, 1% loss |
| `slow` | 300 ms each way, 1 mbit down, 256 kbit up |
| `weak` | `wan`, the host under a 25% CPU quota |

`a_settings` (`link.bulk_window=8MiB`) puts `--set` keys on A's runs alone, for a setting the baseline does not know
or to measure one against the baseline's defaults.

`features` builds A's harness with Cargo features (`hash`) and `extra_scenarios` adds scenario names to every link's set, for what a feature adds; B's
harness is its own commit's, so it does not run a scenario it lacks (a warning). `b_features` builds B's harness with features too, for a B that is
the same ref as A: `ref=SHA baseline_ref=SHA features=compress b_features=compress a_settings=bench.compress=zstd:3
extra_scenarios="read_text_1MiB read_random_1MiB"` measures compression off (B) against zstd level 3 (A) in one build, alternating rounds.

A hosted runner cannot show a gain under about 10%. Read the controls (`raw_*`, `cpu_sha256_*`) before any
change: a claim needs the same job on both commits, more than once, with the controls unchanged.
`summarize.py` merges the rounds of one build (median of the rounds' medians) and prints the A/B table.

## Baseline

The state before any transfer optimisation: Bohío's code at `1248e07` plus the harness (`615c626`, the
bench workflow's `ref`). Each emulated-link file is the merge of three alternating rounds of three samples
(`workflow_dispatch`, run 44683809202):

- `results/baseline-615c626-{lan,fat,wan,lossy,slow,weak}.json`
- `results/baseline-615c626-lan-windows-arm64.json` and `...-lan-linux-arm64.json`: the same `lan` set
  on a Windows ARM64 workstation and in WSL (Linux aarch64), five samples, one round.

The cross-site pair (this workstation to a Linux x86-64 host over a site VPN, 272 ms round trip), three
rounds at different times, measured on the same code:

- `results/cross-site-1248e07-before.json`: the scenarios of the timeline and the controls.
- `results/cross-site-1248e07-sweep.json`: `read_16MiB_warm` with `link.window` 1, 4, 16 MiB and
  `link.max_frame` 64, 256 KiB, both sides alike (metric names carry `base`, `w4`, `w16`, `f256`).
- `results/cross-site-1248e07-flows.json`: 1, 4 and 8 raw downloads at once.

Known gaps in these files: `open_close_x100` and `open_close_x50` fail on a fast Linux link (the host's
stream limit; the metric is missing from the `lan` files made on Linux); the first
cross-site round predates the warm raw control, so `raw_warm_*` there come from two rounds.

## Per-stream windows

`results/link-windows-1ecb486-fat.json` (before) and `results/link-windows-e757886-fat.json` (after, `link.bulk_window=8MiB` on
A's runs alone through `a_settings`): the `fat` link, three alternating rounds of three samples each (run 44725071216). The window
is what limits one stream there (1 MiB per 267 ms round trip, about 3.9 MB/s), so a bulk read widened to 8 MiB moves about 14 MB/s,
and the controls do not move:

| Metric | Before (ms) | After (ms) | Change |
| --- | --- | --- | --- |
| `read_64MiB_warm` | 17492 | 4851 | -72.3% (3.84 to 13.83 MB/s) |
| `write_16MiB` | 4892 | 1711 | -65.0% |
| `raw_down_64MiB` (control) | 7627 | 7627 | +0.0% |
| `raw_warm_down_64MiB` (control) | 5385 | 5385 | +0.0% |
| `cpu_sha256_256MiB` (control) | 154.5 | 154.4 | -0.1% |

`connect` is either about 1000 or about 1300 ms (a round trip apart) and stays so within one job; two jobs of this change gave it
to different builds, so it is a property of the job and not of a build.

## Compression

`results/compress-off-{slow,wan,lan}.json` and `compress-zstd3-{slow,wan,lan}.json` are one build (`180aca12`) with compression off and with `bench.compress=zstd:3`, three alternating rounds of three samples (run 44777792164). On `slow`, `read_text_1MiB` went from 9409 ms to 3615 ms (1049 KiB to 358 KiB on the wire) and `read_random_1MiB` stayed at 9409 ms (the sample sent it raw); `cpu_sha256_256MiB` moved +2.9% and `raw_down_1MiB` 0.0%. On `lan` the 1 MiB text read is slower compressed (2.0 ms against 6.7 ms): compression pays where bandwidth is the limit.
`results/compress-algorithms-weak.json` (run 45021195510, `weak`: the host under a 25% CPU quota) holds `read_text_1MiB` by algorithm: none 707 ms, lz4 503, zstd1 428, zstd3 418, deflate6 421, zstd19 1704 (390 ms host CPU).
