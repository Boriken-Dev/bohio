#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod config;
pub mod info;
pub mod policy;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;

mod platform;

pub use config::{Config, ConfigError, Environment};
pub use policy::{Grant, Permission, Policies, PolicyError};
