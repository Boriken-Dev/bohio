//! A file being written: the target, or a temporary sibling that replaces it when the data is
//! complete. Dropping it before [`Staged::commit`] leaves nothing behind.

use std::ffi::OsString;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use bohio_proto::fs::WriteMode;
use bohio_proto::{ErrorBody, ErrorCode};
use cap_std::fs::{Dir, File, OpenOptions};

use super::io_error;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A destination file opened for writing under a directory handle.
#[derive(Debug)]
pub(crate) struct Staged {
    /// The directory holding the target; every operation below names a single entry of it.
    parent: Dir,
    name: OsString,
    mode: WriteMode,
    file: Option<File>,
    /// The temporary sibling, for `replace`.
    temp: Option<OsString>,
    /// A `create` stream made the target itself, so abandoning it removes it.
    made: bool,
    committed: bool,
}

impl Staged {
    /// Opens the destination `relative` to `root` the way `mode` says.
    ///
    /// # Errors
    ///
    /// `bad_request` for the root itself, a directory target or a mode this host lacks; `exists`
    /// for `create` over an existing entry; otherwise what the operating system reports.
    pub(crate) fn open(
        root: &Dir,
        relative: &Path,
        mode: &WriteMode,
        parents: bool,
        file_mode: Option<u32>,
    ) -> Result<Self, ErrorBody> {
        let Some(name) = relative.file_name() else {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the path names a root, not a file",
            ));
        };
        if cfg!(windows) && file_mode.is_some() {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host has no permission bits",
            ));
        }
        let parent_relative = relative.parent().filter(|p| !p.as_os_str().is_empty());
        if parents && let Some(parent_relative) = parent_relative {
            root.create_dir_all(parent_relative)
                .map_err(|e| io_error(&e))?;
        }
        let parent = match parent_relative {
            Some(parent_relative) => root.open_dir(parent_relative),
            None => root.try_clone(),
        }
        .map_err(|e| io_error(&e))?;
        let name = name.to_owned();

        let mut staged = Self {
            parent,
            name: name.clone(),
            mode: *mode,
            file: None,
            temp: None,
            made: false,
            committed: false,
        };
        match mode {
            WriteMode::Replace => staged.open_temp(file_mode)?,
            WriteMode::Create => {
                let file = open_with(&staged.parent, &name, create_new(file_mode))?;
                staged.made = true;
                staged.file = Some(file);
            }
            WriteMode::Append => {
                let mut options = OpenOptions::new();
                options.append(true).create(true);
                apply_mode(&mut options, file_mode);
                staged.file = Some(open_with(&staged.parent, &name, options)?);
            }
            WriteMode::At { offset } => {
                let mut options = OpenOptions::new();
                options.write(true).create(true);
                apply_mode(&mut options, file_mode);
                let mut file = open_with(&staged.parent, &name, options)?;
                file.seek(SeekFrom::Start(*offset))
                    .map_err(|e| io_error(&e))?;
                staged.file = Some(file);
            }
            _ => {
                return Err(ErrorBody::new(
                    ErrorCode::Unsupported,
                    "this host does not know that write mode",
                ));
            }
        }
        Ok(staged)
    }

    fn open_temp(&mut self, file_mode: Option<u32>) -> Result<(), ErrorBody> {
        let existing = self.parent.symlink_metadata(&self.name).ok();
        if existing.as_ref().is_some_and(cap_std::fs::Metadata::is_dir) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the target is a directory",
            ));
        }
        let temp = OsString::from(format!(
            ".bohio-{}-{}.tmp",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let file = open_with(&self.parent, Path::new(&temp), create_new(file_mode))?;
        self.temp = Some(temp);
        // A replaced file keeps its mode unless the request names one.
        #[cfg(unix)]
        if file_mode.is_none()
            && let Some(existing) = existing.filter(cap_std::fs::Metadata::is_file)
        {
            let _ = file.set_permissions(existing.permissions());
        }
        self.file = Some(file);
        Ok(())
    }

    /// The file to write.
    pub(crate) fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("a staged file stays open until it is committed or dropped")
    }

    /// Puts the data in place: flushes (and syncs when `sync`), then for `replace` renames the
    /// temporary file over the target.
    ///
    /// # Errors
    ///
    /// What the operating system reports; the destination is then left as it was.
    pub(crate) fn commit(mut self, sync: bool) -> Result<(), ErrorBody> {
        let file = self.file.take().expect("a staged file is committed once");
        let flushed = (|| {
            let mut file = file;
            file.flush()?;
            if sync {
                file.sync_all()?;
            }
            Ok::<(), std::io::Error>(())
        })();
        flushed.map_err(|e| io_error(&e))?;
        if let Some(temp) = &self.temp {
            self.parent
                .rename(Path::new(temp), &self.parent, Path::new(&self.name))
                .map_err(|e| io_error(&e))?;
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // The file must be closed before it can be removed on Windows.
        self.file = None;
        if let Some(temp) = &self.temp {
            let _ = self.parent.remove_file(Path::new(temp));
        } else if self.made && matches!(self.mode, WriteMode::Create) {
            let _ = self.parent.remove_file(Path::new(&self.name));
        }
    }
}

fn create_new(file_mode: Option<u32>) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    apply_mode(&mut options, file_mode);
    options
}

#[cfg(unix)]
fn apply_mode(options: &mut OpenOptions, file_mode: Option<u32>) {
    use cap_std::fs::OpenOptionsExt;
    if let Some(bits) = file_mode {
        options.mode(bits & 0o7777);
    }
}

#[cfg(not(unix))]
fn apply_mode(_options: &mut OpenOptions, _file_mode: Option<u32>) {}

fn open_with(
    parent: &Dir,
    name: impl AsRef<Path>,
    options: OpenOptions,
) -> Result<File, ErrorBody> {
    parent
        .open_with(name.as_ref(), &options)
        .map_err(|e| io_error(&e))
}
