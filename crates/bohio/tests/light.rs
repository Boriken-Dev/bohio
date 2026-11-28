// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The host's own sources without the node: `cargo test -p bohio --no-default-features --test light` builds none of the
//! link, peer, grant or engine code, and `Native` still serves processes, files and a terminal. Any build with `node`, which
//! the other tests need, leaves this file empty.

#![cfg(not(feature = "node"))]

use bohio::access::{Native, NativeSettings};
use bohio_api::contract::{self, ContractSettings, files};
use bohio_api::model::{ExecStart, ProgramSpec, TerminalOpen};

fn native() -> Native {
    Native::try_from(NativeSettings::default()).expect("the default settings are valid")
}

fn shell() -> &'static str {
    if cfg!(windows) { "cmd" } else { "sh" }
}

#[test]
fn the_native_exec_source_passes_the_contract_without_the_node() {
    let start = ExecStart {
        shell: Some(shell().to_owned()),
        script: Some("echo contract".to_owned()),
        stdin: true,
        path: std::env::var("PATH").ok(),
        ..ExecStart::default()
    };
    contract::exec(&native(), &start, &ContractSettings::default());
}

#[test]
fn the_native_files_source_passes_the_contract_without_the_node() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().to_str().expect("a text path").to_owned();
    files(&native(), &root, &ContractSettings::default());
}

#[test]
fn the_native_terminal_source_passes_the_contract_without_the_node() {
    let open = TerminalOpen {
        program: Some(ProgramSpec {
            kind: Some(shell().to_owned()),
            ..ProgramSpec::default()
        }),
        path: std::env::var("PATH").ok(),
        ..TerminalOpen::default()
    };
    contract::terminal(&native(), open, &ContractSettings::default());
}
