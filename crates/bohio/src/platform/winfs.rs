//! Filesystem calls on Windows that act on one name inside a directory handle: make a directory, remove,
//! rename, read and make a link, set an attribute, and list. Each opens the name relative to the parent's
//! handle (`NtCreateFile` with `RootDirectory`) without following a reparse point, then works through the
//! handle it got, so no path is built and a directory swapped for a junction between two calls cannot
//! redirect the one that is running. This module is one of the files that hold `unsafe`.
//!
//! A name is a single component: one that holds a separator, a stream suffix or is `.` or `..` is
//! refused, because `NtCreateFile` follows the reparse points in a multi-component name.

#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION,
    FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, FileRenameInformationEx, NtCreateFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED,
    HANDLE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO,
    FILE_DISPOSITION_INFO_EX, FILE_FULL_DIR_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo, FileBasicInfo,
    FileDispositionInfo, FileDispositionInfoEx, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};

/// `FILE_LIST_DIRECTORY`: the right to read a directory's entries.
const LIST_DIRECTORY: u32 = 0x0000_0001;
/// `FILE_FLAG_..` not needed: `OBJ_CASE_INSENSITIVE`, so a name matches as it does through Win32.
const CASE_INSENSITIVE: u32 = 0x40;
const FSCTL_GET_REPARSE_POINT: u32 = 0x0009_00A8;
const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const SYMLINK_FLAG_RELATIVE: u32 = 1;
const MAX_REPARSE_DATA: usize = 16 * 1024;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
/// `FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS`: the name is gone when the call
/// returns, even while another handle to the file is open.
const DELETE_NOW: u32 = 0x1 | 0x2;
/// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` and `FILE_RENAME_FLAG_POSIX_SEMANTICS`.
const RENAME_REPLACE: u32 = 0x1;
const RENAME_POSIX: u32 = 0x2;

fn bad_name() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "a name must be one component without a separator or a stream",
    )
}

/// The UTF-16 of `name`, which must be one plain component.
fn wide_name(name: &OsStr) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let refused = wide.is_empty()
        || wide.len() > usize::from(u16::MAX) / 2
        || wide.iter().any(|&unit| {
            unit == 0
                || unit == u16::from(b'\\')
                || unit == u16::from(b'/')
                || unit == u16::from(b':')
        });
    if refused || wide == [u16::from(b'.')] || wide == [u16::from(b'.'), u16::from(b'.')] {
        return Err(bad_name());
    }
    Ok(wide)
}

fn nt_error(status: i32) -> io::Error {
    // SAFETY: a pure translation of a status code; nothing is read or written through a pointer.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))
}

/// What an open is for.
struct Open {
    access: u32,
    /// `FILE_OPEN` or `FILE_CREATE`.
    disposition: u32,
    /// `FILE_DIRECTORY_FILE` or `FILE_NON_DIRECTORY_FILE`, or neither.
    kind: u32,
}

/// Opens `name` inside `parent` without following a reparse point on it.
fn open_at(parent: &impl AsHandle, name: &OsStr, how: &Open) -> io::Result<OwnedHandle> {
    let mut wide = wide_name(name)?;
    let bytes = u16::try_from(wide.len() * 2).map_err(|_| bad_name())?;
    let object = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(std::mem::size_of::<OBJECT_ATTRIBUTES>()).unwrap_or(0),
        RootDirectory: parent.as_handle().as_raw_handle(),
        ObjectName: &raw const object,
        Attributes: CASE_INSENSITIVE,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle: HANDLE = ptr::null_mut();
    // SAFETY: `attributes` points at `object`, whose buffer is `wide`; all three, `status_block` and
    // `handle` are locals that outlive the call, and `parent` is a handle the caller keeps open.
    let status = unsafe {
        NtCreateFile(
            &raw mut handle,
            how.access | SYNCHRONIZE,
            &raw const attributes,
            &raw mut status_block,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            SHARE_ALL,
            how.disposition,
            how.kind
                | FILE_OPEN_REPARSE_POINT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: the call succeeded, so `handle` is a new handle that nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn open_existing(
    parent: &impl AsHandle,
    name: &OsStr,
    access: u32,
    kind: u32,
) -> io::Result<OwnedHandle> {
    open_at(
        parent,
        name,
        &Open {
            access,
            disposition: FILE_OPEN,
            kind,
        },
    )
}

/// The attributes and the reparse tag of an open file.
fn attributes_of(handle: &OwnedHandle) -> io::Result<(u32, u32)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is a writable value of the size passed, and `handle` is open.
    let done = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            u32::try_from(std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>()).unwrap_or(0),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.FileAttributes, info.ReparseTag))
}

/// Makes the directory `name` in `parent`, failing with `AlreadyExists` when the name is taken.
pub(crate) fn create_dir(parent: &impl AsHandle, name: &OsStr) -> io::Result<()> {
    open_at(
        parent,
        name,
        &Open {
            access: FILE_READ_ATTRIBUTES,
            disposition: FILE_CREATE,
            kind: FILE_DIRECTORY_FILE,
        },
    )
    .map(drop)
}

/// What [`remove`] takes away.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Remove {
    /// An empty directory, or a link to one.
    Directory,
    /// A file, or a link of either kind; a directory is refused.
    FileOrLink,
}

/// Marks an open file or directory for removal; it goes when the handle closes, or at once.
fn dispose(handle: &OwnedHandle) -> io::Result<()> {
    let extended = FILE_DISPOSITION_INFO_EX { Flags: DELETE_NOW };
    // SAFETY: `extended` is a readable value of the size passed, and `handle` is open with `DELETE`.
    let done = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            FileDispositionInfoEx,
            (&raw const extended).cast(),
            u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO_EX>()).unwrap_or(0),
        )
    };
    if done != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if !matches!(
        error.raw_os_error().map(i32::unsigned_abs),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
    ) {
        return Err(error);
    }
    // A filesystem without the extended call: the classic one, which removes when the handle closes.
    let classic: u32 = 1;
    // SAFETY: as above, with a `BOOLEAN`-sized value the classic call reads.
    let done = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            FileDispositionInfo,
            (&raw const classic).cast(),
            1,
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes `name` from `parent`: the entry itself, never what a link points to.
pub(crate) fn remove(parent: &impl AsHandle, name: &OsStr, what: Remove) -> io::Result<()> {
    let kind = if what == Remove::Directory {
        FILE_DIRECTORY_FILE
    } else {
        0
    };
    let handle = open_existing(parent, name, DELETE | FILE_READ_ATTRIBUTES, kind)?;
    let (attributes, _) = attributes_of(&handle)?;
    let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if what == Remove::FileOrLink && is_directory && attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
    {
        return Err(io::Error::from(io::ErrorKind::IsADirectory));
    }
    dispose(&handle)
}

/// Removes `name` and everything in it, depth first through handles. A link is removed and not entered.
pub(crate) fn remove_tree(parent: &impl AsHandle, name: &OsStr) -> io::Result<()> {
    let open_directory = |parent: &OwnedHandle, name: &OsStr| {
        open_existing(
            parent,
            name,
            DELETE | LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_DIRECTORY_FILE,
        )
    };
    let first = open_existing(
        parent,
        name,
        DELETE | LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
        FILE_DIRECTORY_FILE,
    )?;
    let (attributes, _) = attributes_of(&first)?;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return dispose(&first);
    }
    let mut stack = vec![first];
    while let Some(top) = stack.last() {
        let mut descended = false;
        let mut found = false;
        let names: Vec<(OsString, u32)> = {
            let mut listing = Listing::new(top);
            let mut batch = Vec::new();
            for entry in listing.by_ref().take(256) {
                batch.push(entry?);
            }
            batch
        };
        for (child, child_attributes) in names {
            found = true;
            let plain_directory = child_attributes & FILE_ATTRIBUTE_DIRECTORY != 0
                && child_attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0;
            if plain_directory {
                let handle = open_directory(top, &child)?;
                // What the handle says wins over what the listing said a moment ago.
                if attributes_of(&handle)?.0 & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                    stack.push(handle);
                    descended = true;
                    break;
                }
                dispose(&handle)?;
            } else {
                remove(top, &child, Remove::FileOrLink).or_else(|error| {
                    // A directory link made the listing's attributes look like a plain file's.
                    remove(top, &child, Remove::Directory).map_err(|_| error)
                })?;
            }
        }
        if descended {
            continue;
        }
        if !found && let Some(done) = stack.pop() {
            dispose(&done)?;
        }
    }
    Ok(())
}

/// Moves `from_name` of `from_parent` to `to_name` of `to_parent` in one call, replacing what is there only
/// with `replace`. `CrossesDevices` when the two lie on different volumes.
pub(crate) fn rename(
    from_parent: &impl AsHandle,
    from_name: &OsStr,
    to_parent: &impl AsHandle,
    to_name: &OsStr,
    replace: bool,
) -> io::Result<()> {
    let source = open_existing(from_parent, from_name, DELETE | FILE_READ_ATTRIBUTES, 0)?;
    let target = wide_name(to_name)?;
    let flags = RENAME_POSIX | if replace { RENAME_REPLACE } else { 0 };
    let root = to_parent.as_handle().as_raw_handle();
    match send_rename(&source, root, &target, FileRenameInformationEx, flags) {
        Err(error)
            if matches!(
                error.raw_os_error().map(i32::unsigned_abs),
                Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
            ) =>
        {
            // The filesystem has no extended rename: the classic one, which replaces only when told to.
            send_rename(
                &source,
                root,
                &target,
                FileRenameInformation,
                u32::from(replace),
            )
        }
        other => other,
    }
}

/// `NtSetInformationFile` with a `FILE_RENAME_INFORMATION` whose first field is `first` (the flags of the
/// extended form, `ReplaceIfExists` of the classic one). The Win32 `SetFileInformationByHandle` refuses a
/// `RootDirectory`, the system call takes it.
fn send_rename(
    source: &OwnedHandle,
    root: HANDLE,
    name: &[u16],
    class: i32,
    first: u32,
) -> io::Result<()> {
    let name_offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let root_offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, RootDirectory);
    let length_offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileNameLength);
    let name_bytes = name.len() * 2;
    let total = name_offset + name_bytes + 2;
    // Eight-byte units, so the buffer is aligned for the `HANDLE` it holds.
    let mut storage = vec![0_u64; total.div_ceil(8)];
    {
        let bytes = bytes_mut(&mut storage);
        bytes[..4].copy_from_slice(&first.to_le_bytes());
        bytes[root_offset..root_offset + std::mem::size_of::<HANDLE>()]
            .copy_from_slice(&(root as usize).to_le_bytes());
        bytes[length_offset..length_offset + 4].copy_from_slice(
            &u32::try_from(name_bytes)
                .map_err(|_| bad_name())?
                .to_le_bytes(),
        );
        for (index, unit) in name.iter().enumerate() {
            let at = name_offset + index * 2;
            bytes[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `storage` holds a `FILE_RENAME_INFORMATION` of `total` bytes, aligned, whose root handle the
    // caller keeps open and whose name is inside the buffer; `source` is open with `DELETE`.
    let status = unsafe {
        NtSetInformationFile(
            source.as_raw_handle(),
            &raw mut status_block,
            storage.as_ptr().cast(),
            u32::try_from(total).map_err(|_| bad_name())?,
            class,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    Ok(())
}

/// A writable byte view of eight-byte-aligned storage.
fn bytes_mut(storage: &mut [u64]) -> &mut [u8] {
    // SAFETY: a `u64` slice is `len * 8` initialised bytes, and `u8` has no alignment need.
    unsafe { std::slice::from_raw_parts_mut(storage.as_mut_ptr().cast::<u8>(), storage.len() * 8) }
}

/// A byte view of eight-byte-aligned storage.
fn bytes(storage: &[u64]) -> &[u8] {
    // SAFETY: as in `bytes_mut`.
    unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), storage.len() * 8) }
}

fn read_u16(buffer: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buffer.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(buffer: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buffer.get(at..at + 4)?.try_into().ok()?))
}

fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the reparse data is malformed")
}

/// The text of the link `name` in `parent`, as `read_link` of the standard library gives it: a relative
/// target as it was written, an absolute one without the `\??\` prefix.
pub(crate) fn read_link(parent: &impl AsHandle, name: &OsStr) -> io::Result<PathBuf> {
    let handle = open_existing(parent, name, FILE_READ_ATTRIBUTES, 0)?;
    let mut storage = vec![0_u64; MAX_REPARSE_DATA / 8];
    let mut returned = 0_u32;
    // SAFETY: the output buffer is `storage`, writable for the length passed, and `returned` is a local.
    let done = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            storage.as_mut_ptr().cast(),
            u32::try_from(MAX_REPARSE_DATA).unwrap_or(0),
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    let buffer = &bytes(&storage)[..usize::try_from(returned).unwrap_or(0).min(MAX_REPARSE_DATA)];
    let tag = read_u32(buffer, 0).ok_or_else(malformed)?;
    let (names_at, relative) = match tag {
        IO_REPARSE_TAG_SYMLINK => (
            20,
            read_u32(buffer, 16).ok_or_else(malformed)? & SYMLINK_FLAG_RELATIVE != 0,
        ),
        IO_REPARSE_TAG_MOUNT_POINT => (16, false),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an unsupported kind of reparse point",
            ));
        }
    };
    let offset = usize::from(read_u16(buffer, 8).ok_or_else(malformed)?);
    let length = usize::from(read_u16(buffer, 10).ok_or_else(malformed)?);
    let text = buffer
        .get(names_at + offset..names_at + offset + length)
        .ok_or_else(malformed)?;
    let mut wide: Vec<u16> = text
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    if !relative && wide.starts_with(&prefix) {
        wide.drain(..prefix.len());
    }
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

/// Makes the link `name` in `parent` to `target` (a file link, or a directory link with `directory`). It is
/// made empty and given its target through the handle that made it, so nothing else names it between.
pub(crate) fn symlink(
    parent: &impl AsHandle,
    name: &OsStr,
    target: &OsStr,
    directory: bool,
) -> io::Result<()> {
    let handle = open_at(
        parent,
        name,
        &Open {
            access: DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            disposition: FILE_CREATE,
            kind: if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            },
        },
    )?;
    let printed: Vec<u16> = target.encode_wide().collect();
    let absolute = std::path::Path::new(target).is_absolute();
    let substitute: Vec<u16> = if absolute {
        r"\??\"
            .encode_utf16()
            .chain(printed.iter().copied())
            .collect()
    } else {
        printed.clone()
    };
    let (sub_bytes, print_bytes) = (substitute.len() * 2, printed.len() * 2);
    let data_length = 12 + sub_bytes + print_bytes;
    let total = 8 + data_length;
    if data_length > MAX_REPARSE_DATA {
        let _ = dispose(&handle);
        return Err(bad_name());
    }
    let mut storage = vec![0_u64; total.div_ceil(8)];
    {
        let out = bytes_mut(&mut storage);
        let mut put = |at: usize, value: &[u8]| out[at..at + value.len()].copy_from_slice(value);
        put(0, &IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        put(4, &u16::try_from(data_length).unwrap_or(0).to_le_bytes());
        put(8, &0_u16.to_le_bytes());
        put(10, &u16::try_from(sub_bytes).unwrap_or(0).to_le_bytes());
        put(12, &u16::try_from(sub_bytes).unwrap_or(0).to_le_bytes());
        put(14, &u16::try_from(print_bytes).unwrap_or(0).to_le_bytes());
        put(
            16,
            &(if absolute { 0 } else { SYMLINK_FLAG_RELATIVE }).to_le_bytes(),
        );
        for (index, unit) in substitute.iter().chain(printed.iter()).enumerate() {
            put(20 + index * 2, &unit.to_le_bytes());
        }
    }
    let mut returned = 0_u32;
    // SAFETY: the input is `total` bytes of `storage`, readable for the length passed; `handle` is open.
    let done = unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            FSCTL_SET_REPARSE_POINT,
            storage.as_ptr().cast(),
            u32::try_from(total).unwrap_or(0),
            ptr::null_mut(),
            0,
            &raw mut returned,
            ptr::null_mut(),
        )
    };
    if done == 0 {
        let error = io::Error::last_os_error();
        let _ = dispose(&handle);
        return Err(error);
    }
    Ok(())
}

/// Sets or clears the read-only attribute of `name` in `parent`: of the entry itself, never of what a link
/// points to.
pub(crate) fn set_readonly(parent: &impl AsHandle, name: &OsStr, readonly: bool) -> io::Result<()> {
    let handle = open_existing(
        parent,
        name,
        FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
        0,
    )?;
    let (current, _) = attributes_of(&handle)?;
    let mut attributes = current & !FILE_ATTRIBUTE_DIRECTORY & !FILE_ATTRIBUTE_REPARSE_POINT;
    if readonly {
        attributes |= FILE_ATTRIBUTE_READONLY;
    } else {
        attributes &= !FILE_ATTRIBUTE_READONLY;
    }
    if attributes == 0 {
        attributes = FILE_ATTRIBUTE_NORMAL;
    }
    // Times of zero leave the times as they are.
    let info = FILE_BASIC_INFO {
        CreationTime: 0,
        LastAccessTime: 0,
        LastWriteTime: 0,
        ChangeTime: 0,
        FileAttributes: attributes,
    };
    // SAFETY: `info` is a readable value of the size passed, and `handle` is open with write-attributes.
    let done = unsafe {
        SetFileInformationByHandle(
            handle.as_raw_handle(),
            FileBasicInfo,
            (&raw const info).cast(),
            u32::try_from(std::mem::size_of::<FILE_BASIC_INFO>()).unwrap_or(0),
        )
    };
    if done == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The entries of an open directory, read from its handle a buffer at a time: each name with its
/// attributes. `.` and `..` are left out. The handle's own position is used, so a directory is listed
/// through a handle nothing else lists through.
pub(crate) struct Listing<H: AsHandle> {
    directory: H,
    storage: Vec<u64>,
    at: usize,
    end: usize,
    restart: bool,
    done: bool,
}

impl<H: AsHandle> Listing<H> {
    /// Starts at the first entry of `directory`.
    pub(crate) fn new(directory: H) -> Self {
        Self {
            directory,
            storage: vec![0_u64; 8 * 1024],
            at: 0,
            end: 0,
            restart: true,
            done: false,
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        let class = if self.restart {
            FileFullDirectoryRestartInfo
        } else {
            FileFullDirectoryInfo
        };
        self.restart = false;
        let size = u32::try_from(self.storage.len() * 8).unwrap_or(0);
        // SAFETY: the buffer is `storage`, aligned and writable for `size` bytes; the handle is open.
        let done = unsafe {
            GetFileInformationByHandleEx(
                self.directory.as_handle().as_raw_handle(),
                class,
                self.storage.as_mut_ptr().cast(),
                size,
            )
        };
        if done == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error().map(i32::unsigned_abs) == Some(ERROR_NO_MORE_FILES) {
                return Ok(false);
            }
            return Err(error);
        }
        self.at = 0;
        self.end = self.storage.len() * 8;
        Ok(true)
    }
}

impl<H: AsHandle> Iterator for Listing<H> {
    type Item = io::Result<(OsString, u32)>;

    fn next(&mut self) -> Option<Self::Item> {
        let name_at = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName);
        let attributes_at = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileAttributes);
        let length_at = std::mem::offset_of!(FILE_FULL_DIR_INFO, FileNameLength);
        loop {
            if self.done {
                return None;
            }
            if self.at >= self.end {
                match self.fill() {
                    Ok(true) => {}
                    Ok(false) => {
                        self.done = true;
                        return None;
                    }
                    Err(error) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                }
            }
            let buffer = bytes(&self.storage);
            let entry = &buffer[self.at..];
            let parsed = (|| {
                let next = usize::try_from(read_u32(entry, 0)?).ok()?;
                let attributes = read_u32(entry, attributes_at)?;
                let length = usize::try_from(read_u32(entry, length_at)?).ok()?;
                let text = entry.get(name_at..name_at + length)?;
                let wide: Vec<u16> = text
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair))
                    .collect();
                Some((next, attributes, OsString::from_wide(&wide)))
            })();
            let Some((next, attributes, name)) = parsed else {
                self.done = true;
                return Some(Err(malformed()));
            };
            // The last entry of a buffer says zero; the next fill starts a new buffer.
            if next == 0 {
                self.at = self.end;
            } else {
                self.at += next;
            }
            if name == "." || name == ".." {
                continue;
            }
            return Some(Ok((name, attributes)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    use super::{
        Listing, Remove, create_dir, read_link, remove, remove_tree, rename, set_readonly, symlink,
    };

    /// `FILE_FLAG_BACKUP_SEMANTICS`, which opening a directory needs.
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const NAMES: [&str; 3] = ["victim.txt", "mv_from", "ro.txt"];

    /// `root\a\b` and `outside\b` hold the same names; `parent` is a handle on `root\a\b`.
    struct World {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        parent: std::fs::File,
    }

    fn junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let outside = tmp.path().join("outside");
        for base in [root.join("a").join("b"), outside.join("b")] {
            std::fs::create_dir_all(base.join("tree").join("deep")).unwrap();
            for name in NAMES {
                std::fs::write(base.join(name), b"x").unwrap();
            }
            std::fs::write(base.join("tree").join("deep").join("leaf"), b"x").unwrap();
        }
        let parent = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(BACKUP_SEMANTICS)
            .open(root.join("a").join("b"))
            .unwrap();
        World {
            _tmp: tmp,
            root,
            outside,
            parent,
        }
    }

    fn names(directory: &Path) -> BTreeSet<String> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn outside_is_untouched(world: &World) {
        let outside = world.outside.join("b");
        let expected: BTreeSet<String> = NAMES
            .iter()
            .map(|name| (*name).to_owned())
            .chain(["tree".to_owned()])
            .collect();
        assert_eq!(names(&outside), expected, "the outside directory changed");
        assert!(outside.join("tree").join("deep").join("leaf").exists());
        assert!(
            !std::fs::metadata(outside.join("ro.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
    }

    #[test]
    fn every_call_acts_on_the_directory_the_handle_names() {
        let world = world();
        let moved = world.root.join("a").join("b");
        let parent = &world.parent;

        create_dir(parent, "made".as_ref()).unwrap();
        remove(parent, "victim.txt".as_ref(), Remove::FileOrLink).unwrap();
        rename(parent, "mv_from".as_ref(), parent, "mv_to".as_ref(), false).unwrap();
        set_readonly(parent, "ro.txt".as_ref(), true).unwrap();
        remove_tree(parent, "tree".as_ref()).unwrap();
        let listed: BTreeSet<String> = Listing::new(parent)
            .map(|entry| entry.unwrap().0.to_string_lossy().into_owned())
            .collect();
        // A link needs a privilege some accounts lack: the rest is checked either way.
        let linked = symlink(parent, "lnk".as_ref(), "ro.txt".as_ref(), false);
        if linked.is_ok() {
            assert_eq!(
                read_link(parent, "lnk".as_ref()).unwrap(),
                Path::new("ro.txt")
            );
        } else {
            eprintln!("SKIPPED: no symbolic link here: {linked:?}");
        }

        outside_is_untouched(&world);
        let expected: BTreeSet<String> = ["made", "ro.txt", "mv_to"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(listed, expected);
        assert!(moved.join("made").is_dir());
        assert!(!moved.join("victim.txt").exists());
        assert!(moved.join("mv_to").exists() && !moved.join("mv_from").exists());
        assert!(!moved.join("tree").exists());
        assert!(
            std::fs::metadata(moved.join("ro.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
    }

    #[test]
    fn a_junction_is_removed_and_not_entered() {
        let world = world();
        let link = world.root.join("a").join("b").join("jl");
        assert!(junction(&link, &world.outside.join("b")));
        remove_tree(&world.parent, "jl".as_ref()).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        let again = world.root.join("a").join("b").join("jd");
        assert!(junction(&again, &world.outside.join("b")));
        remove(&world.parent, "jd".as_ref(), Remove::Directory).unwrap();
        assert!(std::fs::symlink_metadata(&again).is_err());
        outside_is_untouched(&world);
    }

    #[test]
    fn a_name_is_one_plain_component() {
        let world = world();
        for bad in ["", ".", "..", "x\\y", "x/y", "x:y", "a\\..\\b"] {
            let error = create_dir(&world.parent, bad.as_ref()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
        outside_is_untouched(&world);
    }

    #[test]
    fn a_listing_reads_every_buffer() {
        let world = world();
        let base = world.root.join("a").join("b");
        for index in 0..2500 {
            std::fs::write(
                base.join(format!("entry_{index:05}_with_a_longer_name.dat")),
                b"",
            )
            .unwrap();
        }
        let listed: Vec<_> = Listing::new(&world.parent).collect();
        assert!(listed.iter().all(Result::is_ok));
        assert_eq!(listed.len(), 2500 + 4);
    }

    #[test]
    fn a_rename_without_replace_refuses_an_existing_target_and_with_it_replaces() {
        let world = world();
        let error = rename(
            &world.parent,
            "mv_from".as_ref(),
            &world.parent,
            "ro.txt".as_ref(),
            false,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        rename(
            &world.parent,
            "mv_from".as_ref(),
            &world.parent,
            "ro.txt".as_ref(),
            true,
        )
        .unwrap();
        assert!(!world.root.join("a").join("b").join("mv_from").exists());
    }
}
