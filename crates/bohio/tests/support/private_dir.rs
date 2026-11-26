//! A temporary directory only the running account may use, whatever the machine's temporary folder grants.
//!
//! The owner check refuses a configuration layer or a data directory that another account may change, and a machine can
//! grant a group rights on its temporary folder (another tool's sandbox does): everything made there inherits them. A test
//! that makes layers or a data directory makes them in a directory made here instead: mode `0700` on Unix, and on Windows
//! an access list of this account, SYSTEM and the administrators alone, with nothing inherited.

#![allow(dead_code)]

use std::path::Path;

/// Narrows `path` so only this account (and, on Windows, SYSTEM and the administrators) can change it or read it. No child
/// process is started: a child of a test would inherit the sockets other tests hold open.
pub fn narrow(path: &Path) {
    bohio::testing::make_private(path).expect("a private directory");
}

/// A new temporary directory that only this account may use.
pub fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    narrow(dir.path());
    dir
}
