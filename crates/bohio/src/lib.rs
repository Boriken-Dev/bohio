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
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
mod ownership;
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

/// A temporary directory only this account may use, for the unit tests that make layers and data directories.
#[cfg(test)]
#[path = "../tests/support/private_dir.rs"]
mod private_dir;
