# `bohio::session` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Sessions (`bohio::session`, `[sessions]`)

```rust
pub struct Sessions;                     // a node's detached sessions: Sessions::new(&SessionsConfig, grants, keys, events) -> Arc<Self>; Debug; ends every session when dropped
pub struct SessionsConfig { enabled, max_detached /* 16 */, max_per_peer /* 8 */, scrollback /* 1 MiB */, max_scrollback /* 16 MiB */, linger /* 24h */, keep_exited /* 1h */, check_every /* 5s */, queue_frames /* 16 */ }
```

An exec or shell request with a `session` (and the peer's `session` permission) runs the ordinary engine on an in-process channel that nothing closes when the client's stream ends; `Sessions` reads the engine's frames into a buffer with one byte offset across both output streams and serves `bohio-attach/1` from it. `on_full = pause` (the exec default) leaves frames in the engine's channel so the program waits; `drop_oldest` (the shell default) forgets the oldest. One stream is attached at a time (a newer one supersedes). The sweeper (one thread per node, started with the first session) every `check_every` ends and removes a session whose owner's key is not accepted or whose grant lost `session` or `exec`/`shell`, one detached past its linger, one exited past `keep_exited`; `Drop` ends all. `Node` serves it (`session::wants_exec`/`wants_shell` pick the request); `control` answers `sessions` and `end_session` through `control::SessionsView`. A session's streams are not compressed. `NodeEvent::Session` tells each change.
