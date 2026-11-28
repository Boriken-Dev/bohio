//! How a node reaches the system it is for: sources, the providers behind them, and which source serves each capability.
//!
//! A **capability** (terminal, desktop, power, media) is a `guatiao` kind of `bohio-api`. A **source** is a configured
//! instance of a provider; `[source.<name>]` in `bohio.cfg` builds one, `[access]` lists the sources of each capability
//! (the first is the default), and an embedder adds its own with [`Sources::with_terminal`] and its siblings. `native`,
//! this node's own terminal and screen, is a provider like any other, registered with `register_local`.
//!
//! The engines never name a provider: `shell::serve` asks [`Sources::terminal`], `gui` asks [`Sources::desktop`], and a
//! request reaches a source only after the engine has decided the peer's grant.
//!
//! Without feature `node` this module is only [`Native`], its settings and the desktop error helpers: the host's own
//! processes, files, terminal and screen, with none of the node's registry, link or peers.

#![forbid(unsafe_code)]

mod native;
mod native_files;
#[cfg(feature = "node")]
mod registry;

pub use bohio_x11::source::{DesktopOf, code_of, gui_error};
pub use native::{ID as NATIVE_ID, Native, NativeSettings};
#[cfg(feature = "node")]
pub use registry::*;
