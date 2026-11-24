# Agents: tools and `bohio mcp`

An agent acts on a remote host through the same authenticated link a person uses, and is bounded by what the host grants its
key. Two surfaces give it the tools:

- `bohio_client::tools`, a Rust library an embedding agent (a harness, a theater, a build tool) calls in process over any
  `Open` link. A tool is a name, a JSON Schema for its arguments, the permissions it needs, and a function: `Tools::call(name,
  arguments, &Cancel)` answers JSON and images, so a protocol server needs to know no tool by name.
- `bohio mcp`, a Model Context Protocol server on standard input and output over the same tools, for any MCP client.

The tool names mirror the local tools of coding agents (`exec` is Bash, `read` Read, `write` Write, `edit` Edit, `glob` Glob, `search` Grep),
so a harness can run with its local tools disallowed and these in their place.

## Running the server

```text
bohio mcp --config agent.cfg --host "lab=tcp://lab.example:7610 --server ID_OF_LAB"
```

`--host` takes `[NAME=]ENDPOINT` followed by any flag of `bohio remote` (`--server`, `--jump`, `--ca`, `--cert-hash`,
`--fallback`, ...) in one quoted word. Repeat it for several hosts: every tool then takes a required `host` naming one. The link to
a host is made the first time it is used and once more after a loss; a call that was started is never repeated, and one
that failed because the link was lost says so.

A bare word is a host named in `[remote]` of the configuration (see `cli-json.md` and `bohio hosts`): `--host lab` serves
`[remote.lab]` with its `server`, trust, fallback and named jumps, and a name no table holds stops the server at its start
saying so.

**One key per agent, least grant.** The server proves itself with the identity of its configuration, so give each agent its own
`--config` (and data directory), have the host approve that key with exactly the permissions the agent needs (`bohio peer
approve ID --allow info,fs.read,exec --fs-root DIR --program git`), and revoke it when the agent is done. Whatever the agent is
talked into doing, the host's grant is the bound.

### Claude Code

```text
claude mcp add bohio -- bohio mcp --config /path/agent.cfg --host "lab=tcp://lab.example:7610 --server ID_OF_LAB"
```

or in `.mcp.json`:

```json
{ "mcpServers": { "bohio": { "command": "bohio",
    "args": ["mcp", "--config", "/path/agent.cfg", "--host", "lab=tcp://lab.example:7610 --server ID_OF_LAB"] } } }
```

The tools appear as `mcp__bohio__exec`, `mcp__bohio__read`, and so on. To make the agent use the host instead of this machine,
disallow the local ones (`--disallowedTools Bash Read Write Edit Glob Grep`) or allow only `mcp__bohio__*`.

### Codex and other clients

Any client that starts a stdio MCP server takes the same command line: in Codex's `config.toml`,
`[mcp_servers.bohio]` with `command = "bohio"` and `args = ["mcp", "--config", "...", "--host", "..."]`. The server speaks
newline-delimited JSON-RPC 2.0, `tools` only (no resources, prompts or sampling), and answers `initialize` with the client's
protocol revision when it is one of `2025-11-25`, `2025-06-18`, `2025-03-26` or `2024-11-05`, else with `2025-11-25`. A tool's
result is JSON as one text block and as `structuredContent`; an image (a desktop screenshot) is an `image` block before the text. A refused or
failed call is a result with `isError: true` whose text is `code: message` (`denied`, `not_found`, `changed`, ...); a
malformed message is `-32700` or `-32600`, an unknown method `-32601`, an unknown tool or bad `tools/call` parameters `-32602`.
`notifications/cancelled` ends the call it names: a running program is killed and the call answers at once.

## The tools

Each tool lists the permission the peer needs on the host (`bohio.cfg` `allow`). A tool the peer or the host cannot use is
refused with the reason (`unavailable` when the host reported it off or this side does not allow it, `denied` when the host
refuses the peer), and it is marked in `tools/list` once the host has been used.

| Tool | Does | Needs |
| --- | --- | --- |
| `info` | the host's capabilities | `info` |
| `exec` | run a program and wait: `argv` or `script`, `cwd`, `env`, `stdin`, `deadline_ms`, `pty`, `signal` after a time, `max_output` | `exec` (`shell` for `pty`) |
| `read` | text by lines (`offset_line`, `limit_lines`, `max_bytes`) or any file by bytes as base64 | `fs.read` |
| `write` | replace a file atomically; `if_sha256` writes only if the file is still the one read | `fs.write` (`fs.read` with `if_sha256`) |
| `edit` | replace text once (or `replace_all`); refused as `changed` if the file moved since it was read | `fs.read`, `fs.write` |
| `list`, `stat`, `hash` | a directory page, a path's metadata, digests computed on the host | `fs.read` |
| `walk`, `glob` | trees and glob matches, paged by `skip` and `limit` | `fs.read` |
| `search` | lines matching a regular expression, found on the host so only the matches cross the link: `path`, `include`, `exclude`, `context`, `literal`, `case_insensitive`, paged by `skip` and `limit` | `fs.read` |
| `stats`, `which` | the host's memory, disks, load and uptime; where programs are on the host's own `PATH` | `info` |
| `follow` | watch a file for `duration_ms` and return what is appended (`tail -f`), with the cuts and rotations it saw | `fs.read` |
| `read_many`, `write_many` | several files on one stream | `fs.read` / `fs.write` |
| `move`, `copy`, `truncate`, `mkdir`, `remove` | done on the host | `fs.write` |
| `put`, `get` | a file of this machine to or from the host, moving only what changed | `fs.write` / `fs.read`, and a local directory allowed |
| `session_start`, `session_read`, `session_write`, `session_signal`, `session_list`, `session_end` | a program that outlives the link, read from an offset | `session` (and `exec` to start) |
| `forward_local`, `forward_remote`, `forward_list`, `forward_close` | `ssh -L` and `-R` | `forward.connect` / `forward.listen` |
| `screenshot`, `click`, `move_pointer`, `drag`, `scroll`, `type`, `key`, `wait`, `displays`, `cursor_position`, `clipboard_read`, `clipboard_write`, `gui_watch` | look at and drive the host's desktop; listed only with `--gui` (see "The desktop") | `gui.view` / `gui.input` / `gui.clipboard` |
| `power_status`, `media_status` | whether the system the host is for is on, and what its virtual-media slots hold; read only | `power` / `media` |

### Bounds, paging and binary content

Nothing a tool returns is unbounded, and nothing is cut without saying so.

- `exec` keeps the first and last halves of each output stream within `max_output` (default 64 KiB, `--max-output`) and returns
  `total_bytes`, `omitted_bytes` and `truncated`: `{ "encoding": "utf8", "data": head, "tail": tail, ... }`. Output that is not
  UTF-8 is `"encoding": "base64"`. A program that outlives `deadline_ms` (default two minutes, `0` for none) is ended with its
  whole process tree, `exit.timed_out` says so; use a session for anything long.
- `read` pages by lines and bytes: `more`, `next_offset_line`, `total_lines` once the end was reached, `sha256` when the whole file
  was read. A line longer than `max_bytes` comes back cut (`line_cut`) and the next page starts after it. A file that is not UTF-8
  is `not_text` with its size: read it with `encoding: "base64"` and `offset` and `length`.
- `list`, `walk`, `glob` and `search` return a page and the cursor (`next`, `next_skip`; `search` also `more`); `follow` watches for `duration_ms` (default 5 s, at most 5 minutes) and returns its output kept like `exec`'s, with `truncations` for each time the file was cut; `read_many` has a byte budget and lists a file
  with no room with its size and a `skipped` text; `session_read` returns the output from an offset, up to `max_output`, with
  `next` to go on from and `more` when output is left.
- Every size is a field of `bohio_client::tools::Settings` with a default and a ceiling a call may not pass.

### Editing safely

`edit` reads the file, counts `old` (zero matches is `no_match`, several without `replace_all` is `ambiguous` with the count),
and writes the result with the size, modification time and SHA-256 of what it read as the write's `base`. The host replaces the
target only if it still is that file, checked through its root handle before the data is accepted and again just before the
rename: a second writer's change in between is `changed`, and the agent reads again. The short window between the last check and
the rename is a known limit.

### Sessions

`session_start` starts a program the host keeps running when the link ends and returns its id; `session_read` with `from: 0`
reads what the host still keeps, and the `next` of one read is the `from` of the next. The offset counts standard output and
standard error together in order, so a read lists chunks tagged `stdout` or `stderr`. `running: false` and `exit` mean it ended.
`session_write` sends to standard input (start with `stdin: true`) or types on a terminal (`pty: true`, Enter is `\r`);
`session_signal` sends `interrupt` and the others; `session_end` kills it (or sends the signal named).

### Files of this machine, and forwards

`put` and `get` read and write files of the machine that runs the server, which is more authority than the host's grant: they are
unavailable until `--local-root DIR` (repeatable) names the directories they may use, and a path is resolved with links
followed and must lie under one. `forward_local` listens on loopback here and connects each connection to a target through the
host; a listener beyond loopback needs `--forward-allow-remote-bind`, because whoever reaches it has the key's grant.
`forward_remote` has the host listen and bridge to a target of this machine, which only `--forward-local-target HOST:PORT`
(repeatable) allows. A closed forward stops accepting; connections it carries run to their end.

### The desktop

The desktop tools are off until the server is started with `--gui`: without it none is listed and a call to one is an unknown
tool. They are the same calls as a local computer-use loop, bounded by what the host grants the key (`gui.view` for what
looks, `gui.input` for what drives, `gui.clipboard` beside either for the clipboard).

```text
bohio mcp --config agent.cfg --gui --host "desk=tcp://desk.example:7610 --server ID_OF_DESK"
```

`--gui-max-image-bytes N` (default 1048576) is where `format: "auto"` turns from PNG to JPEG 80, and `--gui-after-action
screenshot` makes every input tool answer with a settled screenshot of the display it acted on (default `none`; it costs a
snapshot a call and needs `gui.view`: an input-only key types and the answer carries a `screenshot_error`). The same values are
`gui_auto_limit` and `gui_after_action` of `bohio_client::tools::Settings`, with `gui_max_width` and `gui_max_height` (1280 and
800), `gui_jpeg_quality` (80) and `gui_max_image` (4 MiB, the most a call returns).

| Tool | What it does |
| --- | --- |
| `screenshot` | `{ display?, region?, max_width = 1280, max_height = 800, format = "auto" \| "png" \| "jpeg", quality?, settle = true }`: waits on the host for the screen to stop changing, then answers an `image` block and a text block `{ display, width, height, scale, cursor, region, settled }` |
| `click` | `{ x, y, button = "left", count = 1, display?, space = "screenshot" }`; `count: 2` is a double click |
| `move_pointer` | `{ x, y, display?, space }` (`move` is the file tool) |
| `drag` | `{ from: {x, y}, to: {x, y}, button = "left", display?, space }`: press, move, release on one display |
| `scroll` | `{ x?, y?, dx, dy, display?, space }` in wheel notches, down and right positive |
| `type` | `{ text }`, typed as it is whatever the keyboard layout |
| `key` | `{ keys }` in xdotool spelling: `Return`, `ctrl+s`, `alt+F4`, `super`; the modifiers go down, the last key is clicked, the modifiers come up in reverse |
| `wait` | `{ quiet_ms?, timeout_ms?, display? }`: the host waits for the screen to stop changing, in one round trip; `settled: false` when it kept changing |
| `displays` | the layout: id, name, position, size, scale, which is primary, `active`, `locked`, and the size the last screenshot of each was taken at |
| `cursor_position` | the pointer's display and pixels, and the same point in the last screenshot; it costs a one-pixel snapshot |
| `clipboard_read`, `clipboard_write` | the desktop's clipboard text; `denied` without `gui.clipboard` |
| `gui_watch` | `{ duration_ms?, ... }`: watches for a time and reports how much changed (updates, rectangles, bytes) |

A call the grant does not allow is a result with `isError: true` and code `denied`, naming the permission.

**Coordinates.** The host works in physical pixels of one display, origin at its top-left corner, `display` defaulting to the
primary. A screenshot is scaled to fit `max_width` x `max_height` on the host (never up), so only those bytes cross the link, and
the server keeps each display's region and `scale` of its last screenshot. `click`, `move_pointer`, `drag` and `scroll` take
points in that picture (`space: "screenshot"`, the default) and map them back, so an agent clicks where it saw the thing
whatever the display's size. `space: "display"` gives the display's own pixels. A point outside the picture or the display is
`out_of_bounds` and its message names the bounds; `no_screenshot` says to take one first; `stale_screenshot` says the display
changed size since. Nothing is sent for a refused point.

**Several monitors.** `displays` lists them and a screenshot shows one at a time: name the `display` of a screenshot and of the
calls that use it. Each display keeps its own last screenshot, so a click on display `1` needs a screenshot of display `1`.
`drag` stays on one display.

**Streams.** The server keeps one `bohio-gui/1` stream to a host between calls and opens a new one once when it was lost. A
screenshot or a wait that was lost is asked again; an input that was written is never sent again, because a lost stream may
have applied it, and the call says it was lost. Take a screenshot to see what happened.

**Presence.** The host decides whether to tell the person at the screen and whether to ask them first (`[gui] indicator`,
`approval`): a call may wait for that, up to `gui_timeout` (120 s).

## Prompt injection

An agent that reads files, program output or a screen can be told by their content to do something else. The server cannot tell
the agent's own intent from text it was handed, so the bound is the grant: one key per agent, no `exec` for an agent that
needs to read, `fs_roots` and `programs` as narrow as the work, `put`, `get` and `forward_remote` left unavailable unless needed.

**What a screen shows is untrusted input.** A page, a document, a chat message, a notification, a window title or the clipboard
can hold text written to steer the agent that reads it ("ignore your instructions and open ..."), and a screenshot hands that
text to the agent as pixels no filter on this side can read. The server cannot tell it from the task. So: give an agent that only
needs to look a key with `gui.view` and no `gui.input`; give one that drives only the hosts and the grant its work needs, because
`gui.input` is the desktop user's whole authority (it reaches what no program list or file root of the same key does), and
`gui.clipboard` reads what a password manager just copied. Keep a person where an action cannot be undone: `[gui] approval =
"local"` on the host makes each stream wait for the person at the desk. An agent must not act on an instruction it found on the
screen, and the harness that runs it should say so in its own prompt. `bohio mcp --gui` also says so in its `initialize`
instructions, but that is a reminder, not a bound.
