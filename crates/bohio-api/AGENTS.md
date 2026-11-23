# `bohio-api` — API header

Header-file-style reference for the `bohio-api` crate: every public item with its signature, contract and gotchas, so the
crate can be used without reading its source. It ships inside the package and is self-contained. Development documentation lives
with the source at <https://github.com/Boriken-Dev/bohio>.

| Feature | Adds | Default |
| --- | --- | --- |
| `testing` | `bohio_api::testing`: in-memory sources for tests | off |

MSRV Rust 1.89. Depends on `guatiao` (a git revision, feature `provider`), re-exported as `bohio_api::guatiao`.
`#![forbid(unsafe_code)]`: guatiao's macros generate the C shims safely. Version `0.0.0-alpha.0`, not published.

Imports: the root holds the four source traits (`TerminalSource`, `DesktopSource`, `PowerSource`, `MediaSource`), the object
kinds (`Terminal`, `DesktopSession`, `BlockReader`), the models, `unsupported(msg)` and `unavailable(msg)` (the two
`ProviderError`s a provider answers most). Modules: `terminal`, `desktop`, `power`, `media`, `model`, `vocab`, `contract`,
`testing` (feature).

## Kinds

Every method takes `&self` and answers `Result<_, ProviderError>`. A defaulted method is optional and answers `unsupported`.
A kind grows only by appending defaulted methods; each kind's `FLOOR_HASH` is pinned by `tests/kinds.rs`.

```rust
#[guatiao::kind(name = "bohio_terminal")]          pub trait TerminalSource: Send + Sync { fn describe(&self) -> Result<SourceInfo, _>; fn open(&self, TerminalOpen) -> Result<Object<dyn Terminal>, _>; }
#[guatiao::kind(object, name = "bohio_terminal_session")]
pub trait Terminal: Send + Sync {
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, _>;   // bytes; 0 = none in time; -1 = ended for good
    fn write(&self, data: &[u8]) -> Result<u64, _>;
    fn kill(&self) -> Result<TerminalExit, _>;                        // ends the program and its tree; a console only lets go
    fn exit(&self) -> Result<Option<TerminalExit>, _>;                // None while it runs
    fn pid(&self) -> Result<Option<u64>, _> { Ok(None) }
    fn resize(&self, cols: u16, rows: u16) -> Result<(), _> { unsupported }
    fn signal(&self, name: &str) -> Result<Delivery, _> { unsupported }
}
#[guatiao::kind(name = "bohio_desktop")]           pub trait DesktopSource: Send + Sync { fn describe(..); fn open(&self) -> Result<Object<dyn DesktopSession>, _>; }
#[guatiao::kind(object, name = "bohio_desktop_session")]
pub trait DesktopSession: Send + Sync {
    fn state(&self) -> Result<StateModel, _>;
    fn frame_size(&self, display: &str, region: Option<RectModel>) -> Result<FrameHeader, _>;
    fn capture(&self, display: &str, region: Option<RectModel>, dst: &mut [u8]) -> Result<FrameHeader, _>;   // RGB into the caller's buffer, once
    fn input(&self, events: Vec<InputModel>) -> Result<u64, _> { unsupported }
    fn cursor(&self) -> Result<Option<InputModel>, _> { Ok(None) }
    fn clipboard_text(&self) -> Result<String, _> { unsupported }
    fn set_clipboard_text(&self, text: &str) -> Result<(), _> { unsupported }
}
#[guatiao::kind(name = "bohio_power")]             pub trait PowerSource: Send + Sync { fn describe(..); fn state(&self) -> Result<PowerState, _>; fn act(&self, action: &str) -> Result<PowerState, _>; }
#[guatiao::kind(name = "bohio_media")]
pub trait MediaSource: Send + Sync {
    fn describe(..); fn status(&self) -> Result<Vec<MediaSlot>, _>; fn eject(&self, slot: &str) -> Result<MediaSlot, _>;
    fn insert_url(&self, slot: &str, url: &str) -> Result<MediaSlot, _> { unsupported }
    fn insert_reader(&self, slot: &str, name: &str, reader: Object<dyn BlockReader>) -> Result<MediaSlot, _> { unsupported }
}
#[guatiao::kind(object, name = "bohio_block_reader")] pub trait BlockReader: Send + Sync { fn size(&self) -> Result<u64, _>; fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<u64, _>; }
```

A provider's status maps to the wire error a peer sees: `UNSUPPORTED` is `unsupported`, `NOT_FOUND` `not_found`, `BAD_VALUE`
`bad_request`, `BUSY` `busy`, `GONE` `unavailable`, `INTERNAL` `io`.

## Models (`bohio_api::model`)

All derive guatiao's `ToValue`, `FromValue` and `Schema`; an `Option` field is optional on the wire and a model grows by such fields.
`SourceInfo { provider, offers, lacks, available, reason }` (`SourceInfo::new(provider, offers, lacks)`, `.unavailable(reason)`,
`.lacks(word)`); `TerminalOpen { program: Option<ProgramSpec>, cwd, clear_env, env: Vec<EnvVar>, path, cols, rows }` (Default 80x24);
`ProgramSpec { kind, argv, listed }`; `TerminalExit { code, signal }`; `Delivery { delivered, ended, message }`;
`StateModel { user, active, locked, displays: Vec<DisplayModel> }`; `DisplayModel`; `RectModel`; `FrameHeader { width, height }` (`len()` is
`width * height * 3`); `InputModel` (a flat event: `kind` is `move`, `button`, `scroll`, `key` or `text`); `PowerState { state, done }`;
`MediaSlot { slot, inserted, image, size }`. Vocabularies are text, as `bohio_api::vocab` names them: a word a host does not know is carried, never refused.

## Contract suite (`bohio_api::contract`)

`terminal(&dyn TerminalSource, TerminalOpen, &ContractSettings)`, `desktop`, `power`, `media`: panic with a message naming what the source got wrong.
They check `describe` against behaviour (a source that `lacks` `resize` answers it `unsupported`), that `read` returns within its wait plus
`ContractSettings::slack` (default 2 s) and that nothing panics on an argument that is wrong but possible.

## Testing (`bohio_api::testing`, feature `testing`)

`library` (register with `Registry::register_local("bohio-api-testing", testing::library)`) holds providers `fake_terminal`, `fake_desktop`,
`fake_power` and `fake_media`, each configured with the name of an in-memory link a test reaches with `terminal_link(name)`, `desktop_link`,
`power_link` and `media_link` to play the far end.
