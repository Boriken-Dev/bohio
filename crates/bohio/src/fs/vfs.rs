//! The directory handles, files and attributes the engines work on, over a `FilesSource`.
//!
//! These types have the shape of the `cap-std` ones the engines were written against: a [`Dir`] resolves every name
//! below it, a [`File`] is a cursor over positional reads and writes, a [`Metadata`] answers `is_file` and `len`.
//! Whatever lies behind them is the source's: the node's own file system keeps its directory-handle safety inside
//! its provider, and a source that resolves names by path says so in its `describe`.

#![forbid(unsafe_code)]

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_api::files::{DirReader, Directory, File as FileObject, FilesSource, io_error};
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{FileStat, OpenModel, TimeModel};
use bohio_api::vocab::file_kind;

/// The source a root is opened from, and where.
#[derive(Clone)]
struct Origin {
    files: Arc<dyn FilesSource>,
    root: String,
}

/// An open directory: every name below it is resolved against it.
#[derive(Clone)]
pub(crate) struct Dir {
    handle: Arc<Object<dyn Directory>>,
    /// Set on a root, which is what a rename between two roots names.
    origin: Option<Origin>,
}

impl core::fmt::Debug for Dir {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dir").finish_non_exhaustive()
    }
}

/// The text of a path handed to a source: names are text, in the node's own separators.
fn text(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a name is not text"))
}

fn dot(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

fn failed(error: &ProviderError) -> io::Error {
    io_error(error)
}

impl Dir {
    /// Opens the root `path` of `files`.
    pub(crate) fn open_root(files: &Arc<dyn FilesSource>, path: &Path) -> io::Result<Self> {
        let root = text(path)?;
        let handle = files.open_dir(root).map_err(|error| failed(&error))?;
        Ok(Self {
            handle: Arc::new(handle),
            origin: Some(Origin {
                files: Arc::clone(files),
                root: root.to_owned(),
            }),
        })
    }

    /// A handle for the directory `relative`.
    pub(crate) fn open_dir(&self, relative: &Path) -> io::Result<Self> {
        let handle = self
            .handle
            .open_dir(text(dot(relative))?)
            .map_err(|error| failed(&error))?;
        Ok(Self {
            handle: Arc::new(handle),
            origin: None,
        })
    }

    /// Another handle for the same directory.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }

    /// The attributes of `relative`, a link followed.
    pub(crate) fn metadata(&self, relative: impl AsRef<Path>) -> io::Result<Metadata> {
        self.stat(relative.as_ref(), true)
    }

    /// The attributes of `relative`, a link not followed.
    pub(crate) fn symlink_metadata(&self, relative: impl AsRef<Path>) -> io::Result<Metadata> {
        self.stat(relative.as_ref(), false)
    }

    fn stat(&self, relative: &Path, follow: bool) -> io::Result<Metadata> {
        self.handle
            .stat(text(dot(relative))?, follow)
            .map(Metadata)
            .map_err(|error| failed(&error))
    }

    /// The entries of the directory `relative`.
    pub(crate) fn read_dir(&self, relative: &Path) -> io::Result<ReadDir> {
        let relative = dot(relative);
        let reader = self
            .handle
            .read_dir(text(relative)?)
            .map_err(|error| failed(&error))?;
        Ok(ReadDir {
            reader,
            dir: self.clone(),
            relative: relative.to_path_buf(),
            ready: Vec::new().into_iter(),
            done: false,
        })
    }

    /// Opens the file `relative` for reading.
    pub(crate) fn open(&self, relative: &Path) -> io::Result<File> {
        self.open_with(relative, OpenOptions::new().read(true))
    }

    /// Opens the file `relative` as `options` say.
    pub(crate) fn open_with(&self, relative: &Path, options: &OpenOptions) -> io::Result<File> {
        let handle = self
            .handle
            .open_file(text(relative)?, options.0.clone())
            .map_err(|error| failed(&error))?;
        Ok(File {
            handle,
            position: 0,
        })
    }

    /// Makes the directory `relative`, and with `parents` the ones above it that are missing.
    pub(crate) fn create_dir(
        &self,
        relative: &Path,
        parents: bool,
        mode: Option<u32>,
    ) -> io::Result<()> {
        self.handle
            .create_dir(text(relative)?, mode, parents)
            .map_err(|error| failed(&error))
    }

    /// Removes the empty directory `relative`.
    pub(crate) fn remove_dir(&self, relative: &Path) -> io::Result<()> {
        self.handle
            .remove_dir(text(relative)?)
            .map_err(|error| failed(&error))
    }

    /// Removes the directory `relative` and what it holds.
    pub(crate) fn remove_dir_all(&self, relative: &Path) -> io::Result<()> {
        self.handle
            .remove_dir_all(text(relative)?)
            .map_err(|error| failed(&error))
    }

    /// Removes the file or link `relative`.
    pub(crate) fn remove_file_or_symlink(&self, relative: &Path) -> io::Result<()> {
        self.handle
            .remove_file(text(relative)?)
            .map_err(|error| failed(&error))
    }

    /// Renames `from` to `to`, both below this directory.
    pub(crate) fn rename(&self, from: &Path, to: &Path, overwrite: bool) -> io::Result<()> {
        self.handle
            .rename(text(from)?, text(to)?, overwrite)
            .map_err(|error| failed(&error))
    }

    /// Renames `from` of this directory to `to` of `to_dir`: in one step when they are one handle's names or two
    /// roots of one source, and `crosses_devices` otherwise.
    pub(crate) fn rename_to(
        &self,
        from: &Path,
        to_dir: &Self,
        to: &Path,
        overwrite: bool,
    ) -> io::Result<()> {
        match (&self.origin, &to_dir.origin) {
            (Some(here), Some(there)) if here.root == there.root => {
                self.rename(from, to, overwrite)
            }
            (Some(here), Some(there)) if Arc::ptr_eq(&here.files, &there.files) => here
                .files
                .rename_across(&here.root, text(from)?, &there.root, text(to)?, overwrite)
                .map_err(|error| failed(&error)),
            _ if Arc::ptr_eq(&self.handle, &to_dir.handle) => self.rename(from, to, overwrite),
            _ => Err(io::ErrorKind::CrossesDevices.into()),
        }
    }

    /// Makes `link` a symbolic link to `target`; `to_directory` marks a link to a directory where that matters.
    pub(crate) fn symlink(&self, target: &str, link: &Path, to_directory: bool) -> io::Result<()> {
        self.handle
            .symlink(target, text(link)?, to_directory)
            .map_err(|error| failed(&error))
    }

    /// The text of the link `relative`.
    pub(crate) fn read_link(&self, relative: &Path) -> io::Result<String> {
        self.handle
            .read_link(text(relative)?)
            .map_err(|error| failed(&error))
    }

    /// Sets the times of `relative`; an absent one is left alone.
    pub(crate) fn set_times(
        &self,
        relative: &Path,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> io::Result<()> {
        self.handle
            .set_times(
                text(relative)?,
                accessed.map(time_model),
                modified.map(time_model),
            )
            .map_err(|error| failed(&error))
    }

    /// Sets the permission bits of `relative`.
    pub(crate) fn set_mode(&self, relative: &Path, mode: u32) -> io::Result<()> {
        self.handle
            .set_mode(text(relative)?, mode)
            .map_err(|error| failed(&error))
    }

    /// Sets or clears the read-only attribute of `relative`.
    pub(crate) fn set_readonly(&self, relative: &Path, readonly: bool) -> io::Result<()> {
        self.handle
            .set_readonly(text(relative)?, readonly)
            .map_err(|error| failed(&error))
    }
}

/// A moment as the source states it.
pub(crate) fn time_model(time: SystemTime) -> TimeModel {
    match time.duration_since(UNIX_EPOCH) {
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

/// The moment a source stated.
pub(crate) fn system_time(time: TimeModel) -> Option<SystemTime> {
    if time.nanos >= 1_000_000_000 {
        return None;
    }
    if time.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(time.secs.unsigned_abs(), time.nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(time.secs.unsigned_abs()))
            .and_then(|before| before.checked_add(Duration::from_nanos(u64::from(time.nanos))))
    }
}

/// What a file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Metadata(pub(crate) FileStat);

impl Metadata {
    pub(crate) fn is_file(&self) -> bool {
        self.0.kind == file_kind::FILE
    }

    pub(crate) fn is_dir(&self) -> bool {
        self.0.kind == file_kind::DIR
    }

    pub(crate) fn is_symlink(&self) -> bool {
        self.0.kind == file_kind::SYMLINK
    }

    pub(crate) fn len(&self) -> u64 {
        self.0.size
    }

    pub(crate) fn modified(&self) -> io::Result<SystemTime> {
        self.0.modified.and_then(system_time).ok_or_else(unknown)
    }

    /// The permission bits, where the source has them.
    pub(crate) fn mode(&self) -> Option<u32> {
        self.0.mode
    }
}

fn unknown() -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, "the source does not say")
}

/// How a file is opened: the shape of `std::fs::OpenOptions`.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenOptions(OpenModel);

impl OpenOptions {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn read(&mut self, on: bool) -> &mut Self {
        self.0.read = on;
        self
    }

    pub(crate) fn write(&mut self, on: bool) -> &mut Self {
        self.0.write = on;
        self
    }

    pub(crate) fn append(&mut self, on: bool) -> &mut Self {
        self.0.append = on;
        self.0.write = self.0.write || on;
        self
    }

    pub(crate) fn create(&mut self, on: bool) -> &mut Self {
        self.0.create = on;
        self
    }

    pub(crate) fn create_new(&mut self, on: bool) -> &mut Self {
        self.0.create_new = on;
        self
    }

    /// The permission bits of a file made.
    pub(crate) fn mode(&mut self, bits: u32) -> &mut Self {
        self.0.mode = Some(bits);
        self
    }
}

/// A listing: names first, each entry's attributes when the source has them.
pub(crate) struct ReadDir {
    reader: Object<dyn DirReader>,
    dir: Dir,
    relative: PathBuf,
    ready: std::vec::IntoIter<bohio_api::model::DirEntryModel>,
    done: bool,
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(entry) = self.ready.next() {
                return Some(Ok(DirEntry {
                    name: entry.name,
                    unnamed: entry.unnamed,
                    stat: entry.stat,
                    dir: self.dir.clone(),
                    relative: self.relative.clone(),
                }));
            }
            if self.done {
                return None;
            }
            match self.reader.next(u64::MAX) {
                Ok(entries) if entries.is_empty() => {
                    self.done = true;
                    return None;
                }
                Ok(entries) => self.ready = entries.into_iter(),
                Err(error) => {
                    self.done = true;
                    return Some(Err(failed(&error)));
                }
            }
        }
    }
}

/// One entry of a listing.
pub(crate) struct DirEntry {
    name: String,
    unnamed: bool,
    stat: Option<FileStat>,
    dir: Dir,
    relative: PathBuf,
}

impl DirEntry {
    /// The name; one that is not text is an `OsString` that cannot be turned into a `String`.
    pub(crate) fn file_name(&self) -> std::ffi::OsString {
        if self.unnamed {
            return not_text();
        }
        std::ffi::OsString::from(&self.name)
    }

    /// What the entry is, a link not followed.
    pub(crate) fn metadata(&self) -> io::Result<Metadata> {
        match &self.stat {
            Some(stat) => Ok(Metadata(stat.clone())),
            None => self.dir.symlink_metadata(self.relative.join(&self.name)),
        }
    }
}

#[cfg(unix)]
fn not_text() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(vec![0xff])
}

#[cfg(windows)]
fn not_text() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    std::ffi::OsString::from_wide(&[0xd800])
}

/// An open file: a cursor over the source's positional reads and writes.
pub(crate) struct File {
    handle: Object<dyn FileObject>,
    position: u64,
}

impl core::fmt::Debug for File {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("File")
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl File {
    /// Reads at `offset` into `dst` without moving the cursor.
    pub(crate) fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        let count = self
            .handle
            .read_at(offset, dst)
            .map_err(|error| failed(&error))?;
        Ok(usize::try_from(count).unwrap_or(dst.len()).min(dst.len()))
    }

    /// The file's attributes.
    pub(crate) fn metadata(&self) -> io::Result<Metadata> {
        self.handle
            .stat()
            .map(Metadata)
            .map_err(|error| failed(&error))
    }

    /// Cuts the file to `len` or extends it with zeros.
    pub(crate) fn set_len(&self, len: u64) -> io::Result<()> {
        self.handle.set_len(len).map_err(|error| failed(&error))
    }

    /// Makes what was written durable.
    pub(crate) fn sync_all(&self) -> io::Result<()> {
        self.handle.sync().map_err(|error| failed(&error))
    }

    /// Sets the permission bits.
    pub(crate) fn set_mode(&self, mode: u32) -> io::Result<()> {
        self.handle.set_mode(mode).map_err(|error| failed(&error))
    }
}

impl Read for File {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let count = self
            .handle
            .read_at(self.position, buffer)
            .map_err(|error| failed(&error))?;
        let count = usize::try_from(count)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        self.position += count as u64;
        Ok(count)
    }
}

impl Write for File {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let count = self
            .handle
            .write_at(self.position, data)
            .map_err(|error| failed(&error))?;
        let count = usize::try_from(count).unwrap_or(data.len()).min(data.len());
        if count == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        self.position += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for File {
    fn seek(&mut self, target: SeekFrom) -> io::Result<u64> {
        let next = match target {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::Current(by) => self.position.checked_add_signed(by),
            SeekFrom::End(by) => self.metadata()?.len().checked_add_signed(by),
        };
        let next = next.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "a seek before the start")
        })?;
        self.position = next;
        Ok(next)
    }
}
