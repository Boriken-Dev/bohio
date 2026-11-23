# Sources and providers

A Bohío node reaches the system it is for through **sources**. A source is a configured instance of a **provider**, and each
thing a source gives is one of four **capabilities**, each a [guatiao](https://github.com/Boriken-Dev/guatiao) kind of the
`bohio-api` crate:

| Capability | Kind | A source is |
| --- | --- | --- |
| terminal | `bohio_terminal` (`TerminalSource`) | a console: a pseudo-terminal, a serial line, a BMC's serial-over-LAN |
| desktop | `bohio_desktop` (`DesktopSource`) | a screen with input: the node's own, a VNC or RDP server, a BMC's KVM |
| power | `bohio_power` (`PowerSource`) | the system's power: a BMC, a hypervisor, a smart switch |
| media | `bohio_media` (`MediaSource`) | virtual media: an image the system boots or installs from |

`native` is this node's own terminal and screen. It is a provider of the same kinds as any other, registered the same way, so
nothing a node can do is reachable only because it is built in. Files and processes are engines over the operating system,
selectable `native` or `none` (`[access] fs`, `exec`) and reported the same way by `info`; they become kinds when a second
provider is planned.

A node serves **one system**: several sources of a capability are alternative paths to it (a serial port and serial-over-LAN
to the same machine). A node in front of many systems is several nodes.

## Configuring sources

```toml
[access]
terminal = ["console", "native"]   # the first is the default
desktop = []                       # this node serves no desktop
fs = "none"                        # files are not served at all
exec = "none"

[source.console]
provider = "serial"
path = "/dev/ttyUSB0"
baud_rate = 115200
reopen_after = "1s"

[peer."sha256:..."]
allow = ["info", "shell", "session"]
sources = ["native"]               # beyond the default console
```

`[source.<name>]` names a provider and gives that provider's own keys; they are checked against the provider's schema before it
is built, so a misspelled key or a wrong type is `ConfigError::Invalid` naming `source.<name>.<key>`, and an unknown provider
lists the known ones. `native` takes no `provider` and no other. `[access]` lists the sources of each capability; the first is
the default. With no `[source]` and no `[access]` a node is what it always was: native for terminal, desktop, files and
processes.

A peer reaches **only each capability's default source** unless its table says `sources = [..]`; a source added to the node
later grants nobody anything. `info.access` lists, for a peer holding `info`, the sources its grant reaches with `provider`,
`default`, `available`, the `reason` when not, and what each `offers` and `lacks`; it is asked of the providers at every `info`
and never cached.

## The console

A source that starts no program (a serial line) is reached with `bohio remote HOST console [--source NAME]`, which opens the
node's **console**, one session the node owns for each such source (`console:<source>`): its output is kept (`[sessions]
scrollback`), a newer attach supersedes an older one, a client's `kill` or a closed stream only detaches, and
`bohio remote HOST attach console:NAME --from OFFSET` (or an agent's `session_read`) reads what was kept. Leave `console` with
`Ctrl-]` then `q`. A request to start a program (`shell`, `exec`) on a source that lacks `programs` is `unsupported`, naming the
source.

## Writing a provider

A provider is one type with the guatiao derives, one settings type and one implementation of a kind. This is
`crates/bohio/examples/provider_echo.rs`, compiled and run by `cargo run -p bohio --example provider_echo -- --peer ID`:

```rust,ignore
struct Echo { line: Arc<Line> }

impl TerminalSource for Echo {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        // A console says honestly what it cannot do: a peer reads it from `info` before it asks.
        Ok(SourceInfo::new("echo", &["read", "write"], &["resize", "signals", "exit_status", "programs"]))
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        if request.program.is_some() {
            return Err(unsupported("the echo console starts no program"));
        }
        Ok(Session { /* .. */ }.into_object())
    }
}
```

- **Calls are synchronous.** The host calls a provider from a thread of its own; a provider that drives an async library runs it
  on threads of its own and blocks in the kind's method. A method documented as blocking (`read` with its `wait_ms`, a power
  `act`) is bounded by the host: `read` by its argument, a power or media call by `[power]` and `[media]` `action_timeout`.
- **`describe` is honest.** `lacks` names what the source cannot do in the kind's vocabulary (`bohio_api::vocab`), and the
  contract suite fails a source that says it lacks `resize` and takes one. `available: false` with a `reason` says the source
  cannot be used now (an unplugged adapter), as availability, not as a failure the first time it is used.
- **Settings are the provider's own type** with `ToValue`, `FromValue` and `Schema`, never a text field. A setting marked
  `#[schema(sensitive)]` holds a reference to a secret, never the secret (see Security).
- **Bounds are settings.** Every queue, interval and size a provider has is a key with a default.
- **A kind grows by appending** methods with a default body; a model grows by fields that default. Each kind's table hash is
  pinned by `crates/bohio-api/tests/kinds.rs`, and `bohio-api.kinds.json` describes the kinds for a provider written in another
  language.
- **Run the contract suite**: `bohio_api::contract::{terminal, desktop, power, media}` over your source, with an endpoint your
  test starts (`socat` pseudo-terminals, a QEMU or VNC server in WSL, `bohio_api::testing`'s in-memory sources), never a production BMC.

A provider linked into the node is registered with `Sources::register_library` (a `guatiao::local_providers!` library) or given
as a value with `Sources::with_terminal` and its siblings, then made the default with `with_access`. `Node::with_sources` hands
the node its sources; `Node::with_secrets` says how secret references are read.

### What is not offered yet

Provider libraries loaded at run time, pinned by SHA-256, are not offered: `guatiao` can hash a library only after it has
mapped it, and a check that runs after the code is mapped protects nothing. Until `guatiao` checks the bytes before it maps
them, every provider is linked into the node's binary.
