#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
// Without `node`, what only the node uses of the shared platform and settings modules is unused.
#![cfg_attr(not(feature = "node"), allow(dead_code, unused_imports))]

pub mod access;
#[cfg(feature = "node")]
pub mod config;
#[cfg(feature = "node")]
pub mod control;
#[cfg(feature = "node")]
pub mod exec;
#[cfg(feature = "node")]
pub mod forward;
#[cfg(feature = "node")]
pub mod fs;
#[cfg(feature = "node")]
pub mod gui;
#[cfg(feature = "node")]
pub mod info;
#[cfg(feature = "node")]
pub mod limit;
#[cfg(feature = "node")]
pub mod link;
#[cfg(feature = "node")]
pub mod listen;
#[cfg(feature = "node")]
pub mod local;
#[cfg(feature = "node")]
pub mod mcp;
#[cfg(feature = "node")]
pub mod media;
#[cfg(feature = "node")]
pub mod node;
#[cfg(feature = "node")]
pub mod ownership;
#[cfg(feature = "node")]
pub mod pair;
#[cfg(feature = "node")]
pub mod peer;
#[cfg(feature = "node")]
pub mod policy;
#[cfg(feature = "node")]
pub mod power;
#[cfg(feature = "node")]
pub mod profile;
#[cfg(feature = "node")]
pub mod secret;
#[cfg(feature = "node")]
pub mod session;
#[cfg(feature = "node")]
pub mod shell;
#[cfg(feature = "node")]
pub mod stats;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;

mod hostfs;
mod knobs;
mod platform;
#[cfg(feature = "node")]
mod stream;

#[cfg(feature = "node")]
pub use config::{Config, ConfigError, Environment};
#[cfg(feature = "node")]
pub use policy::{Grant, Peers, Permission};

/// Support for the tests of this crate and of embedding programs; not a stable interface.
#[doc(hidden)]
pub mod testing {
    use std::path::Path;

    /// Narrows `path` so only the running account (and, on Windows, SYSTEM and the administrators) may use it: mode
    /// `0700` on Unix; on Windows a protected access list set through the security API, with no child process started.
    ///
    /// # Errors
    ///
    /// The operating system's error.
    pub fn make_private(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        }
        #[cfg(windows)]
        {
            crate::platform::winacl::make_private(path)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Ok(())
        }
    }
}

/// A temporary directory only this account may use, for the unit tests that make layers and data directories.
#[cfg(test)]
extern crate self as bohio;

#[cfg(test)]
#[path = "../tests/support/private_dir.rs"]
mod private_dir;
