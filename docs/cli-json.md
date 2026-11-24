# Machine-readable output

`--json` (anywhere on the command line, for every command) makes `bohio` write results a program reads instead of lines a
person reads. The flag is global: `bohio remote HOST stats --json` and `bohio --json remote HOST stats` are the same. There is
no other format switch.

Exit statuses do not change with `--json`: `0` success, `1` the operation failed, `2` the invocation was wrong (and, for
`search`, `1` for no match and `2` for something that could not be read; `which` is `1` when a name was not found).

Two commands do not follow the flag yet: `remote power` and `remote media` print their text lines whatever `--json` says (a
script reads their exit status), and `remote console` is a terminal, so its bytes are its payload. Everything else on this page
does what it says.

## The three kinds of command

| Kind | Standard output | Standard error |
| --- | --- | --- |
| **Result** (one answer: `info`, `id`, `peer list`, `stats`, `sessions`, `put`, ...) | one JSON object on one line | nothing, or one `{"note": TEXT}` object per diagnostic |
| **Stream** (`serve`, `pair`, `remote exec`, `walk`, `glob`, `search`, `hash`, `forward`, `gui watch`) | JSON Lines: one object per line, each with an `event` key naming it | as above |
| **Payload** (`remote read`, `connect`, `listen`, `shell`, `attach` of a shell, `gui snapshot -o -`) | the payload, byte for byte, as without `--json` | the final status as one `{"done": {...}}` object, and the events named below |

An error ends the command with `{"error": {"code": CODE, "message": TEXT}}` on standard error and a status of `1` (a
command that failed after printing some results leaves them on standard output). `CODE` is what failed: for a refusal by the
host, its error code as `protocol.md` names it (`denied`, `not_found`, `outside_root`, `busy`, `bad_request`, ...); otherwise
`timeout`, `closed`, `link`, `transport`, `protocol`, `compression`, `local`, or `failed` for anything else. A stream that
fails for one item and goes on (a file that cannot be hashed) says so with a `failure` event and ends with status `1`.

Every line standard error holds is one JSON object (`note`, `error`, `done`, or an `event`). Diagnostics that `BOHIO_LOG`
turns on are log lines, not objects: they are for a person reading a log.

Payload bytes in a stream are base64 (`b64`, standard alphabet with padding), so a stream stays valid JSON whatever a program
prints.

## The shapes

Each line below is `COMMAND | KIND | KEYS`: the keys the object always has. A key that holds `null` says there is nothing
to report; an object that is a protocol type (`info`, `entry`, `hashes`) may carry more keys when they have a value. `tests/link.rs` runs every command and checks its output against this table.

```shapes
config check | object | ok,files,service,data_dir,listen,peers,dial,hosts,filesystem,execution,shell
info | object | protocol,service,host_id,os,arch,shells,filesystem,exec,shell,sessions,limits,transfer,forward,gui
id | object | id
status | object | id,method,data_dir,config,accepted,waiting,listen,dial,keeps
peer list | object | accepted,waiting
peer approve | object | approved,file
peer revoke | object | revoked,removed,unwritable
join | object | paired,file,peer
hosts | object | hosts
links | object | links
providers | object | check_access,libraries
serve/listening | event | event,endpoint
serve/host | event | event,id
pair/pairing | event | event,endpoint
pair/host | event | event,id
pair/pin | event | event,pin
remote info | object | protocol,service,host_id,os,arch,shells,filesystem,exec,shell,sessions,limits,transfer,forward,gui
remote stats | object | hostname,uptime_s,cpus,load,memory,swap,disks
remote which | object | found
remote write | object | written,hashes
remote put | object | size,literal_bytes,copied_bytes,wire_bytes,delta,skipped
remote get | object | size,literal_bytes,copied_bytes,wire_bytes,delta,skipped
remote sync | object | unchanged,bundled,bundles,singles,deltas,deleted,skipped,bytes,wire_bytes,dry_run,actions,failures
remote sessions | object | sessions
remote end | object | ended
remote exec --detach | object | session
remote connect --check | object | connected
remote exec/started | event | event,pid
remote exec/stdout | event | event,b64
remote exec/stderr | event | event,b64
remote exec/exit | event | event,code,signal,timed_out,killed
remote hash/hash | event | event,path,hashes
remote hash/failure | event | event,path,message
remote walk/entry | event | event,root,path,entry
remote glob/path | event | event,root,path,kind,size
remote search/match | event | event,root,path,line,text,before,after,truncated
remote search/done | event | event,files,matches,skipped,truncated
remote forward/listening | event | event,kind,address,target
remote read/done | stderr | done
remote shell/done | stderr | done
remote gui info | object | available,backend,reason,enabled,input,clipboard,formats,sessions
error | stderr | error
```

`remote exec` is a stream: `started` (the process id), then `stdout` and `stderr` as the program writes them, a `signalled`
when the host answers a signal this side sent for Ctrl-C (`signal`, `delivered`, and `message` when it was not), and last
`exit` (`code`, or `signal` for a program a signal ended, and `timed_out`; the same fields `exec`'s exit has on the wire). The
command's own status is the program's, as without `--json`. `exec --pty` and `shell` are payload commands: the terminal's
bytes go to standard output and the exit is `{"done": {"exit": {...}}}` on standard error.

## What each command prints

### Configuration and the node

- `config check`: the shape above; `listen` is a list of endpoints, `dial` a list of `{peer, endpoint}`, `hosts` the names in
  `[remote]`. A file that is not valid is the error object and status `1`.
- `info`: the host's capabilities exactly as `bohio_proto::info::Info` serializes them (see `protocol.md`, "Control").
- `id`: `{"id": "sha256:..."}`.
- `status`: `config` is the layer files read, `accepted` and `waiting` counts, `listen` a list of `{endpoint, state}`, `dial`
  of `{endpoint, peer}` and `keeps` of `{direction, entry, peer}`.
- `peer list`: `accepted` is a list of `{id, allow, fs_roots, programs, env, connect, listen, dial}` (`programs` is `"listed"`
  or null; `dial` the endpoint the table dials, or null) and `waiting` of `{id, source}`. `peer approve` returns the peer
  as `approved` (the same object) and the `file` it wrote; `peer revoke` the `removed` files and the `unwritable` ones that
  still accept the peer.
- `join`: the host's id as `paired`, the file written, and this side's own id as `peer`.
- `hosts`: `{"hosts": [{name, endpoint, server, ca, cert_hash, fallback, jump}]}`, in name order.
- `links`: `{"links": [...]}` as the node's local endpoint reports them.
- `providers`: `{"check_access": bool, "libraries": [{"path", "state", "detail", "digest"}]}` as the node's local endpoint reports them.
- `serve` streams `listening` (`endpoint`), `certificate` (`hash`, `endpoint`), `control` (`endpoint`), `host` (`id`),
  `dialing` (`endpoint`, `peer`) and, as each forward the node keeps comes up, goes down or fails, `kept` (`direction`
  `local` or `remote`, `state` `up`, `down` or `failed`, `spec`, `target`, `peer`, and `details` with the `address` of an `up`
  or the `reason` and `retry_in_ms` of a `failed`). `serve --stdio` prints nothing: standard output is the link.
- `pair` streams `pairing` (`endpoint`), `certificate`, `host` (`id`), `pin` (`pin`: the PIN is meant to be read out, so it is
  here as it is in text) and `paired` (`peer`); a pairing nobody completes is the error object.
- `gui pending`: `{"pending": [{id, peer, session, want}]}`; `gui allow ID` is `{"allowed": ID}` and `gui deny ID`
  `{"denied": ID}`.
- `service install` and `service uninstall` (Windows): `{"installed": NAME}` and `{"removed": NAME}`.

### `bohio remote`

- `info`, `stats` (`bohio_proto::control::Stats`), `which` (`found`: each name asked with its path or null), `sessions` (`sessions`
  as the host lists them, with `peer` for an `admin`) and `end` (`ended`: the id).
- `write`: `written` bytes and `hashes`, the digests the host answered. `put` and `get`: what travelled, as the text line says
  it (`wire_bytes` of `size`, `copied_bytes` taken from the old copy, `literal_bytes` sent, whether a `delta` was used or the
  file was `skipped` as unchanged). `sync`: the counts of the text line and, with `--dry-run`, the `actions` it would take
  (`{step, path, size}`, `step` `mkdir`, `create`, `update` or `delete`), plus the `failures` (`{path, message}`).
- `hash` streams one `hash` event for each file (`hashes`: algorithm name to hex) and a `failure` for each it could not hash.
  `walk` streams `entry` events (`root`, the `path` as the text output prints it, and the `entry` as the walk protocol
  carries it: `path`, `metadata`, `target`, `hashes`) and `failure` events (`path`, `message`). `glob` streams `path`.
  `search` streams `match` (`path`, `line`, `text`, `before`, `after`, `truncated`), `failure` and last `done` (`files` read,
  `matches`, `skipped`: `{binary, large, unreadable}`, and `truncated` when a bound of the host ended it).
- `read` writes the file; with `-f` or `-F` it goes on until interrupted and standard error carries `{"event": "truncated",
  "size", "replaced"}` when the file is cut or replaced. `connect` writes the connection's bytes and reports `{"done": {}}`;
  `connect --check` prints `{"connected": ADDRESS-or-null}`. `listen` reports `{"event": "listening", "address"}` on
  standard error when the host's listener is bound, then the connection's bytes on standard output and `{"done": {}}`.
- `forward` streams `listening` (`kind` `local`, `remote` or `socks5`, `address`, `target`) as each listener is up, and
  `non_default` (`bind`) for one bound where the operator named. The notes about a lost link are `{"note": ...}` on standard error.
- `exec --detach` and `shell --detach` print `{"session": ID}`; `attach` is `exec` or `shell` as the session is.
- `gui info` prints the desktop's `GuiInfo`; `gui snapshot` writes the image to the file named and prints `{width, height,
  format, display, scale, bytes, path}`, or with `-o -` writes the image to standard output and puts the same object under
  `done` on standard error; `gui click`, `move`, `scroll`, `type`, `key` and `clipboard --set` print `{"ok": true}`;
  `gui clipboard` prints `{"text": ...}`; `gui watch` streams `second` (with `--stats`) and last `total`.
