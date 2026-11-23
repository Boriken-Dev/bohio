#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod apply;
pub mod compress;
pub mod delta;
pub mod hash;
pub mod rolling;
#[cfg(feature = "screen")]
pub mod screen;
pub mod signature;
