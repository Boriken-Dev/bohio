# Using Bohío from hostctl

[hostctl](https://github.com/jose-pr/hostctl) gives Python one `Host` for any target: `Host.run()`, `Host.path()` (a `pathapi.Path`), persistent processes from `Host.spawn()`, and providers that a system host ranks by what each can do. Bohío covers the same operations over one proven link.

This page describes how a future `BohioConfig` and `BohioHost` map hostctl's API onto Bohío's calls. hostctl is unchanged by this page, and the provider does not exist yet: it is the design the protocol was shaped to serve. The wire is in [the protocol](protocol.md).

## Reaching a host

A `BohioConfig` names where the host is and which key to trust. There is no password: a peer is a key.

```python
BohioConfig(endpoint, *, server=None, data_dir=None, command=None, shell="auto", pathname="auto", timeout=Timeout())
```

| Field | Meaning |
| --- | --- |
| `endpoint` | any [carrier](protocol.md#carriers): `tcp://host:port`, `pipe://name`, `ws://host:port`, `wss://host:port`, `webtransport://host:port`, `serial://port?baud=N` |
| `server` | the host's peer id to trust, instead of the `[peer.<id>]` tables `bohio join` wrote |
| `data_dir` | where this client keeps its own identity (the same layout `bohio` uses); the hosts it trusts are the `[peer.<id>]` tables of the `bohio.cfg` layers, or `server` |
| `command` | instead of an endpoint, a program whose standard input and output are the link |
| `shell`, `pathname` | as every hostctl config; `auto` reads them from `info` |

A host in a container needs no listening port. `bohio serve --stdio` serves one link on standard input and output, so the link is a program hostctl starts:

```text
command = ["docker", "exec", "-i", "build-container", "bohio", "serve", "--stdio"]
```

hostctl runs the program directly, never through a shell, and ends its tree when the host closes. The same works for `podman exec -i`, `kubectl exec -i` and `ssh host bohio serve --stdio`. This is `bohio remote --command` on the command line.

The client must hold the proof and link layers. Today that is the Rust `bohio-client` crate and the `bohio remote` command; coqui-link has JavaScript and Dart bindings, and a Python binding of the link would let the provider speak the wire directly. Until one exists, a provider can drive `bohio remote` as a child process for `info`, `exec`, `read` and `shell`, which is enough for `run()`, reading and `spawn()`; the file calls that change things (`write`, `mkdir`, `rename`, ...) need the wire.

## `Host.run`

`Host.run(*cmds, ...)` becomes one `bohio-exec/1` stream per call.

| hostctl | Bohío |
| --- | --- |
| a command and its arguments (an executor with `ExecutorCapability.ARGS`) | `command: {type: "argv", argv: [...]}`: run directly, never through a shell |
| a finalized script (`SCRIPT`) | `command: {type: "shell", kind, script}`, with `kind` one of `info.shells` that matches the host's flavour |
| `cwd=` | `cwd` |
| `env=` (additive) | `env: {set: {...}}`, with `clear` false |
| `input=` | `stdin: "open"`, the bytes as `0x10` frames, then `0x11` |
| no `input` | `stdin: "closed"` |
| `capture_output`, `stdout=`, `stderr=` | the host separates `0x02` and `0x03` frames; hostctl routes each as it does for any executor |
| `timeout=` (`Timeout.total`) | `timeout_ms`; the host ends the whole tree at the deadline |
| `CompletedProcess.returncode` | `exit.code`; a process ended by a signal has `signal` and no `code`, and its `returncode` is `-signal` as `subprocess` reports |
| `subprocess.TimeoutExpired` | `exit.timed_out`: `.pid` from `started`, `.output` and `.stderr` from the frames read, `.orphaned` is false because the tree was ended |
| `check=True` | the same `CalledProcessError` rule hostctl applies to every host |

The executor provider declares `ARGS`, `CWD`, `ENV`, and `SCRIPT` when `info.shells` is not empty. It does not declare `MANAGES_STATUS`: `exit` is the status.

A refusal arrives as one `error` frame before `started`: `denied` (the peer lacks `exec`, or the program is not on its list), `unsupported` (execution is off), `busy` (`max_processes`), `not_found` (no such program or working directory), `bad_request`. Nothing ran, so the provider raises `OperationNotStartedError` and the next provider is tried. After `started` an operation is never replayed.

## `Host.path`

`Host.path()` returns a composite `pathapi.Path` over the path providers. A Bohío path provider's operations map as follows. Each call goes to a `bohio/1` control stream unless it says otherwise; the provider keeps one open and reuses it.

| `pathapi` operation | Bohío |
| --- | --- |
| `stat`, `lstat` | `stat` with `follow` true or false |
| `exists`, `is_file`, `is_dir`, `is_symlink` | `stat`; `not_found` is false |
| `scandir`, `iterdir`, `walk` | `list`, following `next` as `after` until it is `null` |
| `open("rb")`, `read_bytes`, a seek | `bohio-read/1` with `offset` and `length`; one stream per open handle |
| `open("wb")`, `write_bytes` | `bohio-write/1`, mode `replace`: the file changes only when the host's `done` arrives, which is when hostctl's write stream closes |
| `open("ab")` | `bohio-write/1`, mode `append` |
| `open("xb")` | `bohio-write/1`, mode `create` |
| `open("r+b")` at an offset | `bohio-write/1`, mode `at` |
| `mkdir(parents=, mode=)` | `mkdir` |
| `unlink`, `rmdir`, `rmtree` | `remove` with `recursive` false (a file, a link or an empty directory) or true |
| `rename`, `replace` | `rename` with `overwrite` false or true |
| `symlink_to`, `readlink` | `symlink`, `readlink` |
| `touch`, `utime` | `set_times` |
| `chmod` | `set_mode` when `info.filesystem.unix_modes`; on Windows `set_attributes` with `readonly` |
| `NativeChecksum.checksum(algorithm)`, `host_checksum` | `bohio-hash/1`, which hashes on the host and needs no transfer: one stream of items, several algorithms in one pass, a result per file, and `unchanged` when size and time match |
| `NativeChecksum.supported_checksums()` | `info.transfer.hashes`, advisory; an algorithm not listed raises `NotImplementedError` without opening a stream |
| same-host copy | `copy` |

Not in Bohío, so `supports()` answers false and hostctl falls through to the next provider or raises: `hardlink_to`, `chown`, `owner` and `group`, and resolving a path on the host.

The provider's capabilities come from `info` and the peer's grant. With `filesystem.enabled` false, or a peer holding no root, it declares none and is unavailable. A peer with `fs.read` only gets the read-only operations (`READ_ONLY_PATH_OPERATIONS`); `fs.write` adds the rest. The path grammar follows `info.filesystem.separator`: `/` selects `PurePosixPath`, `\` selects `PureWindowsPath`, which is what `pathname="auto"` does. A host with `case_sensitive` false compares names without case.

Errors become the exceptions `pathapi` and `subprocess` callers expect:

| Bohío code | Python |
| --- | --- |
| `not_found` | `FileNotFoundError` |
| `exists` | `FileExistsError` |
| `denied`, `outside_root` | `PermissionError` (an `outside_root` message says the path left the peer's root) |
| `unsupported` | `NotImplementedError`, so an operation that cannot mutate falls through to the next provider for that call |
| `busy` | `OSError` with `EBUSY` |
| `io` | `OSError` carrying the host's message |
| `bad_request` | `ValueError` |
| `internal` | `RuntimeError` |

## Persistent processes and shells

`Host.spawn()` returns a `Process`. Bohío has two streams for it.

| `spawn` call | Stream |
| --- | --- |
| `terminal=True` or `TerminalOptions` | `bohio-shell/1`: `Program` is `kind` for a flavour in `info.shells`, or `argv` for a program; `cols` and `rows` from the options |
| `terminal=None` or `False` | `bohio-exec/1` with `stdin: "open"`, no deadline |

| `Process` method | Bohío |
| --- | --- |
| `read()` | `0x02` frames: standard output for exec, terminal output for a shell |
| `read_stderr()` | `0x03` frames; a shell has no separate error stream |
| `write(data)` | `0x10` frames |
| `send_eof()` | exec: `0x11`; a shell: the caller writes the terminal's end-of-file character |
| `wait()` | the `exit` frame; `returncode` is its `code` |
| `terminate()`, `kill()` | the `kill` frame, which ends the whole tree: Bohío has no gentler signal |
| `resize(columns, rows)` | a shell's `0x11` frame; an exec process cannot be resized |
| `close()`, leaving the `with` block | closing the stream, which ends the tree |

A session lives as long as its stream. A dropped link or a revoked grant ends the stream and the host ends the tree, so a `Process` never outlives its connection on the host. A host started with `[shell] max_sessions` answers `busy` past the limit.

`Host.shell_flavour` and `Host.shell` come from `info`: a `cmd` or `powershell` entry on a Windows host, `sh` or `bash` elsewhere, with the flavour's own quoting.

## Capability-based selection

hostctl ranks providers and picks the first that can serve an operation. A `BohioConfig` supplies two:

- an **executor provider** named `bohio`, available when `info.exec.enabled` is true and the peer is granted `exec`;
- a **path provider** named `bohio`, available when `info.filesystem.enabled` is true and the peer holds a root.

Both share one link, so closing either closes it once. `probe()` connects and proves the key (the one place a refused proof surfaces, with a reason that names what refused: the host did not answer, does not trust this key yet, or this side does not trust the host), then reads `info`. `ProviderProbe.capabilities` narrows the declared set to what `info` and the grant allow, and never adds to it. `availability` is `degraded` when one half is off and the other works, for example a read-only peer.

A system host composes it with others by name, as any provider:

```python
PosixConfig("build.example", executor=("bohio", "ssh"), path=("bohio", "sftp"),
            provider_options={"bohio": BohioConfig("tcp://build.example:7710", server="..."),
                              "ssh": SSHConfig("build.example")})
```

With that order a peer that holds `exec` runs commands over Bohío and, where the grant has no `exec`, `run()` falls back to SSH and logs the one fallback warning hostctl logs for any skipped provider. `host.last_selection` shows the trace. The descriptor `bohio` is registered through the `hostctl.system_providers` entry point, so an installed package is enough.

## What a host reports

`Host.info()` returns `HostInfo(os_family, architecture, ...)` from `info.os` and `info.arch`. `os_name`, `os_version` and `hostname` are `None`: Bohío reports the host id (the key's peer id), not a name, and a caller that wants a name runs a command for it.

## Timeouts

| `Timeout` field | Bohío |
| --- | --- |
| `connect` | the dial and the proof, bounded by `proof_timeout` on the host |
| `total` | `timeout_ms` of an exec request |
| `read`, `write` | how long the client waits for the next frame or for room on the stream; a Bohío config refuses `0` ("do not wait") as hostctl does for a transport that cannot return at once |
