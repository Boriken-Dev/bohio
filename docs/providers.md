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
| exec | `bohio_exec` (`ExecSource`) | processes: the node's own operating system, and later a machine reached over SSH |
| fs | `bohio_files` (`FilesSource`) | a directory tree: the node's own file system, or one reached over a protocol |

`native` is this node's own terminal and screen. It is a provider of the same kinds as any other, registered the same way, so
nothing a node can do is reachable only because it is built in. Processes are a capability like the others: `native` is the
first `exec` source, `[access] exec` lists the sources (`"native"` and `"none"` keep their meaning) and a request names one with
`source` (the default exec source when it names none). Files are a capability too: the filesystem engines (read,
write, list, walk, search, hash, delta, bundle, follow, move, copy) reach files only through `FilesSource`, and `native` is its
first provider. `[access] fs` lists the file sources (the first is the default; `"native"` and `"none"` keep their meaning).
A source opens a root as a directory handle and every name below it is resolved against that handle; `native` keeps the
operating system's directory-handle safety inside the provider (`describe` offers `confined`), and a source that resolves names
by path each call says so by lacking it. `[source.native] list_chunk` (`256`) is how many directory entries one read of a
listing returns.

A node serves **one system**: several sources of a capability are alternative paths to it (a serial port and serial-over-LAN
to the same machine). A node in front of many systems is several nodes.

## Configuring sources

```toml
[access]
terminal = ["console", "native"]   # the first is the default
desktop = []                       # this node serves no desktop
fs = "none"                        # files are not served at all
exec = "none"                      # or a list of sources: ["native"]

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

## A machine over SSH

`provider = "ssh"` reaches another machine through its SSH server, with nothing of Bohío installed there: a **terminal** source (a pty
running the account's login shell or a program), an **exec** source (programs with separate output and error) and a **files** source
(SFTP). One source is one connection to one host; `[access] terminal`, `exec` and `fs` list it beside `native`, and a peer's `sources`
names it.

```toml
[source.build]
provider = "ssh"
host = "build.example"
user = "ci"
agent = true                  # the keys of the ssh agent first (the OpenSSH pipe or Pageant on Windows, SSH_AUTH_SOCK elsewhere)
host_key = "sha256:..."       # or a known_hosts entry; "strict" is the default check
# max_channels = 8, sftp_in_flight = 16, sftp_packet = 32768, operation_timeout = "30s" ...

[access]
exec = ["native", "build"]
fs = ["native", "build"]
terminal = ["native", "build"]

[peer."sha256:..."]
allow = ["info", "exec", "fs.read", "fs.write", "shell"]
sources = ["build"]
fs_root = ["/srv/work"]       # directories of the machine behind the source
```

A request names the source: `bohio remote HOST exec --source build -- uname -a`, `bohio remote HOST shell --source build` (its console),
`bohio remote HOST read --source build /srv/work/log` (and `write`, `hash`, `put`, `get`, `walk`, `glob`, `search`, `sync`), `source` on the
agent tools that touch files and on `exec`. A request that names none uses the capability's default source. The names of a files
source are POSIX paths whatever system the node runs on (`posix_paths`), and the peer's `fs_root`s are held to by the node on that
machine's paths. The source's `lacks` (in `info`) say what the server does not give: no `confined` (a link on the machine can lead out
of a root; the node's check is by path), and no `atomic_replace`, `hard_link`, `statvfs` or `sync` where the server lacks those SFTP
extensions. A source that cannot connect is `available = false` with the reason and the node starts all the same; every key is in
`providers/ssh/AGENTS.md`, and `docs/security.md` says what a peer holding the source can do (everything the account can) and what a weaker
host-key check costs.

### The machine's desktop over SSH

Two settings of an ssh source serve a desktop, with nothing of Bohío on the machine. Neither is on by default.

- `x11_display = ":0"` serves the machine's own X server as the source's **desktop**: the node opens a channel to the display's
  socket (`/tmp/.X11-unix/X0`, `x11_socket_dir`), reads the cookie `xauth list` gives for it (a machine without `xauth` is served
  without one) and runs the X11 backend on that stream. `[access] desktop` lists the source and a peer's `gui.view` and `gui.input`
  reach it. The screen is seen and clicked as the machine's own X session would be; the lock state of the machine's login session is
  not told (`lock_detection` is in `lacks`), and a Wayland-only machine has no X server to reach (its XWayland, such as WSLg's, serves
  the X windows alone).
- `x11_forward = true` makes the programs this source starts show their windows on **the node's own desktop** (the way `ssh -X` does):
  the machine's `sshd` needs `X11Forwarding yes` and an `xauth`, each program's X connections come back over the session and go to the
  node's display (`x11_local_display`, else the node's `DISPLAY`; at most `x11_max_connections` at once). It needs `x11_trusted = true`
  (the way `ssh -Y` does), because this build cannot ask the node's display for an untrusted authorization: a forwarded program can
  read and drive everything on the node's display (`docs/security.md`). A node with no X display (Windows) cannot forward.

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

### Loading a provider library

A provider that is not linked into the node is a library the node maps at start. Nothing loads unless `[providers] paths` names
something, and then only libraries that declare a Bohío kind (`bohio_terminal`, `bohio_desktop`, `bohio_power` or `bohio_media`);
with the default (`paths = []`) no file is read, no library is mapped and nothing extra runs at start.

```toml
[providers]
paths = ["/usr/local/lib/bohio/providers", "/opt/vendor/libbohio_vnc.so"]   # files, or directories of libraries
# check_access = true    # the default: see below
# dlopen = "auto"        # the loader: "auto", "system" or "foreign" (see "Which builds can load")

[source.console]
provider = "serial"       # a provider a library offers is named like a built-in one
path = "/dev/ttyUSB0"
```

- **A file or a directory.** A directory's `.so`, `.dll` or `.dylib` files are taken by name; each is read as data first (headers
  and the kinds it declares), and a file that is not a guatiao library, or declares no Bohío kind, is **skipped** and never mapped.
  Paths are read once, at start; a reload of `bohio.cfg` builds sources again from the libraries already mapped and does not look
  at the paths.
- **Reading the file maps nothing.** A `[source.<name>]` whose provider a library offers is known only once the library is
  mapped, so with `[providers] paths` set `bohio config check` and the other readers of `bohio.cfg` do not check the `[source.*]`
  and `[access]` tables; the node maps the libraries first and refuses a name or a setting nobody offers when it starts, naming
  the same key (`source.<name>.provider`).
- **One failure stops nothing else.** Each library is loaded alone. The node logs one line for each at start and
  `bohio providers` (on the node's local endpoint) prints the same: `loaded`, `skipped` or `failed`, the library's path, why or
  what it offers, and its SHA-256 (`bohio providers --json` for a script). A stale copy of a library beside a newer one
  (the same library id) is `skipped` and names the copy that was loaded; a provider id a built-in already holds stays the
  built-in's (a library never replaces `native` or a built-in `serial`), and the library's report says so.
- **The owner check is a default.** A library, or the directory that holds it, that another account owns or may write is
  **refused** (`failed`, `refused: ...`) while `[providers] check_access` is `true` (the default). Set it to `false` and the node
  loads it without asking the file system, logs `non-default: ...` at start (and `bohio providers` prints it). Only the lowest configuration
  layer sets it, as `[service] check_access`. A path that names nothing, or an empty path, cannot work and is reported `failed`
  (an empty one is a configuration error).
- **What a library may do.** It runs as the node's own account, inside the node's process, with no sandbox: a library is the
  node's code. Loading is unpinned, so nothing says the file is the one you meant; `bohio providers` prints each digest so that
  you can compare it. `docs/security.md` has what that costs.
- **Which builds can load.** Every build can: the glibc, macOS and Windows builds map a library with the platform's loader,
  and the statically linked musl builds (the Linux release archives) have no loader of their own, so on x86_64 and aarch64
  they load through the **system's own** `ld.so` and C library, started inside the node. That needs glibc on the machine
  (the program `ld.so` starts asks for GLIBC_2.2.5 and GLIBC_2.3.3 on x86_64, GLIBC_2.17 on aarch64) and its dynamic linker
  at the usual path (`/lib64/ld-linux-x86-64.so.2`, `/lib/ld-linux-aarch64.so.1`), or `[providers] ld_so` naming it; a
  machine with neither (Alpine, say) reports each library `failed`, with the loader's own reason. A library must be built for
  glibc then, and no C compiler or linker is needed on the machine or at build time. Measured with Bohío's static musl
  test binary serving the serial library built for glibc on aarch64 and x86_64 (`tests/plugins.rs`, run with
  `BOHIO_LIBRARY_DIR` naming the libraries). What follows from it:
  - **One foreign C library for the whole process.** The first library loaded starts it, with the settings of that first
    start (`ld_so`, `allow_reserved_signal_handlers`), and they hold until the node exits.
  - **Signal 33.** glibc takes it when it starts, so the start is refused when the node already handles signal 32 or 33;
    `allow_reserved_signal_handlers = true` starts it anyway and may replace that handler (logged as `non-default:`).
  - **Library code runs on its own threads.** A call into a library is handed to a thread the foreign C library made, and a
    call back into the node comes back to a node thread; that hop is the cost of a call.
  - `[providers] dlopen` chooses the loader: `auto` (the default) takes the foreign one on a static Linux build and the
    platform's elsewhere, `system` never takes the foreign one (a static build then loads nothing: each library is
    `failed`), `foreign` always does (a dynamically linked build loading libraries built for glibc). Any other value than
    `auto` is logged as `non-default:`. `ld_so` and `allow_reserved_signal_handlers` are set by the lowest configuration
    layer only: `ld_so` names a file the node maps and enters.
- **Making one.** `providers/terminal/serial/plugin/` is a provider as a library in one line,
  `bohio_api::guatiao::providers!(bohio_provider_serial::Serial);` in a crate of type `cdylib` that depends on nothing else
  (the entry symbols it writes are `no_mangle`, so it does not take the workspace's `unsafe_code = "forbid"`). A node built with
  feature `serial` already has `serial`; build without it (`--no-default-features --features cli`) to serve it from the library.

### What is not offered yet

Pinning a library by SHA-256 (refusing a file whose digest is not the one named, checked on the bytes that are mapped) is not
offered: `guatiao` can hash a library only after it has mapped it, and a check that runs after the code is mapped protects
nothing. `[providers]` has no `pins` key until it can work; until then the digest `bohio providers` prints is for you to compare.
