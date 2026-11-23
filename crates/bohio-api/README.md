# bohio-api

The provider kinds of Bohío, a remote host runtime. A node reaches the system it is for through **sources**, each a
configured instance of a **provider**, and each capability is one [guatiao](https://github.com/Boriken-Dev/guatiao) kind:

| Kind | Trait | A source is |
| --- | --- | --- |
| `bohio_terminal` | `TerminalSource` | a console: a pseudo-terminal, a serial line, a BMC's serial-over-LAN |
| `bohio_desktop` | `DesktopSource` | a screen with input: the node's own, a VNC or RDP server, a BMC's KVM |
| `bohio_power` | `PowerSource` | the system's power, through a BMC, a hypervisor or a smart switch |
| `bohio_media` | `MediaSource` | virtual media: an image the system boots or installs from |

The host (`bohio`) and every provider compile against this crate and nothing else of Bohío's. It re-exports `guatiao`,
so a provider names guatiao only through it and both sides cannot drift to different versions.

## Features

- `testing`: `bohio_api::testing`, in-memory sources a test plays the far end of. Nothing in it touches a device, a
  screen or a network.

## Writing a provider

A provider is one type with the guatiao derives and one `impl` of a kind. Its settings are its own type, checked
against its schema before the provider is built. `describe` says honestly what the source cannot do (`lacks`), and
`bohio_api::contract` is the suite every provider of a kind passes. `bohio-api.kinds.json` describes the kinds for a
provider written in another language.

```rust,ignore
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::{SourceInfo, Terminal, TerminalOpen, TerminalSource};

struct Mine;

impl TerminalSource for Mine {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new("mine", &["read", "write"], &["resize", "signals", "programs"]))
    }
    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        /* return a value implementing `Terminal`, `.into_object()` */
        # unimplemented!()
    }
}
```

A kind grows by appending methods with a default body; a model grows by fields that default. Each kind's table hash is
pinned by `tests/kinds.rs`, so a change that breaks providers already built is made on purpose.

Licence MPL-2.0.
