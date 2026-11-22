//! Starting a process on Windows so that it holds exactly its three standard streams of this one.
//!
//! The standard library's `Command` starts a child with handle inheritance on, so the child holds
//! every inheritable handle this process has at that instant: `TcpStream::try_clone` makes
//! inheritable sockets, and a connection a child holds neither closes when its owner drops it nor
//! closes cleanly when the child is killed (its peer reads a reset). A handle list
//! (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`) limits what the child inherits to what it names, which
//! `Command` can only do on a nightly compiler. The process also starts inside its job object
//! (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), so nothing it starts in its first instructions is outside the
//! job. This module is one of the files that hold `unsafe`.

#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::io::{self, PipeReader, PipeWriter};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::{
    HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::Spec;

/// A process this module started.
#[derive(Debug)]
pub(super) struct Child {
    process: OwnedHandle,
    id: u32,
    status: Option<ExitStatus>,
}

/// A process and the parent's ends of its standard streams.
pub(super) struct Started {
    pub(super) child: Child,
    pub(super) stdin: Option<PipeWriter>,
    pub(super) stdout: PipeReader,
    pub(super) stderr: PipeReader,
}

impl Child {
    /// The process id.
    pub(super) fn id(&self) -> u32 {
        self.id
    }

    /// The exit status when the process has ended.
    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for(0)
    }

    /// Waits for the process to end.
    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_for(INFINITE)?
            .ok_or_else(|| io::Error::other("the process did not end"))
    }

    fn wait_for(&mut self, milliseconds: u32) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        // SAFETY: the handle is a process handle this value owns and keeps open until it drops.
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), milliseconds) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(None),
            _ => return Err(io::Error::last_os_error()),
        }
        let mut code = 0_u32;
        // SAFETY: as above; `code` is a valid place for the result.
        if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let status = ExitStatus::from_raw(code);
        self.status = Some(status);
        Ok(Some(status))
    }
}

impl AsRawHandle for Child {
    fn as_raw_handle(&self) -> RawHandle {
        self.process.as_raw_handle()
    }
}

/// Starts `spec` inside `job`; the child inherits its three standard streams and nothing else.
///
/// The job holds the process from its first instruction: a process it starts at once is in the job
/// too. A system that cannot put a process in a job at its creation (before Windows 10) makes the
/// start fail.
pub(super) fn start(spec: &Spec, job: &win32job::Job) -> io::Result<Started> {
    let program = resolve(spec)?;
    let mut line = Vec::new();
    append_arg(&mut line, &spec.program, false)?;
    for arg in &spec.args {
        line.push(u16::from(b' '));
        append_arg(&mut line, arg, false)?;
    }
    if let Some(raw) = &spec.raw_args {
        reject_nul(raw)?;
        line.push(u16::from(b' '));
        line.extend(raw.encode_wide());
    }
    line.push(0);
    let environment = environment_block(spec)?;
    let directory = spec.cwd.as_ref().map(|dir| wide(dir.as_os_str()));
    let program = wide(program.as_os_str());

    // The child's standard input is empty unless a pipe was asked for: a pipe whose writer is gone.
    let (child_in, parent_in) = io::pipe()?;
    let parent_in = if spec.stdin {
        Some(parent_in)
    } else {
        drop(parent_in);
        None
    };
    let (parent_out, child_out) = io::pipe()?;
    let (parent_err, child_err) = io::pipe()?;
    let mut handles: [HANDLE; 3] = [
        child_in.as_raw_handle(),
        child_out.as_raw_handle(),
        child_err.as_raw_handle(),
    ];
    for handle in handles {
        make_inheritable(handle)?;
    }

    let mut jobs: [HANDLE; 1] = [job.handle() as HANDLE];
    let attributes = AttributeList::new(&mut handles, &mut jobs)?;
    // SAFETY: an all-zero STARTUPINFOEXW is the documented starting value (every field a number or a null pointer).
    let mut info: STARTUPINFOEXW = unsafe { mem::zeroed() };
    info.StartupInfo.cb = u32::try_from(mem::size_of::<STARTUPINFOEXW>()).unwrap_or(0);
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    info.StartupInfo.hStdInput = handles[0];
    info.StartupInfo.hStdOutput = handles[1];
    info.StartupInfo.hStdError = handles[2];
    info.lpAttributeList = attributes.pointer();
    // SAFETY: an all-zero PROCESS_INFORMATION is the documented starting value.
    let mut created: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: `program`, `line` (mutable, as the call may edit it) and the optional `environment`
    // and `directory` are NUL-terminated UTF-16 that outlive the call; `info` carries the attribute
    // list, which holds the three inheritable handles that `handles` owns and the job that `job`
    // keeps open, until the call returns.
    let ok = unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            environment.as_ptr().cast::<c_void>(),
            directory.as_ref().map_or(ptr::null(), |dir| dir.as_ptr()),
            &raw const info.StartupInfo,
            &raw mut created,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful CreateProcessW hands over the two handles, which nothing else owns.
    let (process, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(created.hProcess),
            OwnedHandle::from_raw_handle(created.hThread),
        )
    };
    // The child's ends belong to the child now: the parent's readers see the end once it closes its.
    drop((child_in, child_out, child_err));
    Ok(Started {
        child: Child {
            process,
            id: created.dwProcessId,
            status: None,
        },
        stdin: parent_in,
        stdout: parent_out,
        stderr: parent_err,
    })
}

fn make_inheritable(handle: HANDLE) -> io::Result<()> {
    // SAFETY: `handle` is open: the caller holds the pipe end it came from.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A process attribute list holding a handle list and a job list.
struct AttributeList {
    /// Aligned storage the operating system fills.
    storage: Vec<usize>,
}

impl AttributeList {
    /// A list that limits the inherited handles to `handles` and puts the process in `jobs`; both
    /// arrays must stay open and in place until the process is created.
    fn new(handles: &mut [HANDLE], jobs: &mut [HANDLE]) -> io::Result<Self> {
        let mut size = 0_usize;
        // SAFETY: a null list with a size place asks for the size the list needs; it fails by design.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &raw mut size) };
        let mut storage = vec![0_usize; size.div_ceil(mem::size_of::<usize>())];
        let pointer: LPPROC_THREAD_ATTRIBUTE_LIST = storage.as_mut_ptr().cast();
        // SAFETY: `pointer` addresses `size` bytes, aligned for a pointer, which this value keeps.
        if unsafe { InitializeProcThreadAttributeList(pointer, 2, 0, &raw mut size) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let list = Self { storage };
        list.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles)?;
        list.set(PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs)?;
        Ok(list)
    }

    /// Sets `attribute` to the handles `values`, which the caller keeps open and in place.
    fn set(&self, attribute: u32, values: &mut [HANDLE]) -> io::Result<()> {
        // SAFETY: the list is initialised with room for this attribute; `values` outlives the
        // process creation the caller does with the list, which reads the array through its pointer.
        let updated = unsafe {
            UpdateProcThreadAttribute(
                self.pointer(),
                0,
                attribute as usize,
                values.as_mut_ptr().cast::<c_void>(),
                mem::size_of_val(values),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if updated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn pointer(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.storage.as_ptr().cast_mut().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if !self.storage.is_empty() {
            // SAFETY: the list was initialised by `new` and is deleted once.
            unsafe { DeleteProcThreadAttributeList(self.pointer()) };
        }
    }
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

fn reject_nul(text: &OsStr) -> io::Result<()> {
    if text.encode_wide().any(|unit| unit == 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a program, argument or variable holds a NUL",
        ));
    }
    Ok(())
}

/// Appends `arg` to a command line as the C runtime's argument parser reads it back.
fn append_arg(line: &mut Vec<u16>, arg: &OsStr, force_quotes: bool) -> io::Result<()> {
    reject_nul(arg)?;
    let quote = force_quotes
        || arg.is_empty()
        || arg
            .encode_wide()
            .any(|unit| unit == u16::from(b' ') || unit == u16::from(b'\t'));
    if quote {
        line.push(u16::from(b'"'));
    }
    let mut backslashes = 0_usize;
    for unit in arg.encode_wide() {
        if unit == u16::from(b'\\') {
            backslashes += 1;
        } else {
            if unit == u16::from(b'"') {
                line.extend((0..=backslashes).map(|_| u16::from(b'\\')));
            }
            backslashes = 0;
        }
        line.push(unit);
    }
    if quote {
        line.extend((0..backslashes).map(|_| u16::from(b'\\')));
        line.push(u16::from(b'"'));
    }
    Ok(())
}

/// The environment the child starts with, as the sorted `NAME=value` block the call takes.
fn environment_block(spec: &Spec) -> io::Result<Vec<u16>> {
    // Names compare without regard to case; the first spelling a name had is kept.
    let mut variables: BTreeMap<Vec<u16>, (OsString, OsString)> = BTreeMap::new();
    let key = |name: &OsStr| -> Vec<u16> {
        name.to_string_lossy()
            .to_uppercase()
            .encode_utf16()
            .collect()
    };
    if !spec.env.clear {
        for (name, value) in std::env::vars_os() {
            variables.insert(key(&name), (name, value));
        }
    }
    for (name, value) in &spec.env.set {
        variables.insert(key(OsStr::new(name)), (name.into(), value.into()));
    }
    for name in &spec.env.remove {
        variables.remove(&key(OsStr::new(name)));
    }
    let mut block = Vec::new();
    for (name, value) in variables.values() {
        reject_nul(name)?;
        reject_nul(value)?;
        // A name may start with `=` (the per-drive directories); only a later `=` is wrong.
        if name
            .encode_wide()
            .skip(1)
            .any(|unit| unit == u16::from(b'='))
            || name.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an environment variable name is empty or holds `=`",
            ));
        }
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// The file `spec.program` names. A name with a directory is that path; a bare name is looked for in
/// the `PATH` the request sets, then the directory of this program, the system directories, and
/// the `PATH` of this process, as the standard library looks, and never in the working directory.
/// `.exe` is tried when the name has no extension. A batch file is refused: `cmd` would read its
/// arguments as commands.
fn resolve(spec: &Spec) -> io::Result<PathBuf> {
    let program = Path::new(&spec.program);
    let names: Vec<OsString> = if program.extension().is_none() {
        let mut exe = spec.program.clone();
        exe.push(".exe");
        vec![exe, spec.program.clone()]
    } else {
        vec![spec.program.clone()]
    };
    let found = if program.components().count() > 1 || program.is_absolute() || program.has_root() {
        names
            .iter()
            .map(PathBuf::from)
            .find(|candidate| candidate.is_file())
    } else {
        let mut directories: Vec<PathBuf> = Vec::new();
        let requested = spec
            .env
            .set
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| OsString::from(value));
        if let Some(path) = &requested {
            directories.extend(std::env::split_paths(path));
        }
        if let Some(own) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned))
        {
            directories.push(own);
        }
        if let Some(root) = std::env::var_os("SystemRoot") {
            let root = PathBuf::from(root);
            directories.push(root.join("System32"));
            directories.push(root);
        }
        if let Some(path) = std::env::var_os("PATH") {
            directories.extend(std::env::split_paths(&path));
        }
        directories
            .iter()
            .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
            .find(|candidate| candidate.is_file())
    };
    let found = found
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the program was not found"))?;
    let batch = found
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        });
    if batch {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a batch file is not started directly: its arguments would be read by cmd; run `cmd /C` on it",
        ));
    }
    Ok(found)
}
