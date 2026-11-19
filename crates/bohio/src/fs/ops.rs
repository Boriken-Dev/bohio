//! The control calls, each resolved against a directory handle for the root its path lies under.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_proto::fs::{Entry, FileKind, Listing, Metadata, Timestamp, WriteMode};
use bohio_proto::{ErrorBody, ErrorCode};
use cap_fs_ext::{DirExt, SystemTimeSpec};
use cap_std::fs::{Dir, DirBuilder, DirEntry, File};
use sha2::{Digest, Sha256};

use super::path::{self, check_link_target};
use super::stage::Staged;
use super::{Context, io_error};

/// The granted roots of one peer.
pub(crate) struct Roots<'a> {
    roots: &'a [PathBuf],
}

impl<'a> Roots<'a> {
    pub(crate) const fn new(roots: &'a [PathBuf]) -> Self {
        Self { roots }
    }

    /// The directory handle of the root `text` lies under, and the path below it.
    pub(crate) fn resolve(&self, text: &str) -> Result<(Dir, PathBuf), ErrorBody> {
        let located = path::locate(self.roots, text)?;
        let root = &self.roots[located.root];
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .map_err(|error| io_error(&error))?;
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

fn timestamp(time: cap_std::time::SystemTime) -> Timestamp {
    let time = time.into_std();
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Timestamp {
            secs: i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            nanos: after.subsec_nanos(),
        },
        Err(error) => {
            let before = error.duration();
            let nanos = before.subsec_nanos();
            let whole = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            if nanos == 0 {
                Timestamp {
                    secs: -whole,
                    nanos: 0,
                }
            } else {
                Timestamp {
                    secs: -whole - 1,
                    nanos: 1_000_000_000 - nanos,
                }
            }
        }
    }
}

fn system_time(stamp: Timestamp) -> Result<SystemTime, ErrorBody> {
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

fn describe(meta: &cap_std::fs::Metadata) -> Metadata {
    let kind = if meta.is_file() {
        FileKind::File
    } else if meta.is_dir() {
        FileKind::Directory
    } else if meta.is_symlink() {
        FileKind::Symlink
    } else {
        FileKind::Other
    };
    let permissions = meta.permissions();
    #[cfg(unix)]
    let mode = {
        use cap_std::fs::PermissionsExt;
        Some(permissions.mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    #[cfg(windows)]
    let attributes = {
        use cap_std::fs::MetadataExt;
        Some(meta.file_attributes())
    };
    #[cfg(not(windows))]
    let attributes = None;
    Metadata {
        kind,
        size: meta.len(),
        modified: meta.modified().ok().map(timestamp),
        accessed: meta.accessed().ok().map(timestamp),
        created: meta.created().ok().map(timestamp),
        mode,
        readonly: permissions.readonly(),
        attributes,
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
    let room = budget.saturating_sub(1024);
    let mut used = 0_usize;
    let mut entries = Vec::new();
    for (name, entry) in kept {
        let Ok(meta) = entry.metadata() else { continue };
        used = used.saturating_add(name.len() + 256);
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
        super::check_mode(bits)?;
    }
    let mut builder = DirBuilder::new();
    builder.recursive(parents);
    #[cfg(unix)]
    if let Some(bits) = mode {
        use cap_std::fs::DirBuilderExt;
        builder.mode(bits & 0o7777);
    }
    dir.create_dir_with(&relative, &builder)
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
    if !overwrite && to_dir.symlink_metadata(&to_relative).is_ok() {
        return Err(ErrorBody::new(ErrorCode::Exists, "the target exists"));
    }
    from_dir
        .rename(&from_relative, &to_dir, &to_relative)
        .map_err(|error| io_error(&error))
}

pub(crate) fn copy(
    roots: &Roots<'_>,
    context: &Context<'_>,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<(), ErrorBody> {
    let (from_dir, from_relative) = roots.resolve_entry(from)?;
    let (to_dir, to_relative) = roots.resolve_entry(to)?;
    let (mut source, meta) = open_regular_in(&from_dir, &from_relative)?;
    #[cfg(unix)]
    let file_mode = {
        use cap_std::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777 & !super::ID_BITS)
    };
    #[cfg(not(unix))]
    let file_mode = {
        let _ = &meta;
        None
    };
    let mode = if overwrite {
        WriteMode::Replace
    } else {
        WriteMode::Create
    };
    let mut staged = Staged::open(&to_dir, &to_relative, &mode, false, file_mode)?;
    let mut buffer = vec![0_u8; context.config.chunk_size];
    loop {
        let count = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        staged
            .file()
            .write_all(&buffer[..count])
            .map_err(|error| io_error(&error))?;
    }
    staged.commit(context.config.sync_writes)
}

pub(crate) fn symlink(roots: &Roots<'_>, target: &str, link: &str) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(link)?;
    let depth = relative.components().count().saturating_sub(1);
    check_link_target(target, depth)?;
    #[cfg(not(windows))]
    {
        dir.symlink(target, &relative)
            .map_err(|error| io_error(&error))
    }
    #[cfg(windows)]
    {
        let native = target.replace('/', "\\");
        let beside = relative
            .parent()
            .map_or_else(|| PathBuf::from(&native), |parent| parent.join(&native));
        let to_directory = dir.metadata(&beside).is_ok_and(|meta| meta.is_dir());
        if to_directory {
            dir.symlink_dir(&native, &relative)
        } else {
            dir.symlink_file(&native, &relative)
        }
        .map_err(|error| io_error(&error))
    }
}

pub(crate) fn readlink(roots: &Roots<'_>, text: &str) -> Result<String, ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    let target = dir
        .read_link_contents(&relative)
        .map_err(|error| io_error(&error))?;
    target.into_os_string().into_string().map_err(|_| {
        ErrorBody::new(
            ErrorCode::Io,
            "the link's text is not valid text and cannot be sent",
        )
    })
}

pub(crate) fn set_times(
    roots: &Roots<'_>,
    text: &str,
    accessed: Option<Timestamp>,
    modified: Option<Timestamp>,
) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    let spec = |stamp: Option<Timestamp>| -> Result<Option<SystemTimeSpec>, ErrorBody> {
        stamp
            .map(|stamp| {
                system_time(stamp)
                    .map(|time| SystemTimeSpec::Absolute(cap_std::time::SystemTime::from_std(time)))
            })
            .transpose()
    };
    dir.set_times(&relative, spec(accessed)?, spec(modified)?)
        .map_err(|error| io_error(&error))
}

#[cfg(unix)]
pub(crate) fn set_mode(roots: &Roots<'_>, text: &str, mode: u32) -> Result<(), ErrorBody> {
    use cap_std::fs::{Permissions, PermissionsExt};
    super::check_mode(mode)?;
    let (dir, relative) = roots.resolve_entry(text)?;
    dir.set_permissions(&relative, Permissions::from_mode(mode))
        .map_err(|error| io_error(&error))
}

#[cfg(not(unix))]
pub(crate) fn set_mode(roots: &Roots<'_>, text: &str, _mode: u32) -> Result<(), ErrorBody> {
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
    let mut permissions = dir
        .metadata(&relative)
        .map_err(|error| io_error(&error))?
        .permissions();
    permissions.set_readonly(readonly);
    dir.set_permissions(&relative, permissions)
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

/// The lowercase hexadecimal text of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// Opens the regular file `relative` under `dir` and returns it with its metadata. A directory or
/// other kind is `bad_request`, judged before the open because some systems refuse to open a
/// directory with an access error.
pub(crate) fn open_regular_in(
    dir: &Dir,
    relative: &Path,
) -> Result<(File, cap_std::fs::Metadata), ErrorBody> {
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
) -> Result<(File, cap_std::fs::Metadata), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    open_regular_in(&dir, &relative)
}

pub(crate) fn checksum(
    roots: &Roots<'_>,
    context: &Context<'_>,
    text: &str,
    offset: u64,
    length: Option<u64>,
) -> Result<(String, u64), ErrorBody> {
    let (mut file, _) = open_regular(roots, text)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| io_error(&error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; context.config.chunk_size];
    let mut remaining = length.unwrap_or(u64::MAX);
    let mut total = 0_u64;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        match file.read(&mut buffer[..want]) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
                total += count as u64;
                remaining -= count as u64;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(&error)),
        }
    }
    Ok((hex(&hasher.finalize()), total))
}
