//! A file being written: the target, or a temporary sibling that replaces it when the data is
//! complete. Dropping it before [`Staged::commit`] leaves nothing behind.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bohio_proto::fs::WriteMode;
use bohio_proto::{ErrorBody, ErrorCode};

use super::io_error;
use super::vfs::{Dir, File, Metadata, OpenOptions};

/// The resume tokens a stream of this process is writing under: a second stream continuing one of them is `busy`.
static CLAIMED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// The name prefix of a temporary file kept for a resumable write.
const RESUME_PREFIX: &str = ".bohio-resume-";
/// What every temporary this engine stages a write in begins with.
const TEMP_PREFIX: &str = ".bohio-";

/// A token a running stream holds, released when it drops.
#[derive(Debug)]
struct Claim(String);

impl Claim {
    fn take(token: &str) -> Option<Self> {
        let mut claimed = CLAIMED.lock().unwrap_or_else(PoisonError::into_inner);
        claimed
            .get_or_insert_with(HashSet::new)
            .insert(token.to_owned())
            .then(|| Self(token.to_owned()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(claimed) = CLAIMED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            claimed.remove(&self.0);
        }
    }
}

fn resume_name(tag: &str, token: &str) -> OsString {
    OsString::from(format!("{RESUME_PREFIX}{tag}-{token}.tmp"))
}

/// Removes the kept temporary files of `parent` that nothing continued for `ttl`.
fn sweep(parent: &Dir, ttl: Duration) {
    let Ok(entries) = parent.read_dir(Path::new(".")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(text) = name.to_str() else { continue };
        if !(text.starts_with(RESUME_PREFIX) && text.ends_with(".tmp")) {
            continue;
        }
        let old = entry
            .metadata()
            .ok()
            .filter(Metadata::is_file)
            .and_then(|meta| meta.modified().ok())
            .and_then(|made| SystemTime::now().duration_since(made).ok())
            .is_some_and(|age| age >= ttl);
        if old {
            let _ = parent.remove_file_or_symlink(Path::new(&name));
        }
    }
}

/// Removes the expired kept temporaries of a resumable write from every directory under `roots`, which name
/// directories of `files`, looking at no more than `limit` entries (`0`: all). Every level is a directory handle, so a
/// directory swapped for a link while the sweep runs is not followed, and a link is never entered. The host calls it once
/// at start.
pub fn sweep_roots(
    files: &std::sync::Arc<dyn bohio_api::FilesSource>,
    roots: &[std::path::PathBuf],
    ttl: Duration,
    limit: usize,
) {
    let mut looked = 0_usize;
    let mut stack: Vec<Dir> = roots
        .iter()
        .filter_map(|root| Dir::open_root(files, root).ok())
        .collect();
    while let Some(directory) = stack.pop() {
        let Ok(entries) = directory.read_dir(Path::new(".")) else {
            continue;
        };
        for entry in entries.flatten() {
            looked += 1;
            if limit != 0 && looked > limit {
                return;
            }
            let name = entry.file_name();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                if let Ok(child) = directory.open_dir(Path::new(&name)) {
                    stack.push(child);
                }
                continue;
            }
            let Some(text) = name.to_str() else { continue };
            if meta.is_file() && text.starts_with(TEMP_PREFIX) && text.ends_with(".tmp") {
                let old = meta
                    .modified()
                    .ok()
                    .and_then(|made| SystemTime::now().duration_since(made).ok())
                    .is_some_and(|age| age >= ttl);
                if old {
                    let _ = directory.remove_file_or_symlink(Path::new(&name));
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod sweep_tests {
    use std::time::Duration;

    use super::{RESUME_PREFIX, sweep_roots};

    /// The expired kept temporaries of a tree are removed, a fresh file is kept, and a link to a directory is not
    /// entered, so what lies behind it is left alone.
    #[test]
    fn the_sweep_removes_expired_temporaries_in_the_roots_and_never_through_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, outside) = (tmp.path().join("root"), tmp.path().join("outside"));
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let old = |name: &str| format!("{RESUME_PREFIX}{name}.tmp");
        std::fs::write(root.join("a/b").join(old("x-1")), b"part").unwrap();
        std::fs::write(root.join(old("x-2")), b"part").unwrap();
        std::fs::write(root.join("a/.bohio-0f1e2d.tmp"), b"staged").unwrap();
        std::fs::write(root.join("keep.txt"), b"keep").unwrap();
        std::fs::write(outside.join(old("x-3")), b"part").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let sources = crate::access::Sources::default();
        let files = sources.files(None).expect("native files").arc();
        std::thread::sleep(Duration::from_millis(50));
        sweep_roots(
            &files,
            std::slice::from_ref(&root),
            Duration::from_millis(10),
            0,
        );
        assert!(!root.join("a/b").join(old("x-1")).exists());
        assert!(!root.join(old("x-2")).exists());
        assert!(!root.join("a/.bohio-0f1e2d.tmp").exists());
        assert!(root.join("keep.txt").exists());
        assert!(
            outside.join(old("x-3")).exists(),
            "the sweep went through a link"
        );
    }
}

/// The directory holding `relative`, made when `parents`, and the name in it.
fn locate(
    root: &Dir,
    relative: &Path,
    parents: bool,
    file_mode: Option<u32>,
) -> Result<(Dir, OsString), ErrorBody> {
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
        root.create_dir(parent_relative, true, None)
            .map_err(|e| io_error(&e))?;
    }
    let parent = match parent_relative {
        Some(parent_relative) => root.open_dir(parent_relative),
        None => root.try_clone(),
    }
    .map_err(|e| io_error(&e))?;
    Ok((parent, name.to_owned()))
}

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
    /// The modification time the file gets just before it is put in place.
    modified: Option<std::time::SystemTime>,
    /// Leave the temporary file where it is when this is dropped, for a write to continue.
    keep: bool,
    /// The token a resumable write is held under while it runs.
    claim: Option<Claim>,
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
        let (parent, name) = locate(root, relative, parents, file_mode)?;
        let mut staged = Self {
            parent,
            name: name.clone(),
            mode: mode.clone(),
            file: None,
            temp: None,
            made: false,
            committed: false,
            modified: None,
            keep: false,
            claim: None,
        };
        match mode {
            WriteMode::Replace | WriteMode::Delta { .. } => staged.open_temp(file_mode)?,
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
        let temp = OsString::from(format!("{TEMP_PREFIX}{}.tmp", super::prepare::token()?));
        self.open_temp_named(file_mode, temp)
    }

    fn open_temp_named(&mut self, file_mode: Option<u32>, temp: OsString) -> Result<(), ErrorBody> {
        let existing = self.parent.symlink_metadata(&self.name).ok();
        if existing.as_ref().is_some_and(Metadata::is_dir) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the target is a directory",
            ));
        }
        let file = open_with(&self.parent, Path::new(&temp), create_new(file_mode))?;
        self.temp = Some(temp);
        // A replaced file keeps its mode unless the request names one, without the set-ID bits.
        if cfg!(unix)
            && file_mode.is_none()
            && let Some(existing) = existing.filter(Metadata::is_file)
            && let Some(mode) = existing.mode()
        {
            let _ = file.set_mode(mode & 0o7777 & !super::ID_BITS);
        }
        self.file = Some(file);
        Ok(())
    }

    /// Opens a `replace` whose temporary file can be kept when the stream ends early, named by `tag` (who asked,
    /// for which path) and a new token, which is returned: the write is continued with [`Staged::resume`]. The
    /// temporary files of the target's directory that nothing continued for `ttl` are removed first.
    ///
    /// # Errors
    ///
    /// As [`Staged::open`].
    pub(crate) fn open_resumable(
        root: &Dir,
        relative: &Path,
        parents: bool,
        file_mode: Option<u32>,
        tag: &str,
        ttl: Duration,
    ) -> Result<(Self, String), ErrorBody> {
        let (parent, name) = locate(root, relative, parents, file_mode)?;
        sweep(&parent, ttl);
        let token = super::prepare::token()?;
        let mut staged = Self {
            parent,
            name,
            mode: WriteMode::Replace,
            file: None,
            temp: None,
            made: false,
            committed: false,
            modified: None,
            keep: false,
            claim: Claim::take(&token),
        };
        staged.open_temp_named(file_mode, resume_name(tag, &token))?;
        Ok((staged, token))
    }

    /// Opens the temporary file `token` names for the write that began it (`tag` says who and which path), to
    /// go on after the bytes it holds; returns how many there are.
    ///
    /// # Errors
    ///
    /// `not_found` for a token nothing holds, or one that expired; `busy` when a stream is still writing under
    /// it; otherwise as [`Staged::open`].
    pub(crate) fn resume(
        root: &Dir,
        relative: &Path,
        tag: &str,
        token: &str,
        ttl: Duration,
    ) -> Result<(Self, u64), ErrorBody> {
        let unknown = || ErrorBody::new(ErrorCode::NotFound, "no write waits under that token");
        if !super::prepare::well_formed(token) {
            return Err(unknown());
        }
        let (parent, name) = locate(root, relative, false, None)?;
        sweep(&parent, ttl);
        let temp = resume_name(tag, token);
        let meta = parent
            .symlink_metadata(Path::new(&temp))
            .map_err(|_| unknown())?;
        if !meta.is_file() {
            return Err(unknown());
        }
        let Some(claim) = Claim::take(token) else {
            return Err(ErrorBody::new(
                ErrorCode::Busy,
                "a stream is still writing under that token",
            ));
        };
        let mut options = OpenOptions::new();
        options.write(true);
        let mut file = open_with(&parent, Path::new(&temp), options)?;
        let offset = file.seek(SeekFrom::End(0)).map_err(|e| io_error(&e))?;
        Ok((
            Self {
                parent,
                name,
                mode: WriteMode::Replace,
                file: Some(file),
                temp: Some(temp),
                made: false,
                committed: false,
                modified: None,
                keep: false,
                claim: Some(claim),
            },
            offset,
        ))
    }

    /// Keeps the temporary file when this is dropped without a commit, so a later write can continue it.
    pub(crate) fn keep_for_resume(&mut self) {
        self.keep = self.claim.is_some();
    }

    /// The bytes of the file as they are now, read from the start into `into` (a resumed write's digests cover
    /// what an earlier stream wrote).
    pub(crate) fn read_back(
        &mut self,
        read_size: usize,
        into: &mut dyn FnMut(&[u8]),
    ) -> Result<(), ErrorBody> {
        use std::io::Read;
        let mut reader = self
            .parent
            .open(Path::new(
                self.temp
                    .as_deref()
                    .map_or_else(|| Path::new(&self.name), Path::new),
            ))
            .map_err(|e| io_error(&e))?;
        let mut buffer = vec![0_u8; read_size.max(1)];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => into(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
    }

    /// Flushes what was written and, when `sync`, makes it durable: the file stays open and writable.
    pub(crate) fn flush(&mut self, sync: bool) -> Result<(), ErrorBody> {
        let file = self.file();
        file.flush().map_err(|e| io_error(&e))?;
        if sync {
            file.sync_all().map_err(|e| io_error(&e))?;
        }
        Ok(())
    }

    /// The file to write.
    pub(crate) fn file(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("a staged file stays open until it is committed or dropped")
    }

    /// Gives the file this modification time when it is put in place, before the rename, so no reader sees it with
    /// another.
    pub(crate) fn set_modified(&mut self, modified: std::time::SystemTime) {
        self.modified = Some(modified);
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
            // A source may acknowledge a write before its machine has: asking it about the file is the call that
            // reports a write that failed, so a failure is never read as done whatever `sync` is.
            file.metadata()?;
            if sync {
                file.sync_all()?;
            }
            Ok::<(), std::io::Error>(())
        })();
        flushed.map_err(|e| io_error(&e))?;
        if let Some(modified) = self.modified {
            let name = self
                .temp
                .as_deref()
                .map_or_else(|| Path::new(&self.name), Path::new);
            self.parent
                .set_times(name, None, Some(modified))
                .map_err(|e| io_error(&e))?;
        }
        if let Some(temp) = &self.temp {
            self.parent
                .rename(Path::new(temp), Path::new(&self.name), true)
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
        if self.keep {
            return;
        }
        if let Some(temp) = &self.temp {
            let _ = self.parent.remove_file_or_symlink(Path::new(temp));
        } else if self.made && matches!(self.mode, WriteMode::Create) {
            let _ = self.parent.remove_file_or_symlink(Path::new(&self.name));
        }
    }
}

fn create_new(file_mode: Option<u32>) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    apply_mode(&mut options, file_mode);
    options
}

fn apply_mode(options: &mut OpenOptions, file_mode: Option<u32>) {
    if cfg!(unix)
        && let Some(bits) = file_mode
    {
        options.mode(bits & 0o7777);
    }
}

fn open_with(
    parent: &Dir,
    name: impl AsRef<Path>,
    options: OpenOptions,
) -> Result<File, ErrorBody> {
    parent
        .open_with(name.as_ref(), &options)
        .map_err(|e| io_error(&e))
}
