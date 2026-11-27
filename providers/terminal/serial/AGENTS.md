# `bohio-provider-serial` — API header

Header-file-style reference for the `bohio-provider-serial` crate: the `serial` provider of the `bohio_terminal` kind, with every
setting and gotcha, so it can be configured and embedded without reading its source. Development documentation lives with the source at
<https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `bohio-api` (the kind), `coqui-transport-serial` (the port) and `coqui-transport` (its `Split`).
`#![forbid(unsafe_code)]`. Version `0.0.0-alpha.0`, not published.

Imports: the root holds `Serial` (the provider, a `TerminalSource`), `SerialConfig` (its settings) and `library` (written by
`guatiao::local_providers!`: `registry.register_local("bohio-serial", bohio_provider_serial::library)`; `bohio` does it with feature `serial`).

## The provider

Provider id `serial`. `[source.<name>] provider = "serial"` and:

| Key | Default | Meaning |
| --- | --- | --- |
| `path` | required | `/dev/ttyUSB0`, `COM3` (`\\.\COM10` above 9) |
| `baud_rate`, `data_bits`, `parity`, `stop_bits`, `flow_control` | `115200`, `8`, `"none"`, `1`, `"none"` | the line (`parity` none/odd/even; `flow_control` none/software/hardware) |
| `dtr`, `rts` | unchanged | the line's state when the port opens (many devices reset when DTR changes) |
| `reopen_after` | `"1s"` | how long after the port vanishes or fails to open it is tried again, and how long `describe` waits for the first try; not zero. A time is `bohio_api::Interval`: a number of seconds (`1`) or text with a unit (`500ms`, `30s`, `5m`, `1h`) |
| `write_stall` | `"5s"` | how long a write may make no progress (the port timing out, flow control holding the line) before it fails; not zero |
| `queue_bytes` | `65536` | bytes read and not yet taken by a peer; the reader then waits, so the driver and the line's flow control hold the rest; at least 1 |

A key it does not have, or a value out of range, is refused naming `source.<name>.<key>` before the provider is built. The port opens the first
time the source is used (`describe` at `info`, or a peer's attach), never when the configuration is read.

`describe`: offers `read`, `write`; lacks `resize`, `signals`, `exit_status`, `programs`, `break`; `available: false` with the operating
system's reason while the device cannot be opened (an unplugged adapter), true again within `reopen_after` of its return. `open` refuses a request
that names a program (`bad_request`) and a port that is not open (`unavailable`). `kill` on a terminal only lets go of its attach; nothing is
ended on the far end. A line has no end of stream: `exit` is `None` and `read` answers `-1` only after the source was dropped or the attach let go.

As a library: `providers/terminal/serial/plugin/` (`bohio-provider-serial-plugin`, a `cdylib`) is this provider behind one `providers!` line, loaded with `[providers] paths`; `crates/bohio/tests/plugins.rs` loads it.

As a program: `examples/serial_program.rs` (`cargo build -p bohio-provider-serial --example serial_program`) serves this provider over its
standard input and output as the provider `serial-program`, for `[[providers.program]]`. A provider a program serves is one instance
configured where it runs, so the source's settings are the program's arguments, each `--KEY VALUE` (`--path /dev/ttyUSB0 --baud_rate 9600`),
and the node's `[source.<name>]` is `provider = "serial-program"` alone. `crates/bohio/tests/programs.rs` runs it over `socat` pseudo-terminals.

Tests: `crates/bohio/tests/console.rs` runs a peer against the source over a pair of pseudo-terminals from `socat` (Unix; `SKIPPED` when it is
not installed) and an in-memory source on every platform; Windows has no COM port to open here.
