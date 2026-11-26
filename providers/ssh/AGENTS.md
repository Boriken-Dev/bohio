| `x11_display` | none | the host's X display to serve as the desktop (`:0`); a number, never a host |
| `x11_socket_dir` | `/tmp/.X11-unix` | where the host keeps its X sockets (`X<n>` in it) |
| `xauth` | `xauth` | the program on the host that lists the display's cookie (`xauth list :N`); a host without it is served with no cookie |
| `x11_forward` | `false` | programs this source starts open their windows on the node's own display (needs `X11Forwarding yes` and an `xauth` on the host); needs `x11_trusted` |
| `x11_trusted` | `false` | forward as `ssh -Y` does; required by `x11_forward` because this build cannot ask the display for an untrusted authorization |
| `x11_local_display` | the node's `DISPLAY` | the node's display forwarded windows go to: `:N` (its socket) or `HOST:N` (TCP port 6000+N) |
| `x11_max_connections` | `64` | forwarded X connections open at once; one past it is refused; at least 1 |
| `client` | `russh` | who speaks SSH: `russh` (this provider's client) or `openssh` (the system's `ssh` program, see below) |
| `ssh_program`, `ssh_args` | `ssh` on `PATH`, none | the `openssh` client's program, and arguments put after the options this provider sets and before the host |
# `bohio-provider-ssh` — API header

Header-file-style reference for the `bohio-provider-ssh` crate: the `ssh` provider of the `bohio_terminal`, `bohio_exec`, `bohio_files` and `bohio_desktop` kinds, with
every setting and gotcha, so it can be configured and embedded without reading its source. Development documentation lives with the
source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `bohio-api` (the kinds), `bohio-x11` (the X11 backend and the desktop types), `x11rb` (the display authority file), `getrandom` (the forwarding cookie), `russh` (pure Rust over `ring`: no C toolchain, no `aws-lc`), `russh-sftp` (the files),
`tokio` (a small runtime for each source) and `tracing`. `#![forbid(unsafe_code)]`. Version `0.0.0-alpha.0`, not published.

Imports: the root holds `Ssh` (the provider), `SshSettings` (its settings; `SshSettings::new(host)` has every default), `ID` (`"ssh"`) and
`library` (written by `guatiao::local_providers!`: `registry.register_local("bohio-ssh", bohio_provider_ssh::library)`; `bohio` does it with
feature `ssh`).

## The provider

Provider id `ssh`. `[source.<name>] provider = "ssh"` and:

| Key | Default | Meaning |
| --- | --- | --- |
| `host` | required | name or address |
| `port` | `22` | 1 to 65535 |
| `user` | `USER` / `USERNAME` | the account on the host |
| `identity` | first of `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa` that exists | a private key file (`~/` expanded) |
| `agent` | `true` | try the keys an agent holds first: the OpenSSH agent's pipe `\\.\pipe\openssh-ssh-agent` (or the pipe `SSH_AUTH_SOCK` names) then Pageant on Windows, `SSH_AUTH_SOCK` elsewhere; no agent is not an error |
| `password`, `passphrase` | none | **references** (`file:PATH`, `credential:NAME`, `keyring:SERVICE/ACCOUNT`), never the secret; the node reads them when it builds the source |
| `host_key` | none | `sha256:HEX` of the host's public key blob; a pinned key is the only one accepted (except under `off`) |
| `known_hosts` | `~/.ssh/known_hosts` | read by `strict` and `tofu` |
| `host_key_check` | `strict` | `strict` (pinned or in known_hosts), `tofu` (a host with no entry is trusted at first contact and that key is held for the life of the source), `off` (any key) |
| `connect_timeout` | `15s` | connecting and logging in; not zero |
| `keepalive`, `keepalive_max` | `30s`, `3` | probe an idle connection; `0s` sends none; drop it after that many unanswered |
| `max_channels` | `8` | session channels open at once (the server's `MaxSessions` is 10 in OpenSSH); a request past it waits `operation_timeout`, then is `busy` naming this key; a server that refuses sooner says so |
| `reconnect`, `reconnect_after` | `true`, `5s` | make the connection again on the next use; after a failed connect the source is `unavailable` without trying for this long (`0s` tries every use) |
| `operation_timeout` | `30s` | waiting for a slot, for the server to open a channel or answer a request, and for a write; not zero |
| `exit_grace` | `1s` | how long output and the end of a channel are awaited after the program exited (a descendant can hold the channel open) |
| `queue_chunks` | `16` | chunks of output held ahead of the reader |
| `worker_threads` | `2` | threads of the source's runtime |
| `sftp_in_flight` | `16` | SFTP requests of one file in flight at once (reads ahead, writes behind); over a slow link throughput is this many packets per round trip, and the host holds this many packets of a file in memory; at least 1 |
| `sftp_packet` | `32768` | bytes of data one SFTP read or write carries; the server's `limits@openssh.com` and 255 KiB cap it; at least 1 |

A key it does not have, or a value that cannot work (a zero timeout, a port of 0, a malformed pin), is refused naming `source.<name>.<key>`
before the provider is built. Everything else weaker than the default is accepted and logged as `non-default:` by the built-in provider.
Building the source never touches the network or a key file; the first `describe`, terminal or process connects.

## Behaviour

- Login order: agent keys, then the key file, then the password. The refusal names each way tried and never a secret.
- **Exec**: one exec channel. The command line is `cd 'DIR' && exec env [-i] 'K=V' [-u 'K'] 'prog' 'arg'...`, every word single-quoted for the host's
  login shell, which must be a POSIX one (a Windows server is not served). The environment is on the line because servers refuse the `env`
  channel request by default. A shell kind (`sh`, `bash`, `zsh`, `powershell`, `pwsh`; `cmd` is refused) runs a script as `KIND -c SCRIPT`.
  Standard output and standard error are separate; exit status or signal arrive in either order with the end of the channel, and the process
  ends `exit_grace` after the exit. A program missing on the host is exit status 127 with the shell's message, not `not_found`; a missing
  working directory is exit status 1.
- **Terminal**: a pty (`TERM` from the request's environment, else `xterm-256color`) running the program, or the account's login shell when
  the request names none. Resize is a window change; `interrupt` writes the control byte `0x03` (the line discipline signals the foreground
  program), the other signals are SSH signal requests.
- **Signals** are SSH `signal` requests: OpenSSH 7.9+ delivers them to the program (not its group), others ignore them, and one that arrives before
  the program has started is lost. `kill` signals KILL, closes the channel and reports the program as ended by signal 9 whether or not the
  host answers; the program's descendants are not ended (`lacks` `kill_tree`).
- A program under a program list is the one the host finds at that name on the account's own `PATH`, not the node's.
- `describe`: asks for the connection (waiting at most `connect_timeout + operation_timeout`); `available: false` with the reason when it
  fails, and for `reconnect_after` after that without trying again.

## Files (SFTP)

One SFTP session on a channel of the connection (it counts against `max_channels`), opened on first use and again after the connection is
remade. `open_dir(path)` takes an **absolute POSIX path** of the host; every name below it is joined to it, and a `..` or an absolute
name is `outside_root` before it reaches the host. `describe` offers `read`, `write`, `symlink`, `set_times`, `set_mode`, `set_readonly`,
`list_stats` and `posix_paths`, plus what the server's version reply advertises:

| Extension | Offered as | Without it |
| --- | --- | --- |
| `posix-rename@openssh.com` | `atomic_replace` (rename over a name in one step) | `rename(.., overwrite)` over an existing name is `unsupported`; to a free name it is the plain rename |
| `hardlink@openssh.com` | `hard_link` | `unsupported` |
| `statvfs@openssh.com` | `statvfs` | `unsupported` |
| `fsync@openssh.com` | `sync` | `unsupported` (the library alone would skip it silently) |

It **lacks `confined`**: the server resolves each name by path on every call, so a symbolic link inside a directory can lead out of it. The
node's own check (a path is under one of the peer's `fs_roots`) is lexical and holds; a link a peer can make with `symlink` (or one the
machine has) is the machine's to police. SFTP version 3 has no "exists" or "not empty" status: the provider asks the server (`lstat`,
`readdir`) to tell `exists`, `not_empty`, `is_a_directory` and `not_a_directory` from a plain failure. Times are whole seconds (the protocol
has no more) and are set both at once (the one left alone is read first); read-only is the write bits of the mode. `remove_dir_all` walks the
tree on the client and never follows a link. A file reads ahead and writes behind by `sftp_in_flight` packets: a write's failure is told by
the next call that can (`write_at`, `sync`, `set_len`, `stat`, `read_at`), and a file dropped with writes in flight waits for them before
its handle is closed. A server without an `sftp` subsystem makes only the files unavailable, with that reason.

## The system's ssh client (`client = "openssh"`)

Each terminal, program and SFTP session is one `ssh` process (`-tt` or `-T` and the command; `-s host sftp` for the files), with
`-o BatchMode=yes`, `ConnectTimeout`, `ServerAliveInterval`/`CountMax` (from `keepalive`), `StrictHostKeyChecking` (`yes`, `accept-new`
for `tofu`, `no` with a null known_hosts for `off`), `UserKnownHostsFile` (when `known_hosts` is set), `-i` and `IdentitiesOnly` (when
`identity` is set), `IdentityAgent=none` (when `agent = false`), `-p` and `-l`, then `ssh_args`. The account's own `ssh_config`,
Kerberos and `ProxyJump` apply, and the program's own agent is what logs in. A session counts against `max_channels` like a channel.
`describe` runs `ssh ... true` (at most `connect_timeout + operation_timeout`) to say whether the host can be reached.

What it cannot do, refused when the source is built: `host_key` (the client keeps `known_hosts`), `password` and `passphrase` (batch
mode never asks), `x11_display` and `x11_forward` (they need channels of one connection). What a session lacks, said in `describe`:
terminal `resize` (a pty starts at the client's default size, and the new size is not passed on) and `signals` and exec `signals`
(`interrupt` on a terminal is still the control byte; `kill` ends the ssh process and the host's program is left to its own hangup).
A program's exit status is the client's: `255` is ssh's own failure, and its message is on the standard error (merged into the
output under a terminal). A desktop through the helper (`[gui] source`) needs nothing more, since it is an exec session. The
test suites run against it with `BOHIO_TEST_SSH_CLIENT=openssh`; tests of what only the russh client does print `SKIPPED`.

## Desktop (X11)

With `x11_display` set the source is a `bohio_desktop` source: each connection of the X backend (`bohio_x11::X11Desktop`, headless, no
lock detection) is a `direct-streamlocal@openssh.com` channel to the display's socket, which does not count against `max_channels`
(OpenSSH's `MaxSessions` counts sessions) and is joined to a loopback connection of the node that only the connection made for it can
take (x11rb reads a socket). The cookie is `xauth list :N` run through an exec channel, asked once and again after a connection
failed. `describe` says `lock_detection` is lacking; without `x11_display` the source is `unavailable` with that reason. A root that
cannot be read (XWayland, WSLg) is composed from the viewable windows by the backend. A Wayland-only host has no display to name.

With `x11_forward` every session (program or terminal) asks the host for X forwarding with a random cookie of the source; a host that
refuses is logged and the program runs without it. The host's `x11` channels are accepted by the handler (russh's default accepts and
drops them), the first request is read, its cookie compared with the source's and replaced with the one `x11rb`'s authority file lookup
gives for `x11_local_display`, and the bytes are relayed until either side ends. Only `x11_trusted = true` is supported: the SECURITY
extension's untrusted authorization has no binding in x11rb, so `x11_forward` without it is refused when the source is built. A node
with no X display (Windows) fails each forwarded connection with the reason in the log.

## Gotchas

- Channels count against the server until they are closed: a handle dropped without `kill` or the program's end still holds a slot until the host
  closes the channel (`exit_grace` after the exit).
- A server accepts only a few failed logins: a wrong key can lock the account's throttle for a while.
- A write that cannot make progress for `operation_timeout` fails; a connection that stops answering is dropped by `keepalive`.
- An SFTP session that stops answering fails a request after `operation_timeout`; the next call opens another session.
- From Windows into WSL's loopback relay a bulk full-duplex transfer has stalled the whole session in measurement: bulk tests run inside WSL.

Tests: `tests/fake.rs` runs the provider against an in-process server (`tests/support/`, with an SFTP subsystem over the real file system; programs need `sh`);
`tests/real_sshd.rs` starts a private no-root `sshd` on loopback (on Windows inside WSL `FedoraLinux-44`; `BOHIO_TEST_REQUIRE_SSHD=1` makes a missing one a
failure instead of a skip). Both run `bohio_api::contract::files`.

`crates/bohio/tests/ssh.rs` has an ignored measurement against a real machine from another site, `a_live_machine_serves_programs_and_files_through_the_ssh_source`
(`BOHIO_TEST_LIVE_SSH=HOST,USER,REMOTE_DIR`, agent login, `BOHIO_TEST_LIVE_BYTES`): a node serves the machine to an in-process peer and prints the time of
`true` and of one file going there and back. A real-server run is a measurement of one link, never a CI result. With `BOHIO_TEST_SSH_CLIENT=openssh`
the real-sshd tests use the system's client (on Windows `icacls` first narrows the test key file, which `ssh.exe` insists on).
