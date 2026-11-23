// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The contract suite over the in-memory sources, built the way a host builds a provider: registered with
//! `register_local`, found by kind, configured from a value checked against the provider's schema.

use bohio_api::contract::{self, ContractSettings};
use bohio_api::guatiao::library::{ProviderError, Registry};
use bohio_api::guatiao::{Map, ToValue};
use bohio_api::model::{SourceInfo, TerminalOpen};
use bohio_api::terminal::{Terminal, TerminalSource};
use bohio_api::testing::{
    self, FakeDesktopConfig, FakeMediaConfig, FakePowerConfig, FakeTerminalConfig,
};
use bohio_api::{DesktopSource, MediaSource, PowerSource, vocab};

fn registry() -> Registry {
    let mut registry = Registry::new("bohio-api-tests", "0");
    registry
        .register_local("bohio-api-testing", testing::library)
        .expect("the fake providers register");
    registry
}

fn built<K: ?Sized + bohio_api::guatiao::library::Kind>(
    registry: &Registry,
    id: &str,
    config: &impl ToValue,
) -> bohio_api::guatiao::library::Instance<K> {
    let offer = registry
        .offer::<K>(id)
        .expect("the provider is registered")
        .expect("its table matches the kind");
    offer
        .instantiate(&config.to_value().expect("a config is a value"))
        .expect("the configuration fits the provider's schema")
}

#[test]
fn a_console_terminal_passes_the_suite() {
    let registry = registry();
    let source = built::<dyn TerminalSource>(
        &registry,
        "fake_terminal",
        &FakeTerminalConfig {
            link: "contract-console".to_owned(),
            console: true,
        },
    );
    contract::terminal(
        &*source,
        TerminalOpen::default(),
        &ContractSettings::default(),
    );
}

#[test]
fn a_full_terminal_passes_the_suite() {
    let registry = registry();
    let source = built::<dyn TerminalSource>(
        &registry,
        "fake_terminal",
        &FakeTerminalConfig {
            link: "contract-full".to_owned(),
            console: false,
        },
    );
    contract::terminal(
        &*source,
        TerminalOpen::default(),
        &ContractSettings::default(),
    );
}

#[test]
fn an_unplugged_terminal_says_so_and_refuses_to_open() {
    let registry = registry();
    let source = built::<dyn TerminalSource>(
        &registry,
        "fake_terminal",
        &FakeTerminalConfig {
            link: "contract-unplugged".to_owned(),
            console: true,
        },
    );
    testing::terminal_link("contract-unplugged").set_available(false);
    let info = source.describe().expect("describe answers");
    assert!(!info.available);
    assert!(info.reason.is_some());
    contract::terminal(
        &*source,
        TerminalOpen::default(),
        &ContractSettings::default(),
    );
}

#[test]
fn a_desktop_passes_the_suite() {
    let registry = registry();
    let source = built::<dyn DesktopSource>(
        &registry,
        "fake_desktop",
        &FakeDesktopConfig {
            link: "contract-screen".to_owned(),
        },
    );
    contract::desktop(&*source, &ContractSettings::default());
}

#[test]
fn a_power_source_that_takes_some_actions_passes_the_suite() {
    let registry = registry();
    let all = built::<dyn PowerSource>(
        &registry,
        "fake_power",
        &FakePowerConfig {
            link: "contract-power".to_owned(),
            actions: Vec::new(),
        },
    );
    contract::power(&*all, &ContractSettings::default());
    let some = built::<dyn PowerSource>(
        &registry,
        "fake_power",
        &FakePowerConfig {
            link: "contract-power-some".to_owned(),
            actions: vec!["on".to_owned(), "off".to_owned()],
        },
    );
    let info = some.describe().expect("describe answers");
    assert!(info.lacks(vocab::power::NMI));
    contract::power(&*some, &ContractSettings::default());
}

#[test]
fn a_media_source_passes_the_suite() {
    let registry = registry();
    let source = built::<dyn MediaSource>(
        &registry,
        "fake_media",
        &FakeMediaConfig {
            link: "contract-media".to_owned(),
        },
    );
    contract::media(&*source, &ContractSettings::default());
}

#[test]
fn a_configuration_the_schema_refuses_is_never_built() {
    let registry = registry();
    let offer = registry
        .offer::<dyn TerminalSource>("fake_terminal")
        .expect("registered")
        .expect("matches");
    let mut config = Map::new();
    config.set("link", "x");
    config.set("no_such_key", true);
    assert!(
        offer.instantiate(&config.into()).is_err(),
        "a misspelled key is refused before the provider is asked"
    );
}

/// A console that says it lacks `resize` and takes it anyway: the suite must fail it.
struct Liar;

impl TerminalSource for Liar {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new(
            "liar",
            &[vocab::terminal::READ],
            &[vocab::terminal::RESIZE],
        ))
    }

    fn open(
        &self,
        _request: TerminalOpen,
    ) -> Result<bohio_api::guatiao::library::Object<dyn Terminal>, ProviderError> {
        struct Open;
        impl Terminal for Open {
            fn read(&self, _dst: &mut [u8], _wait_ms: u64) -> Result<i64, ProviderError> {
                Ok(0)
            }
            fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
                Ok(data.len() as u64)
            }
            fn kill(&self) -> Result<bohio_api::TerminalExit, ProviderError> {
                Ok(bohio_api::TerminalExit::default())
            }
            fn exit(&self) -> Result<Option<bohio_api::TerminalExit>, ProviderError> {
                Ok(None)
            }
            fn resize(&self, _cols: u16, _rows: u16) -> Result<(), ProviderError> {
                Ok(())
            }
        }
        Ok(Open.into_object())
    }
}

#[test]
#[should_panic(expected = "lacks `resize` and does not answer unsupported")]
fn a_source_whose_describe_disagrees_with_its_behaviour_fails_the_suite() {
    contract::terminal(&Liar, TerminalOpen::default(), &ContractSettings::default());
}
