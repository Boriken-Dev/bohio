#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod config;
pub mod control;
pub mod exec;
pub mod fs;
pub mod info;
pub mod limit;
pub mod policy;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;

mod platform;
mod stream;

pub use config::{Config, ConfigError, Environment};
pub use policy::{Grant, Permission, Policies, PolicyError};
