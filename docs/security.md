# Security

Bohío exposes the operating system to peers it has proven. This page says what a peer can reach, what the service itself holds, where isolation stops, and the limits known today.

## Two sets of privileges

The service runs as one operating system account, and each peer holds a grant. They are different things, and the second never exceeds the first.

- **The service's privileges** are those of the account that runs `bohio serve`. Everything a peer does, a process it starts, a file it reads or writes, a shell it opens, happens as that account. A peer cannot do what the account cannot.
- **A peer's grant** is what `peers.cfg` says that key may do: `allow` (any of `info`, `fs.read`, `fs.write`, `exec`, `shell`, `admin`), `fs_roots` (the directories file operations are confined to) and `exec.programs` (the programs it may start). A peer with no entry, or no `allow`, holds nothing. Being proven opens no stream; each stream is decided against the grant when it opens and again every `[link] recheck`, so a revoke ends what is running.

So the account bounds the worst case, and the grant narrows it. Run the service as an account that has only what its peers need, and give each peer the least that works.

A grant of `exec` or `shell` is the account's own authority: a peer that may start a program may do whatever that account may do with the program's arguments, and a peer that holds a shell holds that account's shell. `fs_roots` confines Bohío's file operations; it does not confine a process or a shell a peer starts, which can open any file the account can. Grant `exec` or `shell` only to a peer trusted with the account.

## Identities and pairing

A peer is a key, named by its id. A host proves its own key to a peer, and a peer proves its key to the host, before any stream opens. The host's key is in `identity` in its data directory (mode `0600` on Unix); the peers it trusts are in `peers/`. A key that proves itself and is not accepted is recorded as waiting under its source address and refused; the host's operator approves it with `bohio peer approve`.

`bohio pair` shows a one-time PIN. Anyone who can reach its endpoint may try the PIN once; a wrong PIN uses it up, so a guess costs the next pairing. A paired peer holds nothing until the operator approves it with a grant. Pairing runs over `tcp://` only. Nothing logs a PIN or a key: logs carry a peer id (a public value) and a protocol name.

## What the program list does

`exec.programs` limits **which program starts**, not **what it does**. A list holding `git` lets a peer start `git`, and `git -c alias.x=!command` runs any command; `make`, `find -exec`, `xargs`, `ssh`, `docker` and every interpreter run anything they are given. List only programs that are safe with every argument a peer can give them, and treat a list that names a build tool or an interpreter as no restriction.

With a non-empty list, each name is resolved on the host's own `PATH` to an absolute path and that path is started, never what the request's `PATH` or working directory would find, and an explicit shell script is refused (a shell runs anything). A request may set or remove only the environment variables `exec.env` names (`bohio peer approve --program NAME --env VAR`), never `PATH`, and may not clear the environment: a variable is as strong as an argument, since `LD_PRELOAD` and `DYLD_INSERT_LIBRARIES` load a library the peer chose into the program, and `BASH_ENV`, `GIT_SSH_COMMAND` or `PYTHONPATH` make it run code the peer chose. Name only variables that are safe with any value. The working directory is still the peer's choice, and some programs read their configuration from it (`git` runs `core.fsmonitor` from a repository's own config), so a program that does is as strong as its configuration. An empty list means any program and any environment.

## Filesystem confinement

Every path resolves against a directory handle of a peer's `fs_roots`: `..` and an absolute path outside a root are `outside_root`, and so is a symbolic link that leaves a root, whether at the end of a path or in the middle. On Linux each lookup is one `openat2` with `RESOLVE_BENEATH` (kernel 5.6 and later), and elsewhere an `openat` walk that refuses to follow a link at each component, so a link swapped in while a call runs cannot lead out.

The root's owner is trusted: a hard link another local user makes to a file outside a root exposes that file, and a bind mount or junction an administrator places inside a root is part of the root.

## Container isolation

A container is a boundary only as strong as its runtime's configuration, and Bohío adds none to it.

- `bohio serve --stdio` inside a container, reached with `docker exec -i NAME bohio serve --stdio`, serves the container's own filesystem and processes. A peer's grant bounds what it asks for; the container bounds what that can reach. A peer with `exec` in a container can do what the container's user can do there.
- The image in `packaging/container/` runs as a non-root user with no extra capabilities. A container started with `--privileged`, extra capabilities (`CAP_SYS_ADMIN`, `CAP_NET_ADMIN`), the host's PID or network namespace, a mounted container-engine socket or a bind mount of the host's root hands that reach to every peer that may `exec` or `shell`, and Bohío cannot narrow it.
- Resource limits (memory, processes, CPU) are the runtime's. Bohío's `max_processes`, `max_sessions` and `max_transfers` bound how many things start, not how much they use.
- A shared kernel is shared: a peer that runs code in a container can attack the kernel. Where peers are not trusted with that, give each its own virtual machine.

## Privileged deployments

The risk grows with the account. A service run as `root`, `SYSTEM`, or any account in the `Administrators`, `docker` or `wheel` group, with a peer that holds `exec`, `shell` or `fs.write` outside a narrow root, gives that peer the machine.

- Prefer an unprivileged account. The systemd unit in `packaging/systemd/bohio.service` runs as `User=bohio` with `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome=true` and `PrivateTmp=true`, and writes only `StateDirectory=bohio`. `bohio service install` registers a Windows service as `NT AUTHORITY\LocalService`, which has no administrative rights.
- When a task needs more, widen it in the smallest way: a group that owns one directory, a command allowed through `sudo` for one account, a dedicated account for the deployment. Do not run the whole service as an administrator to let one peer do one thing.
- `fs_roots` set to `/` or a drive root, or a root that holds the account's own configuration (`bohio.cfg`, `peers.cfg`, the `identity` key) lets a peer with `fs.write` change who else is trusted. Keep roots away from the data directory.
- A peer with `admin` is trusted to manage the host; no engine needs it today. Grant it to nobody.
- `ws://` is served without TLS beyond loopback, because the proof seals every message after it. A peer that reads the network sees metadata (message sizes and timing) and the source address, not content. `wss://` is not served yet.
- A listener that is reachable is attackable: bind to the narrowest address, use a firewall, and prefer `pipe://`, `serve --stdio` or a loopback address behind a tunnel when a peer is on the same machine. `[link] max_connections` and `proof_timeout` bound what an unproven connection may cost.

## Known limits

These are recorded and not fixed. Each is either a platform limit or a window that needs a local attacker.

- **Windows directory calls are path-based.** On Windows `list`, `readlink`, `mkdir`, `remove`, `rename`, `symlink` and `set_attributes` resolve the parent through its current path and join the name, because the library that confines file access does not offer handle-relative calls there. A local user who can rename a directory inside a root while one of these calls runs can redirect it outside the root. `stat`, `checksum`, reads and the opens of writes are handle-relative and not affected. The root's own path is opened by path on every platform. The fix needs `NtCreateFile` and `SetFileInformationByHandle` relative to the parent's handle; until then, do not give a root to a peer on a Windows machine where an untrusted local user can write to the root's directories.
- **A Windows process starts a moment before it joins its job object.** A process the program starts in that instant is outside the tree and survives an ended session, a `kill` frame or a deadline. A shell's terminal starts its child itself, so the same moment applies to a shell session. A program that spawns children in its first milliseconds can leave one behind.
- **A killed `serve` on Unix leaves the exec trees it started.** The host ends a tree when its stream closes, on a deadline and on a `kill` frame. A host that dies without running that code (`SIGKILL`, the operating system's out-of-memory killer, power loss) leaves the processes it started, in their own process groups, running. A Windows host's job object ends them when the process dies. The systemd unit uses `KillMode=control-group`, so stopping or restarting the unit ends every process in its control group; a service manager without that does not.
- **A serial line has one peer at a time, and a reconnect can lose a message.** The host opens the port, serves one link and opens it again about 250 ms later, discarding what arrived before. A client whose first message arrives before the host has reopened the port, such as a second `remote` right behind the first, is dropped and must try again. Each failed attempt costs the proof timeout.
- **`peers.cfg` is rewritten whole and without comments** by `peer approve` and `peer revoke`, atomically; two such commands at the same instant can lose one change.
- **A revoked peer's link ends at the second recheck** (`2 x recheck`) at the latest; its streams end at the first. Between a revoke and the recheck a running stream goes on.
- **A message or a key is never logged, but a process list is public.** `exec` argument vectors are not logged by the host; a program started on the host appears in that machine's process list with its arguments. `bohio join --pin` puts the PIN on a command line; without `--pin` it is read from standard input.
- **A service has no console.** A Windows service writes its diagnostics to standard error, which nothing reads; run the command in a terminal with `BOHIO_LOG` to see them. Under systemd they go to the journal.
- **A loopback connect to a closed port in the ephemeral range can connect a socket to itself.** `bohio join` and the client refuse such a connection.

## Reporting

A security problem is reported privately to the maintainers at `contact@boriken.dev`, not in a public issue.
