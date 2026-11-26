//! The `native` provider's files: this node's own file system, through directory handles.
//!
//! A root is opened by its path as a `cap-std` directory; every name below it is resolved against that handle and
//! cannot leave it (`openat2` with `RESOLVE_BENEATH` on Linux, a walk of `openat` with `O_NOFOLLOW` elsewhere on Unix,
//! relative `NtCreateFile` calls on Windows). A directory opened below a handle is a handle of its own, so a caller
//! that keeps one is not affected by what is renamed above it.

#![forbid(unsafe_code)]

use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_api::files::{DirReader, Directory, File, FilesSource, failure, failure_of, word};
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{DirEntryModel, FileStat, OpenModel, SourceInfo, TimeModel};
use bohio_api::vocab::{self, file_kind};
use cap_fs_ext::{DirExt, SystemTimeSpec};
use cap_std::fs::Dir;

use super::native::Native;
use crate::fs::native;

fn failed(error: &io::Error) -> ProviderError {
    failure_of(error)
}

fn bad_text(what: &str) -> ProviderError {
    failure(word::INVALID, what)
}

fn dot(relative: &str) -> &Path {
    if relative.is_empty() {
        Path::new(".")
    } else {
        Path::new(relative)
    }
}

fn stamp(time: cap_std::time::SystemTime) -> TimeModel {
    match time.into_std().duration_since(UNIX_EPOCH) {
        Ok(after) => TimeModel {
            secs: i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            nanos: after.subsec_nanos(),
        },
        Err(error) => {
            let before = error.duration();
            let whole = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            match before.subsec_nanos() {
                0 => TimeModel {
                    secs: -whole,
                    nanos: 0,
                },
                nanos => TimeModel {
                    secs: -whole - 1,
                    nanos: 1_000_000_000 - nanos,
                },
            }
        }
    }
}

fn moment(time: TimeModel) -> Result<SystemTime, ProviderError> {
    let moved = if time.nanos >= 1_000_000_000 {
        None
    } else if time.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(time.secs.unsigned_abs(), time.nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(time.secs.unsigned_abs()))
            .and_then(|before| before.checked_add(Duration::from_nanos(u64::from(time.nanos))))
    };
    moved.ok_or_else(|| bad_text("the time is out of range"))
}

/// What a file is, as the engines report it. `by_handle` says the attributes were read from an open file, which is
/// what Windows needs to know the volume and the file index.
pub(crate) fn stat_of(meta: &cap_std::fs::Metadata, by_handle: bool) -> FileStat {
    let kind = if meta.is_file() {
        file_kind::FILE
    } else if meta.is_dir() {
        file_kind::DIR
    } else if meta.is_symlink() {
        file_kind::SYMLINK
    } else {
        file_kind::OTHER
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
    #[cfg(unix)]
    let (device, inode, links) = {
        use cap_std::fs::MetadataExt;
        (Some(meta.dev()), Some(meta.ino()), Some(meta.nlink()))
    };
    #[cfg(windows)]
    let (device, inode, links) = if by_handle {
        use cap_fs_ext::MetadataExt;
        (Some(meta.dev()), Some(meta.ino()), Some(meta.nlink()))
    } else {
        (None, None, None)
    };
    #[cfg(not(any(unix, windows)))]
    let (device, inode, links) = (None::<u64>, None::<u64>, None::<u64>);
    #[cfg(unix)]
    let owner = {
        use cap_std::fs::MetadataExt;
        Some(u64::from(meta.uid()))
    };
    #[cfg(not(unix))]
    let owner = None::<u64>;
    #[cfg(unix)]
    let _ = by_handle;
    FileStat {
        kind: kind.to_owned(),
        size: meta.len(),
        modified: meta.modified().ok().map(stamp),
        accessed: meta.accessed().ok().map(stamp),
        created: meta.created().ok().map(stamp),
        mode,
        readonly: permissions.readonly(),
        attributes,
        device,
        inode,
        links,
        owner,
    }
}

impl FilesSource for Native {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let mut offers = vec![
            vocab::files::READ,
            vocab::files::WRITE,
            vocab::files::CONFINED,
            vocab::files::SYMLINK,
            vocab::files::HARD_LINK,
            vocab::files::SET_TIMES,
            vocab::files::ATOMIC_REPLACE,
            vocab::files::SYNC,
        ];
        let mut lacks = vec![vocab::files::STATVFS];
        if cfg!(unix) {
            offers.push(vocab::files::SET_MODE);
            lacks.push(vocab::files::SET_READONLY);
        } else {
            offers.push(vocab::files::SET_READONLY);
            lacks.push(vocab::files::SET_MODE);
        }
        lacks.push(vocab::files::LIST_STATS);
        Ok(SourceInfo::new(super::native::ID, &offers, &lacks))
    }

    fn open_dir(&self, path: &str) -> Result<Object<dyn Directory>, ProviderError> {
        let dir = Dir::open_ambient_dir(path, cap_std::ambient_authority())
            .map_err(|error| failed(&error))?;
        Ok(NativeDir {
            dir,
            chunk: usize::try_from(self.list_chunk()).unwrap_or(usize::MAX),
        }
        .into_object())
    }

    fn rename_across(
        &self,
        from_root: &str,
        from: &str,
        to_root: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), ProviderError> {
        let open = |path: &str| {
            Dir::open_ambient_dir(path, cap_std::ambient_authority())
                .map_err(|error| failed(&error))
        };
        let (from_dir, to_dir) = (open(from_root)?, open(to_root)?);
        rename_between(
            &from_dir,
            Path::new(from),
            &to_dir,
            Path::new(to),
            overwrite,
        )
        .map_err(|error| failed(&error))
    }
}

struct NativeDir {
    dir: Dir,
    chunk: usize,
}

impl Directory for NativeDir {
    fn open_dir(&self, relative: &str) -> Result<Object<dyn Directory>, ProviderError> {
        let dir = self
            .dir
            .open_dir(dot(relative))
            .map_err(|error| failed(&error))?;
        Ok(Self {
            dir,
            chunk: self.chunk,
        }
        .into_object())
    }

    fn stat(&self, relative: &str, follow: bool) -> Result<FileStat, ProviderError> {
        let relative = dot(relative);
        let meta = if follow {
            self.dir.metadata(relative)
        } else {
            self.dir.symlink_metadata(relative)
        }
        .map_err(|error| failed(&error))?;
        Ok(stat_of(&meta, false))
    }

    fn read_dir(&self, relative: &str) -> Result<Object<dyn DirReader>, ProviderError> {
        let read = native::read_dir(&self.dir, dot(relative)).map_err(|error| failed(&error))?;
        Ok(Reader {
            read: Mutex::new(read),
            chunk: self.chunk.max(1),
        }
        .into_object())
    }

    fn open_file(
        &self,
        relative: &str,
        options: OpenModel,
    ) -> Result<Object<dyn File>, ProviderError> {
        let mut open = cap_std::fs::OpenOptions::new();
        open.read(options.read)
            .write(options.write)
            .append(options.append)
            .create(options.create)
            .create_new(options.create_new)
            .truncate(options.truncate);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            if let Some(bits) = options.mode {
                open.mode(bits & 0o7777);
            }
            // A named pipe opened for reading would wait for a writer for good, holding the caller's slot: open without
            // waiting, and refuse what is not a regular file below.
            open.custom_flags(rustix::fs::OFlags::NONBLOCK.bits().cast_signed());
        }
        let file = self
            .dir
            .open_with(Path::new(relative), &open)
            .map_err(|error| failed(&error))?;
        #[cfg(unix)]
        if !file
            .metadata()
            .map_err(|error| failed(&error))?
            .file_type()
            .is_file()
        {
            return Err(bad_text("not a regular file"));
        }
        Ok(Handle {
            file: file.into_std(),
            append: options.append,
        }
        .into_object())
    }

    fn create_dir(
        &self,
        relative: &str,
        mode: Option<u32>,
        parents: bool,
    ) -> Result<(), ProviderError> {
        native::create_dir(
            &self.dir,
            dot(relative),
            parents,
            #[cfg(unix)]
            mode,
        )
        .map_err(|error| failed(&error))?;
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }

    fn remove_dir(&self, relative: &str) -> Result<(), ProviderError> {
        native::remove_dir(&self.dir, Path::new(relative)).map_err(|error| failed(&error))
    }

    fn remove_dir_all(&self, relative: &str) -> Result<(), ProviderError> {
        native::remove_dir_all(&self.dir, Path::new(relative)).map_err(|error| failed(&error))
    }

    fn remove_file(&self, relative: &str) -> Result<(), ProviderError> {
        native::remove_file_or_symlink(&self.dir, Path::new(relative))
            .map_err(|error| failed(&error))
    }

    fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ProviderError> {
        rename_between(
            &self.dir,
            Path::new(from),
            &self.dir,
            Path::new(to),
            overwrite,
        )
        .map_err(|error| failed(&error))
    }

    fn hard_link(&self, from: &str, to: &str) -> Result<(), ProviderError> {
        self.dir
            .hard_link(Path::new(from), &self.dir, Path::new(to))
            .map_err(|error| failed(&error))
    }

    fn symlink(&self, target: &str, link: &str, to_directory: bool) -> Result<(), ProviderError> {
        #[cfg(not(windows))]
        {
            let _ = to_directory;
            self.dir
                .symlink(target, Path::new(link))
                .map_err(|error| failed(&error))
        }
        #[cfg(windows)]
        native::symlink(
            &self.dir,
            Path::new(link),
            std::ffi::OsStr::new(target),
            to_directory,
        )
        .map_err(|error| failed(&error))
    }

    fn read_link(&self, relative: &str) -> Result<String, ProviderError> {
        let target =
            native::read_link(&self.dir, Path::new(relative)).map_err(|error| failed(&error))?;
        target.into_os_string().into_string().map_err(|_| {
            failure(
                word::IO,
                "the link's text is not valid text and cannot be sent",
            )
        })
    }

    fn set_times(
        &self,
        relative: &str,
        accessed: Option<TimeModel>,
        modified: Option<TimeModel>,
    ) -> Result<(), ProviderError> {
        let spec = |time: Option<TimeModel>| -> Result<Option<SystemTimeSpec>, ProviderError> {
            time.map(|time| {
                moment(time)
                    .map(|time| SystemTimeSpec::Absolute(cap_std::time::SystemTime::from_std(time)))
            })
            .transpose()
        };
        self.dir
            .set_times(Path::new(relative), spec(accessed)?, spec(modified)?)
            .map_err(|error| failed(&error))
    }

    #[cfg(unix)]
    fn set_mode(&self, relative: &str, mode: u32) -> Result<(), ProviderError> {
        use cap_std::fs::{Permissions, PermissionsExt};
        self.dir
            .set_permissions(Path::new(relative), Permissions::from_mode(mode))
            .map_err(|error| failed(&error))
    }

    #[cfg(not(unix))]
    fn set_mode(&self, relative: &str, mode: u32) -> Result<(), ProviderError> {
        let _ = (relative, mode);
        Err(failure(
            word::UNSUPPORTED,
            "this host has no permission bits",
        ))
    }

    #[cfg(windows)]
    fn set_readonly(&self, relative: &str, readonly: bool) -> Result<(), ProviderError> {
        native::set_readonly(&self.dir, Path::new(relative), readonly)
            .map_err(|error| failed(&error))
    }

    #[cfg(not(windows))]
    fn set_readonly(&self, relative: &str, readonly: bool) -> Result<(), ProviderError> {
        let _ = (relative, readonly);
        Err(failure(
            word::UNSUPPORTED,
            "this host has no file attributes",
        ))
    }
}

/// Moves `from` of `from_dir` to `to` of `to_dir`: over what is there with `overwrite`, and otherwise failing with
/// `AlreadyExists`, which the operating system decides in the rename itself where it has such a call.
///
/// Linux and macOS use `renameat2` (`RENAME_NOREPLACE`) and `renamex_np` (`RENAME_EXCL`), Windows `MoveFileExW`
/// without `MOVEFILE_REPLACE_EXISTING`. Only where there is no such call (FreeBSD and other Unix) or the filesystem
/// refuses the flag (`EINVAL`) is there a gap: a regular file is then given its new name with a hard link, which
/// fails when the name exists, and the old name is removed; a directory, or a file on a filesystem without hard
/// links, is checked first and renamed after, so a name made in between is replaced.
fn rename_between(
    from_dir: &Dir,
    from: &Path,
    to_dir: &Dir,
    to: &Path,
    overwrite: bool,
) -> io::Result<()> {
    if overwrite {
        return native::rename_replacing(from_dir, from, to_dir, to);
    }
    rename_new(from_dir, from, to_dir, to)
}

/// The directory holding `relative` and its last name, both opened under `dir` without leaving it.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn parent_of<'a>(dir: &Dir, relative: &'a Path) -> io::Result<(Dir, &'a std::ffi::OsStr)> {
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

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn rename_new(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> io::Result<()> {
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
fn rename_new(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> io::Result<()> {
    native::rename(from_dir, from, to_dir, to, false)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn rename_new(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> io::Result<()> {
    link_then_remove(from_dir, from, to_dir, to)
}

/// The fallback: a hard link for a regular file, else check then rename.
#[cfg(not(windows))]
fn link_then_remove(from_dir: &Dir, from: &Path, to_dir: &Dir, to: &Path) -> io::Result<()> {
    if from_dir.symlink_metadata(from)?.is_file() {
        match from_dir.hard_link(from, to_dir, to) {
            Ok(()) => return from_dir.remove_file(from),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Err(error),
            // No hard links here (or none to this name): the check below is all there is.
            Err(_) => {}
        }
    }
    if to_dir.symlink_metadata(to).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    from_dir.rename(from, to_dir, to)
}

/// A listing in progress.
struct Reader {
    read: Mutex<native::ReadDir>,
    chunk: usize,
}

impl DirReader for Reader {
    fn next(&self, max: u64) -> Result<Vec<DirEntryModel>, ProviderError> {
        let take = usize::try_from(max).unwrap_or(usize::MAX).min(self.chunk);
        let mut read = self
            .read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = Vec::new();
        while entries.len() < take {
            let Some(entry) = read.next() else { break };
            // An entry that cannot be read is left out, as a listing that cannot see it would.
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            entries.push(match name.into_string() {
                Ok(name) => DirEntryModel {
                    name,
                    stat: None,
                    unnamed: false,
                },
                Err(name) => DirEntryModel {
                    name: name.to_string_lossy().into_owned(),
                    stat: None,
                    unnamed: true,
                },
            });
        }
        Ok(entries)
    }
}

/// An open file.
struct Handle {
    file: std::fs::File,
    append: bool,
}

impl Handle {
    fn meta(&self) -> io::Result<cap_std::fs::Metadata> {
        cap_std::fs::File::from_std(self.file.try_clone()?).metadata()
    }
}

impl File for Handle {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<u64, ProviderError> {
        #[cfg(unix)]
        let count = {
            use std::os::unix::fs::FileExt;
            loop {
                match self.file.read_at(dst, offset) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    other => break other,
                }
            }
        };
        #[cfg(windows)]
        let count = {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(dst, offset)
        };
        count
            .map(|count| count as u64)
            .map_err(|error| failed(&error))
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<u64, ProviderError> {
        use std::io::Write;
        if self.append {
            return (&self.file)
                .write(data)
                .map(|count| count as u64)
                .map_err(|error| failed(&error));
        }
        #[cfg(unix)]
        let count = {
            use std::os::unix::fs::FileExt;
            self.file.write_at(data, offset)
        };
        #[cfg(windows)]
        let count = {
            use std::os::windows::fs::FileExt;
            self.file.seek_write(data, offset)
        };
        count
            .map(|count| count as u64)
            .map_err(|error| failed(&error))
    }

    fn set_len(&self, len: u64) -> Result<(), ProviderError> {
        self.file.set_len(len).map_err(|error| failed(&error))
    }

    fn sync(&self) -> Result<(), ProviderError> {
        self.file.sync_all().map_err(|error| failed(&error))
    }

    fn stat(&self) -> Result<FileStat, ProviderError> {
        self.meta()
            .map(|meta| stat_of(&meta, true))
            .map_err(|error| failed(&error))
    }

    fn set_modified(&self, time: TimeModel) -> Result<(), ProviderError> {
        self.file
            .set_modified(moment(time)?)
            .map_err(|error| failed(&error))
    }

    #[cfg(unix)]
    fn set_mode(&self, mode: u32) -> Result<(), ProviderError> {
        use std::os::unix::fs::PermissionsExt;
        self.file
            .set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|error| failed(&error))
    }

    #[cfg(not(unix))]
    fn set_mode(&self, mode: u32) -> Result<(), ProviderError> {
        let _ = mode;
        Err(failure(
            word::UNSUPPORTED,
            "this host has no permission bits",
        ))
    }
}
