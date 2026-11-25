// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The files of a machine over SFTP: one SFTP session on a channel of the source's connection, and the `bohio_files` kind
//! on top of it.
//!
//! Names are resolved by the server by path on every call, so a directory handle here is the path it was opened with: a
//! symbolic link inside a root can lead out of it, which the source says by lacking `confined`. Extensions that only some
//! servers advertise (`posix-rename@openssh.com`, `hardlink@openssh.com`, `statvfs@openssh.com`, `fsync@openssh.com`) are
//! offered when the server's version reply lists them. SFTP version 3 has no code for "exists" or "not empty": the failure
//! is told apart by asking the server.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use bohio_api::files::{failure, word};
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{DirEntryModel, FileStat, OpenModel, SourceInfo, TimeModel, VfsStat};
use bohio_api::{DirReader, Directory, File, FilesSource, unavailable, vocab};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, SftpHandle, StatusCode};
use tokio::sync::OwnedSemaphorePermit;

use crate::link::{Link, Slot, lock};
use crate::settings::Settings;

/// What the server's version reply advertised.
#[derive(Debug, Clone, Copy, Default)]
struct Exts {
    posix_rename: bool,
    hardlink: bool,
    statvfs: bool,
    fsync: bool,
}

/// An SFTP session on a channel, with the slot of `max_channels` it holds.
struct Sftp {
    raw: Arc<RawSftpSession>,
    exts: Exts,
    _permit: OwnedSemaphorePermit,
}

/// A failed request: what to tell the caller, and whether the session is gone and must be opened again.
struct Fail {
    error: ProviderError,
    dead: bool,
}

impl From<ProviderError> for Fail {
    fn from(error: ProviderError) -> Self {
        Self { error, dead: false }
    }
}

type Res<T> = Result<T, Fail>;

/// The status of a refusal by the server, when that is what the failure is.
fn status_of(error: &SftpError) -> Option<StatusCode> {
    match error {
        SftpError::Status(status) => Some(status.status_code),
        _ => None,
    }
}

impl From<SftpError> for Fail {
    fn from(error: SftpError) -> Self {
        let text = error.to_string();
        let (error, dead) = match &error {
            SftpError::Status(status) => match status.status_code {
                StatusCode::NoSuchFile => (failure(word::NOT_FOUND, &text), false),
                StatusCode::PermissionDenied => (failure(word::DENIED, &text), false),
                StatusCode::OpUnsupported => (failure(word::UNSUPPORTED, &text), false),
                StatusCode::BadMessage => (failure(word::INVALID, &text), false),
                StatusCode::NoConnection | StatusCode::ConnectionLost => {
                    (unavailable(&format!("sftp: {text}")), true)
                }
                _ => (failure(word::IO, &text), false),
            },
            SftpError::Limited(_) => (failure(word::INVALID, &text), false),
            SftpError::Timeout => (unavailable("sftp: the host did not answer in time"), true),
            _ => (unavailable(&format!("sftp: {text}")), true),
        };
        Self { error, dead }
    }
}

fn io(message: &str) -> Fail {
    failure(word::IO, message).into()
}

/// The names of the SFTP extensions a version reply lists.
fn exts_of(version: &russh_sftp::protocol::Version) -> Exts {
    let has = |name: &str| version.extensions.contains_key(name);
    Exts {
        posix_rename: has("posix-rename@openssh.com"),
        hardlink: has("hardlink@openssh.com"),
        statvfs: has("statvfs@openssh.com"),
        fsync: has("fsync@openssh.com"),
    }
}

/// The SFTP side of a source: the connection, and the session opened on it when first needed.
pub(crate) struct Files {
    link: Arc<Link>,
    current: tokio::sync::Mutex<Option<Arc<Sftp>>>,
}

impl Files {
    pub(crate) fn new(link: Arc<Link>) -> Arc<Self> {
        Arc::new(Self {
            link,
            current: tokio::sync::Mutex::new(None),
        })
    }

    fn settings(&self) -> &Settings {
        &self.link.settings
    }

    async fn open_session(&self) -> Result<Arc<Sftp>, ProviderError> {
        let Slot {
            mut channel,
            permit,
        } = self.link.channel().await?;
        let settings = self.settings();
        let no_sftp = |why: &dyn core::fmt::Display| {
            failure(
                word::UNSUPPORTED,
                &format!(
                    "{}: the host has no sftp subsystem ({why})",
                    self.link.target()
                ),
            )
        };
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|error| no_sftp(&error))?;
        loop {
            match channel.wait().await {
                Some(russh::ChannelMsg::Success) => break,
                Some(russh::ChannelMsg::Failure) | None => {
                    return Err(no_sftp(&"it refused the request"));
                }
                Some(_) => {}
            }
        }
        let config = russh_sftp::client::Config {
            max_packet_len: 256 * 1024,
            max_concurrent_reads: settings.sftp_in_flight,
            max_concurrent_writes: settings.sftp_in_flight,
            max_write_packet_len: u32::try_from(settings.sftp_packet).unwrap_or(u32::MAX),
            request_timeout_secs: settings.operation_timeout.as_secs().max(1),
        };
        let mut raw = RawSftpSession::new_with_config(channel.into_stream(), config);
        let version = raw
            .init()
            .await
            .map_err(|error| unavailable(&format!("sftp: {error}")))?;
        if let Ok(limits) = raw.limits().await {
            raw.set_limits(limits.into());
        }
        Ok(Arc::new(Sftp {
            raw: Arc::new(raw),
            exts: exts_of(&version),
            _permit: permit,
        }))
    }

    /// The session, opened when there is none.
    async fn session(&self) -> Result<Arc<Sftp>, ProviderError> {
        let mut current = self.current.lock().await;
        if let Some(sftp) = current.as_ref() {
            return Ok(Arc::clone(sftp));
        }
        let sftp = self.open_session().await?;
        *current = Some(Arc::clone(&sftp));
        Ok(sftp)
    }

    async fn forget(&self, sftp: &Arc<Sftp>) {
        let mut current = self.current.lock().await;
        if current.as_ref().is_some_and(|held| Arc::ptr_eq(held, sftp)) {
            *current = None;
        }
    }

    /// Runs `body` against the session for at most `operation_timeout`; a session that turns out dead is forgotten, so
    /// the next call opens another (as the source's `reconnect` allows).
    fn call<T, F, Fut>(self: &Arc<Self>, body: F) -> Result<T, ProviderError>
    where
        F: FnOnce(Arc<Sftp>) -> Fut,
        Fut: Future<Output = Res<T>>,
    {
        let wait = self.settings().operation_timeout;
        let this = Arc::clone(self);
        self.link.rt.block_on(async move {
            let attempt = tokio::time::timeout(wait, async {
                let sftp = this.session().await?;
                match body(Arc::clone(&sftp)).await {
                    Ok(value) => Ok(value),
                    Err(fail) => {
                        if fail.dead {
                            this.forget(&sftp).await;
                        }
                        Err(fail.error)
                    }
                }
            })
            .await;
            match attempt {
                Ok(result) => result,
                Err(_) => Err(unavailable(&format!(
                    "{}: the host did not answer in time",
                    this.link.target()
                ))),
            }
        })
    }
}

/// The attributes of a file as the kind reports them.
fn stat_of(attrs: &FileAttributes) -> FileStat {
    let mode = attrs.permissions.unwrap_or(0);
    let kind = match mode & 0o170_000 {
        0o040_000 => vocab::file_kind::DIR,
        0o120_000 => vocab::file_kind::SYMLINK,
        0o100_000 => vocab::file_kind::FILE,
        _ => vocab::file_kind::OTHER,
    };
    let time = |secs: Option<u32>| {
        secs.map(|secs| TimeModel {
            secs: i64::from(secs),
            nanos: 0,
        })
    };
    FileStat {
        kind: kind.to_owned(),
        size: attrs.size.unwrap_or(0),
        modified: time(attrs.mtime),
        accessed: time(attrs.atime),
        created: None,
        mode: attrs.permissions.map(|mode| mode & 0o7777),
        readonly: attrs.permissions.is_some_and(|mode| mode & 0o222 == 0),
        attributes: None,
        device: None,
        inode: None,
        links: None,
    }
}

fn is_dir(attrs: &FileAttributes) -> bool {
    attrs
        .permissions
        .is_some_and(|mode| mode & 0o170_000 == 0o040_000)
}

impl Sftp {
    async fn lstat(&self, path: &str) -> Res<FileAttributes> {
        Ok(self.raw.lstat(path).await?.attrs)
    }

    async fn stat(&self, path: &str) -> Res<FileAttributes> {
        Ok(self.raw.stat(path).await?.attrs)
    }

    async fn exists(&self, path: &str) -> bool {
        self.raw.lstat(path).await.is_ok()
    }

    /// `failure` of `word`, or `exists` when `path` is there.
    async fn or_exists(&self, error: SftpError, path: &str) -> Fail {
        if status_of(&error) == Some(StatusCode::Failure) && self.exists(path).await {
            return failure(word::EXISTS, "the name exists").into();
        }
        error.into()
    }

    async fn mkdir(&self, path: &str, mode: Option<u32>) -> Res<()> {
        let attrs = FileAttributes {
            permissions: mode.map(|mode| mode & 0o7777),
            ..FileAttributes::empty()
        };
        match self.raw.mkdir(path, attrs).await {
            Ok(_) => Ok(()),
            Err(error) => Err(self.or_exists(error, path).await),
        }
    }

    async fn rmdir(&self, path: &str) -> Res<()> {
        match self.raw.rmdir(path).await {
            Ok(_) => Ok(()),
            Err(error) => {
                if status_of(&error) == Some(StatusCode::Failure) {
                    if let Ok(attrs) = self.lstat(path).await {
                        if !is_dir(&attrs) {
                            return Err(failure(word::NOT_A_DIRECTORY, "not a directory").into());
                        }
                    }
                    if self.has_entries(path).await {
                        return Err(failure(word::NOT_EMPTY, "the directory is not empty").into());
                    }
                }
                Err(error.into())
            }
        }
    }

    async fn has_entries(&self, path: &str) -> bool {
        let Ok(handle) = self.raw.opendir(path).await else {
            return false;
        };
        let mut found = false;
        while let Ok(names) = self.raw.readdir(handle.handle.clone()).await {
            if names
                .files
                .iter()
                .any(|file| file.filename != "." && file.filename != "..")
            {
                found = true;
                break;
            }
        }
        let _ = self.raw.close(handle.handle).await;
        found
    }

    async fn remove_file(&self, path: &str) -> Res<()> {
        match self.raw.remove(path).await {
            Ok(_) => Ok(()),
            Err(error) => {
                if status_of(&error) == Some(StatusCode::Failure)
                    && self.lstat(path).await.is_ok_and(|attrs| is_dir(&attrs))
                {
                    return Err(failure(word::IS_A_DIRECTORY, "a directory").into());
                }
                Err(error.into())
            }
        }
    }

    /// The directory's entries without `.` and `..`, read to the end.
    async fn entries(&self, path: &str) -> Res<Vec<(String, FileAttributes)>> {
        let handle = self
            .raw
            .opendir(path)
            .await
            .map_err(|error| Fail::from(error))?;
        let mut all = Vec::new();
        loop {
            match self.raw.readdir(handle.handle.clone()).await {
                Ok(names) => {
                    for file in names.files {
                        let name = file.filename.to_string();
                        if name != "." && name != ".." {
                            all.push((name, file.attrs));
                        }
                    }
                }
                Err(error) if status_of(&error) == Some(StatusCode::Eof) => break,
                Err(error) => {
                    let _ = self.raw.close(handle.handle).await;
                    return Err(error.into());
                }
            }
        }
        let _ = self.raw.close(handle.handle).await;
        Ok(all)
    }

    /// Removes `path` and everything below it, never following a link.
    async fn remove_tree(&self, path: &str) -> Res<()> {
        let mut pending = vec![path.to_owned()];
        let mut directories = Vec::new();
        while let Some(dir) = pending.pop() {
            for (name, attrs) in self.entries(&dir).await? {
                let child = join(&dir, &name);
                if is_dir(&attrs) {
                    pending.push(child);
                } else {
                    self.remove_file(&child).await?;
                }
            }
            directories.push(dir);
        }
        for dir in directories.into_iter().rev() {
            self.rmdir(&dir).await?;
        }
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Res<()> {
        if overwrite {
            if self.exts.posix_rename {
                self.raw.posix_rename(from, to).await?;
                return Ok(());
            }
            if self.exists(to).await {
                return Err(failure(
                    word::UNSUPPORTED,
                    "this server cannot replace a name in one step (no posix-rename@openssh.com)",
                )
                .into());
            }
        }
        match self.raw.rename(from, to).await {
            Ok(_) => Ok(()),
            Err(error) => Err(self.or_exists(error, to).await),
        }
    }

    async fn setstat(&self, path: &str, attrs: FileAttributes) -> Res<()> {
        self.raw.setstat(path, attrs).await?;
        Ok(())
    }
}

/// `root` and the name below it as one path; the root is absolute and has no trailing slash unless it is `/`.
fn join(root: &str, relative: &str) -> String {
    if relative.is_empty() || relative == "." {
        return root.to_owned();
    }
    if root.ends_with('/') {
        format!("{root}{relative}")
    } else {
        format!("{root}/{relative}")
    }
}

/// A name below a directory handle: plain, with no `..` and not absolute.
fn below(root: &str, relative: &str) -> Result<String, ProviderError> {
    if relative.starts_with('/')
        || relative.split('/').any(|part| part == "..")
        || relative.contains('\0')
    {
        return Err(failure(
            word::OUTSIDE,
            "a name below a directory has no `..` and is not absolute",
        ));
    }
    Ok(join(root, relative.trim_start_matches("./")))
}

fn time_secs(time: TimeModel) -> u32 {
    u32::try_from(time.secs.max(0)).unwrap_or(u32::MAX)
}

/// The files of a machine, as the kind.
pub(crate) struct SftpFiles(pub(crate) Arc<Files>);

impl SftpFiles {
    pub(crate) fn describe(&self) -> SourceInfo {
        use vocab::files as w;
        // What the server advertises is known once a session is open; ask for it (a failure makes the source
        // unavailable with the reason).
        let exts = self.0.call(|sftp| async move { Ok(sftp.exts) });
        let (exts, reason) = match exts {
            Ok(exts) => (exts, None),
            Err(error) => (Exts::default(), Some(error.message().to_string())),
        };
        let mut offers = vec![
            w::READ,
            w::WRITE,
            w::SYMLINK,
            w::SET_TIMES,
            w::SET_MODE,
            w::SET_READONLY,
            w::LIST_STATS,
            w::POSIX_PATHS,
        ];
        let mut lacks = vec![w::CONFINED];
        for (has, word) in [
            (exts.posix_rename, w::ATOMIC_REPLACE),
            (exts.hardlink, w::HARD_LINK),
            (exts.statvfs, w::STATVFS),
            (exts.fsync, w::SYNC),
        ] {
            if has || reason.is_some() {
                offers.push(word);
            } else {
                lacks.push(word);
            }
        }
        let info = SourceInfo::new(crate::ID, &offers, &lacks);
        match reason {
            None => info,
            Some(reason) => info.unavailable(&reason),
        }
    }
}

struct SftpDir {
    files: Arc<Files>,
    root: String,
}

impl SftpDir {
    fn path(&self, relative: &str) -> Result<String, ProviderError> {
        below(&self.root, relative)
    }
}

impl FilesSource for SftpFiles {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SftpFiles::describe(self))
    }

    fn open_dir(&self, path: &str) -> Result<Object<dyn Directory>, ProviderError> {
        if !path.starts_with('/') {
            return Err(failure(
                word::INVALID,
                "a directory of an ssh source is an absolute path",
            ));
        }
        let root = if path.len() > 1 {
            path.trim_end_matches('/').to_owned()
        } else {
            path.to_owned()
        };
        let probe = root.clone();
        let attrs = self
            .0
            .call(move |sftp| async move { sftp.stat(&probe).await })?;
        if !is_dir(&attrs) {
            return Err(failure(word::NOT_A_DIRECTORY, "not a directory"));
        }
        Ok(SftpDir {
            files: Arc::clone(&self.0),
            root,
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
        let (from, to) = (below(from_root, from)?, below(to_root, to)?);
        self.0
            .call(move |sftp| async move { sftp.rename(&from, &to, overwrite).await })
    }
}

impl Directory for SftpDir {
    fn open_dir(&self, relative: &str) -> Result<Object<dyn Directory>, ProviderError> {
        let path = self.path(relative)?;
        let probe = path.clone();
        let attrs = self
            .files
            .call(move |sftp| async move { sftp.stat(&probe).await })?;
        if !is_dir(&attrs) {
            return Err(failure(word::NOT_A_DIRECTORY, "not a directory"));
        }
        Ok(SftpDir {
            files: Arc::clone(&self.files),
            root: path,
        }
        .into_object())
    }

    fn stat(&self, relative: &str, follow: bool) -> Result<FileStat, ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            let attrs = if follow {
                sftp.stat(&path).await?
            } else {
                sftp.lstat(&path).await?
            };
            Ok(stat_of(&attrs))
        })
    }

    fn read_dir(&self, relative: &str) -> Result<Object<dyn DirReader>, ProviderError> {
        let path = self.path(relative)?;
        let handle = self.files.call(move |sftp| async move {
            let handle = sftp.raw.opendir(path).await?;
            Ok((sftp, handle.handle))
        })?;
        Ok(Reader {
            files: Arc::clone(&self.files),
            sftp: handle.0,
            handle: handle.1,
            pending: Mutex::new(VecDeque::new()),
            ended: Mutex::new(false),
        }
        .into_object())
    }

    fn open_file(
        &self,
        relative: &str,
        options: OpenModel,
    ) -> Result<Object<dyn File>, ProviderError> {
        let path = self.path(relative)?;
        let mut flags = OpenFlags::empty();
        if options.read {
            flags |= OpenFlags::READ;
        }
        if options.write || options.append {
            flags |= OpenFlags::WRITE;
        }
        if options.append {
            flags |= OpenFlags::APPEND;
        }
        if options.create || options.create_new {
            flags |= OpenFlags::CREATE;
        }
        if options.create_new {
            flags |= OpenFlags::EXCLUDE;
        }
        if options.truncate {
            flags |= OpenFlags::TRUNCATE;
        }
        let attrs = FileAttributes {
            permissions: options.mode.map(|mode| mode & 0o7777),
            ..FileAttributes::empty()
        };
        let (sftp, handle) = self.files.call(move |sftp| async move {
            match sftp.raw.open(path.as_str(), flags, attrs).await {
                Ok(handle) => Ok((sftp, handle.handle)),
                Err(error) => {
                    if status_of(&error) == Some(StatusCode::Failure) {
                        if let Ok(found) = sftp.lstat(&path).await {
                            return Err(if is_dir(&found) {
                                failure(word::IS_A_DIRECTORY, "a directory").into()
                            } else if options.create_new {
                                failure(word::EXISTS, "the name exists").into()
                            } else {
                                Fail::from(error)
                            });
                        }
                    }
                    Err(error.into())
                }
            }
        })?;
        Ok(SftpFile {
            files: Arc::clone(&self.files),
            sftp,
            handle,
            state: Mutex::new(FileState::default()),
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
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            if !parents {
                return sftp.mkdir(&path, mode).await;
            }
            // Every missing parent below the root, then the directory itself; a name that is already a directory is fine.
            let mut made = String::new();
            for part in path.split('/').filter(|part| !part.is_empty()) {
                made.push('/');
                made.push_str(part);
                match sftp.stat(&made).await {
                    Ok(found) if is_dir(&found) => continue,
                    Ok(_) => {
                        return Err(
                            failure(word::NOT_A_DIRECTORY, "a name on the path is a file").into(),
                        );
                    }
                    Err(_) => sftp.mkdir(&made, mode).await?,
                }
            }
            Ok(())
        })
    }

    fn remove_dir(&self, relative: &str) -> Result<(), ProviderError> {
        let path = self.path(relative)?;
        self.files
            .call(move |sftp| async move { sftp.rmdir(&path).await })
    }

    fn remove_dir_all(&self, relative: &str) -> Result<(), ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            match sftp.lstat(&path).await {
                Ok(attrs) if is_dir(&attrs) => sftp.remove_tree(&path).await,
                Ok(_) => Err(failure(word::NOT_A_DIRECTORY, "not a directory").into()),
                Err(error) => Err(error),
            }
        })
    }

    fn remove_file(&self, relative: &str) -> Result<(), ProviderError> {
        let path = self.path(relative)?;
        self.files
            .call(move |sftp| async move { sftp.remove_file(&path).await })
    }

    fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<(), ProviderError> {
        let (from, to) = (self.path(from)?, self.path(to)?);
        self.files
            .call(move |sftp| async move { sftp.rename(&from, &to, overwrite).await })
    }

    fn hard_link(&self, from: &str, to: &str) -> Result<(), ProviderError> {
        let (from, to) = (self.path(from)?, self.path(to)?);
        self.files.call(move |sftp| async move {
            if !sftp.exts.hardlink {
                return Err(failure(
                    word::UNSUPPORTED,
                    "this server makes no hard links (no hardlink@openssh.com)",
                )
                .into());
            }
            match sftp.raw.hardlink(&from, &to).await {
                Ok(_) => Ok(()),
                Err(error) => Err(sftp.or_exists(error, &to).await),
            }
        })
    }

    fn symlink(&self, target: &str, link: &str, _to_directory: bool) -> Result<(), ProviderError> {
        let link = self.path(link)?;
        let target = target.to_owned();
        self.files.call(move |sftp| async move {
            // OpenSSH reads the first field of the request as the target.
            match sftp.raw.symlink(target.as_str(), link.as_str()).await {
                Ok(_) => Ok(()),
                Err(error) => Err(sftp.or_exists(error, &link).await),
            }
        })
    }

    fn read_link(&self, relative: &str) -> Result<String, ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            let names = sftp.raw.readlink(path).await?;
            names
                .files
                .into_iter()
                .next()
                .map(|file| file.filename.to_string())
                .ok_or_else(|| io("the host answered no target"))
        })
    }

    fn set_times(
        &self,
        relative: &str,
        accessed: Option<TimeModel>,
        modified: Option<TimeModel>,
    ) -> Result<(), ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            // The protocol sets both times together: the one left alone is read first.
            let current = sftp.stat(&path).await?;
            let attrs = FileAttributes {
                atime: accessed.map(time_secs).or(current.atime),
                mtime: modified.map(time_secs).or(current.mtime),
                ..FileAttributes::empty()
            };
            sftp.setstat(&path, attrs).await
        })
    }

    fn set_mode(&self, relative: &str, mode: u32) -> Result<(), ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            sftp.setstat(
                &path,
                FileAttributes {
                    permissions: Some(mode & 0o7777),
                    ..FileAttributes::empty()
                },
            )
            .await
        })
    }

    fn set_readonly(&self, relative: &str, readonly: bool) -> Result<(), ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            let current = sftp.stat(&path).await?.permissions.unwrap_or(0o644) & 0o7777;
            let mode = if readonly {
                current & !0o222
            } else {
                current | 0o200
            };
            sftp.setstat(
                &path,
                FileAttributes {
                    permissions: Some(mode),
                    ..FileAttributes::empty()
                },
            )
            .await
        })
    }

    fn statvfs(&self, relative: &str) -> Result<VfsStat, ProviderError> {
        let path = self.path(relative)?;
        self.files.call(move |sftp| async move {
            if !sftp.exts.statvfs {
                return Err(failure(
                    word::UNSUPPORTED,
                    "this server cannot tell the space of a file system (no statvfs@openssh.com)",
                )
                .into());
            }
            let found = sftp.raw.statvfs(path).await?;
            Ok(VfsStat {
                block_size: found.fragment_size.max(1),
                blocks: found.blocks,
                blocks_free: found.blocks_free,
                blocks_avail: found.blocks_avail,
                files: Some(found.inodes),
                files_free: Some(found.inodes_free),
                name_max: Some(found.name_max),
            })
        })
    }
}

/// A listing in progress: the server's directory handle, read a batch at a time.
struct Reader {
    files: Arc<Files>,
    sftp: Arc<Sftp>,
    handle: SftpHandle,
    pending: Mutex<VecDeque<DirEntryModel>>,
    ended: Mutex<bool>,
}

impl DirReader for Reader {
    fn next(&self, max: u64) -> Result<Vec<DirEntryModel>, ProviderError> {
        let max = usize::try_from(max).unwrap_or(usize::MAX).max(1);
        loop {
            {
                let mut pending = lock(&self.pending);
                if !pending.is_empty() {
                    let count = max.min(pending.len());
                    return Ok(pending.drain(..count).collect());
                }
            }
            if *lock(&self.ended) {
                return Ok(Vec::new());
            }
            let sftp = Arc::clone(&self.sftp);
            let handle = self.handle.clone();
            let batch = self.files.link.rt.block_on(async {
                tokio::time::timeout(
                    self.files.settings().operation_timeout,
                    sftp.raw.readdir(handle),
                )
                .await
            });
            match batch {
                Ok(Ok(names)) => {
                    let mut pending = lock(&self.pending);
                    for file in names.files {
                        let name = file.filename.to_string();
                        if name == "." || name == ".." {
                            continue;
                        }
                        pending.push_back(DirEntryModel {
                            name,
                            stat: Some(stat_of(&file.attrs)),
                            unnamed: false,
                        });
                    }
                }
                Ok(Err(error)) if status_of(&error) == Some(StatusCode::Eof) => {
                    *lock(&self.ended) = true;
                }
                Ok(Err(error)) => return Err(Fail::from(error).error),
                Err(_) => return Err(unavailable("sftp: the host did not answer in time")),
            }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let (sftp, handle) = (Arc::clone(&self.sftp), self.handle.clone());
        self.files.link.rt.handle().spawn(async move {
            let _ = sftp.raw.close(handle).await;
        });
    }
}

/// What a file has in flight and ahead.
#[derive(Default)]
struct FileState {
    /// Writes sent and not yet acknowledged, oldest first.
    writing: VecDeque<russh_sftp::client::rawsession::PendingWrite>,
    /// The first failure of a write that was acknowledged late, told to the next call that can.
    failed: Option<ProviderError>,
    /// Bytes read ahead: where they start, and what they are.
    ahead: Option<(u64, Vec<u8>)>,
    /// The offset after the latest read, to tell a sequential reader.
    next_read: u64,
    /// The offset after the latest write, for an append file's length.
    written: u64,
}

/// An open file. Reads go ahead and writes behind by `sftp_in_flight` requests; a call that observes the file
/// (`stat`, `sync`, `set_len`, `read_at`, a drop) waits for the writes first and tells their failure.
struct SftpFile {
    files: Arc<Files>,
    sftp: Arc<Sftp>,
    handle: SftpHandle,
    state: Mutex<FileState>,
    append: bool,
}

impl SftpFile {
    fn block<T>(
        &self,
        future: impl Future<Output = Result<T, ProviderError>>,
    ) -> Result<T, ProviderError> {
        self.files.link.rt.block_on(future)
    }

    /// Waits for every write in flight, and answers the first failure there has been.
    fn flush(&self, state: &mut FileState) -> Result<(), ProviderError> {
        let wait = self.files.settings().operation_timeout;
        while let Some(pending) = state.writing.pop_front() {
            let done = self
                .files
                .link
                .rt
                .block_on(async { tokio::time::timeout(wait, pending).await });
            match done {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    state.failed.get_or_insert(Fail::from(error).error);
                }
                Err(_) => {
                    state
                        .failed
                        .get_or_insert(unavailable("sftp: the host did not answer in time"));
                }
            }
        }
        state.ahead = None;
        match state.failed.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn packet(&self) -> usize {
        let configured = self.files.settings().sftp_packet;
        // A server's own limit on one read or write caps it.
        configured.min(255 * 1024)
    }

    fn fetch(&self, offset: u64, want: usize) -> Result<Vec<u8>, ProviderError> {
        let packet = self.packet();
        let window = self.files.settings().sftp_in_flight;
        let raw = Arc::clone(&self.sftp.raw);
        let handle = self.handle.clone();
        let wait = self.files.settings().operation_timeout;
        self.block(async move {
            let mut out = Vec::with_capacity(want);
            let mut at = offset;
            while out.len() < want {
                let mut set = tokio::task::JoinSet::new();
                let mut asked = 0;
                let mut index = 0_usize;
                while asked < want - out.len() && index < window {
                    let len = packet.min(want - out.len() - asked);
                    let (raw, handle) = (Arc::clone(&raw), handle.clone());
                    let from = at + asked as u64;
                    set.spawn(async move {
                        let got = tokio::time::timeout(
                            wait,
                            raw.read(handle, from, u32::try_from(len).unwrap_or(u32::MAX)),
                        )
                        .await;
                        (index, len, got)
                    });
                    asked += len;
                    index += 1;
                }
                let mut parts: Vec<Option<(usize, Vec<u8>)>> = (0..index).map(|_| None).collect();
                while let Some(joined) = set.join_next().await {
                    let (at_index, len, got) = joined
                        .map_err(|_| failure(word::IO, "a read task ended without an answer"))?;
                    match got {
                        Ok(Ok(data)) => parts[at_index] = Some((len, data.data)),
                        Ok(Err(error)) if status_of(&error) == Some(StatusCode::Eof) => {
                            parts[at_index] = Some((len, Vec::new()));
                        }
                        Ok(Err(error)) => return Err(Fail::from(error).error),
                        Err(_) => return Err(unavailable("sftp: the host did not answer in time")),
                    }
                }
                let before = out.len();
                for (len, data) in parts.into_iter().flatten() {
                    let short = data.len() < len;
                    out.extend_from_slice(&data);
                    if short {
                        break;
                    }
                }
                // A short answer that is not the end of the file is asked for again from where it stopped; the end of
                // the file answers nothing.
                if out.len() == before {
                    break;
                }
                at += (out.len() - before) as u64;
            }
            out.truncate(want);
            Ok(out)
        })
    }
}

impl File for SftpFile {
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<u64, ProviderError> {
        let mut state = lock(&self.state);
        if !state.writing.is_empty() || state.failed.is_some() {
            self.flush(&mut state)?;
        }
        if let Some((from, bytes)) = &state.ahead {
            if offset >= *from && offset < *from + bytes.len() as u64 {
                let start = usize::try_from(offset - *from).unwrap_or(usize::MAX);
                let count = dst.len().min(bytes.len() - start);
                dst[..count].copy_from_slice(&bytes[start..start + count]);
                state.next_read = offset + count as u64;
                return Ok(count as u64);
            }
        }
        let sequential = offset == state.next_read;
        let ahead = self.files.settings().sftp_in_flight * self.packet();
        let want = if sequential {
            dst.len().max(ahead)
        } else {
            dst.len()
        };
        let got = self.fetch(offset, want)?;
        let count = dst.len().min(got.len());
        dst[..count].copy_from_slice(&got[..count]);
        state.next_read = offset + count as u64;
        state.ahead = (got.len() > count).then(|| (offset, got));
        Ok(count as u64)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<u64, ProviderError> {
        let mut state = lock(&self.state);
        state.ahead = None;
        if let Some(error) = state.failed.take() {
            return Err(error);
        }
        let packet = self.packet();
        let window = self.files.settings().sftp_in_flight;
        let wait = self.files.settings().operation_timeout;
        let mut at = offset;
        for chunk in data.chunks(packet) {
            if state.writing.len() >= window {
                if let Some(oldest) = state.writing.pop_front() {
                    let done = self
                        .files
                        .link
                        .rt
                        .block_on(async { tokio::time::timeout(wait, oldest).await });
                    match done {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            let error = Fail::from(error).error;
                            let _ = self.flush(&mut state);
                            return Err(error);
                        }
                        Err(_) => {
                            return Err(unavailable("sftp: the host did not answer in time"));
                        }
                    }
                }
            }
            let sent = self.sftp.raw.write_nowait(&self.handle, at, chunk);
            match sent {
                Ok(pending) => state.writing.push_back(pending),
                Err(error) => return Err(Fail::from(error).error),
            }
            at += chunk.len() as u64;
        }
        state.written = state.written.max(at);
        Ok(data.len() as u64)
    }

    fn set_len(&self, len: u64) -> Result<(), ProviderError> {
        let mut state = lock(&self.state);
        self.flush(&mut state)?;
        let (raw, handle) = (Arc::clone(&self.sftp.raw), self.handle.clone());
        self.files.call(move |_| async move {
            raw.fsetstat(
                handle,
                FileAttributes {
                    size: Some(len),
                    ..FileAttributes::empty()
                },
            )
            .await?;
            Ok(())
        })
    }

    fn sync(&self) -> Result<(), ProviderError> {
        let mut state = lock(&self.state);
        self.flush(&mut state)?;
        if !self.sftp.exts.fsync {
            return Err(failure(
                word::UNSUPPORTED,
                "this server cannot flush a file (no fsync@openssh.com)",
            ));
        }
        let (raw, handle) = (Arc::clone(&self.sftp.raw), self.handle.clone());
        self.files.call(move |_| async move {
            raw.fsync(handle).await?;
            Ok(())
        })
    }

    fn stat(&self) -> Result<FileStat, ProviderError> {
        let mut state = lock(&self.state);
        self.flush(&mut state)?;
        let (raw, handle) = (Arc::clone(&self.sftp.raw), self.handle.clone());
        self.files
            .call(move |_| async move { Ok(stat_of(&raw.fstat(handle).await?.attrs)) })
    }

    fn set_modified(&self, time: TimeModel) -> Result<(), ProviderError> {
        let mut state = lock(&self.state);
        self.flush(&mut state)?;
        let (raw, handle) = (Arc::clone(&self.sftp.raw), self.handle.clone());
        self.files.call(move |_| async move {
            let current = raw.fstat(handle.clone()).await?.attrs;
            raw.fsetstat(
                handle,
                FileAttributes {
                    atime: current.atime,
                    mtime: Some(time_secs(time)),
                    ..FileAttributes::empty()
                },
            )
            .await?;
            Ok(())
        })
    }

    fn set_mode(&self, mode: u32) -> Result<(), ProviderError> {
        let mut state = lock(&self.state);
        self.flush(&mut state)?;
        let (raw, handle) = (Arc::clone(&self.sftp.raw), self.handle.clone());
        self.files.call(move |_| async move {
            raw.fsetstat(
                handle,
                FileAttributes {
                    permissions: Some(mode & 0o7777),
                    ..FileAttributes::empty()
                },
            )
            .await?;
            Ok(())
        })
    }
}

impl Drop for SftpFile {
    fn drop(&mut self) {
        let (sftp, handle) = (Arc::clone(&self.sftp), self.handle.clone());
        let writing: Vec<_> = lock(&self.state).writing.drain(..).collect();
        let wait = self.files.settings().operation_timeout;
        let _ = self.append;
        // The acknowledgements of what was written are awaited before the handle is closed, so a file dropped without
        // a call that observes it still lands.
        self.files.link.rt.handle().spawn(async move {
            for pending in writing {
                let _ = tokio::time::timeout(wait, pending).await;
            }
            let _ = sftp.raw.close(handle).await;
        });
    }
}
