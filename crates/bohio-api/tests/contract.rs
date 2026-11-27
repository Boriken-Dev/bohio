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
use bohio_api::{DesktopSession, DesktopSource, MediaSource, PowerSource, vocab};

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

/// A terminal whose `describe` and behaviour the test chooses.
struct Scripted {
    offers: &'static [&'static str],
    lacks: &'static [&'static str],
    /// Bytes a write takes (`None`: all of them); a refusal when `writes` is false.
    writes: bool,
    takes_all: bool,
    signals: bool,
}

impl Scripted {
    const FULL: Self = Self {
        offers: &[vocab::terminal::READ, vocab::terminal::WRITE],
        lacks: &[],
        writes: true,
        takes_all: true,
        signals: true,
    };
}

impl TerminalSource for Scripted {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new("scripted", self.offers, self.lacks))
    }

    fn open(
        &self,
        _request: TerminalOpen,
    ) -> Result<bohio_api::guatiao::library::Object<dyn Terminal>, ProviderError> {
        struct Open {
            writes: bool,
            takes_all: bool,
            signals: bool,
        }
        impl Terminal for Open {
            fn read(&self, _dst: &mut [u8], _wait_ms: u64) -> Result<i64, ProviderError> {
                Ok(0)
            }
            fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
                if !self.writes {
                    return Err(bohio_api::unsupported("no write"));
                }
                let len = data.len() as u64;
                Ok(if self.takes_all { len } else { len / 2 })
            }
            fn kill(&self) -> Result<bohio_api::TerminalExit, ProviderError> {
                Ok(bohio_api::TerminalExit::default())
            }
            fn exit(&self) -> Result<Option<bohio_api::TerminalExit>, ProviderError> {
                Ok(None)
            }
            fn signal(&self, _name: &str) -> Result<bohio_api::model::Delivery, ProviderError> {
                if self.signals {
                    Ok(bohio_api::model::Delivery {
                        delivered: true,
                        ..bohio_api::model::Delivery::default()
                    })
                } else {
                    Err(bohio_api::unsupported("no signal"))
                }
            }
        }
        Ok(Open {
            writes: self.writes,
            takes_all: self.takes_all,
            signals: self.signals,
        }
        .into_object())
    }
}

fn terminal_suite(source: &Scripted) {
    contract::terminal(
        source,
        TerminalOpen::default(),
        &ContractSettings::default(),
    );
}

#[test]
fn a_terminal_that_does_what_it_offers_passes() {
    terminal_suite(&Scripted {
        offers: &[
            vocab::terminal::READ,
            vocab::terminal::WRITE,
            vocab::terminal::SIGNALS,
        ],
        lacks: &[vocab::terminal::RESIZE],
        ..Scripted::FULL
    });
}

#[test]
#[should_panic(expected = "offers `signals` and fails it")]
fn a_terminal_that_offers_signals_and_fails_them_fails_the_suite() {
    terminal_suite(&Scripted {
        offers: &[
            vocab::terminal::READ,
            vocab::terminal::WRITE,
            vocab::terminal::SIGNALS,
        ],
        lacks: &[],
        signals: false,
        ..Scripted::FULL
    });
}

#[test]
#[should_panic(expected = "lacks `signals` and does not answer unsupported")]
fn a_terminal_that_lacks_signals_and_takes_them_fails_the_suite() {
    terminal_suite(&Scripted {
        lacks: &[vocab::terminal::SIGNALS],
        ..Scripted::FULL
    });
}

#[test]
#[should_panic(expected = "a write of")]
fn a_terminal_that_takes_part_of_a_write_fails_the_suite() {
    terminal_suite(&Scripted {
        takes_all: false,
        ..Scripted::FULL
    });
}

#[test]
#[should_panic(expected = "lacks `write` and does not answer unsupported")]
fn a_terminal_that_lacks_write_and_takes_it_fails_the_suite() {
    terminal_suite(&Scripted {
        offers: &[vocab::terminal::READ],
        lacks: &[vocab::terminal::WRITE],
        ..Scripted::FULL
    });
}

#[test]
#[should_panic(expected = "lacks `programs` and opened one")]
fn a_terminal_that_lacks_programs_and_opens_one_fails_the_suite() {
    terminal_suite(&Scripted {
        lacks: &[vocab::terminal::PROGRAMS],
        ..Scripted::FULL
    });
}

#[test]
#[should_panic(expected = "never arrived")]
fn a_terminal_that_prints_nothing_fails_a_suite_that_expects_output() {
    let settings = ContractSettings {
        terminal_output: Some(b"expected"),
        terminal_output_wait: std::time::Duration::from_millis(300),
        ..ContractSettings::default()
    };
    contract::terminal(&Scripted::FULL, TerminalOpen::default(), &settings);
}

#[test]
fn a_terminal_whose_device_prints_passes_a_suite_that_expects_output() {
    let registry = registry();
    let source = built::<dyn TerminalSource>(
        &registry,
        "fake_terminal",
        &FakeTerminalConfig {
            link: "contract-output".to_owned(),
            console: true,
        },
    );
    testing::terminal_link("contract-output").device_writes(b"login: ");
    let settings = ContractSettings {
        terminal_output: Some(b"login: "),
        ..ContractSettings::default()
    };
    contract::terminal(&*source, TerminalOpen::default(), &settings);
    let typed = testing::terminal_link("contract-output").typed();
    assert_eq!(typed, settings.terminal_input, "the suite typed its bytes");
}

/// A desktop whose `describe` and behaviour the test chooses; one display, `d`.
struct Screen {
    offers: &'static [&'static str],
    lacks: &'static [&'static str],
    /// The status a display it does not have gets.
    missing: bohio_api::guatiao::Status,
    /// What the clipboard does: remembers (`true`) or forgets what was set.
    remembers: bool,
    /// Whether a clipboard and input are implemented at all.
    implemented: bool,
}

impl Screen {
    const FULL: Self = Self {
        offers: &[vocab::desktop::INPUT, vocab::desktop::CLIPBOARD],
        lacks: &[],
        missing: bohio_api::guatiao::Status::GUATIAO_ERR_NOT_FOUND,
        remembers: true,
        implemented: true,
    };
}

impl DesktopSource for Screen {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new("screen", self.offers, self.lacks))
    }

    fn open(
        &self,
    ) -> Result<bohio_api::guatiao::library::Object<dyn bohio_api::DesktopSession>, ProviderError>
    {
        use bohio_api::model::{FrameHeader, InputModel, RectModel, StateModel};
        struct Open {
            missing: bohio_api::guatiao::Status,
            remembers: bool,
            implemented: bool,
            text: std::sync::Mutex<String>,
        }
        impl DesktopSession for Open {
            fn state(&self) -> Result<StateModel, ProviderError> {
                Ok(StateModel::default())
            }
            fn frame_size(
                &self,
                _display: &str,
                _region: Option<RectModel>,
            ) -> Result<FrameHeader, ProviderError> {
                Err(ProviderError::new(self.missing, "no such display"))
            }
            fn capture(
                &self,
                _display: &str,
                _region: Option<RectModel>,
                _dst: &mut [u8],
            ) -> Result<FrameHeader, ProviderError> {
                Err(ProviderError::new(self.missing, "no such display"))
            }
            fn input(&self, events: Vec<InputModel>) -> Result<u64, ProviderError> {
                if self.implemented {
                    Ok(events.len() as u64)
                } else {
                    Err(bohio_api::unsupported("no input"))
                }
            }
            fn clipboard_text(&self) -> Result<String, ProviderError> {
                if self.implemented {
                    Ok(self.text.lock().expect("lock").clone())
                } else {
                    Err(bohio_api::unsupported("no clipboard"))
                }
            }
            fn set_clipboard_text(&self, text: &str) -> Result<(), ProviderError> {
                if !self.implemented {
                    return Err(bohio_api::unsupported("no clipboard"));
                }
                if self.remembers {
                    *self.text.lock().expect("lock") = text.to_owned();
                }
                Ok(())
            }
        }
        Ok(Open {
            missing: self.missing,
            remembers: self.remembers,
            implemented: self.implemented,
            text: std::sync::Mutex::new(String::new()),
        }
        .into_object())
    }
}

#[test]
fn a_desktop_that_does_what_it_offers_passes() {
    contract::desktop(&Screen::FULL, &ContractSettings::default());
    contract::desktop(
        &Screen {
            offers: &[],
            lacks: &[vocab::desktop::INPUT, vocab::desktop::CLIPBOARD],
            implemented: false,
            ..Screen::FULL
        },
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "offers `input` and fails it")]
fn a_desktop_that_offers_input_and_refuses_it_fails_the_suite() {
    contract::desktop(
        &Screen {
            offers: &[vocab::desktop::INPUT],
            implemented: false,
            ..Screen::FULL
        },
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "offers `clipboard` and fails to set it")]
fn a_desktop_that_offers_a_clipboard_and_refuses_it_fails_the_suite() {
    contract::desktop(
        &Screen {
            offers: &[vocab::desktop::CLIPBOARD],
            implemented: false,
            ..Screen::FULL
        },
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "did not round-trip")]
fn a_desktop_whose_clipboard_forgets_fails_the_suite() {
    contract::desktop(
        &Screen {
            remembers: false,
            ..Screen::FULL
        },
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "lacks `input` and does not answer unsupported")]
fn a_desktop_that_lacks_input_and_takes_it_fails_the_suite() {
    contract::desktop(
        &Screen {
            offers: &[],
            lacks: &[vocab::desktop::INPUT],
            ..Screen::FULL
        },
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "lacks `clipboard` and sets it")]
fn a_desktop_that_lacks_a_clipboard_and_sets_it_fails_the_suite() {
    struct NoRead(Screen);
    impl DesktopSource for NoRead {
        fn describe(&self) -> Result<SourceInfo, ProviderError> {
            self.0.describe()
        }
        fn open(
            &self,
        ) -> Result<bohio_api::guatiao::library::Object<dyn bohio_api::DesktopSession>, ProviderError>
        {
            // Reads are refused, writes are taken: a source that half-has the word.
            struct Half(bohio_api::guatiao::library::Object<dyn bohio_api::DesktopSession>);
            impl bohio_api::DesktopSession for Half {
                fn state(&self) -> Result<bohio_api::model::StateModel, ProviderError> {
                    self.0.state()
                }
                fn frame_size(
                    &self,
                    display: &str,
                    region: Option<bohio_api::model::RectModel>,
                ) -> Result<bohio_api::model::FrameHeader, ProviderError> {
                    self.0.frame_size(display, region)
                }
                fn capture(
                    &self,
                    display: &str,
                    region: Option<bohio_api::model::RectModel>,
                    dst: &mut [u8],
                ) -> Result<bohio_api::model::FrameHeader, ProviderError> {
                    self.0.capture(display, region, dst)
                }
                fn clipboard_text(&self) -> Result<String, ProviderError> {
                    Err(bohio_api::unsupported("no clipboard"))
                }
                fn set_clipboard_text(&self, _text: &str) -> Result<(), ProviderError> {
                    Ok(())
                }
            }
            Ok(Half(self.0.open()?).into_object())
        }
    }
    contract::desktop(
        &NoRead(Screen {
            offers: &[],
            lacks: &[vocab::desktop::CLIPBOARD],
            ..Screen::FULL
        }),
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "is not not_found")]
fn a_desktop_that_answers_a_missing_display_with_another_status_fails_the_suite() {
    contract::desktop(
        &Screen {
            missing: bohio_api::guatiao::Status::GUATIAO_ERR_BAD_VALUE,
            ..Screen::FULL
        },
        &ContractSettings::default(),
    );
}

/// A power source that offers `on` and never takes it.
struct StuckPower;

impl PowerSource for StuckPower {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new(
            "stuck",
            &[vocab::power::STATUS, vocab::power::ON],
            &[
                vocab::power::OFF,
                vocab::power::SOFT_OFF,
                vocab::power::CYCLE,
                vocab::power::RESET,
                vocab::power::NMI,
            ],
        ))
    }

    fn state(&self) -> Result<bohio_api::model::PowerState, ProviderError> {
        Ok(bohio_api::model::PowerState {
            state: vocab::power_state::OFF.to_owned(),
            done: true,
        })
    }

    fn act(&self, action: &str) -> Result<bohio_api::model::PowerState, ProviderError> {
        if action == vocab::power::ON {
            Err(ProviderError::new(
                bohio_api::guatiao::Status::GUATIAO_ERR_GONE,
                "the BMC is deaf",
            ))
        } else {
            Err(bohio_api::unsupported("no such action"))
        }
    }
}

#[test]
fn the_suite_takes_no_power_action_unless_it_is_told_to() {
    contract::power(&StuckPower, &ContractSettings::default());
}

#[test]
#[should_panic(expected = "offers `on` and fails it")]
fn a_power_source_that_offers_an_action_and_fails_it_fails_a_suite_that_may_take_it() {
    let settings = ContractSettings {
        power_actions: &[vocab::power::ON],
        ..ContractSettings::default()
    };
    contract::power(&StuckPower, &settings);
}

#[test]
fn a_power_source_that_takes_every_action_passes_a_suite_that_may_take_them() {
    let registry = registry();
    let source = built::<dyn PowerSource>(
        &registry,
        "fake_power",
        &FakePowerConfig {
            link: "contract-power-taken".to_owned(),
            actions: Vec::new(),
        },
    );
    let settings = ContractSettings {
        power_actions: &vocab::power::ACTIONS,
        ..ContractSettings::default()
    };
    contract::power(&*source, &settings);
    assert_eq!(
        testing::power_link("contract-power-taken").actions(),
        vocab::power::ACTIONS,
        "the suite took each action it was allowed"
    );
    let untouched = built::<dyn PowerSource>(
        &registry,
        "fake_power",
        &FakePowerConfig {
            link: "contract-power-untouched".to_owned(),
            actions: Vec::new(),
        },
    );
    contract::power(&*untouched, &ContractSettings::default());
    assert!(
        testing::power_link("contract-power-untouched")
            .actions()
            .is_empty(),
        "the default settings take no action"
    );
}

/// A media source whose slots and ejection the test chooses.
struct Slots {
    slots: Vec<bohio_api::model::MediaSlot>,
    eject_missing: bohio_api::guatiao::Status,
}

impl MediaSource for Slots {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new(
            "slots",
            &[vocab::media::STATUS, vocab::media::EJECT],
            &[vocab::media::INSERT_URL, vocab::media::INSERT_READER],
        ))
    }

    fn status(&self) -> Result<Vec<bohio_api::model::MediaSlot>, ProviderError> {
        Ok(self.slots.clone())
    }

    fn eject(&self, slot: &str) -> Result<bohio_api::model::MediaSlot, ProviderError> {
        self.slots
            .iter()
            .find(|have| have.slot == slot)
            .cloned()
            .ok_or_else(|| ProviderError::new(self.eject_missing, "no such slot"))
    }
}

fn slot(name: &str) -> bohio_api::model::MediaSlot {
    bohio_api::model::MediaSlot {
        slot: name.to_owned(),
        ..bohio_api::model::MediaSlot::default()
    }
}

fn slots(slots: Vec<bohio_api::model::MediaSlot>) -> Slots {
    Slots {
        slots,
        eject_missing: bohio_api::guatiao::Status::GUATIAO_ERR_NOT_FOUND,
    }
}

#[test]
fn media_slots_that_agree_with_themselves_pass() {
    contract::media(
        &slots(vec![slot("cd"), slot("floppy")]),
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "twice")]
fn media_that_names_a_slot_twice_fails_the_suite() {
    contract::media(
        &slots(vec![slot("cd"), slot("cd")]),
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "names an image or a size")]
fn media_whose_empty_slot_names_an_image_fails_the_suite() {
    let mut named = slot("cd");
    named.image = Some("ghost.iso".to_owned());
    contract::media(&slots(vec![named]), &ContractSettings::default());
}

#[test]
#[should_panic(expected = "ejected a slot it does not have")]
fn media_that_ejects_a_slot_it_does_not_have_with_another_status_fails_the_suite() {
    contract::media(
        &Slots {
            eject_missing: bohio_api::guatiao::Status::GUATIAO_ERR_BAD_VALUE,
            ..slots(vec![slot("cd")])
        },
        &ContractSettings::default(),
    );
}

#[test]
#[should_panic(expected = "offers `insert_url`")]
fn media_that_offers_insert_url_and_refuses_it_fails_the_suite() {
    struct Offers(Slots);
    impl MediaSource for Offers {
        fn describe(&self) -> Result<SourceInfo, ProviderError> {
            Ok(SourceInfo::new(
                "offers",
                &[vocab::media::STATUS, vocab::media::INSERT_URL],
                &[],
            ))
        }
        fn status(&self) -> Result<Vec<bohio_api::model::MediaSlot>, ProviderError> {
            self.0.status()
        }
        fn eject(&self, slot: &str) -> Result<bohio_api::model::MediaSlot, ProviderError> {
            self.0.eject(slot)
        }
    }
    contract::media(
        &Offers(slots(vec![slot("cd")])),
        &ContractSettings::default(),
    );
}
