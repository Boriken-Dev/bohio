// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The contract of the kinds: each kind's table hash is pinned and `bohio-api.kinds.json` is what this build
//! describes. A change that moves either breaks every provider already built, so it is made on purpose:
//!
//! ```text
//! GUATIAO_WRITE_HEADER=1 cargo test -p bohio-api --test kinds
//! ```
//!
//! rewrites the description, and the pinned hashes below are then updated by hand in the same commit.

use std::path::PathBuf;

use bohio_api::desktop::{DesktopSessionVtable, DesktopSourceVtable};
use bohio_api::guatiao::Alloc;
use bohio_api::media::{BlockReaderVtable, MediaSourceVtable};
use bohio_api::power::PowerSourceVtable;
use bohio_api::terminal::{TerminalSourceVtable, TerminalVtable};
use guatiao_serde::Presentation;
use guatiao_serde::text::json;

/// The floor hash of each kind: FNV-1a over every slot's signature.
const HASHES: [(&str, u32); 7] = [
    ("terminal", 4_168_119_332),
    ("terminal_session", 1_879_845_119),
    ("desktop", 1_612_473_616),
    ("desktop_session", 27_463_504),
    ("power", 3_362_560_333),
    ("media", 240_719_559),
    ("block_reader", 3_458_871_641),
];

fn actual() -> [(&'static str, u32); 7] {
    [
        ("terminal", TerminalSourceVtable::FLOOR_HASH),
        ("terminal_session", TerminalVtable::FLOOR_HASH),
        ("desktop", DesktopSourceVtable::FLOOR_HASH),
        ("desktop_session", DesktopSessionVtable::FLOOR_HASH),
        ("power", PowerSourceVtable::FLOOR_HASH),
        ("media", MediaSourceVtable::FLOOR_HASH),
        ("block_reader", BlockReaderVtable::FLOOR_HASH),
    ]
}

fn rendered() -> String {
    let kinds = [
        TerminalSourceVtable::description(),
        TerminalVtable::description(),
        DesktopSourceVtable::description(),
        DesktopSessionVtable::description(),
        PowerSourceVtable::description(),
        MediaSourceVtable::description(),
        BlockReaderVtable::description(),
    ];
    let document = format!("{{\"kinds\":[{}]}}", kinds.join(","));
    let value = json::from_str(&document, Alloc::rust(), Presentation::new())
        .expect("a kind's description is JSON");
    let mut text = json::to_string_pretty(&value, Presentation::new()).expect("written");
    text.push('\n');
    text
}

#[test]
fn the_committed_description_is_what_this_build_describes() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bohio-api.kinds.json");
    let fresh = rendered();
    if std::env::var_os("GUATIAO_WRITE_HEADER").is_some() {
        std::fs::write(&path, &fresh).expect("the description is written");
    }
    let committed = std::fs::read_to_string(&path).expect("the kind description is committed");
    assert!(
        committed.replace("\r\n", "\n") == fresh,
        "the committed kind description is not what this build describes. If the change is meant to break providers \
         already built, regenerate it:\n  GUATIAO_WRITE_HEADER=1 cargo test -p bohio-api --test kinds"
    );
}

#[test]
fn each_kinds_table_hash_is_the_pinned_one() {
    let pinned: Vec<_> = HASHES.to_vec();
    let found = actual();
    for ((name, want), (_, have)) in pinned.iter().zip(found.iter()) {
        assert_eq!(
            have, want,
            "the `{name}` kind's table hash changed (now {have}): a provider built before this fails to load. Pin the \
             new hash in the commit that means to break providers"
        );
    }
}

#[test]
fn a_description_carries_the_hash_a_provider_writes() {
    for (description, hash) in [
        (
            TerminalSourceVtable::description(),
            TerminalSourceVtable::FLOOR_HASH,
        ),
        (TerminalVtable::description(), TerminalVtable::FLOOR_HASH),
        (
            DesktopSessionVtable::description(),
            DesktopSessionVtable::FLOOR_HASH,
        ),
        (
            PowerSourceVtable::description(),
            PowerSourceVtable::FLOOR_HASH,
        ),
    ] {
        assert!(
            description.contains(&format!("\"floor_hash\":{hash},")),
            "{description}"
        );
    }
}
