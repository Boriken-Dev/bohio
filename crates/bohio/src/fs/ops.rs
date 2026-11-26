//! The control calls, each resolved against a directory handle for the root its path lies under.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_api::FilesSource;
use bohio_api::vocab::file_kind;
use bohio_proto::fs::{Entry, FileKind, Listing, Metadata, Timestamp};
use bohio_proto::{ErrorBody, ErrorCode};

use super::path::{self, check_link_target};
use super::vfs::{self, Dir, DirEntry, File};
use super::{Context, io_error};

/// The granted roots of one peer, and the source that holds them.
pub(crate) struct Roots<'a> {
    roots: &'a [PathBuf],
    files: Arc<dyn FilesSource>,
    syntax: path::Syntax,
}

impl<'a> Roots<'a> {
    pub(crate) fn new(roots: &'a [PathBuf], files: super::FilesRef<'_>) -> Self {
        Self {
            roots,
            files: Arc::clone(files.source),
            syntax: files.syntax(),
        }
    }

    /// The directory handle of the root `text` lies under, and the path below it.
    pub(crate) fn resolve(&self, text: &str) -> Result<(Dir, PathBuf), ErrorBody> {
        let located = path::locate_in(self.roots, text, self.syntax)?;
        let root = &self.roots[located.root];
        let dir = Dir::open_root(&self.files, root).map_err(|error| io_error(&error))?;
        Ok((dir, located.relative))
    }

    /// As [`Roots::resolve`], for an operation that cannot act on a root itself.
    pub(crate) fn resolve_entry(&self, text: &str) -> Result<(Dir, PathBuf), ErrorBody> {
        let (dir, relative) = self.resolve(text)?;
        if relative.as_os_str().is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "this operation cannot act on a root itself",
            ));
        }
        Ok((dir, relative))
    }
}

fn dot(relative: &Path) -> &Path {
    if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    }
}

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

fn stamp(time: bohio_api::model::TimeModel) -> Timestamp {
    Timestamp {
        secs: time.secs,
        nanos: time.nanos,
    }
}

pub(crate) fn system_time(stamp: Timestamp) -> Result<SystemTime, ErrorBody> {
    if stamp.nanos >= 1_000_000_000 {
        return Err(bad("a time's nanoseconds must be below one billion"));
    }
    let moved = if stamp.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(stamp.secs.unsigned_abs(), stamp.nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(stamp.secs.unsigned_abs()))
            .and_then(|before| before.checked_add(Duration::from_nanos(u64::from(stamp.nanos))))
    };
    moved.ok_or_else(|| bad("the time is out of range"))
}

pub(crate) fn describe(meta: &vfs::Metadata) -> Metadata {
    let stat = &meta.0;
    let kind = match stat.kind.as_str() {
        file_kind::FILE => FileKind::File,
        file_kind::DIR => FileKind::Directory,
        file_kind::SYMLINK => FileKind::Symlink,
        _ => FileKind::Other,
    };
    Metadata {
        kind,
        size: stat.size,
        modified: stat.modified.map(stamp),
        accessed: stat.accessed.map(stamp),
        created: stat.created.map(stamp),
        mode: stat.mode,
        readonly: stat.readonly,
        attributes: stat.attributes,
        device: stat.device.filter(|_| cfg!(unix)),
        inode: stat.inode.filter(|_| cfg!(unix)),
        links: stat.links.filter(|_| cfg!(unix)),
    }
}

pub(crate) fn stat(roots: &Roots<'_>, text: &str, follow: bool) -> Result<Metadata, ErrorBody> {
    let (dir, relative) = roots.resolve(text)?;
    let meta = if follow {
        dir.metadata(dot(&relative))
    } else {
        dir.symlink_metadata(dot(&relative))
    }
    .map_err(|error| io_error(&error))?;
    Ok(describe(&meta))
}

/// Bytes of a listing reply that are not entries: what a page's estimate leaves room for.
const LIST_ENVELOPE: usize = 1024;

/// Bytes of an entry's encoding besides its name: an estimate that keeps a page inside the stream's message bound, which a
/// short page costs a request for the rest and a long one costs a refused reply.
const ENTRY_OVERHEAD: usize = 256;

pub(crate) fn list(
    roots: &Roots<'_>,
    context: &Context<'_>,
    text: &str,
    after: Option<&str>,
    limit: Option<u64>,
    budget: usize,
) -> Result<Listing, ErrorBody> {
    let (dir, relative) = roots.resolve(text)?;
    let page = limit.map_or(context.config.list_page, |wanted| {
        usize::try_from(wanted).unwrap_or(usize::MAX)
    });
    let page = page.clamp(1, context.config.list_page);
    let read = dir
        .read_dir(dot(&relative))
        .map_err(|error| io_error(&error))?;

    // Only the `page + 1` smallest names above `after` are kept, so a huge directory costs one
    // pass and a page of memory.
    let mut kept: BTreeMap<String, DirEntry> = BTreeMap::new();
    let mut skipped = 0_u64;
    for entry in read {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            skipped += 1;
            continue;
        };
        if after.is_some_and(|after| name.as_str() <= after) {
            continue;
        }
        kept.insert(name, entry);
        if kept.len() > page + 1 {
            kept.pop_last();
        }
    }
    let mut more = kept.len() > page;
    if more {
        kept.pop_last();
    }

    // A reply must fit the stream: stop adding entries when the estimate says it would not.
    let room = budget.saturating_sub(LIST_ENVELOPE);
    let mut used = 0_usize;
    let mut entries = Vec::new();
    for (name, entry) in kept {
        let Ok(meta) = entry.metadata() else { continue };
        used = used.saturating_add(name.len() + ENTRY_OVERHEAD);
        if !entries.is_empty() && used > room {
            more = true;
            break;
        }
        entries.push(Entry {
            name,
            metadata: describe(&meta),
        });
    }
    let next = if more {
        entries.last().map(|entry| entry.name.clone())
    } else {
        None
    };
    Ok(Listing {
        entries,
        next,
        skipped,
    })
}

pub(crate) fn mkdir(
    roots: &Roots<'_>,
    text: &str,
    parents: bool,
    mode: Option<u32>,
    allow_id_bits: bool,
) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve(text)?;
    if relative.as_os_str().is_empty() {
        return if parents {
            Ok(())
        } else {
            Err(ErrorBody::new(ErrorCode::Exists, "the root already exists"))
        };
    }
    if cfg!(windows) && mode.is_some() {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host has no permission bits",
        ));
    }
    if let Some(bits) = mode {
        super::check_mode(bits, allow_id_bits)?;
    }
    dir.create_dir(&relative, parents, mode)
        .map_err(|error| io_error(&error))
}

pub(crate) fn remove(roots: &Roots<'_>, text: &str, recursive: bool) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    let meta = dir
        .symlink_metadata(&relative)
        .map_err(|error| io_error(&error))?;
    let removed = if meta.is_dir() {
        if recursive {
            dir.remove_dir_all(&relative)
        } else {
            dir.remove_dir(&relative)
        }
    } else {
        dir.remove_file_or_symlink(&relative)
    };
    removed.map_err(|error| io_error(&error))
}

pub(crate) fn rename(
    roots: &Roots<'_>,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<(), ErrorBody> {
    let (from_dir, from_relative) = roots.resolve_entry(from)?;
    let (to_dir, to_relative) = roots.resolve_entry(to)?;
    from_dir
        .rename_to(&from_relative, &to_dir, &to_relative, overwrite)
        .map_err(|error| io_error(&error))
}

pub(crate) fn symlink(roots: &Roots<'_>, target: &str, link: &str) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(link)?;
    let depth = relative.components().count().saturating_sub(1);
    check_link_target(target, depth)?;
    make_link(&dir, &relative, target).map_err(|error| io_error(&error))
}

/// Makes `relative` a link to `target`, a Windows link to a directory when the target is one.
pub(crate) fn make_link(dir: &Dir, relative: &Path, target: &str) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        dir.symlink(target, relative, false)
    }
    #[cfg(windows)]
    {
        let native_text = target.replace('/', "\\");
        let beside = relative.parent().map_or_else(
            || PathBuf::from(&native_text),
            |parent| parent.join(&native_text),
        );
        let to_directory = dir.metadata(&beside).is_ok_and(|meta| meta.is_dir());
        dir.symlink(&native_text, relative, to_directory)
    }
}
pub(crate) fn readlink(roots: &Roots<'_>, text: &str) -> Result<String, ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    dir.read_link(&relative).map_err(|error| io_error(&error))
}

pub(crate) fn set_times(
    roots: &Roots<'_>,
    text: &str,
    accessed: Option<Timestamp>,
    modified: Option<Timestamp>,
) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    let spec = |stamp: Option<Timestamp>| -> Result<Option<SystemTime>, ErrorBody> {
        stamp.map(system_time).transpose()
    };
    dir.set_times(&relative, spec(accessed)?, spec(modified)?)
        .map_err(|error| io_error(&error))
}

#[cfg(unix)]
pub(crate) fn set_mode(
    roots: &Roots<'_>,
    text: &str,
    mode: u32,
    allow_id_bits: bool,
) -> Result<(), ErrorBody> {
    super::check_mode(mode, allow_id_bits)?;
    let (dir, relative) = roots.resolve_entry(text)?;
    dir.set_mode(&relative, mode)
        .map_err(|error| io_error(&error))
}

#[cfg(not(unix))]
pub(crate) fn set_mode(
    roots: &Roots<'_>,
    text: &str,
    _mode: u32,
    _allow_id_bits: bool,
) -> Result<(), ErrorBody> {
    // The path is still judged first: a refusal for leaving a root does not depend on the host.
    roots.resolve_entry(text)?;
    Err(ErrorBody::new(
        ErrorCode::Unsupported,
        "this host has no permission bits",
    ))
}

#[cfg(windows)]
pub(crate) fn set_attributes(
    roots: &Roots<'_>,
    text: &str,
    readonly: Option<bool>,
) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    let Some(readonly) = readonly else {
        return Ok(());
    };
    dir.set_readonly(&relative, readonly)
        .map_err(|error| io_error(&error))
}

#[cfg(not(windows))]
pub(crate) fn set_attributes(
    roots: &Roots<'_>,
    text: &str,
    _readonly: Option<bool>,
) -> Result<(), ErrorBody> {
    roots.resolve_entry(text)?;
    Err(ErrorBody::new(
        ErrorCode::Unsupported,
        "this host has no file attributes",
    ))
}

/// Opens the regular file `relative` under `dir` and returns it with its metadata. A directory or
/// other kind is `bad_request`, judged before the open because some systems refuse to open a
/// directory with an access error.
pub(crate) fn open_regular_in(
    dir: &Dir,
    relative: &Path,
) -> Result<(File, vfs::Metadata), ErrorBody> {
    let before = dir.metadata(relative).map_err(|error| io_error(&error))?;
    if !before.is_file() {
        return Err(bad("the path is not a regular file"));
    }
    let file = dir.open(relative).map_err(|error| io_error(&error))?;
    let meta = file.metadata().map_err(|error| io_error(&error))?;
    if !meta.is_file() {
        return Err(bad("the path is not a regular file"));
    }
    Ok((file, meta))
}

/// As [`open_regular_in`], for the path text of a request.
pub(crate) fn open_regular(
    roots: &Roots<'_>,
    text: &str,
) -> Result<(File, vfs::Metadata), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    open_regular_in(&dir, &relative)
}

/// The wire's time for a moment.
pub(crate) fn timestamp(time: SystemTime) -> Timestamp {
    stamp(vfs::time_model(time))
}
