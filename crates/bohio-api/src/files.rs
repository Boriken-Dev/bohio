// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The files kind: a directory tree a source holds, reached through directory handles.
//!
//! The node's own file system and a machine reached over SFTP are the same kind. The host has already confined a
//! peer's path to its granted roots (lexically); a source opens a root as a [`Directory`] and every name below it is
//! resolved against that handle. A source that keeps a name from leaving the handle (the node's own file system
//! does, with `openat` and `RESOLVE_BENEATH` or the Windows equivalent) offers `confined`; one that resolves names
//! by path each call (SFTP) lacks it and says so in `describe`.
//!
//! A failure carries its kind as a word at the start of the message: [`failure`] writes it and [`failure_kind`]
//! reads it, so a host maps the same words to its own errors for every source.

use guatiao::Status;
use guatiao::library::{Object, ProviderError};

use crate::model::{DirEntryModel, FileStat, OpenModel, SourceInfo, TimeModel, VfsStat};
use crate::unsupported;

/// The words a failure's message starts with (`not_found: ...`).
pub mod word {
    /// The name does not exist.
    pub const NOT_FOUND: &str = "not_found";
    /// The source refused access.
    pub const DENIED: &str = "denied";
    /// The name leads out of the directory it is resolved against.
    pub const OUTSIDE: &str = "outside_root";
    /// The name exists.
    pub const EXISTS: &str = "exists";
    /// A name on the path is not a directory.
    pub const NOT_A_DIRECTORY: &str = "not_a_directory";
    /// The name is a directory.
    pub const IS_A_DIRECTORY: &str = "is_a_directory";
    /// The directory is not empty.
    pub const NOT_EMPTY: &str = "not_empty";
    /// The source rejected the name or the arguments.
    pub const INVALID: &str = "invalid";
    /// The source cannot do this.
    pub const UNSUPPORTED: &str = "unsupported";
    /// Any other failure.
    pub const IO: &str = "io";
    /// The two names are on different file systems and cannot be renamed in one step.
    pub const CROSSES_DEVICES: &str = "crosses_devices";

    /// Every word, for a reader of a message.
    pub const ALL: [&str; 11] = [
        NOT_FOUND,
        DENIED,
        OUTSIDE,
        EXISTS,
        NOT_A_DIRECTORY,
        IS_A_DIRECTORY,
        NOT_EMPTY,
        INVALID,
        UNSUPPORTED,
        IO,
        CROSSES_DEVICES,
    ];
}

/// A failure of `word` (see [`word`]) with `message` for a person.
#[must_use]
pub fn failure(word: &str, message: &str) -> ProviderError {
    let status = match word {
        word::NOT_FOUND => Status::GUATIAO_ERR_NOT_FOUND,
        word::UNSUPPORTED => Status::GUATIAO_ERR_UNSUPPORTED,
        word::IO => Status::GUATIAO_ERR_INTERNAL,
        _ => Status::GUATIAO_ERR_BAD_VALUE,
    };
    ProviderError::new(status, &format!("{word}: {message}"))
}

/// The word a failure's message starts with, when it is one of [`word`]'s.
#[must_use]
pub fn failure_kind(error: &ProviderError) -> Option<&'static str> {
    let message = error.message();
    let (head, _) = message.split_once(": ")?;
    word::ALL.into_iter().find(|known| *known == head)
}

/// The [`std::io::ErrorKind`] a failure stands for, and the failure for an error: the two directions of one table, so
/// a host and a source agree on what each word means.
#[must_use]
pub fn io_error(error: &ProviderError) -> std::io::Error {
    use std::io::{Error, ErrorKind};
    let text = error.message();
    let detail = text.split_once(": ").map_or(text, |(_, rest)| rest);
    match failure_kind(error) {
        Some(word::NOT_FOUND) => Error::new(ErrorKind::NotFound, detail),
        Some(word::DENIED) => Error::new(ErrorKind::PermissionDenied, detail),
        Some(word::OUTSIDE) => Error::new(
            ErrorKind::PermissionDenied,
            "a path led outside of the filesystem",
        ),
        Some(word::EXISTS) => Error::new(ErrorKind::AlreadyExists, detail),
        Some(word::NOT_A_DIRECTORY) => Error::new(ErrorKind::NotADirectory, detail),
        Some(word::IS_A_DIRECTORY) => Error::new(ErrorKind::IsADirectory, detail),
        Some(word::NOT_EMPTY) => Error::new(ErrorKind::DirectoryNotEmpty, detail),
        Some(word::INVALID) => Error::new(ErrorKind::InvalidInput, detail),
        Some(word::UNSUPPORTED) => Error::new(ErrorKind::Unsupported, detail),
        Some(word::CROSSES_DEVICES) => Error::new(ErrorKind::CrossesDevices, detail),
        _ if error.status == Status::GUATIAO_ERR_NOT_FOUND => Error::new(ErrorKind::NotFound, text),
        _ if error.status == Status::GUATIAO_ERR_UNSUPPORTED => {
            Error::new(ErrorKind::Unsupported, text)
        }
        _ => Error::other(text.to_owned()),
    }
}

/// The failure that stands for an operating-system error: see [`io_error`].
#[must_use]
pub fn failure_of(error: &std::io::Error) -> ProviderError {
    use std::io::ErrorKind;
    let message = error.to_string();
    let word = match error.kind() {
        ErrorKind::NotFound => word::NOT_FOUND,
        ErrorKind::PermissionDenied if message.contains("outside of the filesystem") => {
            word::OUTSIDE
        }
        ErrorKind::PermissionDenied => word::DENIED,
        ErrorKind::AlreadyExists => word::EXISTS,
        ErrorKind::NotADirectory => word::NOT_A_DIRECTORY,
        ErrorKind::IsADirectory => word::IS_A_DIRECTORY,
        ErrorKind::DirectoryNotEmpty => word::NOT_EMPTY,
        ErrorKind::InvalidInput | ErrorKind::InvalidFilename => word::INVALID,
        ErrorKind::Unsupported => word::UNSUPPORTED,
        ErrorKind::CrossesDevices => word::CROSSES_DEVICES,
        _ => word::IO,
    };
    failure(word, &message)
}

/// A source of directory trees.
#[guatiao::kind(name = "bohio_files")]
pub trait FilesSource: Send + Sync {
    /// What this source is and can do now; asked at every `info`, never cached by the host.
    ///
    /// # Errors
    ///
    /// Only a source that cannot even describe itself.
    fn describe(&self) -> Result<SourceInfo, ProviderError>;

    /// Opens the directory `path` (absolute, in the source's own syntax) as the handle names below it are resolved
    /// against.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`not_found`, `denied`, `not_a_directory`...), or `GONE` when the source is unavailable.
    fn open_dir(&self, path: &str) -> Result<Object<dyn Directory>, ProviderError>;

    /// Renames `from` below the directory `from_root` to `to` below `to_root`, two directories of this source, in
    /// one step. Only a source whose directories can share a file system (the node's own) offers it.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`crosses_devices`, `unsupported`).
    fn rename_across(
        &self,
        from_root: &str,
        from: &str,
        to_root: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), ProviderError> {
        let _ = (from_root, from, to_root, to, overwrite);
        Err(failure(
            word::UNSUPPORTED,
            "this source renames only within one directory",
        ))
    }
}

/// One open directory. A `relative` name is below it, with `.` for the directory itself; a `confined` source refuses
/// one that leaves it.
#[guatiao::kind(object, name = "bohio_directory")]
pub trait Directory: Send + Sync {
    /// A handle for the directory `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn open_dir(&self, relative: &str) -> Result<Object<dyn Directory>, ProviderError>;

    /// The attributes of `relative`; a symbolic link is followed only when `follow`.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn stat(&self, relative: &str, follow: bool) -> Result<FileStat, ProviderError>;

    /// Starts listing the directory `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn read_dir(&self, relative: &str) -> Result<Object<dyn DirReader>, ProviderError>;

    /// Opens the file `relative` as `options` say.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn open_file(
        &self,
        relative: &str,
        options: OpenModel,
    ) -> Result<Object<dyn File>, ProviderError>;

    /// Makes the directory `relative`, with `mode` where the system has modes. With `parents` the missing
    /// directories above it are made too and one that exists is not a failure.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`exists`, `not_found` for a missing parent).
    fn create_dir(
        &self,
        relative: &str,
        mode: Option<u32>,
        parents: bool,
    ) -> Result<(), ProviderError>;

    /// Removes the empty directory `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`not_empty`).
    fn remove_dir(&self, relative: &str) -> Result<(), ProviderError>;

    /// Removes the directory `relative` and everything in it, without following a link.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn remove_dir_all(&self, relative: &str) -> Result<(), ProviderError>;

    /// Removes the file or symbolic link `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn remove_file(&self, relative: &str) -> Result<(), ProviderError>;

    /// Renames `from` to `to`, both below this directory. With `overwrite` an existing `to` is replaced in one
    /// step; without it an existing `to` is `exists`.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`exists`; `unsupported` when `overwrite` cannot be done in one step and the source lacks
    /// `atomic_replace`).
    fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ProviderError>;

    /// Makes `to` another name of the file `from`.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`unsupported` for a source that lacks `hard_link`).
    fn hard_link(&self, from: &str, to: &str) -> Result<(), ProviderError> {
        let _ = (from, to);
        Err(failure(
            word::UNSUPPORTED,
            "this source makes no hard links",
        ))
    }

    /// Makes the symbolic link `link` to `target` (text, as the link holds it); `to_directory` says a Windows link
    /// is to a directory.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn symlink(&self, target: &str, link: &str, to_directory: bool) -> Result<(), ProviderError>;

    /// The text of the symbolic link `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn read_link(&self, relative: &str) -> Result<String, ProviderError>;

    /// Sets the times of `relative`; an absent one is left alone.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn set_times(
        &self,
        relative: &str,
        accessed: Option<TimeModel>,
        modified: Option<TimeModel>,
    ) -> Result<(), ProviderError>;

    /// Sets the permission bits of `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`unsupported` where the system has no modes).
    fn set_mode(&self, relative: &str, mode: u32) -> Result<(), ProviderError>;

    /// Sets or clears the read-only attribute of `relative`.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn set_readonly(&self, relative: &str, readonly: bool) -> Result<(), ProviderError>;

    /// The space of the file system `relative` is on.
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source cannot tell (it lacks `statvfs`).
    fn statvfs(&self, relative: &str) -> Result<VfsStat, ProviderError> {
        let _ = relative;
        Err(unsupported(
            "this source cannot tell the space of a file system",
        ))
    }
}

/// A listing in progress.
#[guatiao::kind(object, name = "bohio_dir_reader")]
pub trait DirReader: Send + Sync {
    /// The next entries, at most `max` of them, in no order. An empty answer is the end.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn next(&self, max: u64) -> Result<Vec<DirEntryModel>, ProviderError>;
}

/// An open file. Reads and writes carry their offset, so one handle serves threads that read different places.
#[guatiao::kind(object, name = "bohio_file")]
pub trait File: Send + Sync {
    /// Reads at `offset` into `dst` and answers the bytes read; fewer than `dst` only at the end, `0` at it.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<u64, ProviderError>;

    /// Writes `data` at `offset` and answers the bytes written, all of them unless the file system is full.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<u64, ProviderError>;

    /// Sets the file's length, cutting it or extending it with zeros.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn set_len(&self, len: u64) -> Result<(), ProviderError>;

    /// Waits until what was written is on the medium, when the source can say so.
    ///
    /// # Errors
    ///
    /// A [`failure`]; `unsupported` for a source that lacks `sync`.
    fn sync(&self) -> Result<(), ProviderError>;

    /// The file's attributes.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn stat(&self) -> Result<FileStat, ProviderError>;

    /// Sets the file's modification time.
    ///
    /// # Errors
    ///
    /// A [`failure`].
    fn set_modified(&self, time: TimeModel) -> Result<(), ProviderError>;

    /// Sets the file's permission bits.
    ///
    /// # Errors
    ///
    /// A [`failure`] (`unsupported` where the system has no modes).
    fn set_mode(&self, mode: u32) -> Result<(), ProviderError>;
}
