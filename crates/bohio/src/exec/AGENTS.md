# `bohio::exec` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Execution (`bohio::exec`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a ExecutionConfig, pub compression: &'a CompressionConfig, pub slots: &'a Slots, pub peer: &'a str /* the peer's share of `slots` is counted by it */, pub path: &'a OsStr, pub sources: &'a Sources }   // Copy; `path` is the host's own PATH
pub fn serve(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<bohio_proto::exec::Exit, ErrorBody>;
```

Serves one `bohio-exec/1` stream: `metadata` is the open's CBOR request. Order of checks: decode (`bad_request`), execution enabled
(`unsupported`), the grant holds `exec` (`denied`), the command (empty argv or a NUL: `bad_request`; unlisted program or a shell
script under a program list: `denied`), the working directory exists (`not_found`), a free slot (`busy`), the start
(`not_found`, `denied`, else `io`). A refused request gets one `error` frame, the same error is returned, and the stream stays
open for the caller to close.

A started process runs in a tree of its own (a job object on Windows; a process group on Unix, plus a walk of the descendants (`/proc` on Linux, `ps` elsewhere)
for processes that left it). Frames out: `started`, then `stdout` and `stderr` chunks as the process writes them, then `exit`
after every output byte. The tree is ended by a `kill` frame, the stream closing or failing, the request's `timeout_ms`
(`Exit::timed_out` and `killed`), and always after the process exits: output is awaited for `drain_timeout`, then whatever the
process left running is killed, so nothing outlives a request. Sending waits while the stream has no room, so a slow client
slows the process instead of growing a buffer. `serve` does not close the channel.


Signals: a `signal` frame (`0x15`) is answered with `signalled` (`delivered`, and a `message` when it was not or when delivering meant something else). On Unix a signal goes to the program's process group (`SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2`) and `kill` ends the tree as the `kill` frame does. On Windows `terminate` and `kill` end the tree through the job object; `interrupt` is `CTRL_BREAK_EVENT` to the program's group when the host has a console (the program is then started in a group of its own on that console, so `platform::exec_signals()` lists `interrupt` only then) and `delivered: false` otherwise; the others are `delivered: false`. `info.exec.signals` and `info.shell.signals` say what the host delivers.
- `serve` takes `Arc<dyn Channel>` because its output threads outlive a wedged pipe: a descendant the platform cannot end that
  keeps a pipe open leaves a thread behind after `drain_timeout`, not a hung stream.
- A `kill` frame queued behind standard input the process is not reading is seen only when it reads, when the deadline passes or
  when the stream closes.
- Standard input sent to a request that asked for `Stdin::Closed` is ignored.
- Without a program list the name is looked up as the host's `PATH` finds it, with the request's own `PATH` if it sets one (on Windows
  also the application's directory and the system directories, never the current one): such a peer may run anything anyway. `programs` compares the name exactly as
  requested, so `git` and `/usr/bin/git` are different entries.
- Windows: a process started by `exec` holds exactly its three standard streams of the host (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`); the
  standard library's `Command` would pass every inheritable handle, and `TcpStream::try_clone` makes inheritable sockets, so a connection a
  child held neither closed when its owner dropped it nor closed cleanly when the child was killed. A batch file (`.bat`, `.cmd`) is not
  started directly (its arguments would be read by `cmd`): run `cmd /C` on it, or use a `cmd` shell command. A shell's terminal (ConPTY)
  inherits no handle either. A program that the `stdio://` transport of a client starts is `coqui-transport`'s, and still inherits.
- Windows: a process starts a moment before it joins its job object; a process it starts in that instant is outside the tree. A
  shell's terminal (ConPTY) starts the child itself, so the same moment applies to a shell session.
