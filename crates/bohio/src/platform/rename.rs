//! A rename that refuses an existing target in the call itself, on Windows: `MoveFileExW` without
//! `MOVEFILE_REPLACE_EXISTING` fails with `ERROR_ALREADY_EXISTS` when the name is taken, for a file and for a
//! directory alike, so there is no gap between looking and renaming. This module is one of the files that hold
//! `unsafe`.
//!
//! The call takes paths, not handles: the parents' paths are read from their directory handles at the call, so a
//! peer or local user who can rename a directory inside the root during the call can redirect it, which is the
//! limit `fs/mod.rs` already states for every Windows rename.

#![allow(unsafe_code)]

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW, MoveFileExW, VOLUME_NAME_DOS,
};

/// The path a directory handle names now, without the `\\?\` prefix when it has a drive letter.
fn path_of(handle: &impl AsRawHandle) -> io::Result<PathBuf> {
    let raw = handle.as_raw_handle();
    let mut buffer = vec![0_u16; 512];
    loop {
        // SAFETY: `raw` is a handle the caller's directory owns for the whole call and `buffer` is a valid
        // writable region of the length passed.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                raw,
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length >= buffer.len() {
            buffer.resize(length + 1, 0);
            continue;
        }
        let text = OsString::from_wide(&buffer[..length]);
        let text = text.to_string_lossy();
        let trimmed = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned))
            .unwrap_or_else(|| text.into_owned());
        return Ok(PathBuf::from(trimmed));
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// Renames `from_name` in the directory `from_parent` to `to_name` in `to_parent`, failing with
/// `AlreadyExists` when the target is taken, and with `CrossesDevices` when the two lie on different volumes.
pub(crate) fn rename_no_replace(
    from_parent: &impl AsRawHandle,
    from_name: &std::ffi::OsStr,
    to_parent: &impl AsRawHandle,
    to_name: &std::ffi::OsStr,
) -> io::Result<()> {
    let from = path_of(from_parent)?.join(from_name);
    let to = path_of(to_parent)?.join(to_name);
    let (from, to) = (wide(&from), wide(&to));
    // SAFETY: both are NUL-terminated wide strings that live to the end of the call.
    let moved = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) };
    if moved == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
