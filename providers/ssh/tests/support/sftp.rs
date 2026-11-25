//! An SFTP subsystem over the real file system, for the in-process server: the requests the provider makes, with the
//! OpenSSH extensions it looks for, and POSIX-looking paths (`/c/Users/x` is `c:/Users/x` where the host is Windows).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, SftpHandle, SftpString,
    Status, StatusCode, Version,
};

/// What the server advertises, so a test can run a server that lacks an extension.
#[derive(Clone, Copy)]
pub struct Advertise {
    pub posix_rename: bool,
    pub hardlink: bool,
    pub statvfs: bool,
    pub fsync: bool,
}

impl Default for Advertise {
    fn default() -> Self {
        Self {
            posix_rename: true,
            hardlink: true,
            statvfs: true,
            fsync: true,
        }
    }
}

/// The file system path a protocol path names.
pub fn native(path: &str) -> PathBuf {
    if cfg!(windows) {
        let bytes = path.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b'/'
        {
            return PathBuf::from(format!("{}:{}", char::from(bytes[1]), &path[2..]));
        }
        if bytes.len() == 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() {
            return PathBuf::from(format!("{}:/", char::from(bytes[1])));
        }
    }
    PathBuf::from(path)
}

enum Open {
    File(std::fs::File),
    Dir { entries: Vec<File>, at: usize },
}

pub struct Sftp {
    handles: HashMap<String, Open>,
    next: u64,
    advertise: Advertise,
    /// Permission bits a client set, where the host has no modes of its own.
    modes: Arc<Mutex<HashMap<PathBuf, u32>>>,
}

impl Sftp {
    pub fn new(advertise: Advertise, modes: Arc<Mutex<HashMap<PathBuf, u32>>>) -> Self {
        Self {
            handles: HashMap::new(),
            next: 0,
            advertise,
            modes,
        }
    }
}

fn status(id: u32, code: StatusCode) -> Status {
    Status {
        id,
        status_code: code,
        error_message: code.to_string(),
        language_tag: "en-US".to_owned(),
    }
}

fn code_of(error: &std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

fn secs(time: std::io::Result<std::time::SystemTime>) -> Option<u32> {
    time.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|after| u32::try_from(after.as_secs()).ok())
}

impl Sftp {
    fn attributes(&self, path: Option<&Path>, metadata: &std::fs::Metadata) -> FileAttributes {
        let kind: u32 = if metadata.is_dir() {
            0o040_000
        } else if metadata.file_type().is_symlink() {
            0o120_000
        } else {
            0o100_000
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            let _ = path;
            metadata.permissions().mode() & 0o7777
        };
        #[cfg(not(unix))]
        let mode = path
            .and_then(|path| self.modes.lock().unwrap().get(path).copied())
            .unwrap_or(if metadata.is_dir() { 0o755 } else { 0o644 });
        FileAttributes {
            size: Some(metadata.len()),
            permissions: Some(kind | mode),
            atime: secs(metadata.accessed()),
            mtime: secs(metadata.modified()),
            ..FileAttributes::default()
        }
    }

    fn apply(&self, path: &Path, attrs: &FileAttributes) -> Result<(), StatusCode> {
        if let Some(size) = attrs.size {
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|error| code_of(&error))?;
            file.set_len(size).map_err(|error| code_of(&error))?;
        }
        if let Some(mode) = attrs.permissions {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
                    .map_err(|error| code_of(&error))?;
            }
            #[cfg(not(unix))]
            self.modes
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), mode & 0o7777);
        }
        if let Some(mtime) = attrs.mtime {
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|error| code_of(&error))?;
            file.set_modified(UNIX_EPOCH + Duration::from_secs(u64::from(mtime)))
                .map_err(|error| code_of(&error))?;
        }
        Ok(())
    }

    fn open_handle(&mut self, prefix: &str, open: Open) -> String {
        self.next += 1;
        let handle = format!("{prefix}{}", self.next);
        self.handles.insert(handle.clone(), open);
        handle
    }
}

/// The SSH strings of an extension request's data.
fn strings(mut data: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    while data.len() >= 4 {
        let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if data.len() < 4 + len {
            break;
        }
        found.push(String::from_utf8_lossy(&data[4..4 + len]).into_owned());
        data = &data[4 + len..];
    }
    found
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        let advertise = self.advertise;
        for (on, name) in [
            (advertise.posix_rename, "posix-rename@openssh.com"),
            (advertise.hardlink, "hardlink@openssh.com"),
            (advertise.statvfs, "statvfs@openssh.com"),
            (advertise.fsync, "fsync@openssh.com"),
        ] {
            if on {
                version.extensions.insert(name.to_owned(), "1".to_owned());
            }
        }
        Ok(version)
    }

    async fn open(
        &mut self,
        id: u32,
        filename: SftpString,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = native(&filename.to_string());
        let mut options = OpenOptions::new();
        options
            .read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE))
            .append(pflags.contains(OpenFlags::APPEND))
            .truncate(pflags.contains(OpenFlags::TRUNCATE));
        if pflags.contains(OpenFlags::CREATE) && pflags.contains(OpenFlags::EXCLUDE) {
            options.create_new(true);
        } else {
            options.create(pflags.contains(OpenFlags::CREATE));
        }
        if path.is_dir() {
            return Err(StatusCode::Failure);
        }
        let existed = path.exists();
        let file = options.open(&path).map_err(|e| code_of(&e))?;
        if !existed {
            if let Some(mode) = attrs.permissions {
                let _ = self.apply(
                    &path,
                    &FileAttributes {
                        permissions: Some(mode),
                        ..FileAttributes::default()
                    },
                );
            }
        }
        let handle = self.open_handle("h", Open::File(file));
        Ok(Handle {
            id,
            handle: SftpHandle::from(handle),
        })
    }

    async fn close(&mut self, id: u32, handle: SftpHandle) -> Result<Status, Self::Error> {
        self.handles
            .remove(&String::from_utf8_lossy(handle.as_bytes()).into_owned());
        Ok(status(id, StatusCode::Ok))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let key = String::from_utf8_lossy(handle.as_bytes()).into_owned();
        let Some(Open::File(file)) = self.handles.get_mut(&key) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| code_of(&e))?;
        let mut data = vec![0u8; len as usize];
        let read = file.read(&mut data).map_err(|e| code_of(&e))?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: SftpHandle,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let key = String::from_utf8_lossy(handle.as_bytes()).into_owned();
        let Some(Open::File(file)) = self.handles.get_mut(&key) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| code_of(&e))?;
        file.write_all(&data).map_err(|e| code_of(&e))?;
        Ok(status(id, StatusCode::Ok))
    }

    async fn fstat(&mut self, id: u32, handle: SftpHandle) -> Result<Attrs, Self::Error> {
        let key = String::from_utf8_lossy(handle.as_bytes()).into_owned();
        let Some(Open::File(file)) = self.handles.get(&key) else {
            return Err(StatusCode::Failure);
        };
        let metadata = file.metadata().map_err(|e| code_of(&e))?;
        Ok(Attrs {
            id,
            attrs: self.attributes(None, &metadata),
        })
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: SftpHandle,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let key = String::from_utf8_lossy(handle.as_bytes()).into_owned();
        let Some(Open::File(file)) = self.handles.get(&key) else {
            return Err(StatusCode::Failure);
        };
        if let Some(size) = attrs.size {
            file.set_len(size).map_err(|e| code_of(&e))?;
        }
        if let Some(mtime) = attrs.mtime {
            file.set_modified(UNIX_EPOCH + Duration::from_secs(u64::from(mtime)))
                .map_err(|e| code_of(&e))?;
        }
        Ok(status(id, StatusCode::Ok))
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: SftpString,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.apply(&native(&path.to_string()), &attrs)?;
        Ok(status(id, StatusCode::Ok))
    }

    async fn stat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        let path = native(&path.to_string());
        let metadata = std::fs::metadata(&path).map_err(|e| code_of(&e))?;
        Ok(Attrs {
            id,
            attrs: self.attributes(Some(&path), &metadata),
        })
    }

    async fn lstat(&mut self, id: u32, path: SftpString) -> Result<Attrs, Self::Error> {
        let path = native(&path.to_string());
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| code_of(&e))?;
        Ok(Attrs {
            id,
            attrs: self.attributes(Some(&path), &metadata),
        })
    }

    async fn opendir(&mut self, id: u32, path: SftpString) -> Result<Handle, Self::Error> {
        let path = native(&path.to_string());
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path).map_err(|e| code_of(&e))? {
            let entry = entry.map_err(|e| code_of(&e))?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|e| code_of(&e))?;
            entries.push(File::new(
                entry.file_name().to_string_lossy().into_owned(),
                self.attributes(Some(&entry.path()), &metadata),
            ));
        }
        let handle = self.open_handle("d", Open::Dir { entries, at: 0 });
        Ok(Handle {
            id,
            handle: SftpHandle::from(handle),
        })
    }

    async fn readdir(&mut self, id: u32, handle: SftpHandle) -> Result<Name, Self::Error> {
        let key = String::from_utf8_lossy(handle.as_bytes()).into_owned();
        let Some(Open::Dir { entries, at }) = self.handles.get_mut(&key) else {
            return Err(StatusCode::Failure);
        };
        // Pages of a few entries, as a server's are, so a listing is more than one request.
        if *at >= entries.len() {
            return Err(StatusCode::Eof);
        }
        let end = (*at + 3).min(entries.len());
        let files = entries[*at..end].to_vec();
        *at = end;
        Ok(Name { id, files })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: SftpString,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = native(&path.to_string());
        std::fs::create_dir(&path).map_err(|e| code_of(&e))?;
        if attrs.permissions.is_some() {
            let _ = self.apply(&path, &attrs);
        }
        Ok(status(id, StatusCode::Ok))
    }

    async fn rmdir(&mut self, id: u32, path: SftpString) -> Result<Status, Self::Error> {
        std::fs::remove_dir(native(&path.to_string())).map_err(|e| code_of(&e))?;
        Ok(status(id, StatusCode::Ok))
    }

    async fn remove(&mut self, id: u32, filename: SftpString) -> Result<Status, Self::Error> {
        let path = native(&filename.to_string());
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| code_of(&e))?;
        if metadata.is_dir() {
            return Err(StatusCode::Failure);
        }
        // A link to a directory is removed as a directory on Windows.
        std::fs::remove_file(&path)
            .or_else(|_| std::fs::remove_dir(&path))
            .map_err(|e| code_of(&e))?;
        Ok(status(id, StatusCode::Ok))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: SftpString,
        newpath: SftpString,
    ) -> Result<Status, Self::Error> {
        let (old, new) = (native(&oldpath.to_string()), native(&newpath.to_string()));
        // The protocol's rename does not replace.
        if std::fs::symlink_metadata(&new).is_ok() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(old, new).map_err(|e| code_of(&e))?;
        Ok(status(id, StatusCode::Ok))
    }

    async fn realpath(&mut self, id: u32, path: SftpString) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }

    async fn readlink(&mut self, id: u32, path: SftpString) -> Result<Name, Self::Error> {
        let target = std::fs::read_link(native(&path.to_string())).map_err(|e| code_of(&e))?;
        Ok(Name {
            id,
            files: vec![File::dummy(target.to_string_lossy().replace('\\', "/"))],
        })
    }

    async fn symlink(
        &mut self,
        id: u32,
        linkpath: SftpString,
        targetpath: SftpString,
    ) -> Result<Status, Self::Error> {
        // OpenSSH's server reads the first field of the request as the target and the second as the link, whatever the
        // draft's names say.
        let (target, link) = (targetpath_first(&linkpath), native(&targetpath.to_string()));
        if std::fs::symlink_metadata(&link).is_ok() {
            return Err(StatusCode::Failure);
        }
        #[cfg(windows)]
        {
            let resolved = link
                .parent()
                .map(|dir| dir.join(&target))
                .unwrap_or_default();
            let made = if resolved.is_dir() {
                std::os::windows::fs::symlink_dir(&target, &link)
            } else {
                std::os::windows::fs::symlink_file(&target, &link)
            };
            made.map_err(|e| code_of(&e))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).map_err(|e| code_of(&e))?;
        Ok(status(id, StatusCode::Ok))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        let given = strings(&data);
        match (request.as_str(), given.as_slice()) {
            ("posix-rename@openssh.com", [old, new]) if self.advertise.posix_rename => {
                std::fs::rename(native(old), native(new)).map_err(|e| code_of(&e))?;
                Ok(Packet::Status(status(id, StatusCode::Ok)))
            }
            ("hardlink@openssh.com", [old, new]) if self.advertise.hardlink => {
                std::fs::hard_link(native(old), native(new)).map_err(|e| code_of(&e))?;
                Ok(Packet::Status(status(id, StatusCode::Ok)))
            }
            ("fsync@openssh.com", [_]) if self.advertise.fsync => {
                Ok(Packet::Status(status(id, StatusCode::Ok)))
            }
            ("statvfs@openssh.com", [path]) if self.advertise.statvfs => {
                std::fs::metadata(native(path)).map_err(|e| code_of(&e))?;
                let space = russh_sftp::extensions::Statvfs {
                    block_size: 4096,
                    fragment_size: 4096,
                    blocks: 1_000_000,
                    blocks_free: 500_000,
                    blocks_avail: 400_000,
                    inodes: 100_000,
                    inodes_free: 50_000,
                    inodes_avail: 40_000,
                    fs_id: 7,
                    flags: 0,
                    name_max: 255,
                };
                let bytes = russh_sftp::ser::to_bytes(&space).map_err(|_| StatusCode::Failure)?;
                Ok(Packet::ExtendedReply(russh_sftp::protocol::ExtendedReply {
                    id,
                    data: bytes.to_vec(),
                }))
            }
            _ => Err(StatusCode::OpUnsupported),
        }
    }
}

fn targetpath_first(text: &SftpString) -> String {
    text.to_string()
}
