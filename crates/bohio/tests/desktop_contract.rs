// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The native desktop path (a `Desktop` offered as a `DesktopSource`) passes the desktop kind's contract suite over a
//! painted desktop.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;

use std::sync::Arc;

use bohio::access::DesktopOf;
use bohio_api::contract::{ContractSettings, desktop};
use painted::Painted;

#[test]
fn the_painted_desktop_passes_the_contract_suite() {
    let painted = Arc::new(Painted::new());
    let source = DesktopOf::new(Arc::clone(&painted) as Arc<dyn bohio::gui::Desktop>);
    desktop(&source, &ContractSettings::default());
    // What the suite did to the desktop it did through the kind: a pointer move arrived and the clipboard was put
    // back as it was.
    assert!(
        !painted.log().is_empty(),
        "the suite's pointer move reached the desktop"
    );
    assert_eq!(
        painted.clipboard(),
        "",
        "the suite left the clipboard as it found it"
    );
}
