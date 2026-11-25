//! The calls that list a directory, make, remove, rename and link a name, and set an attribute, each on a
//! directory handle: a name below `dir` is reached by opening the directory that holds it under `dir`
//! (which cannot leave it) and acting on the last name through that handle.
//!
//! Where the library does this itself (`openat`, `*at`) these are its calls. On Windows the library turns
//! the parent's handle into a path and acts on the path, so they are `platform::winfs`, which uses the
//! handle for the call itself.

#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;

/// One entry of a directory listing: its name, and its own metadata (a link is not followed).
pub(crate) struct DirEntry {
    #[cfg(not(windows))]
    inner: cap_std::fs::DirEntry,
    #[cfg(windows)]
    name: std::ffi::OsString,
}

impl DirEntry {
    pub(crate) fn file_name(&self) -> std::ffi::OsString {
        #[cfg(not(windows))]
        {
            self.inner.file_name()
        }
        #[cfg(windows)]
        {
            self.name.clone()
        }
    }
}

/// The entries of a directory, in no order.
pub(crate) struct ReadDir {
    #[cfg(not(windows))]
    inner: cap_std::fs::ReadDir,
    #[cfg(windows)]
    inner: crate::platform::winfs::Listing<std::sync::Arc<Dir>>,
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    #[cfg(not(windows))]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|entry| entry.map(|inner| DirEntry { inner }))
    }

    #[cfg(windows)]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner
            .next()
            .map(|entry| entry.map(|(name, _)| DirEntry { name }))
    }
}

/// The directory holding `relative` and its last name, both opened under `dir` without leaving it.
#[cfg(windows)]
fn split<'a>(dir: &Dir, relative: &'a Path) -> io::Result<(Dir, &'a std::ffi::OsStr)> {
    let name = relative
        .file_name()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let parent = match relative
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        Some(parent) => dir.open_dir(parent)?,
        None => dir.try_clone()?,
    };
    Ok((parent, name))
}

/// The entries of the directory `relative` (`.` for `dir` itself).
pub(crate) fn read_dir(dir: &Dir, relative: &Path) -> io::Result<ReadDir> {
    #[cfg(not(windows))]
    {
        dir.read_dir(relative).map(|inner| ReadDir { inner })
    }
    #[cfg(windows)]
    {
        // A handle of its own, so the position of the listing is nobody else's.
        let opened = std::sync::Arc::new(dir.open_dir(relative)?);
        Ok(ReadDir {
            inner: crate::platform::winfs::Listing::new(opened),
        })
    }
}

/// The text of the link `relative`.
pub(crate) fn read_link(dir: &Dir, relative: &Path) -> io::Result<PathBuf> {
    #[cfg(not(windows))]
    {
        dir.read_link_contents(relative)
    }
    #[cfg(windows)]
    {
        let (parent, name) = split(dir, relative)?;
        crate::platform::winfs::read_link(&parent, name)
    }
}

/// Makes the directory `relative`, and with `parents` the ones above it that are missing.
pub(crate) fn create_dir(
    dir: &Dir,
    relative: &Path,
    parents: bool,
    #[cfg(unix)] mode: Option<u32>,
) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        use cap_std::fs::DirBuilder;
        let mut builder = DirBuilder::new();
        builder.recursive(parents);
        #[cfg(unix)]
        if let Some(bits) = mode {
            use cap_std::fs::DirBuilderExt;
            builder.mode(bits & 0o7777);
        }
        dir.create_dir_with(relative, &builder)
    }
    #[cfg(windows)]
    {
        if !parents {
            let (parent, name) = split(dir, relative)?;
            return crate::platform::winfs::create_dir(&parent, name);
        }
        let mut current = dir.try_clone()?;
        for component in relative.components() {
            let name = component.as_os_str();
            match crate::platform::winfs::create_dir(&current, name) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            current = current.open_dir(name)?;
        }
        Ok(())
    }
}

/// Removes the empty directory `relative`.
pub(crate) fn remove_dir(dir: &Dir, relative: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        dir.remove_dir(relative)
    }
    #[cfg(windows)]
    {
        let (parent, name) = split(dir, relative)?;
        crate::platform::winfs::remove(&parent, name, crate::platform::winfs::Remove::Directory)
    }
}

/// Removes the directory `relative` and what it holds (a link is removed and not entered).
pub(crate) fn remove_dir_all(dir: &Dir, relative: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        dir.remove_dir_all(relative)
    }
    #[cfg(windows)]
    {
        let (parent, name) = split(dir, relative)?;
        crate::platform::winfs::remove_tree(&parent, name)
    }
}

/// Removes the file or link `relative`.
pub(crate) fn remove_file_or_symlink(dir: &Dir, relative: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        dir.remove_file_or_symlink(relative)
    }
    #[cfg(windows)]
    {
        let (parent, name) = split(dir, relative)?;
        crate::platform::winfs::remove(&parent, name, crate::platform::winfs::Remove::FileOrLink)
    }
}

/// Moves `from` of `from_dir` to `to` of `to_dir` in one call, replacing what is there only with `replace`,
/// where the system has such a call.
#[cfg(windows)]
pub(crate) fn rename(
    from_dir: &Dir,
    from: &Path,
    to_dir: &Dir,
    to: &Path,
    replace: bool,
) -> io::Result<()> {
    let (from_parent, from_name) = split(from_dir, from)?;
    let (to_parent, to_name) = split(to_dir, to)?;
    crate::platform::winfs::rename(&from_parent, from_name, &to_parent, to_name, replace)
}

/// Moves `from` to `to`, replacing what is there.
pub(crate) fn rename_replacing(
    from_dir: &Dir,
    from: &Path,
    to_dir: &Dir,
    to: &Path,
) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        from_dir.rename(from, to_dir, to)
    }
    #[cfg(windows)]
    {
        rename(from_dir, from, to_dir, to, true)
    }
}

/// The link `relative` made to `target`, a directory link with `directory`.
#[cfg(windows)]
pub(crate) fn symlink(
    dir: &Dir,
    relative: &Path,
    target: &std::ffi::OsStr,
    directory: bool,
) -> io::Result<()> {
    let (parent, name) = split(dir, relative)?;
    crate::platform::winfs::symlink(&parent, name, target, directory)
}

/// The read-only attribute of `relative` set or cleared.
#[cfg(windows)]
pub(crate) fn set_readonly(dir: &Dir, relative: &Path, readonly: bool) -> io::Result<()> {
    let (parent, name) = split(dir, relative)?;
    crate::platform::winfs::set_readonly(&parent, name, readonly)
}
