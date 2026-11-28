# bohio-provider-ssh

[![Version](https://img.shields.io/crates/v/bohio-provider-ssh.svg)](https://crates.io/crates/bohio-provider-ssh)
[![Rust version](https://img.shields.io/crates/msrv/bohio-provider-ssh.svg)](https://crates.io/crates/bohio-provider-ssh)
[![License: MPL-2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](https://github.com/Boriken-Dev/bohio/blob/main/LICENSE)
[![Docs](https://img.shields.io/docsrs/bohio-provider-ssh)](https://boriken-dev.github.io/bohio/rust/bohio_provider_ssh/)
[![CI](https://img.shields.io/github/actions/workflow/status/Boriken-Dev/bohio/test.yml)](https://github.com/Boriken-Dev/bohio/actions/workflows/test.yml)

A provider for [Bohío](https://github.com/Boriken-Dev/bohio): **a machine reached over SSH, with nothing of Bohío installed on it**.
Its terminal, its processes and (with SFTP) its files are served to a node's peers through the same grants as the node's own.
Documentation: <https://boriken-dev.github.io/bohio/rust/bohio_provider_ssh/>.

## Features

- A terminal source (a pty running the account's login shell or a program), an exec source (programs with separate output and
  error) and a files source (SFTP), over one connection to one host; the system's `ssh` client can serve the terminal and
  programs instead (`client = "openssh"`).
- The machine's X display as a desktop source, and Bohío's GUI helper started there for a machine that is not an X server.
- Host-key checks, an agent or key file for the login, and bounds on channels, SFTP requests and every wait, each a setting.

## Installation

```text
cargo add bohio-provider-ssh --git https://github.com/Boriken-Dev/bohio
```

| Item | Purpose |
| --- | --- |
| `SshSettings` | the provider's settings: every key, its default and what a weaker choice costs |

`providers/ssh/AGENTS.md` is the API header and lists every key.

## Quick start

A node built with feature `ssh` (on by default) registers the provider as `ssh`; `bohio-provider-ssh-plugin` is the same provider as a
library a node maps at run time.

## API overview

```toml
[source.build]
provider = "ssh"
host = "build.example"
user = "ci"
agent = true
host_key = "sha256:..."

[access]
exec = ["native", "build"]
```

then `bohio remote HOST exec --source build -- uname -a`.

## The connection as a library

A program that wants the host and not a node's sources builds a `Connection` from the same settings:

```rust,no_run
use bohio_provider_ssh::{Connection, SshSettings};

let connection = Connection::new(&SshSettings::new("build.example"))?;
let platform = connection.platform()?;            // os `linux`, arch `x86_64`
let found = connection.run("echo hello", 1024)?;  // the output, capped, and how the command ended
let forward = connection.reverse_forward("127.0.0.1", 0, "127.0.0.1:8080")?;
println!("{platform:?} {:?} port {}", found.stdout, forward.remote_port());
# Ok::<(), Box<dyn std::error::Error>>(())
```

`open_socket` gives a stream to a unix socket on the host and `trusted_host_key` the key to pin after `host_key_check = "tofu"`;
`providers/ssh/AGENTS.md` has each call's limits.

## Development

```text
cargo fmt --all --check
cargo clippy -p bohio-provider-ssh --all-targets -- -D warnings
cargo test -p bohio-provider-ssh
RUSTDOCFLAGS="-D warnings" cargo doc -p bohio-provider-ssh --no-deps
```

### Releasing

Nothing is published to a registry: the workspace depends on git revisions, and a `v*` tag of the repository builds the release
archives.

## License

MPL-2.0.
