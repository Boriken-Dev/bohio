# `bohio::shell` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Shells (`bohio::shell`)

```rust
pub struct Context<'a> { pub grant: &'a Grant, pub config: &'a ShellConfig, pub slots: &'a Slots, pub peer: &'a str, pub path: &'a OsStr, pub sources: &'a Sources }   // Copy
pub fn serve(channel: Arc<dyn Channel>, metadata: &[u8], context: &Context<'_>) -> Result<bohio_proto::shell::Exit, ErrorBody>;
```

Serves one `bohio-shell/1` stream: `metadata` is the open's CBOR request, `path` the host's `PATH` (where a shell kind or a program
without a directory is looked for; the library reads no environment variable). One stream is one session, bounded by
`[shell] max_sessions` across peers. Order of checks: decode (`bad_request`), shells enabled (`unsupported`), the grant holds `shell`
(`denied`), the program (`Program::Kind`: `Other` is `bad_request`, a shell not found on `path` is `not_found`; `Program::Argv`: empty
or a NUL is `bad_request`, a name not on `path` is `not_found`, and under a non-empty `exec.programs` list a name not on it is `denied`,
while a kind is always allowed: `shell` already means a shell), the size (`bad_request` outside `1..=32767`), the working
directory (`not_found`), a free slot (`busy`), the start (`not_found`, `denied`, else `io`). A refused request gets one `error` frame,
the same error is returned, and the stream stays open for the caller to close. With `argv` the request is `exec` on a terminal: `timeout_ms` is its deadline (`Exit::timed_out`, `killed`). On Windows an `interrupt` signal types Ctrl-C (`0x03`) at the pseudo-console, which reaches the program unless Ctrl-C is ignored in the host's process chain (inherited; measured); on Unix signals go to the program's process group.

The program runs on a terminal (`portable-pty`: `openpty` on Unix, ConPTY on Windows) in a tree of its own (a job object on Windows; on
Unix `portable-pty` makes it a session and group leader, and the host also walks the descendants (`/proc` on Linux, `ps` elsewhere) for what left the group, such as a
job-control shell's background jobs). Frames out: `started {pid}`, `data` as the terminal produces output, then `exit` after every
output byte (the terminal must be quiet for `drain_quiet` after the program exited, since a pseudo-console can deliver the last of
the output after the exit). The tree is ended by a `kill` frame, the stream closing or failing, and always after the program exits:
nothing outlives a session. `TERM` is `xterm-256color` unless the request's environment sets it. A resize outside `1..=32767` is
ignored. Typed bytes go to the terminal as they are (Enter is `\r`). Sending waits while the stream has no room, so a client that
reads slowly slows the shell.
- Windows ConPTY asks the terminal for its cursor position before the program's first output and holds that output until it is
  answered: the host removes the query (`ESC[6n`) from the output and answers it with `ESC[1;1R`.
- A `kill` frame queued behind typed input the shell is not reading is seen only when it reads, or when the stream closes.
