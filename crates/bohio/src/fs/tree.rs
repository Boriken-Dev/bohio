//! Whole files and trees moved, copied and cut on the host: `rename` and `move` without a race on the
//! target, `copy` of a directory, and `truncate`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use bohio_proto::fs::WriteMode;
use bohio_proto::{ErrorBody, ErrorCode};
use cap_fs_ext::{DirExt, SystemTimeSpec};
use cap_std::fs::{Dir, File, OpenOptions};

use super::native;
use super::ops::{Roots, open_regular_in};
use super::path::check_link_target;
use super::stage::Staged;
use super::{Context, io_error};

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

/// The directory holding `relative` and its last name, both opened under `dir` without leaving it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn parent_of<'a>(dir: &Dir, relative: &'a Path) -> std::io::Result<(Dir, &'a std::ffi::OsStr)> {
    let name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let parent = match relative
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        Some(parent) => dir.open_dir(parent)?,
        None => dir.try_clone()?,
    };
    Ok((parent, name))
}

/// Renames `from` to `to`, replacing what is at `to` only when `overwrite`.
///
/// Without `overwrite` the operating system decides, in the rename itself, for files and directories alike:
/// Linux and macOS with `renameat2` (`RENAME_NOREPLACE`) and `renamex_np` (`RENAME_EXCL`), Windows with
/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`. Only where there is no such call (FreeBSD and other
/// Unix) or the filesystem refuses the flag (`EINVAL`) is there a gap: a regular file is then given its new
/// name with a hard link, which fails when the name exists, and the old name is removed; a directory, or a
/// file on a filesystem without hard links, is checked first and renamed after, so a name made in between is
/// replaced.
pub(crate) fn rename_in(
    from_dir: &Dir,
    from: &Path,
    to_dir: &Dir,
    to: &Path,
    overwrite: bool,
) -> std::io::Result<()> {
    if overwrite {
        return super::native::rename_replacing(from_dir, from, to_dir, to);
    }
    rename_new(from_dir, from, to_dir, to)
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn rename_new(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{RenameFlags, renameat_with};
    use rustix::io::Errno;
    let (from_parent, from_name) = parent_of(from_dir, from)?;
    let (to_parent, to_name) = parent_of(to_dir, to)?;
    match renameat_with(
        &from_parent,
        from_name,
        &to_parent,
        to_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => Ok(()),
        // The filesystem or the kernel has no such flag: the fallback is all there is.
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {
            link_then_remove(from_dir, from, to_dir, to)
        }
        Err(errno) => Err(errno.into()),
    }
}

#[cfg(windows)]
fn rename_new(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> std::io::Result<()> {
    super::native::rename(from_dir, from, to_dir, to, false)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn rename_new(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> std::io::Result<()> {
    link_then_remove(from_dir, from, to_dir, to)
}

/// The fallback: a hard link for a regular file, else check then rename.
#[cfg(not(windows))]
fn link_then_remove(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> std::io::Result<()> {
    if from_dir.symlink_metadata(from)?.is_file() {
        match from_dir.hard_link(from, to_dir, to) {
            Ok(()) => return from_dir.remove_file(from),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Err(error),
            // No hard links here (or none to this name): the check below is all there is.
            Err(_) => {}
        }
    }
    check_then_rename(from_dir, from, to_dir, to)
}

#[cfg(not(windows))]
fn check_then_rename(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> std::io::Result<()> {
    if to_dir.symlink_metadata(to).is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    from_dir.rename(from, to_dir, to)
}

/// `move`: [`rename_in`], and when the names lie on different filesystems a copy that keeps permission bits
/// and modification time followed by removal of the original.
pub(crate) fn move_to(
    roots: &Roots<'_>,
    context: &Context<'_>,
    from: &str,
    to: &str,
    overwrite: bool,
) -> Result<(), ErrorBody> {
    let (from_dir, from_relative) = roots.resolve_entry(from)?;
    let (to_dir, to_relative) = roots.resolve_entry(to)?;
    match rename_in(&from_dir, &from_relative, &to_dir, &to_relative, overwrite) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => move_across(
            context,
            &Place::new(&from_dir, &from_relative),
            &Place::new(&to_dir, &to_relative),
            overwrite,
        ),
        Err(error) => Err(io_error(&error)),
    }
}

/// A name under a directory handle.
struct Place<'a> {
    dir: &'a Dir,
    relative: &'a Path,
}

impl<'a> Place<'a> {
    const fn new(dir: &'a Dir, relative: &'a Path) -> Self {
        Self { dir, relative }
    }
}

fn move_across(
    context: &Context<'_>,
    from: &Place<'_>,
    to: &Place<'_>,
    overwrite: bool,
) -> Result<(), ErrorBody> {
    let meta = from
        .dir
        .symlink_metadata(from.relative)
        .map_err(|error| io_error(&error))?;
    let made_target = to.dir.symlink_metadata(to.relative).is_err();
    let copied = copy_entry(
        context,
        from,
        to,
        &CopyOptions {
            overwrite,
            recursive: true,
            preserve: true,
        },
    );
    if let Err(error) = copied {
        // A copy that failed leaves the original where it was; what it made of a new name goes.
        if made_target {
            let _ = if meta.is_dir() {
                native::remove_dir_all(to.dir, to.relative)
            } else {
                native::remove_file_or_symlink(to.dir, to.relative)
            };
        }
        return Err(error);
    }
    let removed = if meta.is_dir() {
        native::remove_dir_all(from.dir, from.relative)
    } else {
        native::remove_file_or_symlink(from.dir, from.relative)
    };
    removed.map_err(|error| io_error(&error))
}

/// What a copy does besides copying the bytes.
#[derive(Clone, Copy)]
pub(crate) struct CopyOptions {
    pub(crate) overwrite: bool,
    pub(crate) recursive: bool,
    pub(crate) preserve: bool,
}

/// `copy`: a regular file, a link, or with `recursive` a directory and what it holds.
pub(crate) fn copy(
    roots: &Roots<'_>,
    context: &Context<'_>,
    from: &str,
    to: &str,
    options: &CopyOptions,
) -> Result<(), ErrorBody> {
    let (from_dir, from_relative) = roots.resolve_entry(from)?;
    let (to_dir, to_relative) = roots.resolve_entry(to)?;
    copy_entry(
        context,
        &Place::new(&from_dir, &from_relative),
        &Place::new(&to_dir, &to_relative),
        options,
    )
}

fn copy_entry(
    context: &Context<'_>,
    from: &Place<'_>,
    to: &Place<'_>,
    options: &CopyOptions,
) -> Result<(), ErrorBody> {
    // A path through a link is followed by `metadata`, as a copy of a file does: the final name is not.
    let meta = from
        .dir
        .symlink_metadata(from.relative)
        .map_err(|error| io_error(&error))?;
    if meta.is_dir() {
        if !options.recursive {
            return Err(bad("the path is a directory: copy it with recursive"));
        }
        return copy_tree(context, from, to, options);
    }
    if meta.is_symlink() && options.recursive {
        return copy_link(from, to, options.overwrite);
    }
    copy_file(context, from, to, options)
}

fn copy_file(
    context: &Context<'_>,
    from: &Place<'_>,
    to: &Place<'_>,
    options: &CopyOptions,
) -> Result<(), ErrorBody> {
    let (mut source, meta) = open_regular_in(from.dir, from.relative)?;
    #[cfg(unix)]
    let file_mode = {
        use cap_std::fs::PermissionsExt;
        Some(meta.permissions().mode() & 0o7777 & !super::ID_BITS)
    };
    #[cfg(not(unix))]
    let file_mode = None;
    let mode = if options.overwrite {
        WriteMode::Replace
    } else {
        WriteMode::Create
    };
    let mut staged = Staged::open(to.dir, to.relative, &mode, false, file_mode)?;
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
    if options.preserve
        && let Ok(modified) = meta.modified()
    {
        staged.set_modified(modified.into_std());
    }
    staged.commit(context.config.sync_writes)
}

/// A symbolic link made again at `to` with the same text, which must still stay inside the root.
fn copy_link(from: &Place<'_>, to: &Place<'_>, overwrite: bool) -> Result<(), ErrorBody> {
    let text = native::read_link(from.dir, from.relative).map_err(|error| io_error(&error))?;
    let text = text.into_os_string().into_string().map_err(|_| {
        ErrorBody::new(
            ErrorCode::Io,
            "the link's text is not valid text and cannot be copied",
        )
    })?;
    let depth = to.relative.components().count().saturating_sub(1);
    check_link_target(&text, depth)?;
    if overwrite
        && to
            .dir
            .symlink_metadata(to.relative)
            .is_ok_and(|meta| !meta.is_dir())
    {
        native::remove_file_or_symlink(to.dir, to.relative).map_err(|error| io_error(&error))?;
    }
    make_link(to, &text)
}

#[cfg(not(windows))]
fn make_link(to: &Place<'_>, text: &str) -> Result<(), ErrorBody> {
    to.dir
        .symlink(text, to.relative)
        .map_err(|error| io_error(&error))
}

#[cfg(windows)]
fn make_link(to: &Place<'_>, text: &str) -> Result<(), ErrorBody> {
    let native_text = text.replace('/', "\\");
    let beside = to.relative.parent().map_or_else(
        || PathBuf::from(&native_text),
        |parent| parent.join(&native_text),
    );
    let to_directory = to.dir.metadata(&beside).is_ok_and(|meta| meta.is_dir());
    native::symlink(to.dir, to.relative, native_text.as_ref(), to_directory)
        .map_err(|error| io_error(&error))
}

/// A directory copied with everything in it, depth first and without recursion. Links are made again as
/// links, so a loop of them cannot run it on; anything that is neither a file, a link nor a directory is
/// refused. A copy that fails partway leaves what it made, as `cp -r` does.
fn copy_tree(
    context: &Context<'_>,
    from: &Place<'_>,
    to: &Place<'_>,
    options: &CopyOptions,
) -> Result<(), ErrorBody> {
    let limit = context.config.max_tree_entries;
    let mut count = 0_usize;
    // Directories whose permission bits and times are set after what is in them is made.
    let mut finish: Vec<(PathBuf, cap_std::fs::Metadata)> = Vec::new();
    let mut stack = vec![(from.relative.to_path_buf(), to.relative.to_path_buf(), true)];
    while let Some((source, target, top)) = stack.pop() {
        let meta = from
            .dir
            .symlink_metadata(&source)
            .map_err(|error| io_error(&error))?;
        match to.dir.symlink_metadata(&target) {
            Ok(existing) if existing.is_dir() => {
                if top && !options.overwrite {
                    return Err(ErrorBody::new(ErrorCode::Exists, "the target exists"));
                }
            }
            Ok(_) => return Err(ErrorBody::new(ErrorCode::Exists, "the target exists")),
            Err(_) => native::create_dir(
                to.dir,
                &target,
                false,
                #[cfg(unix)]
                None,
            )
            .map_err(|error| io_error(&error))?,
        }
        if options.preserve {
            finish.push((target.clone(), meta));
        }
        let mut children = Vec::new();
        for entry in native::read_dir(from.dir, &source).map_err(|error| io_error(&error))? {
            let entry = entry.map_err(|error| io_error(&error))?;
            count += 1;
            if limit != 0 && count > limit {
                return Err(ErrorBody::new(
                    ErrorCode::Busy,
                    "the tree holds more entries than [filesystem] max_tree_entries allows",
                ));
            }
            children.push(entry.file_name());
        }
        // Sorted so a failed copy leaves a predictable prefix; directories are made when popped.
        children.sort();
        for name in children.into_iter().rev() {
            let child_source = source.join(&name);
            let child_target = target.join(&name);
            let kind = from
                .dir
                .symlink_metadata(&child_source)
                .map_err(|error| io_error(&error))?;
            if kind.is_dir() {
                stack.push((child_source, child_target, false));
            } else if kind.is_symlink() {
                copy_link(
                    &Place::new(from.dir, &child_source),
                    &Place::new(to.dir, &child_target),
                    options.overwrite,
                )?;
            } else if kind.is_file() {
                copy_file(
                    context,
                    &Place::new(from.dir, &child_source),
                    &Place::new(to.dir, &child_target),
                    options,
                )?;
            } else {
                return Err(bad(
                    "the tree holds something that is not a file, a link or a directory",
                ));
            }
        }
    }
    for (target, meta) in finish.into_iter().rev() {
        #[cfg(unix)]
        {
            use cap_std::fs::{Permissions, PermissionsExt};
            let bits = meta.permissions().mode() & 0o7777 & !super::ID_BITS;
            to.dir
                .set_permissions(&target, Permissions::from_mode(bits))
                .map_err(|error| io_error(&error))?;
        }
        if let Ok(modified) = meta.modified() {
            to.dir
                .set_times(&target, None, Some(SystemTimeSpec::Absolute(modified)))
                .map_err(|error| io_error(&error))?;
        }
    }
    Ok(())
}

/// `truncate`: a regular file's length set to `length`.
pub(crate) fn truncate(roots: &Roots<'_>, text: &str, length: u64) -> Result<(), ErrorBody> {
    let (dir, relative) = roots.resolve_entry(text)?;
    let before = dir.metadata(&relative).map_err(|error| io_error(&error))?;
    if !before.is_file() {
        return Err(bad("the path is not a regular file"));
    }
    let file: File = dir
        .open_with(&relative, OpenOptions::new().write(true))
        .map_err(|error| io_error(&error))?;
    if !file.metadata().map_err(|error| io_error(&error))?.is_file() {
        return Err(bad("the path is not a regular file"));
    }
    file.set_len(length).map_err(|error| io_error(&error))
}
