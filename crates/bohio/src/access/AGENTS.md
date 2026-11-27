# `bohio::access`, `bohio::power`, `bohio::media` and `bohio::secret` — API header

Part of the `bohio` API header (`../../AGENTS.md` in the crate root): its features, imports and environment variables are there.

## Sources and access (`bohio::access`, `[source.<name>]`, `[access]`)

```rust
pub struct Sources;     // Default = today's node; the registry of providers (guatiao) lives as long as it
impl Sources {
    pub fn from_config(source: &SourceTables, access: &AccessConfig) -> Result<Self, (String, String)>;   // (key, why)
    pub fn unconfigured() -> Self;                                                  // no source, not even native; built-in providers registered
    pub fn register_library(&self, name, describe) -> Result<(), String>;           // a linked guatiao library (`local_providers!`)
    pub fn load_libraries(&self, config: &ProvidersConfig) -> ProvidersReport;   // maps [providers] paths and starts [[providers.program]] into the registry (Node::new does, before it builds the sources); a library never replaces a built-in provider id
    pub fn libraries(&self) -> ProvidersReport;    // what the start did: { check_access, libraries: Vec<Library { path, state: Loaded|Skipped|Failed, detail, digest: Option<"sha256:..">  }>, programs: Vec<Program { name, path, state: Running|Failed, detail, digest, restarts }> (live: an exit is noticed here) }
    pub fn reconfigure(&self, source, access) -> Result<(), (String, String)>;      // unchanged tables keep their instance; a changed one is rebuilt and the old retired
    pub fn with_terminal / with_desktop / with_power / with_media / with_exec / with_files(self, name, Arc<dyn Kind>) -> Self;   // an embedder's own source; add_terminal / add_desktop / add_power / add_media / add_exec / add_files on &self
    pub fn with_access(self, Capability, names) -> Self;
    pub fn terminal / desktop / power / media(&self, name: Option<&str>) -> Result<Source<dyn Kind>, ErrorBody>;   // None: the default; unsupported when none
    pub fn names(&self, Capability) -> Vec<String>;  pub fn fs(&self) -> Switch;  pub fn exec(&self) -> Switch;
}
pub struct Source<K: ?Sized>;   // name(), get(), is_current() (false once its table changed: what runs on it ends `unavailable`, "the source was reconfigured")
```

`[source.<name>]` is `provider = "<id>"` plus that provider's own keys, checked against its schema before the provider is built
(`ConfigError::Invalid` naming `source.<name>.<key>`; an unknown provider lists the known ones; `native` needs no `provider` and takes no other). `[access]`
lists the sources of each capability, the first the default: `terminal` and `desktop` (absent: `["native"]`; `[]` serves none), `power`, `media` (default none), and
`fs` and `exec` (`"native"` or `"none"`, read when the node is built: `"none"` makes the engine `unsupported`). `[source.native]` keys, all optional: `read_chunk` `16384`,
`queue_chunks` `16`, `windows_capture` `"auto"`, `windows_frame_wait` `250ms`, `windows_capture_idle` `10s`, `windows_clipboard_wait` `500ms`, `clipboard_max_bytes` `1048576`, and for the Linux desktop `x11_display`, `x11_wayland` and `x11_session`
(absent: the command fills them from `DISPLAY`, `WAYLAND_DISPLAY` or `XDG_SESSION_TYPE`, and `XDG_SESSION_ID`; `Environment::{display, wayland, session_id}`), `x11_in_wayland` `false`, `x11_system_bus` (absent), `x11_bus_wait` `1s`, `x11_key_wait` `20ms`,
`x11_clipboard_wait` `500ms`, `x11_selection` `"CLIPBOARD"`, `x11_max_repeat` `1000`, `own_session` `"none"` (`none`, `x11`, `wayland`), `own_runtime_dir` (absent: `/run/user/UID`), `own_session_check_server` `true` (the X server on the own desktop's abstract socket must be run by the node's uid; `false` is logged `non-default:`), for the Wayland desktop (feature `gui-wayland`; used when the session is Wayland and `x11_in_wayland` is not set) `wayland_persist` `"node"` (`none`, `node`, `always`),
`wayland_restore_file` (absent: the command puts `gui-wayland-restore` in the data directory when `WAYLAND_DISPLAY` is set), `wayland_capture_wait` `5s` (not zero), `wayland_start_wait` `10s`, `wayland_retry` `30s`, `wayland_max_repeat` `1000`, and for the macOS desktop `macos_capture_wait` `10s` (not zero) and `macos_max_repeat` `1000`.
`Node::with_sources(Sources)` replaces them (call it first); `Node::sources()` reaches them for a reload; the desktop is chosen by `[access] desktop`, and the Windows capture keys are `[source.native]` keys, not `[gui]` ones.

## Power, media, secrets and the console

`bohio::power::serve` and `bohio::media::serve(&Arc<dyn Channel>, metadata, &Context)` serve one `bohio-power/1` and `bohio-media/1` stream (one request, one answer; the checks and errors are in `docs/protocol.md`): `Context { grant, config, slots: &Arc<Slots>, sources, peer, events }`. A source call is bounded by `action_timeout` (`power::within`); one that outlives it finishes on its own thread, its answer is dropped, and the thread holds its slot (`Slots::acquire_owned`) until the source returns. `NodeEvent::Power { peer, source, action, outcome }` and `NodeEvent::Media { peer, source, op, slot, outcome }` never name an image. An image path is opened by `fs::open_confined` inside the peer's `fs_roots` and read by the source through a `BlockReader`.

`bohio::secret`: `Secret` (`expose()`, `Debug`/`Display` print `[redacted]`), `SecretResolver::resolve(&self, reference, max_bytes) -> Result<Secret, SecretError>`, `NoSecrets`, `split(reference)` (`file:`, `credential:`). A sensitive setting of a provider's schema holds a reference or is refused naming `source.<name>.<key>`; `Node::new` leaves a source whose settings hold one out until `Node::with_secrets`.

A terminal source that starts no program is a **console**: `bohio-shell/1` with `Program::Console { source }` is served by `session::Sessions::serve_console` as one session the node owns, `console:<source>` (kept output `[sessions] scrollback`, a newer attach supersedes, a `kill` only detaches, never swept for an owner or a linger); it is listed by `sessions` and read by `bohio-attach/1` for a peer whose grant reaches the source.
