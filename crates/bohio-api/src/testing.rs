// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! In-memory sources for tests (feature `testing`): nothing here touches a device, a screen or a network.
//!
//! Each provider is built from a configuration naming a **link**: a shared in-memory endpoint a test reaches with
//! [`terminal_link`], [`desktop_link`], [`power_link`] and [`media_link`], so the test plays the far end (the
//! device a console is wired to, the screen, the BMC). [`library`] registers all four with
//! `Registry::register_local`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use guatiao::library::{Object, ProviderError};
use guatiao::{FromValue, Provider, Schema, Status, ToValue};

use crate::desktop::{DesktopSession, DesktopSource};
use crate::media::{BlockReader, MediaSource};
use crate::model::{
    Delivery, DisplayModel, FrameHeader, InputModel, MediaSlot, PowerState, RectModel, SourceInfo,
    StateModel, TerminalExit, TerminalOpen,
};
use crate::power::PowerSource;
use crate::terminal::{Terminal, TerminalSource};
use crate::unsupported;
use crate::vocab;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn registry<T>() -> &'static Mutex<BTreeMap<String, Arc<T>>>
where
    T: LinkKind,
{
    T::registry()
}

trait LinkKind: Sized + 'static {
    fn registry() -> &'static Mutex<BTreeMap<String, Arc<Self>>>;
    fn fresh() -> Self;
}

fn link<T: LinkKind>(name: &str) -> Arc<T> {
    Arc::clone(
        lock(registry::<T>())
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(T::fresh())),
    )
}

// ---------------------------------------------------------------------------------------------------------------
// Terminal

/// The far end of a fake terminal: what the device would write and read.
#[derive(Debug)]
pub struct TerminalLink {
    output: Mutex<VecDeque<u8>>,
    arrived: Condvar,
    typed: Mutex<Vec<u8>>,
    available: AtomicBool,
    ended: AtomicBool,
    opened: Mutex<u32>,
    killed: Mutex<u32>,
}

impl LinkKind for TerminalLink {
    fn registry() -> &'static Mutex<BTreeMap<String, Arc<Self>>> {
        static LINKS: Mutex<BTreeMap<String, Arc<TerminalLink>>> = Mutex::new(BTreeMap::new());
        &LINKS
    }

    fn fresh() -> Self {
        Self {
            output: Mutex::new(VecDeque::new()),
            arrived: Condvar::new(),
            typed: Mutex::new(Vec::new()),
            available: AtomicBool::new(true),
            ended: AtomicBool::new(false),
            opened: Mutex::new(0),
            killed: Mutex::new(0),
        }
    }
}

impl TerminalLink {
    /// The device writes `bytes`: the terminal's reader gets them.
    pub fn device_writes(&self, bytes: &[u8]) {
        lock(&self.output).extend(bytes.iter().copied());
        self.arrived.notify_all();
    }

    /// What was typed at the terminal so far, taken.
    pub fn typed(&self) -> Vec<u8> {
        std::mem::take(&mut *lock(&self.typed))
    }

    /// Waits up to `limit` for `typed` to hold `want`, answering what it held.
    pub fn typed_within(&self, want: &[u8], limit: Duration) -> Vec<u8> {
        let end = Instant::now() + limit;
        loop {
            let seen = lock(&self.typed).clone();
            if seen.windows(want.len().max(1)).any(|w| w == want) || Instant::now() >= end {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The device is gone (`false`) or back (`true`): `describe` says so at the next `info`.
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    /// The device closes its end: every open terminal reads the end.
    pub fn device_closes(&self) {
        self.ended.store(true, Ordering::Release);
        self.arrived.notify_all();
    }

    /// The device is open again for the next terminal.
    pub fn device_reopens(&self) {
        self.ended.store(false, Ordering::Release);
    }

    /// How many terminals were opened on it.
    pub fn opened(&self) -> u32 {
        *lock(&self.opened)
    }

    /// How many were killed (the host ended them).
    pub fn killed(&self) -> u32 {
        *lock(&self.killed)
    }
}

/// The in-memory link `name`, made on first use and shared by every source configured with it.
pub fn terminal_link(name: &str) -> Arc<TerminalLink> {
    link(name)
}

/// The settings of the fake terminal provider.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct FakeTerminalConfig {
    /// The in-memory link this source is wired to.
    pub link: String,
    /// A console: it starts no program, cannot resize and cannot be signalled. `false` is a terminal that does all.
    #[map(default = true)]
    pub console: bool,
}

/// A terminal source over a [`TerminalLink`].
#[derive(Debug, Clone, Default, Provider)]
#[provider(
    TerminalSource,
    id = "fake_terminal",
    name = "fake terminal",
    crate = ::guatiao,
    config = FakeTerminalConfig
)]
pub struct FakeTerminal {
    link: Option<Arc<TerminalLink>>,
    console: bool,
}

impl TryFrom<FakeTerminalConfig> for FakeTerminal {
    type Error = ProviderError;

    fn try_from(config: FakeTerminalConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            link: Some(terminal_link(&config.link)),
            console: config.console,
        })
    }
}

impl FakeTerminal {
    fn link(&self) -> Result<&Arc<TerminalLink>, ProviderError> {
        self.link
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no link"))
    }
}

impl TerminalSource for FakeTerminal {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let link = self.link()?;
        let info = if self.console {
            SourceInfo::new(
                "fake_terminal",
                &[vocab::terminal::READ, vocab::terminal::WRITE],
                &[
                    vocab::terminal::RESIZE,
                    vocab::terminal::SIGNALS,
                    vocab::terminal::EXIT_STATUS,
                    vocab::terminal::PROGRAMS,
                    vocab::terminal::BREAK,
                ],
            )
        } else {
            SourceInfo::new(
                "fake_terminal",
                &[
                    vocab::terminal::READ,
                    vocab::terminal::WRITE,
                    vocab::terminal::RESIZE,
                    vocab::terminal::SIGNALS,
                    vocab::terminal::PROGRAMS,
                ],
                &[vocab::terminal::BREAK],
            )
        };
        Ok(if link.available.load(Ordering::Acquire) {
            info
        } else {
            info.unavailable("the fake device is unplugged")
        })
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        let link = self.link()?;
        if !link.available.load(Ordering::Acquire) {
            return Err(crate::unavailable("the fake device is unplugged"));
        }
        if self.console && request.program.is_some() {
            return Err(ProviderError::new(
                Status::GUATIAO_ERR_BAD_VALUE,
                "this console starts no program",
            ));
        }
        *lock(&link.opened) += 1;
        Ok(FakeSession {
            link: Arc::clone(link),
            console: self.console,
            done: AtomicBool::new(false),
            size: Mutex::new((request.cols, request.rows)),
        }
        .into_object())
    }
}

struct FakeSession {
    link: Arc<TerminalLink>,
    console: bool,
    done: AtomicBool,
    size: Mutex<(u16, u16)>,
}

impl Terminal for FakeSession {
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        let end = Instant::now() + Duration::from_millis(wait_ms);
        let mut output = lock(&self.link.output);
        loop {
            if !output.is_empty() {
                let count = dst.len().min(output.len());
                for slot in dst.iter_mut().take(count) {
                    *slot = output.pop_front().unwrap_or(0);
                }
                return Ok(i64::try_from(count).unwrap_or(i64::MAX));
            }
            if self.done.load(Ordering::Acquire) || self.link.ended.load(Ordering::Acquire) {
                return Ok(-1);
            }
            let now = Instant::now();
            if now >= end {
                return Ok(0);
            }
            output = self
                .link
                .arrived
                .wait_timeout(output, end - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        if self.done.load(Ordering::Acquire) || self.link.ended.load(Ordering::Acquire) {
            return Err(crate::unavailable("the fake device closed"));
        }
        lock(&self.link.typed).extend_from_slice(data);
        Ok(data.len() as u64)
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        if !self.done.swap(true, Ordering::AcqRel) {
            *lock(&self.link.killed) += 1;
        }
        self.link.arrived.notify_all();
        Ok(TerminalExit::default())
    }

    fn exit(&self) -> Result<Option<TerminalExit>, ProviderError> {
        Ok(
            (!self.console && self.done.load(Ordering::Acquire)).then_some(TerminalExit {
                code: Some(0),
                signal: None,
            }),
        )
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), ProviderError> {
        if self.console {
            return Err(unsupported("a console has no size"));
        }
        *lock(&self.size) = (cols, rows);
        Ok(())
    }

    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        if self.console {
            return Err(unsupported("a console has no program to signal"));
        }
        Ok(Delivery {
            delivered: matches!(name, "interrupt" | "terminate" | "hangup" | "quit" | "kill"),
            ended: false,
            message: None,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Desktop

/// The screen of a fake desktop, which a test paints and reads the input of.
#[derive(Debug)]
pub struct DesktopLink {
    width: Mutex<u64>,
    height: Mutex<u64>,
    colour: Mutex<[u8; 3]>,
    input: Mutex<Vec<InputModel>>,
    clipboard: Mutex<String>,
    available: AtomicBool,
}

impl LinkKind for DesktopLink {
    fn registry() -> &'static Mutex<BTreeMap<String, Arc<Self>>> {
        static LINKS: Mutex<BTreeMap<String, Arc<DesktopLink>>> = Mutex::new(BTreeMap::new());
        &LINKS
    }

    fn fresh() -> Self {
        Self {
            width: Mutex::new(8),
            height: Mutex::new(4),
            colour: Mutex::new([10, 20, 30]),
            input: Mutex::new(Vec::new()),
            clipboard: Mutex::new(String::new()),
            available: AtomicBool::new(true),
        }
    }
}

impl DesktopLink {
    /// Fills the screen with one colour.
    pub fn paint(&self, rgb: [u8; 3]) {
        *lock(&self.colour) = rgb;
    }

    /// Resizes the screen.
    pub fn resize(&self, width: u64, height: u64) {
        *lock(&self.width) = width;
        *lock(&self.height) = height;
    }

    /// The input events taken so far.
    pub fn input(&self) -> Vec<InputModel> {
        lock(&self.input).clone()
    }

    /// The desktop is reachable (`true`) or not.
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }
}

/// The in-memory desktop `name`.
pub fn desktop_link(name: &str) -> Arc<DesktopLink> {
    link(name)
}

/// The settings of the fake desktop provider.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct FakeDesktopConfig {
    /// The in-memory desktop this source shows.
    pub link: String,
}

/// A desktop source over a [`DesktopLink`].
#[derive(Debug, Clone, Default, Provider)]
#[provider(
    DesktopSource,
    id = "fake_desktop",
    name = "fake desktop",
    crate = ::guatiao,
    config = FakeDesktopConfig
)]
pub struct FakeDesktop {
    link: Option<Arc<DesktopLink>>,
}

impl TryFrom<FakeDesktopConfig> for FakeDesktop {
    type Error = ProviderError;

    fn try_from(config: FakeDesktopConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            link: Some(desktop_link(&config.link)),
        })
    }
}

impl DesktopSource for FakeDesktop {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let link = self
            .link
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no link"))?;
        let info = SourceInfo::new(
            "fake_desktop",
            &[
                vocab::desktop::CAPTURE,
                vocab::desktop::INPUT,
                vocab::desktop::CLIPBOARD,
            ],
            &[
                vocab::desktop::LOCK_DETECTION,
                vocab::desktop::MULTI_DISPLAY,
            ],
        );
        Ok(if link.available.load(Ordering::Acquire) {
            info
        } else {
            info.unavailable("the fake screen is off")
        })
    }

    fn open(&self) -> Result<Object<dyn DesktopSession>, ProviderError> {
        let link = self
            .link
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no link"))?;
        if !link.available.load(Ordering::Acquire) {
            return Err(crate::unavailable("the fake screen is off"));
        }
        Ok(FakeScreen {
            link: Arc::clone(link),
        }
        .into_object())
    }
}

struct FakeScreen {
    link: Arc<DesktopLink>,
}

impl FakeScreen {
    fn check(&self, display: &str) -> Result<(), ProviderError> {
        if display == "fake-1" {
            Ok(())
        } else {
            Err(ProviderError::new(
                Status::GUATIAO_ERR_NOT_FOUND,
                "no such display",
            ))
        }
    }

    fn size_of(&self, region: Option<RectModel>) -> FrameHeader {
        region.map_or_else(
            || FrameHeader {
                width: *lock(&self.link.width),
                height: *lock(&self.link.height),
            },
            |rect| FrameHeader {
                width: rect.width,
                height: rect.height,
            },
        )
    }
}

impl DesktopSession for FakeScreen {
    fn state(&self) -> Result<StateModel, ProviderError> {
        Ok(StateModel {
            user: Some("fake".to_owned()),
            active: true,
            locked: None,
            displays: vec![DisplayModel {
                id: "fake-1".to_owned(),
                name: Some("Fake".to_owned()),
                primary: true,
                x: 0,
                y: 0,
                width: *lock(&self.link.width),
                height: *lock(&self.link.height),
                scale: 1.0,
                rotation: None,
            }],
        })
    }

    fn frame_size(
        &self,
        display: &str,
        region: Option<RectModel>,
    ) -> Result<FrameHeader, ProviderError> {
        self.check(display)?;
        Ok(self.size_of(region))
    }

    fn capture(
        &self,
        display: &str,
        region: Option<RectModel>,
        dst: &mut [u8],
    ) -> Result<FrameHeader, ProviderError> {
        self.check(display)?;
        let header = self.size_of(region);
        let wanted = usize::try_from(header.len()).unwrap_or(usize::MAX);
        let rgb = *lock(&self.link.colour);
        let target = dst.get_mut(..wanted).ok_or_else(|| {
            ProviderError::new(Status::GUATIAO_ERR_BAD_VALUE, "the buffer is too small")
        })?;
        for pixel in target.as_chunks_mut::<3>().0 {
            *pixel = rgb;
        }
        Ok(header)
    }

    fn input(&self, events: Vec<InputModel>) -> Result<u64, ProviderError> {
        let count = events.len() as u64;
        lock(&self.link.input).extend(events);
        Ok(count)
    }

    fn clipboard_text(&self) -> Result<String, ProviderError> {
        Ok(lock(&self.link.clipboard).clone())
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), ProviderError> {
        *lock(&self.link.clipboard) = text.to_owned();
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Power

/// The system a fake power source controls, which a test reads and can make slow.
#[derive(Debug)]
pub struct PowerLink {
    state: Mutex<String>,
    log: Mutex<Vec<String>>,
    delay: Mutex<Duration>,
    available: AtomicBool,
}

impl LinkKind for PowerLink {
    fn registry() -> &'static Mutex<BTreeMap<String, Arc<Self>>> {
        static LINKS: Mutex<BTreeMap<String, Arc<PowerLink>>> = Mutex::new(BTreeMap::new());
        &LINKS
    }

    fn fresh() -> Self {
        Self {
            state: Mutex::new(vocab::power_state::OFF.to_owned()),
            log: Mutex::new(Vec::new()),
            delay: Mutex::new(Duration::ZERO),
            available: AtomicBool::new(true),
        }
    }
}

impl PowerLink {
    /// The actions taken so far, in order.
    pub fn actions(&self) -> Vec<String> {
        lock(&self.log).clone()
    }

    /// Every action takes this long before it answers.
    pub fn set_delay(&self, delay: Duration) {
        *lock(&self.delay) = delay;
    }

    /// The system is reachable (`true`) or not.
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    /// The system's power state now.
    pub fn state(&self) -> String {
        lock(&self.state).clone()
    }
}

/// The in-memory system `name`.
pub fn power_link(name: &str) -> Arc<PowerLink> {
    link(name)
}

/// The settings of the fake power provider.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct FakePowerConfig {
    /// The in-memory system this source controls.
    pub link: String,
    /// The actions the source takes (every one when empty).
    #[map(default)]
    pub actions: Vec<String>,
}

/// A power source over a [`PowerLink`].
#[derive(Debug, Clone, Default, Provider)]
#[provider(
    PowerSource,
    id = "fake_power",
    name = "fake power",
    crate = ::guatiao,
    config = FakePowerConfig
)]
pub struct FakePower {
    link: Option<Arc<PowerLink>>,
    actions: Vec<String>,
}

impl TryFrom<FakePowerConfig> for FakePower {
    type Error = ProviderError;

    fn try_from(config: FakePowerConfig) -> Result<Self, Self::Error> {
        let actions = if config.actions.is_empty() {
            vocab::power::ACTIONS
                .iter()
                .map(|word| (*word).to_owned())
                .collect()
        } else {
            config.actions
        };
        Ok(Self {
            link: Some(power_link(&config.link)),
            actions,
        })
    }
}

impl FakePower {
    fn link(&self) -> Result<&Arc<PowerLink>, ProviderError> {
        self.link
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no link"))
    }
}

impl PowerSource for FakePower {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let link = self.link()?;
        let lacks: Vec<&str> = vocab::power::ACTIONS
            .iter()
            .copied()
            .filter(|action| !self.actions.iter().any(|have| have == action))
            .collect();
        let mut offers: Vec<&str> = vec![vocab::power::STATUS];
        offers.extend(self.actions.iter().map(String::as_str));
        let info = SourceInfo::new("fake_power", &offers, &lacks);
        Ok(if link.available.load(Ordering::Acquire) {
            info
        } else {
            info.unavailable("the fake system cannot be reached")
        })
    }

    fn state(&self) -> Result<PowerState, ProviderError> {
        let link = self.link()?;
        if !link.available.load(Ordering::Acquire) {
            return Err(crate::unavailable("the fake system cannot be reached"));
        }
        Ok(PowerState {
            state: lock(&link.state).clone(),
            done: true,
        })
    }

    fn act(&self, action: &str) -> Result<PowerState, ProviderError> {
        let link = self.link()?;
        if !vocab::power::ACTIONS.contains(&action) {
            return Err(ProviderError::new(
                Status::GUATIAO_ERR_BAD_VALUE,
                "not a power action",
            ));
        }
        if !self.actions.iter().any(|have| have == action) {
            return Err(unsupported("this source does not take that action"));
        }
        if !link.available.load(Ordering::Acquire) {
            return Err(crate::unavailable("the fake system cannot be reached"));
        }
        let delay = *lock(&link.delay);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        lock(&link.log).push(action.to_owned());
        let mut state = lock(&link.state);
        match action {
            "on" | "cycle" | "reset" => vocab::power_state::ON.clone_into(&mut state),
            "off" | "soft_off" => vocab::power_state::OFF.clone_into(&mut state),
            _ => {}
        }
        Ok(PowerState {
            state: state.clone(),
            done: true,
        })
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Media

/// The virtual-media slots of a fake system, and what the fake read of an inserted file.
#[derive(Debug)]
pub struct MediaLink {
    slots: Mutex<BTreeMap<String, MediaSlot>>,
    read: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl LinkKind for MediaLink {
    fn registry() -> &'static Mutex<BTreeMap<String, Arc<Self>>> {
        static LINKS: Mutex<BTreeMap<String, Arc<MediaLink>>> = Mutex::new(BTreeMap::new());
        &LINKS
    }

    fn fresh() -> Self {
        let slots = ["cd", "floppy"]
            .into_iter()
            .map(|slot| {
                (
                    slot.to_owned(),
                    MediaSlot {
                        slot: slot.to_owned(),
                        ..MediaSlot::default()
                    },
                )
            })
            .collect();
        Self {
            slots: Mutex::new(slots),
            read: Mutex::new(BTreeMap::new()),
        }
    }
}

impl MediaLink {
    /// Everything the fake read of the image inserted through `insert_reader` in `slot`.
    pub fn read_of(&self, slot: &str) -> Option<Vec<u8>> {
        lock(&self.read).get(slot).cloned()
    }
}

/// The in-memory media of system `name`.
pub fn media_link(name: &str) -> Arc<MediaLink> {
    link(name)
}

/// The settings of the fake media provider.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct FakeMediaConfig {
    /// The in-memory system whose media this source serves.
    pub link: String,
}

/// A media source over a [`MediaLink`]; `insert_reader` reads the whole image at once, as a BMC that copies it would.
#[derive(Debug, Clone, Default, Provider)]
#[provider(
    MediaSource,
    id = "fake_media",
    name = "fake media",
    crate = ::guatiao,
    config = FakeMediaConfig
)]
pub struct FakeMedia {
    link: Option<Arc<MediaLink>>,
}

impl TryFrom<FakeMediaConfig> for FakeMedia {
    type Error = ProviderError;

    fn try_from(config: FakeMediaConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            link: Some(media_link(&config.link)),
        })
    }
}

impl FakeMedia {
    fn link(&self) -> Result<&Arc<MediaLink>, ProviderError> {
        self.link
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no link"))
    }

    fn slot(
        &self,
        name: &str,
        change: impl FnOnce(&mut MediaSlot),
    ) -> Result<MediaSlot, ProviderError> {
        let link = self.link()?;
        let mut slots = lock(&link.slots);
        let slot = slots
            .get_mut(name)
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NOT_FOUND, "no such slot"))?;
        change(slot);
        Ok(slot.clone())
    }
}

impl MediaSource for FakeMedia {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        self.link()?;
        Ok(SourceInfo::new(
            "fake_media",
            &[
                vocab::media::STATUS,
                vocab::media::INSERT_URL,
                vocab::media::INSERT_READER,
                vocab::media::EJECT,
            ],
            &[],
        ))
    }

    fn status(&self) -> Result<Vec<MediaSlot>, ProviderError> {
        Ok(lock(&self.link()?.slots).values().cloned().collect())
    }

    fn insert_url(&self, slot: &str, url: &str) -> Result<MediaSlot, ProviderError> {
        let shown = url.split(['?', '#']).next().unwrap_or("").to_owned();
        self.slot(slot, |slot| {
            slot.inserted = true;
            slot.image = Some(shown);
        })
    }

    fn insert_reader(
        &self,
        slot: &str,
        name: &str,
        reader: Object<dyn BlockReader>,
    ) -> Result<MediaSlot, ProviderError> {
        let size = BlockReader::size(&*reader)?;
        let mut image = vec![0_u8; usize::try_from(size).unwrap_or(0)];
        let mut at = 0_usize;
        while at < image.len() {
            let want = (image.len() - at).min(64 * 1024);
            let got = reader.read_at(at as u64, &mut image[at..at + want])?;
            if got == 0 {
                break;
            }
            at += usize::try_from(got).unwrap_or(0);
        }
        image.truncate(at);
        let link = self.link()?;
        let described = self.slot(slot, |slot| {
            slot.inserted = true;
            slot.image = Some(name.to_owned());
            slot.size = Some(size);
        })?;
        lock(&link.read).insert(slot.to_owned(), image);
        Ok(described)
    }

    fn eject(&self, slot: &str) -> Result<MediaSlot, ProviderError> {
        self.slot(slot, |slot| {
            slot.inserted = false;
            slot.image = None;
            slot.size = None;
        })
    }
}

guatiao::local_providers!(
    id = "bohio-api-testing",
    version = env!("CARGO_PKG_VERSION"),
    providers = [FakeTerminal, FakeDesktop, FakePower, FakeMedia]
);
