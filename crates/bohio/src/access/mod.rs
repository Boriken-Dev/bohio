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
mod native_files;
mod plugins;
mod programs;
mod value;

pub use plugins::{Library, LibraryState, Report as ProvidersReport};
use programs::Programs;
pub use programs::{Program, ProgramState};

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use bohio_api::guatiao::library::{InstantiateError, KindMismatch, Registry};
use bohio_api::guatiao::{ToValue, Value};
use bohio_api::{DesktopSource, ExecSource, FilesSource, MediaSource, PowerSource, TerminalSource};
use bohio_proto::info::SourceEntry;
use bohio_proto::{ErrorBody, ErrorCode};

pub use bohio_x11::source::{DesktopOf, code_of, gui_error};
pub use desktop::KindDesktop;
pub use native::{ID as NATIVE_ID, Native, NativeSettings};

use crate::config::{AccessConfig, NATIVE, SourceTable, SourceTables};
use crate::secret::{self, NoSecrets, Secret, SecretResolver};

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
    /// Processes.
    Exec,
    /// Files.
    Files,
}

impl Capability {
    /// Every capability, in the order `info` lists them.
    pub const ALL: [Self; 6] = [
        Self::Terminal,
        Self::Desktop,
        Self::Power,
        Self::Media,
        Self::Exec,
        Self::Files,
    ];

    /// The name `[access]` and `info` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Desktop => "desktop",
            Self::Power => "power",
            Self::Media => "media",
            Self::Exec => "exec",
            Self::Files => "fs",
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

    /// The instance, shared: what a call that may outlive its stream holds.
    #[must_use]
    pub fn arc(&self) -> Arc<K> {
        Arc::clone(&self.instance)
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
    exec: Table<dyn ExecSource>,
    files: Table<dyn FilesSource>,
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
            exec: Table::empty(),
            files: Table::empty(),
            tables: SourceTables::new(),
        }
    }
}

/// The sources of a node. One registry lives as long as anything it offered.
pub struct Sources {
    registry: Mutex<Registry>,
    state: RwLock<State>,
    /// Reads the references a sensitive setting holds, and the most bytes one may be (`[secrets] max_bytes`).
    secrets: Mutex<(Arc<dyn SecretResolver>, usize)>,
    /// Sources whose settings hold a secret reference are left out until a resolver is given.
    deferring: AtomicBool,
    /// `[secrets] allow_literal`: a sensitive setting may hold the secret itself.
    literal: AtomicBool,
    /// The configuration the sources were last built from, to build them again when a resolver arrives.
    last: Mutex<Option<(SourceTables, AccessConfig)>>,
    /// What loading `[providers] paths` did.
    libraries: Mutex<ProvidersReport>,
    /// The provider programs `[[providers.program]]` named.
    programs: Mutex<Arc<Programs>>,
}

/// What a sensitive setting's reference is turned into when a source is built.
enum Resolve<'a> {
    /// The reference's shape is checked and nothing is read: a configuration is validated before anything is opened.
    Shape(bool),
    /// A source with a secret reference is left out: the node has no resolver yet.
    Defer(bool),
    /// The reference is read.
    With(&'a dyn SecretResolver, usize, bool),
}

impl Resolve<'_> {
    /// Whether a sensitive setting may hold the secret itself (`[secrets] allow_literal`).
    const fn literal(&self) -> bool {
        match self {
            Self::Shape(literal) | Self::Defer(literal) | Self::With(_, _, literal) => *literal,
        }
    }
}

/// The default of `[secrets] max_bytes`.
pub const DEFAULT_SECRET_BYTES: usize = 65_536;

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
            secrets: Mutex::new((Arc::new(NoSecrets), DEFAULT_SECRET_BYTES)),
            deferring: AtomicBool::new(false),
            literal: AtomicBool::new(false),
            last: Mutex::new(None),
            libraries: Mutex::new(ProvidersReport::default()),
            programs: Mutex::new(Arc::default()),
        }
    }

    /// Leaves out the sources whose settings hold a secret reference until [`Sources::set_secrets`] gives a resolver:
    /// what a node does when it is built, before its owner has said how references are read.
    pub fn defer_secrets(&self) {
        self.deferring.store(true, Ordering::Release);
    }

    /// Reads the references of sensitive settings with `resolver`, at most `max_bytes` each, and builds the sources that
    /// were left out for want of one.
    ///
    /// # Errors
    ///
    /// As [`Sources::from_config`]: a reference the resolver refuses is the key's refusal (`source.<name>.<key>`), never
    /// quoting the secret.
    pub fn set_secrets(
        &self,
        resolver: Arc<dyn SecretResolver>,
        max_bytes: usize,
    ) -> Result<(), Refusal> {
        *self.secrets.lock().unwrap_or_else(PoisonError::into_inner) = (resolver, max_bytes);
        self.deferring.store(false, Ordering::Release);
        let last = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match last {
            Some((source, access)) => self.reconfigure(&source, &access),
            None => Ok(()),
        }
    }

    /// Lets a sensitive setting hold the secret itself (`[secrets] allow_literal`); it is refused otherwise.
    pub fn set_allow_literal(&self, allow: bool) {
        self.literal.store(allow, Ordering::Release);
    }

    /// The most bytes of one secret, `[secrets] max_bytes`.
    pub fn set_secret_limit(&self, max_bytes: usize) {
        self.secrets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1 = max_bytes;
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

    /// Maps the provider libraries `config` names into the registry, so `[source.<name>]` can name their providers, and
    /// remembers what became of each ([`Sources::libraries`]). Nothing loads when `paths` is empty. One library failing does
    /// not stop the others, and a provider id a built-in already holds stays the built-in's.
    ///
    /// A library runs as this process's account once mapped, and stays mapped for as long as the registry lives.
    pub fn load_libraries(&self, config: &crate::config::ProvidersConfig) -> ProvidersReport {
        let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let (report, hosted) = plugins::load(&mut registry, config);
        let programs = Programs::start(&mut registry, config, &hosted);
        drop(registry);
        *self
            .libraries
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = report;
        *self.programs.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(programs);
        self.libraries()
    }

    /// What loading `[providers] paths` did, as the node started.
    #[must_use]
    pub fn libraries(&self) -> ProvidersReport {
        let mut report = self
            .libraries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let programs = Arc::clone(&self.programs.lock().unwrap_or_else(PoisonError::into_inner));
        report.programs = programs.report();
        // A library run in a process of its own is reported as the library it is, with how its host fares now.
        for (index, program) in programs.hosted_report() {
            if let Some(library) = report.libraries.get_mut(index) {
                library.state = match program.state {
                    ProgramState::Running => LibraryState::Loaded,
                    ProgramState::Failed => LibraryState::Failed,
                };
                library.detail = format!("in a process of its own: {}", program.detail);
            }
        }
        report
    }

    /// Starts again the provider programs that exited and may be, and builds the sources that name what they offer anew,
    /// since the instances of the old run answer `gone`. A lookup of a source asks for it first.
    fn revive(&self) {
        let programs = Arc::clone(&self.programs.lock().unwrap_or_else(PoisonError::into_inner));
        if programs.is_empty() || !programs.wants_restart() {
            return;
        }
        let Some(_one_at_a_time) = programs.begin_restart() else {
            return;
        };
        let offered = {
            let mut registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
            programs.restart(&mut registry)
        };
        if offered.is_empty() {
            return;
        }
        let last = self
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some((source, access)) = last else {
            return;
        };
        let names: Vec<String> = source
            .iter()
            .filter(|(_, table)| {
                table
                    .provider
                    .as_deref()
                    .is_some_and(|provider| offered.iter().any(|id| id == provider))
            })
            .map(|(name, _)| name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        let (resolver, max) = {
            let held = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
            (Arc::clone(&held.0), held.1)
        };
        let literal = self.literal.load(Ordering::Acquire);
        let mode = if self.deferring.load(Ordering::Acquire) {
            Resolve::Defer(literal)
        } else {
            Resolve::With(resolver.as_ref(), max, literal)
        };
        if let Err((key, message)) = self.apply(&source, &access, &mode, &names) {
            tracing::warn!(
                "a restarted provider program's sources could not be built again: {key} {message}"
            );
        }
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
        let (resolver, max) = {
            let held = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
            (Arc::clone(&held.0), held.1)
        };
        let literal = self.literal.load(Ordering::Acquire);
        let mode = if self.deferring.load(Ordering::Acquire) {
            Resolve::Defer(literal)
        } else {
            Resolve::With(resolver.as_ref(), max, literal)
        };
        self.apply(source, access, &mode, &[])?;
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((source.clone(), access.clone()));
        Ok(())
    }

    /// Builds the sources, reading references as `mode` says.
    fn apply(
        &self,
        source: &SourceTables,
        access: &AccessConfig,
        mode: &Resolve<'_>,
        rebuild: &[String],
    ) -> Result<(), Refusal> {
        let registry = self.registry.lock().unwrap_or_else(PoisonError::into_inner);
        let old = self.state.read().unwrap_or_else(PoisonError::into_inner);
        let mut fresh = State::empty();
        let mut waiting: Vec<String> = Vec::new();
        let mut tables = source.clone();
        tables.entry(NATIVE.to_owned()).or_default();
        for (name, table) in &tables {
            if old.tables.get(name) == Some(table) && !rebuild.contains(name) {
                // The same table: the same instances, and what runs on them goes on.
                keep(&old.terminal, &mut fresh.terminal, name);
                keep(&old.desktop, &mut fresh.desktop, name);
                keep(&old.power, &mut fresh.power, name);
                keep(&old.media, &mut fresh.media, name);
                keep(&old.exec, &mut fresh.exec, name);
                keep(&old.files, &mut fresh.files, name);
                continue;
            }
            if !build(&registry, name, table, &mut fresh, mode)? {
                waiting.push(name.clone());
            }
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
        for (name, held) in &old.exec.by_name {
            if held.embedded {
                fresh.exec.by_name.insert(name.clone(), held.clone());
            }
        }
        for (name, held) in &old.files.by_name {
            if held.embedded {
                fresh.files.by_name.insert(name.clone(), held.clone());
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
        fresh.exec.order = order_of(Capability::Exec, &access.execs(), &fresh.exec.by_name)?;
        fresh.files.order = order_of(
            Capability::Files,
            &access.filesystems(),
            &fresh.files.by_name,
        )?;
        // A source left out for want of a secret resolver is built, not kept, when one arrives.
        for name in &waiting {
            tables.remove(name);
        }
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
        retire_missing(&previous.exec, &self.state_ref(), |s| &s.exec);
        retire_missing(&previous.files, &self.state_ref(), |s| &s.files);
        Ok(())
    }

    fn state_ref(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes over what `other` holds: its sources, their order and the way it reads secrets. The handle stays the same, so
    /// what already holds it (the node's grants, its desktop state) sees the new sources.
    pub(crate) fn absorb(&self, other: Sources) {
        let mut theirs = other.state.write().unwrap_or_else(PoisonError::into_inner);
        let taken = std::mem::replace(&mut *theirs, State::empty());
        drop(theirs);
        let mut ours = self.state_mut();
        let previous = std::mem::replace(&mut *ours, taken);
        drop(ours);
        let now = self.state_ref();
        retire_missing(&previous.terminal, &now, |s| &s.terminal);
        retire_missing(&previous.desktop, &now, |s| &s.desktop);
        retire_missing(&previous.power, &now, |s| &s.power);
        retire_missing(&previous.media, &now, |s| &s.media);
        retire_missing(&previous.exec, &now, |s| &s.exec);
        retire_missing(&previous.files, &now, |s| &s.files);
        drop(now);
        let registry = std::mem::replace(
            &mut *other
                .registry
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            builtin_registry(),
        );
        *self.registry.lock().unwrap_or_else(PoisonError::into_inner) = registry;
        let libraries = std::mem::take(
            &mut *other
                .libraries
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        *self
            .libraries
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = libraries;
        let last = other
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = last;
    }

    /// Whether `grant` reaches the source `name`: each capability's default, or one the grant's `sources` names.
    #[must_use]
    pub fn permits(&self, grant: &crate::policy::Grant, name: &str) -> bool {
        grant.permits_source(name) || self.default_names().iter().any(|default| default == name)
    }

    /// The names of each capability's default source, which a peer reaches without being named.
    pub(crate) fn default_names(&self) -> Vec<String> {
        let state = self.state_ref();
        [
            state.terminal.order.first(),
            state.desktop.order.first(),
            state.power.order.first(),
            state.media.order.first(),
            state.exec.order.first(),
            state.files.order.first(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
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

    /// Adds an exec source of the embedder's own.
    #[must_use]
    pub fn with_exec(self, name: &str, source: Arc<dyn ExecSource>) -> Self {
        self.state_mut()
            .exec
            .by_name
            .insert(name.to_owned(), Source::new(name, source, true));
        self
    }

    /// Adds a files source of the embedder's own.
    #[must_use]
    pub fn with_files(self, name: &str, source: Arc<dyn FilesSource>) -> Self {
        self.state_mut()
            .files
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
                Capability::Exec => state.exec.order = names,
                Capability::Files => state.files.order = names,
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
            Capability::Exec => &mut state.exec.order,
            Capability::Files => &mut state.files.order,
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
        self.revive();
        self.state_ref().terminal.pick(Capability::Terminal, name)
    }

    /// The desktop source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn desktop(&self, name: Option<&str>) -> Result<Source<dyn DesktopSource>, ErrorBody> {
        self.revive();
        self.state_ref().desktop.pick(Capability::Desktop, name)
    }

    /// The power source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn power(&self, name: Option<&str>) -> Result<Source<dyn PowerSource>, ErrorBody> {
        self.revive();
        self.state_ref().power.pick(Capability::Power, name)
    }

    /// The media source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn media(&self, name: Option<&str>) -> Result<Source<dyn MediaSource>, ErrorBody> {
        self.revive();
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
            Capability::Exec => state.exec.order.clone(),
            Capability::Files => state.files.order.clone(),
        }
    }

    /// The exec source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn exec(&self, name: Option<&str>) -> Result<Source<dyn ExecSource>, ErrorBody> {
        self.revive();
        self.state_ref().exec.pick(Capability::Exec, name)
    }

    /// The files source `name`, or the default one when `None`.
    ///
    /// # Errors
    ///
    /// As [`Sources::terminal`].
    pub fn files(&self, name: Option<&str>) -> Result<Source<dyn FilesSource>, ErrorBody> {
        self.revive();
        self.state_ref().files.pick(Capability::Files, name)
    }

    /// What `info` says of the sources `grant` reaches: each source's name, provider, whether it is the default, and its
    /// `describe`, asked now. Only a peer holding `info` is told, only of a capability whose permission it also holds
    /// (`shell`, `gui.view` or `gui.input`, `power`, `media`, `exec`, `fs.read` or `fs.write`), and a source that cannot
    /// describe itself is listed unavailable with that as the reason. The sources' lock is not held while they describe.
    #[must_use]
    pub fn info(&self, grant: &crate::policy::Grant) -> bohio_proto::info::AccessInfo {
        use crate::policy::Permission;
        type Picked<K> = Vec<(usize, String, Source<K>)>;
        fn picked<K: ?Sized>(
            table: &Table<K>,
            grant: &crate::policy::Grant,
            held: bool,
        ) -> Picked<K> {
            if !held {
                return Vec::new();
            }
            table
                .order
                .iter()
                .enumerate()
                .filter(|(_, name)| grant.permits_source(name))
                .filter_map(|(at, name)| {
                    table
                        .by_name
                        .get(name)
                        .map(|source| (at, name.clone(), source.clone()))
                })
                .collect()
        }
        fn entries<K: ?Sized>(
            picked: Picked<K>,
            describe: impl Fn(
                &K,
            ) -> Result<
                bohio_api::SourceInfo,
                bohio_api::guatiao::library::ProviderError,
            >,
        ) -> Vec<SourceEntry> {
            picked
                .into_iter()
                .map(|(at, name, source)| {
                    let info = describe(source.get()).unwrap_or_else(|error| {
                        bohio_api::SourceInfo::default().unavailable(error.message())
                    });
                    SourceEntry {
                        name,
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
        let holds = |permission| grant.allows(permission);
        let (terminal, desktop, power, media, exec, fs) = {
            let state = self.state_ref();
            (
                picked(&state.terminal, grant, holds(Permission::Shell)),
                picked(
                    &state.desktop,
                    grant,
                    holds(Permission::GuiView) || holds(Permission::GuiInput),
                ),
                picked(&state.power, grant, holds(Permission::Power)),
                picked(&state.media, grant, holds(Permission::Media)),
                picked(&state.exec, grant, holds(Permission::Exec)),
                picked(
                    &state.files,
                    grant,
                    holds(Permission::FsRead) || holds(Permission::FsWrite),
                ),
            )
        };
        bohio_proto::info::AccessInfo {
            terminal: entries(terminal, |source| source.describe()),
            desktop: entries(desktop, |source| source.describe()),
            power: entries(power, |source| source.describe()),
            media: entries(media, |source| source.describe()),
            exec: entries(exec, |source| source.describe()),
            fs: entries(fs, |source| source.describe()),
        }
    }

    /// Ends every instance: what runs on them ends `unavailable`. A node that stops calls it.
    pub fn retire(&self) {
        let state = self.state_ref();
        state.terminal.retire_all();
        state.desktop.retire_all();
        state.power.retire_all();
        state.media.retire_all();
        state.exec.retire_all();
        state.files.retire_all();
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
    #[cfg(feature = "ssh")]
    registry
        .register_local("bohio-ssh", bohio_provider_ssh::library)
        .expect("the ssh provider registers");
    registry
}

/// Builds `[source.<name>]` into `into`, one instance for each kind the provider serves.
fn build(
    registry: &Registry,
    name: &str,
    table: &SourceTable,
    into: &mut State,
    mode: &Resolve<'_>,
) -> Result<bool, Refusal> {
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
                let instance: Arc<$trait> = if offer.builds_instances() {
                    let Some(settings) = settings_of(name, &offer, table, mode)? else {
                        // A secret this node cannot read yet: the source waits for its resolver.
                        tracing::info!(source = name, "a source waits for a secret resolver");
                        return Ok(false);
                    };
                    let instance = offer
                        .instantiate(&settings)
                        .map_err(|error| instantiate_refusal(name, &error))?;
                    Arc::new(instance)
                } else {
                    // A provider a program serves (or a library with no `create`) is its one instance: it is configured where
                    // it runs, so a setting named here has nowhere to go.
                    if let Some(setting) = table.settings.keys().next() {
                        return Err((
                            key(setting),
                            "is not a setting of this provider: it is one instance and takes none here; configure it where it runs".to_owned(),
                        ));
                    }
                    offer.shared().ok_or_else(|| {
                        (
                            format!("source.{name}"),
                            format!("provider `{id}` cannot be used as it stands"),
                        )
                    })?
                };
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
    kind!(dyn ExecSource, exec);
    kind!(dyn FilesSource, files);
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
    Ok(true)
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

/// The settings of `table` as the provider is built from them: a sensitive one must be a reference, read as `mode` says.
/// `None` when a reference cannot be read yet.
fn settings_of<K: ?Sized + bohio_api::guatiao::library::Kind>(
    name: &str,
    offer: &bohio_api::guatiao::library::Offer<K>,
    table: &SourceTable,
    mode: &Resolve<'_>,
) -> Result<Option<Value>, Refusal> {
    let mut map = value::from_table(&table.settings);
    let sensitive: Vec<String> = offer
        .config_schema()
        .and_then(bohio_api::guatiao::schema::SchemaRef::new)
        .map(|schema| {
            schema
                .fields()
                .filter(bohio_api::guatiao::schema::FieldRef::is_sensitive)
                .map(|field| field.key().to_owned())
                .collect()
        })
        .unwrap_or_default();
    for key in sensitive {
        let Some(setting) = table.settings.get(&key) else {
            continue;
        };
        let at = format!("source.{name}.{key}");
        let reference = setting
            .as_str()
            .filter(|text| secret::split(text).is_some());
        let Some(reference) = reference else {
            if mode.literal() && setting.as_str().is_some() {
                // `[secrets] allow_literal`: the setting is the secret itself.
                continue;
            }
            // Never quote what was written: it may be the secret itself.
            return Err((
                at,
                "is a secret and must name where it is kept (`file:PATH` or `credential:NAME`), never hold it (`[secrets] allow_literal = true` allows a literal)".to_owned(),
            ));
        };
        let resolved = match mode {
            Resolve::Shape(_) => Secret::new("x"),
            Resolve::Defer(_) => return Ok(None),
            Resolve::With(resolver, max, _) => resolver
                .resolve(reference, *max)
                .map_err(|error| (at.clone(), error.to_string()))?,
        };
        map.set(key.as_str(), Value::from(resolved.expose()));
    }
    Ok(Some(map.into()))
}

/// Checks a configuration's `[source.*]` and `[access]` without keeping what it builds or reading what it references: the
/// key, and what is wrong.
///
/// # Errors
///
/// As [`Sources::from_config`].
pub fn check(
    source: &SourceTables,
    access: &AccessConfig,
    allow_literal: bool,
) -> Result<(), Refusal> {
    Sources::bare().apply(source, access, &Resolve::Shape(allow_literal), &[])
}

/// A value for a provider that is its own configuration, for tests and embedders that build an instance by hand.
///
/// # Errors
///
/// When the settings are not a value.
pub fn settings_value(settings: &impl ToValue) -> Result<Value, String> {
    settings.to_value().map_err(|error| error.to_string())
}

/// The refusal of a start on a source that hands its programs the node's own display (`x11_forward`) to a peer that does not
/// hold the GUI grants: such a program reads the windows, types into them and sets the clipboard, which `gui.view`,
/// `gui.input` and `gui.clipboard` are for.
///
/// # Errors
///
/// `denied` when `described` says the source forwards the display and `grant` lacks one of the three.
pub(crate) fn refuse_display_forward<E>(
    grant: &crate::policy::Grant,
    described: Result<bohio_api::SourceInfo, E>,
) -> Result<(), bohio_proto::ErrorBody> {
    use crate::policy::Permission;
    let Ok(described) = described else {
        return Ok(());
    };
    let forwards = described
        .offers
        .iter()
        .any(|word| word == bohio_api::vocab::exec::X11_FORWARD);
    if forwards
        && !(grant.allows(Permission::GuiView)
            && grant.allows(Permission::GuiInput)
            && grant.allows(Permission::GuiClipboard))
    {
        return Err(bohio_proto::ErrorBody::new(
            bohio_proto::ErrorCode::Denied,
            "this source gives the programs it starts the node's own display: a peer needs gui.view, gui.input and gui.clipboard to use it",
        ));
    }
    Ok(())
}
