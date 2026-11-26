//! Windows access lists, read and kept through the security API, with no child process.
//!
//! [`read`] answers who owns a file or directory and which accounts the access list allows, as security id text, and
//! which account this process runs as: what `ownership` judges. [`replace_keeping_access`] puts one file over another
//! with `ReplaceFileW`, which leaves the replaced file's own access list on the result, so a layer narrowed below its
//! folder stays narrowed when `bohio peer approve` rewrites it. This module is one of the files that hold `unsafe`.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, GetAce, GetAclInformation, GetSecurityDescriptorDacl,
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_IGNORE_MERGE_ERRORS, ReplaceFileW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `ACCESS_ALLOWED_ACE_TYPE`.
const ALLOWED: u8 = 0;
/// `NO_PROPAGATE_INHERIT_ACE` and `INHERIT_ONLY_ACE`, as .NET's `PropagationFlags` numbers them (1 and 2).
const NO_PROPAGATE: u8 = 0x04;
const INHERIT_ONLY: u8 = 0x08;
/// Full control, for a list that is absent (everyone may do anything).
const FULL: u32 = 0x1F_01FF;

/// What an object's security descriptor says that the owner check judges.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Acl {
    /// The account this process runs as.
    pub(crate) me: String,
    /// The owner.
    pub(crate) owner: String,
    /// The allow entries: the account, the rights, and the propagation flags (1: no propagate, 2: inherit only).
    pub(crate) allowed: Vec<(String, u32, u32)>,
}

/// Frees a block the security API allocated with `LocalAlloc` when dropped.
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from the security API, which allocates with LocalAlloc, and is freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Closes a handle when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by OpenProcessToken and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// The text of the security id `sid`.
///
/// # Safety
///
/// `sid` points at a valid security id.
unsafe fn sid_text(sid: PSID) -> Result<String, String> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: the caller's `sid` is valid and `text` is a place for the allocated string.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return Err(format!(
            "a security id cannot be written: {}",
            io::Error::last_os_error()
        ));
    }
    let _free = Local(text.cast());
    let mut length = 0;
    // SAFETY: the string is NUL-terminated, so reading up to the NUL stays inside it.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `length` units were just read from the string.
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(text, length)
    }))
}

/// The account this process runs as, as security id text.
fn me() -> Result<String, String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the current process pseudo-handle is always valid and `token` is a place for the handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(format!(
            "this process's token cannot be opened: {}",
            io::Error::last_os_error()
        ));
    }
    let token = Handle(token);
    let mut needed = 0_u32;
    // SAFETY: a null buffer of length 0 asks for the size.
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &raw mut needed) };
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8).max(1)];
    // SAFETY: the buffer holds `needed` bytes, aligned for the structure the call writes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    } == 0
    {
        return Err(format!(
            "this process's account cannot be read: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: the call filled the buffer with a TOKEN_USER whose SID points inside it.
    unsafe { sid_text((*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid) }
}

/// The owner and the allow entries of the file or directory `path`, and this process's account.
///
/// # Errors
///
/// Why the security descriptor could not be read.
pub(crate) fn read(path: &Path) -> Result<Acl, String> {
    let name = wide(path);
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated and the out pointers are places for what the call returns.
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            ptr::null_mut(),
            &raw mut dacl,
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(
            io::Error::from_raw_os_error(i32::try_from(status).unwrap_or(i32::MAX)).to_string(),
        );
    }
    // The owner and the list point into this block.
    let _descriptor = Local(descriptor);
    // SAFETY: a successful call with OWNER_SECURITY_INFORMATION returned a valid owner inside the descriptor.
    let owner = unsafe { sid_text(owner) }?;
    let mut listed = Acl {
        me: me()?,
        owner,
        allowed: Vec::new(),
    };
    if dacl.is_null() {
        // No list at all lets everyone do anything.
        listed.allowed.push(("S-1-1-0".to_owned(), FULL, 0));
        return Ok(listed);
    }
    let mut size = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` is a valid list inside the descriptor and `size` is the structure this class fills.
    if unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size).cast(),
            u32::try_from(mem::size_of::<ACL_SIZE_INFORMATION>()).unwrap_or(0),
            AclSizeInformation,
        )
    } == 0
    {
        return Err(format!(
            "the access list cannot be measured: {}",
            io::Error::last_os_error()
        ));
    }
    for index in 0..size.AceCount {
        let mut entry: *mut c_void = ptr::null_mut();
        // SAFETY: `index` is below the entry count of a valid list.
        if unsafe { GetAce(dacl, index, &raw mut entry) } == 0 {
            return Err(format!(
                "an access entry cannot be read: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: every entry starts with its header.
        let header = unsafe { *entry.cast::<ACE_HEADER>() };
        if header.AceType != ALLOWED {
            continue;
        }
        let entry = entry.cast::<ACCESS_ALLOWED_ACE>();
        // SAFETY: an allow entry holds its mask and, from `SidStart`, the id it is for, inside the list.
        let (mask, sid) = unsafe { ((*entry).Mask, (&raw mut (*entry).SidStart).cast::<c_void>()) };
        // SAFETY: `sid` is the entry's own security id.
        let account = unsafe { sid_text(sid) }?;
        let propagation = u32::from(header.AceFlags & NO_PROPAGATE != 0)
            | if header.AceFlags & INHERIT_ONLY != 0 {
                2
            } else {
                0
            };
        listed.allowed.push((account, mask, propagation));
    }
    Ok(listed)
}

/// Puts `replacement` over `replaced`, atomically, and leaves `replaced`'s own access list, attributes and identity
/// on the result (`ReplaceFileW`): a file narrowed below its folder stays narrowed.
///
/// # Errors
///
/// The operating system's error; `replacement` is left where it was then.
pub(crate) fn replace_keeping_access(replaced: &Path, replacement: &Path) -> io::Result<()> {
    let (replaced, replacement) = (wide(replaced), wide(replacement));
    let mut wait = std::time::Duration::from_millis(2);
    loop {
        // SAFETY: both names are NUL-terminated; the backup name and the two reserved pointers are null.
        let done = unsafe {
            ReplaceFileW(
                replaced.as_ptr(),
                replacement.as_ptr(),
                ptr::null(),
                REPLACEFILE_IGNORE_MERGE_ERRORS,
                ptr::null(),
                ptr::null(),
            )
        };
        if done != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // A scanner or another handle may hold the file for a moment: wait a little, as `rename` is retried.
        if error.kind() == io::ErrorKind::PermissionDenied
            && wait <= std::time::Duration::from_millis(512)
        {
            std::thread::sleep(wait);
            wait *= 2;
            continue;
        }
        return Err(error);
    }
}

/// Makes `path` private to this account: a protected access list of this account, SYSTEM and the administrators, each
/// with full control and passed on to what is made inside, and nothing inherited from the parent. No child process is
/// started, so no handle of this process is inherited by one.
///
/// # Errors
///
/// The operating system's error.
pub(crate) fn make_private(path: &Path) -> io::Result<()> {
    let account = me().map_err(io::Error::other)?;
    let sddl: Vec<u16> = format!("D:PAI(A;OICI;FA;;;{account})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and `descriptor` is a place for the allocated descriptor.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let _descriptor = Local(descriptor);
    let (mut present, mut defaulted) = (0_i32, 0_i32);
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: `descriptor` is the valid descriptor just made, and the out pointers are places for what the call returns.
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut name = wide(path);
    // SAFETY: `name` is NUL-terminated and writable, `dacl` points into the descriptor kept alive above, and the owner,
    // group and system list are not set (null).
    let status = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(
            i32::try_from(status).unwrap_or(i32::MAX),
        ));
    }
    Ok(())
}
