//! Provider libraries mapped into the node at start (`[providers] paths`).
//!
//! A library is a file that exports guatiao's entry symbol and declares the Bohío kinds it serves. Mapping one runs its code
//! as the node's own account, so each candidate is read as data first ([`probe`]): a file that is not a library, or declares
//! no Bohío kind, is never mapped. Loading goes through guatiao's [`Registry::load_file`], which holds no `unsafe` of ours:
//! nothing here needs it. One library failing loads nothing from it and leaves the others to load.
//!
//! Every check is a default the operator may weaken: a library or its directory that another account may write is refused
//! while `[providers] check_access` holds, and loaded with a `non-default:` line in the log and the report when it does not.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::fs::File;
use std::path::{Path, PathBuf};

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
use bohio_api::guatiao::library::foreign::Settings;
use bohio_api::guatiao::library::{Dlopen, Loading, Registry, probe};
use bohio_proto::hash::Algorithm;
use bohio_transfer::hash::digest_reader;

use crate::config::{DlopenChoice, ProvidersConfig};
use crate::ownership;

/// The kinds of `bohio-api`: a library that declares none of them is not a Bohío provider library.
const KINDS: [&str; 4] = [
    "bohio_terminal",
    "bohio_desktop",
    "bohio_power",
    "bohio_media",
];

/// What became of one library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryState {
    /// Mapped; its providers are registered.
    Loaded,
    /// Passed over, which is an answer and not a fault: not a library, no Bohío kind, already loaded.
    Skipped,
    /// Could not be loaded (refused by the owner check, missing, or the loader's own failure).
    Failed,
}

impl LibraryState {
    /// The word the log and `bohio providers` use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

/// One library of `[providers] paths`, as the start found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    /// The file, or the configured path that named nothing.
    pub path: PathBuf,
    /// What became of it.
    pub state: LibraryState,
    /// Why, or what it offers.
    pub detail: String,
    /// `sha256:` and the digest of the file's bytes, when it was read.
    pub digest: Option<String>,
}

/// What loading `[providers] paths` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// `[providers] check_access`: `false` is the non-default choice.
    pub check_access: bool,
    /// One entry for each configured path that is a file or names nothing, and for each library file of a directory.
    pub libraries: Vec<Library>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            check_access: true,
            libraries: Vec::new(),
        }
    }
}

/// Tells the registry which loader maps a library and how the foreign one starts, and logs each weakened choice.
///
/// The foreign loader (the system's own `ld.so` and glibc, started inside the process) is what a statically linked Linux
/// build on x86_64 or aarch64 loads libraries with. The process starts it once, with the settings of the first registry that
/// needs it, and one foreign C library takes signal 33.
fn apply_loader(registry: &mut Registry, config: &ProvidersConfig) {
    registry.set_dlopen(match config.dlopen {
        DlopenChoice::Auto => Dlopen::Auto,
        DlopenChoice::System => Dlopen::System,
        DlopenChoice::Foreign => Dlopen::Foreign,
    });
    if config.dlopen != DlopenChoice::Auto {
        tracing::warn!(
            "non-default: [providers] dlopen is {}: the loader a library is mapped with is not the one this build would choose",
            config.dlopen.name()
        );
    }
    if let Some(path) = &config.ld_so {
        tracing::warn!(
            "non-default: [providers] ld_so names {}: the foreign loader maps and enters that file, so whoever can write it chooses the code this node runs",
            path.display()
        );
    }
    if config.allow_reserved_signal_handlers {
        tracing::warn!(
            "non-default: [providers] allow_reserved_signal_handlers is true: the foreign loader starts even when this node handles signal 32 or 33, and may replace that handler"
        );
    }
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        let mut settings = Settings::default();
        settings.ld_so.clone_from(&config.ld_so);
        settings.allow_reserved_signal_handlers = config.allow_reserved_signal_handlers;
        registry.set_foreign_settings(settings);
    }
}

/// Loads what `config` names into `registry` and reports each library.
pub(super) fn load(registry: &mut Registry, config: &ProvidersConfig) -> Report {
    let mut report = Report {
        check_access: config.check_access,
        libraries: Vec::new(),
    };
    if config.paths.is_empty() {
        return report;
    }
    if !config.check_access {
        tracing::warn!(
            "non-default: [providers] check_access is false: who owns and who may write a provider library and its directory is not checked: whoever can write there chooses the code this node runs"
        );
    }
    apply_loader(registry, config);
    let mut checked = ownership::Checked::new();
    for path in &config.paths {
        for candidate in candidates(path, &mut report) {
            let library = load_one(registry, &candidate, config.check_access, &mut checked);
            log(&library);
            report.libraries.push(library);
        }
    }
    report
}

/// The library files `path` names: itself, or the files of a directory that look like libraries, by name. A path that
/// is neither is a failed entry of `report` and names no candidate.
fn candidates(path: &Path, report: &mut Report) -> Vec<PathBuf> {
    let failed = |report: &mut Report, why: String| {
        let library = Library {
            path: path.to_owned(),
            state: LibraryState::Failed,
            detail: why,
            digest: None,
        };
        log(&library);
        report.libraries.push(library);
        Vec::new()
    };
    if path.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|file| file.is_file() && has_library_extension(file))
                    .collect();
                files.sort();
                files
            }
            Err(error) => failed(report, format!("the directory cannot be listed: {error}")),
        }
    } else if path.is_file() {
        vec![path.to_owned()]
    } else {
        failed(report, "names no file or directory".to_owned())
    }
}

fn has_library_extension(file: &Path) -> bool {
    let wanted = if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    file.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(wanted))
}

fn load_one(
    registry: &mut Registry,
    file: &Path,
    check_access: bool,
    checked: &mut ownership::Checked,
) -> Library {
    let mut library = Library {
        path: file.to_owned(),
        state: LibraryState::Skipped,
        detail: String::new(),
        digest: None,
    };
    // With the check off nothing is asked of the file system: the operator chose to trust the place.
    if check_access {
        let problems = ownership::library_problems(file, checked);
        if !problems.is_empty() {
            let texts: Vec<String> = problems.into_iter().map(|problem| problem.text).collect();
            library.state = LibraryState::Failed;
            library.detail = format!("refused: {}", texts.join("; "));
            return library;
        }
    }
    let probed = match probe(file) {
        Ok(probed) => probed,
        Err(error) => {
            library.detail = format!("could not be read as a library file: {error}");
            return library;
        }
    };
    if !probed.entry {
        library.detail = "not a guatiao library".to_owned();
        library.digest = digest_of(file);
        return library;
    }
    if !probed.declared.kinds().any(|kind| KINDS.contains(&kind)) {
        let declared: Vec<&str> = probed.declared.kinds().collect();
        library.detail = if declared.is_empty() {
            "declares no kind, so it is not mapped".to_owned()
        } else {
            format!(
                "declares no Bohío kind (it declares {}), so it is not mapped",
                declared.join(", ")
            )
        };
        library.digest = digest_of(file);
        return library;
    }
    match registry.load_file(file) {
        Ok(Loading::Loaded(loaded)) => {
            library.state = LibraryState::Loaded;
            let mut detail = format!(
                "{} {}: {} provider{}",
                loaded.id,
                loaded.version,
                loaded.providers,
                if loaded.providers == 1 { "" } else { "s" }
            );
            for skipped in &loaded.skipped {
                let _ = write!(detail, "; {skipped}");
            }
            library.detail = detail;
            library.digest = loaded.digest().ok().flatten().or_else(|| digest_of(file));
        }
        Ok(Loading::Skipped(why)) => {
            library.detail = why.to_string();
            library.digest = digest_of(file);
        }
        Err(error) => {
            library.state = LibraryState::Failed;
            library.detail = error.to_string();
            library.digest = digest_of(file);
        }
    }
    library
}

/// `sha256:` and the digest of the file, or `None` when it cannot be read.
fn digest_of(file: &Path) -> Option<String> {
    let mut reader = File::open(file).ok()?;
    let (digests, _) = digest_reader(&mut reader, &[Algorithm::Sha256], 64 * 1024).ok()?;
    digests
        .get(&Algorithm::Sha256)
        .map(|hex| format!("sha256:{hex}"))
}

fn log(library: &Library) {
    let digest = library.digest.as_deref().unwrap_or("no digest");
    let line = format!(
        "provider library {}: {}: {} ({digest})",
        library.path.display(),
        library.state.name(),
        library.detail
    );
    match library.state {
        LibraryState::Loaded | LibraryState::Skipped => tracing::info!("{line}"),
        LibraryState::Failed => tracing::warn!("{line}"),
    }
}
