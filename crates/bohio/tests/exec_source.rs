// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `native` exec source against the exec contract suite, and what the node says of it.

use bohio::access::{Capability, Sources};
use bohio::policy::{Grant, Permission};
use bohio_api::contract::{self, ContractSettings};
use bohio_api::model::{EnvVar, ExecStart};
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

/// The standard output of a program the native source starts, read to its end.
fn output_of(start: ExecStart) -> String {
    let sources = Sources::default();
    let source = sources.exec(None).expect("the node serves exec");
    let process = source.get().start(start).expect("the program starts");
    let (mut out, mut chunk) = (Vec::new(), [0_u8; 1024]);
    loop {
        match process.read(bohio_api::exec::STDOUT, &mut chunk, 5000) {
            Ok(-1) | Err(_) => break,
            Ok(0) => {}
            Ok(count) => out.extend_from_slice(&chunk[..usize::try_from(count).unwrap_or(0)]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn a_variable_removed_and_then_set_has_the_value_it_was_set_to() {
    let name = "BOHIO_ENV_ORDER";
    let change = |value: Option<&str>| EnvVar {
        name: name.to_owned(),
        value: value.map(str::to_owned),
    };
    let echo = ExecStart {
        script: Some(if cfg!(windows) {
            format!("echo [%{name}%]")
        } else {
            format!("echo \"[${{{name}-unset}}]\"")
        }),
        ..printing()
    };
    let removed_then_set = ExecStart {
        env: vec![change(None), change(Some("2"))],
        ..echo.clone()
    };
    assert!(output_of(removed_then_set).contains("[2]"));
    let set_then_removed = ExecStart {
        env: vec![change(Some("2")), change(None)],
        ..echo
    };
    let shown = output_of(set_then_removed);
    assert!(!shown.contains("[2]"), "{shown}");
}
