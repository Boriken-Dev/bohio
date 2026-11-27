//! Whole files and trees moved, copied and cut on the host: `rename` and `move` without a race on the
//! target, `copy` of a directory, and `truncate`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use bohio_proto::fs::WriteMode;
use bohio_proto::{ErrorBody, ErrorCode};

use super::ops::{Roots, make_link, open_regular_in};
use super::path::check_link_target;
use super::stage::Staged;
use super::vfs::{Dir, Metadata, OpenOptions};
use super::{Context, io_error};

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

/// `move`: a rename, and when the names lie on different filesystems a copy that keeps permission bits
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
    match from_dir.rename_to(&from_relative, &to_dir, &to_relative, overwrite) {
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
                to.dir.remove_dir_all(to.relative)
            } else {
                to.dir.remove_file_or_symlink(to.relative)
            };
        }
        return Err(error);
    }
    let removed = if meta.is_dir() {
        from.dir.remove_dir_all(from.relative)
    } else {
        from.dir.remove_file_or_symlink(from.relative)
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

/// Whether `inner` is `outer` or a name below it, word by word; a source that is not POSIX may fold case.
fn lies_within(inner: &Path, outer: &Path, posix: bool) -> bool {
    let mut inner = inner.components();
    outer.components().all(|word| {
        inner.next().is_some_and(|other| {
            let (one, two) = (word.as_os_str(), other.as_os_str());
            one == two || (!posix && one.eq_ignore_ascii_case(two))
        })
    })
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
        if from.dir.same_root(to.dir)
            && lies_within(to.relative, from.relative, context.source.posix)
        {
            return Err(bad("a directory cannot be copied into itself"));
        }
        return copy_tree(context, from, to, options);
    }
    if meta.is_symlink() && options.recursive {
        return copy_link(from, to, options.overwrite, context.source.posix);
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
    let file_mode = if cfg!(unix) {
        meta.mode().map(|mode| mode & 0o7777 & !super::ID_BITS)
    } else {
        None
    };
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
        staged.set_modified(modified);
    }
    staged.commit(context.config.sync_writes)
}

/// A symbolic link made again at `to` with the same text, which must still stay inside the root.
fn copy_link(
    from: &Place<'_>,
    to: &Place<'_>,
    overwrite: bool,
    posix: bool,
) -> Result<(), ErrorBody> {
    let text = from
        .dir
        .read_link(from.relative)
        .map_err(|error| io_error(&error))?;
    let depth = to.relative.components().count().saturating_sub(1);
    check_link_target(&text, depth, posix)?;
    if overwrite
        && to
            .dir
            .symlink_metadata(to.relative)
            .is_ok_and(|meta| !meta.is_dir())
    {
        to.dir
            .remove_file_or_symlink(to.relative)
            .map_err(|error| io_error(&error))?;
    }
    make_link(to.dir, to.relative, &text, posix).map_err(|error| io_error(&error))
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
    let mut finish: Vec<(PathBuf, Metadata)> = Vec::new();
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
            Err(_) => to
                .dir
                .create_dir(&target, false, None)
                .map_err(|error| io_error(&error))?,
        }
        if options.preserve {
            finish.push((target.clone(), meta));
        }
        let mut children = Vec::new();
        for entry in from
            .dir
            .read_dir(&source)
            .map_err(|error| io_error(&error))?
        {
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
                    context.source.posix,
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
        if cfg!(unix)
            && let Some(mode) = meta.mode()
        {
            to.dir
                .set_mode(&target, mode & 0o7777 & !super::ID_BITS)
                .map_err(|error| io_error(&error))?;
        }
        if let Ok(modified) = meta.modified() {
            to.dir
                .set_times(&target, None, Some(modified))
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
    let file = dir
        .open_with(&relative, OpenOptions::new().write(true))
        .map_err(|error| io_error(&error))?;
    if !file.metadata().map_err(|error| io_error(&error))?.is_file() {
        return Err(bad("the path is not a regular file"));
    }
    file.set_len(length).map_err(|error| io_error(&error))
}
