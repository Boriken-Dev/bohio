#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod access;
pub mod config;
pub mod control;
pub mod exec;
pub mod forward;
pub mod fs;
pub mod gui;
pub mod info;
pub mod limit;
pub mod link;
pub mod listen;
pub mod local;
pub mod media;
pub mod node;
pub mod ownership;
pub mod pair;
pub mod peer;
pub mod policy;
pub mod power;
pub mod profile;
pub mod secret;
pub mod session;
pub mod shell;
pub mod stats;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;

mod platform;
mod stream;

pub use config::{Config, ConfigError, Environment};
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
