// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `native` exec source against the exec contract suite, and what the node says of it.

use bohio::access::{Capability, Sources};
use bohio::policy::{Grant, Permission};
use bohio_api::contract::{self, ContractSettings};
use bohio_api::model::ExecStart;
use bohio_proto::ErrorCode;

fn printing() -> ExecStart {
    ExecStart {
        shell: Some(if cfg!(windows) { "cmd" } else { "sh" }.to_owned()),
        script: Some("echo contract".to_owned()),
        stdin: true,
        path: std::env::var("PATH").ok(),
        ..ExecStart::default()
    }
}

#[test]
fn the_native_exec_source_passes_the_contract() {
    let sources = Sources::default();
    let source = sources.exec(None).expect("the node serves exec");
    assert_eq!(source.name(), "native");
    contract::exec(source.get(), &printing(), &ContractSettings::default());
}

#[test]
fn info_lists_the_native_exec_source_with_what_it_offers() {
    let sources = Sources::default();
    let grant = Grant::allowing([Permission::Info, Permission::Exec]);
    let listed = sources.info(&grant_with_defaults(&sources, grant)).exec;
    assert_eq!(listed.len(), 1, "{listed:?}");
    let entry = &listed[0];
    assert_eq!(entry.name, "native");
    assert!(entry.default && entry.available);
    for word in [
        "start",
        "stdin",
        "stderr",
        "kill_tree",
        "listed",
        "exit_status",
    ] {
        assert!(entry.offers.iter().any(|offered| offered == word), "{word}");
    }
    assert_eq!(sources.names(Capability::Exec), ["native"]);
}

#[test]
fn a_name_the_node_has_no_exec_source_of_is_not_found() {
    let sources = Sources::default();
    let error = sources.exec(Some("elsewhere")).expect_err("no such source");
    assert_eq!(error.code, ErrorCode::NotFound);
}

fn grant_with_defaults(sources: &Sources, grant: Grant) -> Grant {
    let names = sources.names(Capability::Terminal);
    let _ = names;
    grant.with_default_sources(
        [Capability::Terminal, Capability::Exec]
            .into_iter()
            .flat_map(|capability| sources.names(capability).into_iter().take(1)),
    )
}
