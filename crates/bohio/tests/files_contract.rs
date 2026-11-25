// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The `native` files source passes the files kind's contract suite and reports what it does.

use bohio::access::Sources;
use bohio_api::contract::{ContractSettings, files};

#[test]
fn the_native_files_source_passes_the_contract_suite() {
    let sources = Sources::default();
    let source = sources.files(None).expect("the node has a files source");
    let dir = tempfile::tempdir().expect("a scratch directory");
    let root = dir.path().to_str().expect("a text path").to_owned();
    files(source.get(), &root, &ContractSettings::default());
}

#[test]
fn the_native_files_source_says_it_is_confined_and_where_it_stops() {
    let sources = Sources::default();
    let source = sources.files(None).expect("the node has a files source");
    let info = source.get().describe().expect("it describes itself");
    assert!(info.available);
    assert!(info.offers.iter().any(|word| word == "confined"));
    assert!(info.offers.iter().any(|word| word == "atomic_replace"));
    assert!(info.lacks("statvfs"), "no statvfs is claimed");
}
