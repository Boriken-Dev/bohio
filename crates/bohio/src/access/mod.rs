//! How a node reaches the system it is for: sources, the providers behind them, and which source serves each capability.
//!
//! A **capability** (terminal, desktop, power, media) is a `guatiao` kind of `bohio-api`. A **source** is a configured
//! instance of a provider; `[source.<name>]` in `bohio.cfg` builds one, `[access]` lists the sources of each capability
//! (the first is the default), and an embedder adds its own with [`Sources::with_terminal`] and its siblings. `native`,
//! this node's own terminal and screen, is a provider like any other, registered with `register_local`.
//!
//! The engines never name a provider: `shell::serve` asks [`Sources::terminal`], `gui` asks [`Sources::desktop`], and a
//! request reaches a source only after the engine has decided the peer's grant.

#![forbid(unsafe_code)]

mod desktop;
mod native;
mod value;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use bohio_api::guatiao::library::{InstantiateError, KindMismatch, Registry};
use bohio_api::guatiao::{ToValue, Value};
use bohio_api::{DesktopSource, MediaSource, PowerSource, TerminalSource};
use bohio_proto::info::SourceEntry;
use bohio_proto::{ErrorBody, ErrorCode};

pub use desktop::{DesktopOf, KindDesktop, code_of, gui_error};
pub use native::{ID as NATIVE_ID, Native, NativeSettings};

use crate::config::{AccessConfig, NATIVE, SourceTable, SourceTables, Switch};

/// What a source gives a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Capability {
    /// A console.
    Terminal,
    /// A screen with input.
    Desktop,
    /// The system's power.
    Power,
    /// Virtual media.
    Media,
}

impl Capability {
    /// Every capability, in the order `info` lists them.
    pub const ALL: [Self; 4] = [Self::Terminal, Self::Desktop, Self::Power, Self::Media];

    /// The name `[access]` and `info` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Desktop => "desktop",
            Self::Power => "power",
            Self::Media => "media",
        }
    }
}

/// One configured source of a capability: the instance, and whether it is still the one the node was configured with.
pub struct Source<K: ?Sized> {
    name: Arc<str>,
    instance: Arc<K>,
    retired: Arc<AtomicBool>,
    embedded: bool,
}

impl<K: ?Sized> Clone for Source<K> {
    fn clone(&self) -> Self {
        Self {
            name: Arc::clone(&self.name),
            instance: Arc::clone(&self.instance),
            retired: Arc::clone(&self.retired),
            embedded: self.embedded,
        }
    }
}

impl<K: ?Sized> core::fmt::Debug for Source<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Source")
            .field("name", &self.name)
            .field("retired", &self.retired.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl<K: ?Sized> Source<K> {
    fn new(name: &str, instance: Arc<K>, embedded: bool) -> Self {
        Self {
            name: Arc::from(name),
            instance,
            retired: Arc::new(AtomicBool::new(false)),
            embedded,
        }
    }

    /// The name `[source.<name>]` gave it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The provider's instance.
    #[must_use]
    pub fn get(&self) -> &K {
        &self.instance
    }

    /// Whether this is still the instance the node serves: a reconfiguration that changed the source's table retires it,
    /// and what runs on a retired one ends `unavailable`.
    #[must_use]
    pub fn is_current(&self) -> bool {
        !self.retired.load(Ordering::Acquire)
    }
}

/// The sources of one capability, and the order `[access]` gave them.
struct Table<K: ?Sized> {
    by_name: BTreeMap<String, Source<K>>,
    order: Vec<String>,
}

impl<K: ?Sized> Table<K> {
    fn empty() -> Self {
        Self {
            by_name: BTreeMap::new(),
            order: Vec::new(),
        }
    }

    fn pick(&self, capability: Capability, name: Option<&str>) -> Result<Source<K>, ErrorBody> {
        let chosen = match name {
            Some(name) => name,
            None => self.order.first().map(String::as_str).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::Unsupported,
                    format!(
                        "this node serves no {} ([access] {})",
                        capability.name(),
                        capability.name()
                    ),
                )
            })?,
        };
        self.by_name.get(chosen).cloned().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("this node has no {} source of that name", capability.name()),
            )
        })
    }

    fn retire_all(&self) {
        for source in self.by_name.values() {
            source.retired.store(true, Ordering::Release);
        }
    }
}

struct State {
    terminal: Table<dyn TerminalSource>,
    desktop: Table<dyn DesktopSource>,
    power: Table<dyn PowerSource>,
    media: Table<dyn MediaSource>,
    fs: Switch,
    exec: Switch,
    /// The `[source.*]` tables these sources were built from, to tell which a reconfiguration changed.
    tables: SourceTables,
}

impl State {
    fn empty() -> Self {
        Self {
            terminal: Table::empty(),
            desktop: Table::empty(),
            power: Table::empty(),
            media: Table::empty(),
            fs: Switch::Native,
            exec: Switch::Native,
            tables: SourceTables::new(),
        }
    }
}

/// The sources of a node. One registry lives as long as anything it offered.
pub struct Sources {
    registry: Mutex<Registry>,
    state: RwLock<State>,
}

impl core::fmt::Debug for Sources {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sources").finish_non_exhaustive()
    }
}

/// A refusal of a configuration: the key, as written in the file, and what is wrong with it.
pub type Refusal = (String, String);

impl Default for Sources {
    /// Today's node: the `native` source for terminal and desktop, files and processes served.
    fn default() -> Self {
        Self::from_config(&SourceTables::new(), &AccessConfig::default())
            .unwrap_or_else(|_| Self::bare())
    }
}

impl Sources {
    /// No source at all, not even `native`.
    fn bare() -> Self {
        Self {
            registry: Mutex::new(builtin_registry()),
            state: RwLock::new(State::empty()),
        }
    }

    /// Sources with no source at all, not even `native`, and the built-in providers registered: add a provider with
    /// [`Sources::register_library`], then build from a configuration with [`Sources::reconfigure`].
    #[must_use]
    pub fn unconfigured() -> Self {
        Self::bare()
    }

    /// Registers a library the host links (`guatiao::local_providers!` writes its `library`) so `[source.<name>]` can name
    /// its providers, as a provider the node was built with.
    ///
    /// # Errors
    ///
    /// Why the registry refused it (an id already taken).
    pub fn register_library<'a, L: Into<bohio_api::guatiao::library::LocalLibrary<'a>>>(
        &self,
        name: &str,
        describe: impl FnOnce(bohio_api::guatiao::library::Host) -> Option<L>,
    ) -> Result<(), String> {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        registry
            .register_local(name, describe)
            .map(drop)
            .map_err(|error| error.to_string())
    }

    /// The sources `source` and `access` describe, with `native` always among them.
    ///
    /// # Errors
    ///
    /// The key and the reason: an unknown provider (the known ones are listed), a key the provider's schema does not
    /// declare or a value it does not accept (`source.<name>.<key>`), `native` declared with another provider, an
    /// `[access]` list naming a source that does not exist or does not serve that capability.
    pub fn from_config(source: &SourceTables, access: &AccessConfig) -> Result<Self, Refusal> {
        let this = Self::bare();
        this.reconfigure(source, access)?;
        Ok(this)
    }

    /// Builds the sources again from `source` and `access`: a source whose table did not change keeps its instance (and
    /// whatever runs on it), one whose table changed is built anew and the old instance is retired, so what runs on it
    /// ends `unavailable` ("the source was reconfigured"). A library already mapped stays mapped. Sources an embedder
    /// added are kept. Nothing changes when the configuration is refused.
    ///
    /// # Errors
    ///
    /// As [`Sources::from_config`].
    pub fn reconfigure(&self, source: &SourceTables, access: &AccessConfig) -> Result<(), Refusal> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let old = self.state.read().unwrap_or_else(PoisonError::into_inner);
        let mut fresh = State::empty();
        let mut tables = source.clone();
        tables.entry(NATIVE.to_owned()).or_default();
        for (name, table) in &tables {
            if old.tables.get(name) == Some(table) {
                // The same table: the same instances, and what runs on them goes on.
                keep(&old.terminal, &mut fresh.terminal, name);
                keep(&old.desktop, &mut fresh.desktop, name);
                keep(&old.power, &mut fresh.power, name);
                keep(&old.media, &mut fresh.media, name);
                continue;
            }
            build(&registry, name, table, &mut fresh)?;
        }
        // What an embedder added stays.
        for (name, held) in &old.terminal.by_name {
            if held.embedded {
                fresh.terminal.by_name.insert(name.clone(), held.clone());
            }
        }
        for (name, held) in &old.desktop.by_name {
            if held.embedded {
                fresh.desktop.by_name.insert(name.clone(), held.clone());
            }
        }
        for (name, held) in &old.power.by_name {
            if held.embedded {
                fresh.power.by_name.insert(name.clone(), held.clone());
            }
        }
        for (name, held) in &old.media.by_name {
            if held.embedded {
                fresh.media.by_name.insert(name.clone(), held.clone());
            }
        }
        fresh.terminal.order = order_of(
            Capability::Terminal,
            &access.terminals(),
            &fresh.terminal.by_name,
        )?;
        fresh.desktop.order = order_of(
            Capability::Desktop,
            &access.desktops(),
            &fresh.desktop.by_name,
        )?;
        fresh.power.order = order_of(Capability::Power, &access.power, &fresh.power.by_name)?;
        fresh.media.order = order_of(Capability::Media, &access.media, &fresh.media.by_name)?;
        fresh.fs = access.fs;
        fresh.exec = access.exec;
        fresh.tables = tables;
        drop(old);
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let previous = std::mem::replace(&mut *state, fresh);
        drop(state);
        // An instance that is not in the new state is retired; one that is, is not.
        retire_missing(&previous.terminal, &self.state_ref(), |s| &s.terminal);
        retire_missing(&previous.desktop, &self.state_ref(), |s| &s.desktop);
        retire_missing(&previous.power, &self.state_ref(), |s| &s.power);
        retire_missing(&previous.media, &self.state_ref(), |s| &s.media);
        Ok(())
    }

    fn state_ref(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a terminal source of the embedder's own, as `name`. It is not a default until [`Sources::with_access`] says
    /// so.
    #[must_use]
    pub fn with_terminal(self, name: &str, source: Arc<dyn TerminalSource>) -> Self {
        self.add_terminal(name, source);
        self
    }

    /// As [`Sources::with_terminal`], on sources already shared.
    pub fn add_terminal(&self, name: &str, source: Arc<dyn TerminalSource>) {
        self.state_mut()
            .terminal
            .by_name
            .insert(name.to_owned(), Source::new(name, source, true));
    }

    /// Adds a desktop source of the embedder's own.
    #[must_use]
    pub fn with_desktop(self, name: &str, source: Arc<dyn DesktopSource>) -> Self {
        self.add_desktop(name, source);
        self
    }

    /// As [`Sources::with_desktop`], on sources already shared.
    pub fn add_desktop(&self, name: &str, source: Arc<dyn DesktopSource>) {
        self.state_mut()
            .desktop
            .by_name
            .insert(name.to_owned(), Source::new(name, source, true));
    }

    /// Adds a power source of the embedder's own.
    #[must_use]
    pub fn with_power(self, name: &str, source: Arc<dyn PowerSource>) -> Self {
        self.state_mut()
            .power
            .by_name
            .insert(name.to_owned(), Source::new(name, source, true));
        self
    }

    /// Adds a media source of the embedder's own.
    #[must_use]
    pub fn with_media(self, name: &str, source: Arc<dyn MediaSource>) -> Self {
        self.state_mut()
            .media
            .by_name
            .insert(name.to_owned(), Source::new(name, source, true));
        self
    }

    /// Sets which sources serve `capability`, the first being the default. A name no source has is ignored here and refused
    /// by [`Sources::from_config`].
    #[must_use]
    pub fn with_access<'a>(
        self,
        capability: Capability,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let names: Vec<String> = names.into_iter().map(str::to_owned).collect();
        {
            let mut state = self.state_mut();
            match capability {
                Capability::Terminal => state.terminal.order = names,
                Capability::Desktop => state.desktop.order = names,
                Capability::Power => state.power.order = names,
                Capability::Media => state.media.order = names,
            }
        }
        self
    }

    /// Makes `name` the default of `capability`, keeping the others after it.
    pub(crate) fn make_default(&self, capability: Capability, name: &str) {
        let mut state = self.state_mut();
        let order = match capability {
            Capability::Terminal => &mut state.terminal.order,
            Capability::Desktop => &mut state.desktop.order,
            Capability::Power => &mut state.power.order,
            Capability::Media => &mut state.media.order,
        };
        order.retain(|held| held != name);
        order.insert(0, name.to_owned());
    }

    fn state_mut(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The terminal source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// `unsupported` when the node serves no terminal, `not_found` for a name it has no terminal source of.
    pub fn terminal(&self, name: Option<&str>) -> Result<Source<dyn TerminalSource>, ErrorBody> {
        self.state_ref().terminal.pick(Capability::Terminal, name)
    }

    /// The desktop source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn desktop(&self, name: Option<&str>) -> Result<Source<dyn DesktopSource>, ErrorBody> {
        self.state_ref().desktop.pick(Capability::Desktop, name)
    }

    /// The power source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn power(&self, name: Option<&str>) -> Result<Source<dyn PowerSource>, ErrorBody> {
        self.state_ref().power.pick(Capability::Power, name)
    }

    /// The media source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn media(&self, name: Option<&str>) -> Result<Source<dyn MediaSource>, ErrorBody> {
        self.state_ref().media.pick(Capability::Media, name)
    }

    /// The names of the sources serving `capability`, the default first.
    #[must_use]
    pub fn names(&self, capability: Capability) -> Vec<String> {
        let state = self.state_ref();
        match capability {
            Capability::Terminal => state.terminal.order.clone(),
            Capability::Desktop => state.desktop.order.clone(),
            Capability::Power => state.power.order.clone(),
            Capability::Media => state.media.order.clone(),
        }
    }

    /// Whether files are served: `[access] fs`.
    #[must_use]
    pub fn fs(&self) -> Switch {
        self.state_ref().fs
    }

    /// Whether processes are run: `[access] exec`.
    #[must_use]
    pub fn exec(&self) -> Switch {
        self.state_ref().exec
    }

    /// What `info` says of the sources `grant` reaches: each source's name, provider, whether it is the default, and its
    /// `describe`, asked now. Only a peer holding `info` is told, and a source that cannot describe itself is listed
    /// unavailable with that as the reason.
    #[must_use]
    pub fn info(&self, _grant: &crate::policy::Grant) -> bohio_proto::info::AccessInfo {
        let state = self.state_ref();
        fn entries<K: ?Sized>(
            table: &Table<K>,
            describe: impl Fn(
                &K,
            ) -> Result<
                bohio_api::SourceInfo,
                bohio_api::guatiao::library::ProviderError,
            >,
        ) -> Vec<SourceEntry> {
            table
                .order
                .iter()
                .enumerate()
                .filter_map(|(at, name)| table.by_name.get(name).map(|source| (at, name, source)))
                .map(|(at, name, source)| {
                    let info = describe(source.get()).unwrap_or_else(|error| {
                        bohio_api::SourceInfo::default().unavailable(error.message())
                    });
                    SourceEntry {
                        name: name.clone(),
                        provider: info.provider,
                        default: at == 0,
                        available: info.available,
                        reason: info.reason,
                        offers: info.offers,
                        lacks: info.lacks,
                    }
                })
                .collect()
        }
        bohio_proto::info::AccessInfo {
            terminal: entries(&state.terminal, |source| source.describe()),
            desktop: entries(&state.desktop, |source| source.describe()),
            power: entries(&state.power, |source| source.describe()),
            media: entries(&state.media, |source| source.describe()),
            fs: state.fs.name().to_owned(),
            exec: state.exec.name().to_owned(),
        }
    }

    /// Ends every instance: what runs on them ends `unavailable`. A node that stops calls it.
    pub fn retire(&self) {
        let state = self.state_ref();
        state.terminal.retire_all();
        state.desktop.retire_all();
        state.power.retire_all();
        state.media.retire_all();
    }
}

fn keep<K: ?Sized>(old: &Table<K>, fresh: &mut Table<K>, name: &str) {
    if let Some(held) = old.by_name.get(name) {
        fresh.by_name.insert(name.to_owned(), held.clone());
    }
}

/// Retires what `previous` held that the node does not serve any more.
fn retire_missing<K: ?Sized>(
    previous: &Table<K>,
    state: &State,
    table: impl Fn(&State) -> &Table<K>,
) {
    let now = table(state);
    for (name, held) in &previous.by_name {
        let same = now
            .by_name
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(&current.retired, &held.retired));
        if !same {
            held.retired.store(true, Ordering::Release);
        }
    }
}

fn order_of<K: ?Sized>(
    capability: Capability,
    names: &[String],
    held: &BTreeMap<String, Source<K>>,
) -> Result<Vec<String>, Refusal> {
    for name in names {
        if !held.contains_key(name) {
            return Err((
                format!("access.{}", capability.name()),
                format!(
                    "names `{name}`, which is no source serving {} (a `[source.{name}]` table whose provider offers it)",
                    capability.name()
                ),
            ));
        }
    }
    Ok(names.to_vec())
}

/// The registry every node starts with: its built-in providers, registered the way a loaded one is found.
fn builtin_registry() -> Registry {
    let mut registry = Registry::new("bohio", env!("CARGO_PKG_VERSION"));
    registry
        .register_local("bohio-native", native::library)
        .expect("the native provider registers");
    #[cfg(feature = "serial")]
    registry
        .register_local("bohio-serial", bohio_provider_serial::library)
        .expect("the serial provider registers");
    registry
}

/// Builds `[source.<name>]` into `into`, one instance for each kind the provider serves.
fn build(
    registry: &Registry,
    name: &str,
    table: &SourceTable,
    into: &mut State,
) -> Result<(), Refusal> {
    let key = |setting: &str| format!("source.{name}.{setting}");
    let provider = match (name == NATIVE, table.provider.as_deref()) {
        (true, None | Some(NATIVE)) => NATIVE,
        (true, Some(_)) => {
            return Err((
                key("provider"),
                "must be `native`: the native source is the native provider".to_owned(),
            ));
        }
        (false, Some(provider)) => provider,
        (false, None) => {
            return Err((
                format!("source.{name}"),
                "names no provider (`provider = \"<id>\"`)".to_owned(),
            ));
        }
    };
    let id = if provider == NATIVE {
        NATIVE_ID
    } else {
        provider
    };
    let settings: Value = value::from_table(&table.settings).into();
    let mut served = 0_usize;
    macro_rules! kind {
        ($trait:ty, $field:ident) => {
            // A provider with no table for a kind simply does not serve it; a table that does not fit is a refusal.
            let offer = match registry.offer::<$trait>(id) {
                None | Some(Err(KindMismatch::NoTable)) => None,
                Some(Ok(offer)) => Some(offer),
                Some(Err(error)) => {
                    return Err((
                        format!("source.{name}"),
                        format!("provider `{id}` does not match this build's kind: {error:?}"),
                    ));
                }
            };
            if let Some(offer) = offer {
                let instance = offer
                    .instantiate(&settings)
                    .map_err(|error| instantiate_refusal(name, &error))?;
                let instance: Arc<$trait> = Arc::new(instance);
                into.$field
                    .by_name
                    .insert(name.to_owned(), Source::new(name, instance, false));
                served += 1;
            }
        };
    }
    kind!(dyn TerminalSource, terminal);
    kind!(dyn DesktopSource, desktop);
    kind!(dyn PowerSource, power);
    kind!(dyn MediaSource, media);
    if served == 0 {
        let known: Vec<&str> = registry.all().iter().map(|known| known.id()).collect();
        return Err((
            key("provider"),
            format!(
                "names `{provider}`, which is no provider here (known: {})",
                known.join(", ")
            ),
        ));
    }
    Ok(())
}

fn instantiate_refusal(name: &str, error: &InstantiateError) -> Refusal {
    match error {
        InstantiateError::Invalid(invalid) => {
            use bohio_api::guatiao::schema::ValidationError;
            match invalid {
                ValidationError::UnknownOption { key, known } => (
                    format!("source.{name}.{key}"),
                    format!(
                        "is not a setting of this provider (it has: {})",
                        known.join(", ")
                    ),
                ),
                ValidationError::BadValue { key, expected } => (
                    format!("source.{name}.{key}"),
                    format!("must be {expected}"),
                ),
                other => (format!("source.{name}"), other.to_string()),
            }
        }
        InstantiateError::Refused(refused) => {
            (format!("source.{name}"), refused.message().to_string())
        }
        other => (format!("source.{name}"), format!("{other:?}")),
    }
}

/// Checks a configuration's `[source.*]` and `[access]` without keeping what it builds: the key, and what is wrong.
///
/// # Errors
///
/// As [`Sources::from_config`].
pub fn check(source: &SourceTables, access: &AccessConfig) -> Result<(), Refusal> {
    Sources::from_config(source, access).map(drop)
}

/// A value for a provider that is its own configuration, for tests and embedders that build an instance by hand.
///
/// # Errors
///
/// When the settings are not a value.
pub fn settings_value(settings: &impl ToValue) -> Result<Value, String> {
    settings.to_value().map_err(|error| error.to_string())
}
