# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- A program list (`exec.programs`) is now checked against what starts: each listed name is resolved on the host's own
  `PATH` to an absolute path and that path is spawned, and a request that sets or removes `PATH`, or clears the
  environment, is refused. A peer allowed `git` can no longer run its own `git` by pointing `PATH` at a directory it
  controls. `exec::Context` and `shell::Context` carry the host's `PATH` (`exec::Context::path`).

### Added

- `bohio service install|uninstall|run` runs the host as a Windows service (`windows-service`): `install` registers it to start
  automatically as `NT AUTHORITY\LocalService` (or `--account`), `uninstall` stops and removes it, and the service serves the configured
  `[[listen]]` endpoints until the manager stops it. The account must be able to write `[service] data_dir`.
- Every coqui transport carries the link: `pipe://` (Windows named pipes and Unix domain sockets, `coqui-transport-pipe`), `ws://`
  (WebSocket binary messages, `coqui-transport-web`), `serial://port?baud=N` (COBS frames with a CRC-32C, `coqui-transport-serial`) and
  standard input and output. `bohio serve --stdio` serves one link on standard input and output (for `docker exec -i`), and
  `bohio remote --command "program args"` or `stdio://program args` starts a program and uses its standard streams as the link.
  `[[listen]]` takes the settings of its scheme (`socket_mode`, `remote_clients`, `origins`, `baud`, `data_bits`, `parity`,
  `stop_bits`, `flow_control`), `bohio status` reports every configured endpoint, and `bohio-client` has `dial::Endpoint`.
  The proof and the grants are unchanged; `wss://` is refused until its TLS setup exists.
- The authenticated link over TCP, on `coqui-link`: `bohio serve` proves each peer (Noise by default) within `[link] proof_timeout` and
  serves its streams; a key that proves itself and is not accepted is recorded as waiting under its source address and refused. Each
  stream is decided from `peers.cfg` when it opens and every `[link] recheck` while it runs, so a revoke or a narrowed grant ends
  running processes, shells and transfers (the peer sees a `Permission` failure).
- Memory per link is bounded by `[link]` settings with documented defaults: `window`, `max_frame`, `max_message`, `queue_bytes`,
  `read_ahead`, `queued_bytes`, `send_limit`, `send_wait`, `poll_interval`, plus `max_connections` and `proof_timeout`. A 64 MiB file
  crosses the link in both directions with the host's resident memory growing by about 1 MiB.
- `bohio-client`: `connect::Connection` (a proven, owned `Open` over TCP or any coqui channel), `relay::Relay` (an owned, bounded channel over
  one stream of a link) and `link::Settings`.
- Commands: `bohio serve`, `status`, `id`, `peer list|approve|revoke`, `pair` (a PIN), `join` and `remote info|exec|read|shell`.
- Interactive shells (`bohio::shell`, `bohio-shell/1`): a shell kind from `info` or an explicit program on a
  pseudo-terminal (`portable-pty`: `openpty`, ConPTY), terminal output streamed raw, input, resize and kill frames, a
  bound on concurrent sessions (`[shell] max_sessions`), and the program's whole tree ended when the stream closes.
  `bohio-proto` has the shell request and frames; `bohio-client` has `shell::Shell`.

- `bohio-proto`: the wire types of protocol version 1 and their CBOR codec: control calls
  (`hello`, `info`), structured errors with a closed code list, the host's capabilities, shell kinds
  and the `bohio-exec/1` request and frames.
- `bohio` library: `bohio.cfg` configuration (unknown keys refused, every setting documented with
  its default, searched in `$BOHIO_CONFIG`, then the user's, then the system's location),
  `peers.cfg` per-peer grants (`info`, `fs.read`, `fs.write`, `exec`, `shell`, `admin`, filesystem
  roots, a program list), and the host's capabilities.
- `bohio config check` and `bohio info` (text or `--format json`); exit status 0, 1 or 2.
- Process execution (`bohio::exec::serve`): argv run directly or an explicit shell script, working
  directory and environment changes, standard input, streamed output and error, a deadline, kill and
  closed-stream handling that end the whole process tree, and a bound on concurrent processes
  (`[execution] max_processes`).
- `bohio-client`: the `Open` trait for a link and `exec::Exec` to run a process.
- Filesystem access confined to a peer's `fs_roots` (`bohio::fs`, `bohio::control`): `stat`, `list` (paged), `mkdir`,
  `remove`, `rename`, `copy`, `symlink`, `readlink`, `set_times`, `set_mode`, `set_attributes` and `checksum` on the control
  stream, and file streams `bohio-read/1` and `bohio-write/1` (atomic replace through a temporary sibling, or create, append
  and write at an offset). A `..` or a symbolic link that leaves a root is refused as `outside_root`, with every component
  resolved against a directory handle; the header states what is race-free on each operating system. New `[filesystem]`
  settings bound transfers, chunks, listing pages and idle time.
- `bohio-proto`: the filesystem calls, metadata and the transfer frames. `bohio-client`: `control::Control`, `fs::Fs`,
  `fs::Reader` and `fs::Writer`.
