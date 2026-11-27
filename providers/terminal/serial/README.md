# bohio-provider-serial

[![Version](https://img.shields.io/crates/v/bohio-provider-serial.svg)](https://crates.io/crates/bohio-provider-serial)
[![Rust version](https://img.shields.io/crates/msrv/bohio-provider-serial.svg)](https://crates.io/crates/bohio-provider-serial)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-provider-serial)](https://boriken-dev.github.io/bohio/rust/bohio_provider_serial/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

A terminal provider for [Bohío](https://github.com/Boriken-Dev/bohio): **the console of a system reached through a serial line**, a KVM or
a terminal server. Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_provider_serial/>.

## Features

- A console source over a serial port: `path`, the line settings (baud rate, data bits, parity, stop bits, flow control) and the bounds
  of the reader and the writer, each a setting.
- A write that cannot make progress for `write_stall` ends instead of waiting for good.

## Installation

```text
cargo add bohio-provider-serial --git https://github.com/Boriken-Dev/bohio
```

| Item | Purpose |
| --- | --- |
| the `serial` provider | a `bohio_terminal` source over a serial port |

`providers/terminal/serial/AGENTS.md` is the API header and lists every key.

## Quick start

A node built with feature `serial` (on by default) registers the provider as `serial`; `bohio-provider-serial-plugin` is the same
provider as a library a node maps at run time.

## API overview

```toml
[source.console]
provider = "serial"
path = "/dev/ttyUSB0"
baud_rate = 115200

[access]
terminal = ["native", "console"]
```

then `bohio remote HOST console --source console`.

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-provider-serial --all-targets -- -D warnings
cargo test -p bohio-provider-serial
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-provider-serial --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
