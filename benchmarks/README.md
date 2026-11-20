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
| `cpu_hash_<algorithm>_<size>` | (feature `hash`) the control: the same algorithm over memory, no I/O |
| `remote_read_<size>`, `remote_info` | `bohio remote ...` as a child process: process start counts |
| `timeline_read_<size>`, `timeline_info` | the parts of one cold request: `_dial`, `_proof`, `_open` (to the header), `_first` (to the first byte), `_body`, `_total`; with the bytes received by time in `curves` |
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
harness is its own commit's, so it does not run a scenario it lacks (a warning).

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
