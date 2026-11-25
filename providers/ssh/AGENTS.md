# `bohio-provider-ssh` — API header

Header-file-style reference for the `bohio-provider-ssh` crate: the `ssh` provider of the `bohio_terminal` and `bohio_exec` kinds, with
every setting and gotcha, so it can be configured and embedded without reading its source. Development documentation lives with the
source at <https://github.com/Boriken-Dev/bohio>.

No Cargo features. MSRV Rust 1.89. Depends on `bohio-api` (the kinds), `russh` (pure Rust over `ring`: no C toolchain, no `aws-lc`),
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

## Gotchas

- Channels count against the server until they are closed: a handle dropped without `kill` or the program's end still holds a slot until the host
  closes the channel (`exit_grace` after the exit).
- A server accepts only a few failed logins: a wrong key can lock the account's throttle for a while.
- A write that cannot make progress for `operation_timeout` fails; a connection that stops answering is dropped by `keepalive`.
- From Windows into WSL's loopback relay a bulk full-duplex transfer has stalled the whole session in measurement: bulk tests run inside WSL.

Tests: `tests/fake.rs` runs the provider against an in-process server (`tests/support/`, needs `sh`); `tests/real_sshd.rs` starts a private no-root
`sshd` on loopback (on Windows inside WSL `FedoraLinux-44`).
