# Bohío

**A remote host runtime: a proven peer runs commands, reads and writes files and opens shells on a machine without SSH, WinRM or Python**, over TCP, a named pipe, a WebSocket (with TLS), WebTransport, a serial line or standard input and output, on computers, virtual machines and containers.

Three rules hold the design together:

- **A proof admits, a grant allows.** Each side holds a key and proves it to the other. Being proven opens nothing: the peer's `[peer.<id>]` table in `bohio.cfg` says what it may do, and every stream is decided again while it runs, so a revoke ends it.
- **One stream per operation.** A client opens a stream, names a protocol and sends the request as the open's metadata. Control messages are CBOR and file or process bytes travel raw, behind a one-byte tag, so a client in any language can speak it.
- **No hard-coded limits.** Every bound is a setting in `bohio.cfg` with a documented default, and nothing buffers without bound.

A node reaches the system it is for through **sources** (a console, a screen, power, virtual media, programs, files), each a configured
provider: its own pseudo-terminal and screen are the `native` provider, and a serial line, a BMC or a hypervisor are others. See
[Sources and providers](providers.md).

## Installation

```bash
git clone https://github.com/Boriken-Dev/bohio
cd bohio
cargo build --release
```

Nothing is published to a registry yet: the workspace depends on git revisions of the Coqui crates.

## 30-second tour

On the host:

```bash
bohio serve --listen tcp://0.0.0.0:7620
bohio pair
```

On a peer, with the PIN `pair` shows:

```bash
bohio join tcp://host.example:7620 --pin 123456
bohio id
```

Back on the host, approve the peer by the id it printed and say what it may do:

```bash
bohio peer approve <id> --allow info,exec,fs.read --fs-root /srv/data
```

Then from the peer:

```bash
bohio remote tcp://host.example:7620 info
bohio remote tcp://host.example:7620 exec -- git status
bohio remote tcp://host.example:7620 read /srv/data/file
```

For a container, `bohio serve --stdio` serves one link on standard input and output, and a peer reaches it with `bohio remote --command "docker exec -i box bohio serve --stdio" info`.

## A host behind a firewall

A host that nobody can reach dials out instead, and is controlled over the link it made. On the controller, which listens at `controller.example` (`192.0.2.10`):

```bash
bohio serve --listen tcp://0.0.0.0:7620
bohio id
```

On the host, a table for the controller's id with an `endpoint` makes `bohio serve` keep a link to it, with nothing to listen on; `allow` is what the controller may do here:

```toml
[peer."sha256:<controller id>"]
endpoint = "tcp://controller.example:7620"
allow = ["info", "exec", "fs.read"]
fs_roots = ["/srv/data"]
```

The controller sees the host's key waiting (`bohio peer list`), accepts it with `bohio peer approve <host id>` (no `--allow`: it grants the host nothing), and the next attempt links. The controller's own node then holds the link, so it is controlled through that node's local endpoint:

```bash
bohio links
bohio remote --via <host id> exec -- hostname
```

Who dialed decides nothing: each side's tables say what the other may do there. [Security](security.md#links-in-both-directions) has the rest.

## Reaching a service through a host

A host can connect to a target for a peer, from its own network position: a database on its network, a container engine's socket, another machine's `ssh`. The host grants it by target, not by "network access": `forward.connect` and a rule for each target.

```bash
bohio peer approve <id> --connect db.example:5432 --connect 192.0.2.0/24:22 --connect unix:/run/docker.sock
```

From the peer, `-L` listens on this machine and has the host connect for each connection (the local side is `127.0.0.1` unless another address is named), and `connect` bridges the target to standard input and output, so it is `netcat` and `ssh`'s `ProxyCommand`:

```bash
bohio remote tcp://host.example:7620 forward -L 5433=db.example:5432 -L unix:/tmp/docker.sock=unix:/run/docker.sock
psql -h 127.0.0.1 -p 5433 app

ssh -o ProxyCommand="bohio remote tcp://host.example:7620 connect %h:%p" ops@192.0.2.20
bohio remote tcp://host.example:7620 connect --check db.example:5432   # connected 192.0.2.30:5432
```

A target the grant does not cover is refused before the host connects to anything. [The protocol](protocol.md#forwarding-bohio-connect1-and-tcp1) has the wire and the rules, and [Security](security.md#what-a-forward-gives-a-peer) what a forward gives a peer.

## One port for many targets, and hosts behind a gateway

`forward -D` serves SOCKS5 on this machine, so a browser, `curl` or `git` reaches whatever the host's grant covers without a `-L` for each target. A domain name goes to the host unresolved, so it is the host's resolver that answers, as with `ssh -D`:

```bash
bohio remote tcp://host.example:7620 forward -D 1080                  # prints: socks5 127.0.0.1:1080
curl --socks5-hostname 127.0.0.1:1080 http://intranet.example:8080/
```

Each CONNECT is a `forward.connect` check on the host for that target; a target no rule covers answers the client "not allowed by ruleset", one that refused it "connection refused". BIND and UDP ASSOCIATE are not supported.

`--jump` reaches a host that only a gateway reaches, as `ssh -J`. The gateway carries a stream to the next host and sees sealed bytes only: this side proves itself to each host and each host to this side, so a gateway cannot stand in for the host behind it (name the host's id with `--server`, the gateway's with `--jump-server`). The gateway grants this peer `forward.connect` for the next host's address and nothing else, or, for a gateway that runs a container, `exec` of the program:

```bash
bohio remote tcp://inner.example:7620 --server <inner id> --jump tcp://gw.example:7620 --jump-server <gateway id> exec -- hostname
bohio remote --command "bohio serve --stdio" --server <inner id> --jump tcp://gw.example:7620 --jump-server <gateway id> info
bohio remote tcp://inner.example:7620 --server <inner id> --jump tcp://gw.example:7620 --jump-server <gateway id> forward -D 1080
```

`--jump` repeats for a chain, in order. A refusal names the hop: `jump 1 (tcp://gw.example:7620) would not carry the link to the host (...)`. [Security](security.md#what-a-socks-listener-and-a-jump-give) says what each gives.

## A host's desktop

A host that runs in, or is attached to, a graphical session can show it and be driven. It is off until the owner enables `[gui]` and grants a peer `gui.view`, `gui.input` or `gui.clipboard`; `bohio peer approve ID --allow gui.view,gui.input` writes the grant. The person at the screen gets a notice while someone views or controls it, and `approval = "local"` makes each peer wait for `bohio gui allow`.

```bash
bohio remote tcp://host.example:7620 gui info
bohio remote tcp://host.example:7620 gui snapshot --max-size 1280x800 --settle -o screen.png
bohio remote tcp://host.example:7620 gui watch --seconds 30 --stats
bohio remote tcp://host.example:7620 gui click 640 400
bohio remote tcp://host.example:7620 gui type "hello" && bohio remote tcp://host.example:7620 gui key ctrl+s
```

`gui watch` receives the screen as a stream of the tiles that changed, lossless where that is cheap and JPEG where it is not, never more than `frames_in_flight` updates ahead of what the client acknowledged, and steps down in quality, size and rate when the link falls behind (`--stats` prints the rate, the bytes and the host's own measure of how late acknowledgements are). The client keeps the frame the updates build (`--out DIR` writes it as PNG files).

On Windows the node must run in the user's own session (a terminal, or a task that runs at logon), not as the service: a service is in session 0, which has no desktop, and says so in `gui info` (`available: false`). `bohio serve` with `[gui] enabled = true` then serves that session's displays (DXGI Desktop Duplication, with GDI where it cannot serve), keyboard, mouse and clipboard text; it refuses input and capture, and says why, while the lock screen or an elevation prompt has the screen.

On Linux a node serves the X display it was started in (`DISPLAY`): each RandR monitor is a display, a snapshot is the root window's pixels, input is XTest, and `text` is typed whatever the keyboard layout. The lock state comes from logind (`LockedHint`); with no logind it is unknown and `gui info` says `lock_detection: false`. A Wayland session's X display is XWayland's, which holds only the X11 windows: it is refused unless `[source.native] x11_in_wayland = true`, and then a snapshot is those windows over black (WSLg's display is one). A Wayland session's own windows need the compositor's portals, and a build with the `gui-wayland` feature (a glibc build that links the system's libpipewire; the release archives are static and do not have it) serves a Wayland session through them: the portal's own dialog is the system's approval and appears when the first request reaches the node (`info` included), each monitor is a display, a snapshot is the PipeWire stream's latest picture, and input goes through the RemoteDesktop portal where the desktop has one (GNOME and KDE do; `xdg-desktop-portal-wlr` does not, so a wlroots desktop is a view and `gui info` says `input: false`). A build without the feature reports a Wayland session as unavailable and names `gui-wayland`. `[source.native] wayland_persist` chooses whether the portal remembers its approval.

On macOS a node serves the console session's screen (macOS 14 or later for snapshots): ScreenCaptureKit for the pixels, CGEvents for the keyboard and mouse, the general pasteboard for the clipboard text. Run it as the logged-in user (a terminal, or a LaunchAgent: `packaging/launchd/bohio.plist`); a LaunchDaemon is a system service with no window server session and cannot capture or drive anyone's screen. The system asks a person, in System Settings, Privacy & Security, to allow the program Screen Recording and Accessibility; a node cannot be granted them over ssh, and a grant belongs to the program at its path, so a moved or replaced `bohio` must be allowed again. Until then `gui info` still lists the displays and `gui snapshot` and the input commands answer `unavailable` naming the pane and the path.

A node that runs as a service has no desktop of its own (a Windows service is in session 0; a systemd unit has no display). The desktop user starts `bohio gui-helper` at logon (a Task Scheduler task, a systemd user unit, a LaunchAgent: `packaging/windows/bohio-gui-helper.xml`, `packaging/systemd/bohio-gui-helper.service`, `packaging/launchd/bohio-gui-helper.plist`) and it connects out to the node's helper endpoint, so `gui info` lists that session (`ACCOUNT:NAME`) and a peer's snapshot, input and clipboard are served from it. The node lists the accounts it serves in `[gui] helper_accounts` and closes any other account's connection unread; the helper serves only the account named by `--node-account` or `[gui] node_account` (by default `NT AUTHORITY\LocalService` on Windows and `bohio` on Unix) and exits 1, saying who answered, when another account serves the endpoint. The helper holds no key; its user's own `[gui]` can only narrow what the node allows. [Security](security.md#what-the-per-user-gui-helper-gives) says what it lends.

### A service account's own desktop (Linux)

A node that runs as a systemd service can have a screen of its own, with nobody logged in. The account's own user units run a headless compositor (sway) and an X server on it (Xwayland, rootful, as a window that fills the output), and the node serves that display: `[source.native] own_session = "x11"` derives the runtime directory (`/run/user/UID`) and the display (`:UID`) from the uid the node runs as, checks that the directory is the account's and mode 0700 (otherwise `gui info` says `available: false` and names the path), and serves the display. Lock detection is off and the desktop is never locked (there is no lock screen). Input is XTest on that X server; the whole root window is readable, so a snapshot is the real screen. A program the account starts with `DISPLAY=:UID` appears on it.

Everything needed comes with `packaging/systemd/own-desktop/`: `bohio.service` (the system unit, `User=` the account, after its `user@UID.service`), the user units `bohio-desktop.target`, `bohio-compositor.service` and `bohio-xwayland.service`, `sway.conf` (the output's size), the optional `bohio-desktop-portal.target` (the distribution's PipeWire, WirePlumber and `xdg-desktop-portal` units, and their wlroots configuration, for `own_session = "wayland"`, a view only), `own-desktop.cfg` and `install.sh`. As root, `packaging/systemd/own-desktop/install.sh [--account NAME] [--portal] [--binary FILE]` creates the account (a system account, no login shell, no password), enables lingering for it, installs and starts the user units, installs the configuration when there is none, and installs and starts the system unit; it prints each step, a second run changes nothing, `--dry-run` prints without doing, and `--uninstall` undoes it (`--remove-account` also removes the account and its home, which holds the node's identity). The steps by hand are in the header of `bohio.service`. The output's size is `resolution` in `sway.conf` and `-geometry` in `bohio-xwayland.service`, the same in both.

The simplest input that works is the X11 backend on that X server: it needs nothing beyond sway and Xwayland, where `xdg-desktop-portal-wlr` has no RemoteDesktop (so `own_session = "wayland"` on a wlroots compositor is a view) and wlroots' virtual pointer and keyboard would need a backend of their own. The cost is that the screen is the X server's: a program that speaks only Wayland is not on it. Whatever `gui.input` reaches there is whatever the account can run; see [Security](security.md#known-limits).


[The protocol](protocol.md#desktop-bohio-gui1) has the wire and [Security](security.md#what-gui-access-gives-a-peer) what a view of the screen gives a peer. A host that already runs a VNC server is reached with `forward -L` under a `forward.connect` rule for it.
## Having a host listen for you

The other direction: the host listens and each connection that arrives is brought to this side, `ssh -R`. The grant is `forward.listen` and a `listen` rule for each bind, and a bind defaults to loopback:

```bash
bohio peer approve <id> --listen 127.0.0.1:0 --listen 127.0.0.1:18090-18099 --listen unix:/run/user/1000/app.sock
```

```bash
bohio remote tcp://host.example:7620 forward -R 0=127.0.0.1:8080          # prints: remote 127.0.0.1:41873 -> 127.0.0.1:8080
bohio remote tcp://host.example:7620 forward -R unix:/run/user/1000/app.sock=unix:/tmp/app.sock
bohio remote tcp://host.example:7620 listen 127.0.0.1:18090 --keep          # each arrival meets standard input and output
```

Every kind of end pairs with every kind on the other side: a TCP port, a Unix socket, a Windows named pipe, or standard input and output, in either direction (`-L`, `-R`, `connect` and `listen`). A socket or pipe the host makes is for the host's own account. [The protocol](protocol.md#remote-forwards-bohio-listen1-and-bohio-accept1) has the wire, and [Security](security.md#what-a-remote-forward-gives-a-peer) what a remote forward gives a peer.

## Forwards that survive a lost link

A forward declared in `bohio.cfg` is kept by the node, as a WireGuard peer is: it is made again when the other side comes back, and nothing is run. On the node that dials (or is dialed by) the host:

```toml
[peer."<host id>"]
endpoint = "tcp://host.example:7620"          # this node dials it, and dials again after a loss

[peer."<host id>".forward]
local  = ["127.0.0.1:5432=db.lan:5432", "unix:/run/app.sock=pipe:app"]   # -L: this machine listens, the host connects
remote = ["127.0.0.1:0=127.0.0.1:8080"]                                    # -R: the host listens, this machine connects
```

A `local` listener stays bound while the link is down; a connection that arrives then waits up to `[forward] wait_for_link` (30 s) and is closed after. Each `remote` entry is asked of the host again whenever the link is up, for the port the host chose the first time. The host must grant it (`forward.connect` and a `connect` rule for the targets, `forward.listen` and a `listen` rule that covers the port, such as `127.0.0.1:*`). Entries are read again every `[link] recheck`. `bohio remote ... forward`, `connect` and `listen` do the same for one run, and end when the link does only with `--once`; a connection that is running still ends with its link. [Security](security.md#what-a-kept-forward-costs) says what a standing forward costs.

## Programs that survive a lost link

`exec --detach` starts a program that keeps running when the command ends or the link is lost, with its output kept on the host; the command prints the session's id. The peer needs `session` as well as `exec` (or `shell`). A new link picks it up again from any offset, and a Ctrl-C of a command is an interrupt to the program, not the end of the command:

```bash
bohio remote tcp://host.example:7620 exec --detach --name build -- make all
# prints 4f1c...e9
bohio remote tcp://host.example:7620 sessions
bohio remote tcp://host.example:7620 attach 4f1c...e9 --from 0       # everything the host still keeps, then live
bohio remote tcp://host.example:7620 end 4f1c...e9 --signal interrupt
bohio remote tcp://host.example:7620 exec --pty --timeout-ms 60000 -- top    # a program on a terminal, with a deadline
```

`shell --detach` does the same for a shell, whose terminal output the host keeps. [Security](security.md#what-a-session-gives-a-peer) says what a session costs the host.

## Finding things and reading logs without a shell

A peer that holds only `fs.read` can search a tree on the host, so only the matches cross the link, and follow a log as it grows. `stats` and `which` need `info`:

```bash
bohio remote tcp://host.example:7620 search "TODO|FIXME" /srv/data -g "*.rs" -C 2
bohio remote tcp://host.example:7620 glob "**/*.log" /srv/data
bohio remote tcp://host.example:7620 read --follow /srv/data/app.log
bohio remote tcp://host.example:7620 stats
bohio remote tcp://host.example:7620 which git python3
```

A search reads what a read would and never follows a link out of the root; [Security](security.md#what-search-stats-which-and-a-follow-give-a-peer) says what it costs the host. Every command takes the global `--format json` for output a script can parse ([the shapes](cli-json.md)), and a host named in `[remote.NAME]` of the configuration is reached as `bohio remote NAME ...`.

## An agent on a host

`bohio mcp` is a Model Context Protocol server on standard input and output that gives an agent the tools of a coding harness (`exec`, `read`, `write`, `edit`, `glob`, `search`, and sessions for programs that outlive a call) on the host, bounded by what the host grants its key. Give each agent its own configuration, so the host approves that key with the least it needs:

```bash
bohio peer approve <agent id> --allow info,fs.read,exec --fs-root /srv/data --program git
claude mcp add bohio -- bohio mcp --config agent.cfg --host "lab=tcp://host.example:7620 --server <host id>"
```

[Agents and MCP](agent-tools.md) lists the tools, their bounds and how to configure Claude Code, Codex and other clients, and [Security](security.md#what-an-agents-tools-give) says why the grant is the only bound.

## Configuration

`bohio.cfg` is read as layers: the system file (`/etc/bohio/bohio.cfg`, `%ProgramData%\bohio\bohio.cfg`) and then the user's (`~/.config/bohio/bohio.cfg`, `%APPDATA%\bohio\bohio.cfg`) merged over it, or the files `--config` names, alone. Tables merge by key and lists and values replace, so a higher layer can narrow a list. Every setting, with its default, is in [`examples/bohio.cfg`](https://github.com/Boriken-Dev/bohio/blob/main/examples/bohio.cfg); the `[peer.*]` tables are read again at every decision, the rest when the node starts.

## Learn more

- [The protocol](protocol.md): the wire, for an implementer in any language, with test vectors.
- [Security](security.md): what the service holds, what a peer is granted, where isolation stops, and the known limits.
- [Agents and MCP](agent-tools.md): the tools an agent has on a host, and `bohio mcp` for any MCP client.
- [Machine-readable output](cli-json.md): `--format json` for every command, and its shapes.
- [Profiles](profiles.md): named starting points for the transfer settings: a LAN, a weak host, a slow link, a long fast path.
- [Embedding a node](embedding.md): running a node inside another application, from its own settings, key and peers.
- [Using it from hostctl](hostctl.md): how a hostctl provider maps `run`, paths, shells and capability selection onto Bohío.
- [The Rust API reference](https://boriken-dev.github.io/bohio/rust/): every public item of `bohio`, `bohio-client` and `bohio-proto`.
