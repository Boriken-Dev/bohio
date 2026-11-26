//! Ending a started program's tree when this process dies without ending it (Unix).
//!
//! A host that is killed cannot run any cleanup, and the kernel ends only what it is told to: a
//! parent-death signal (`PR_SET_PDEATHSIG`, `PROC_PDEATHSIG_CTL`) reaches the one child it was set
//! on, never that child's descendants, and macOS has none; a terminal's hangup reaches only the
//! foreground group of a shell, and a shell that gives each background job a group of its own
//! (`dash`, `bash` with job control) keeps them out of it. So every program has a guardian: a
//! process that blocks reading a pipe only this process holds the write end of. When this process
//! ends, the kernel closes the write end, the guardian reads end-of-file and signals the tree.
//! The mechanism is the same on Linux, FreeBSD and macOS.
//!
//! * A program of `exec` leads a process group of its own, and its guardian is made between the
//!   fork and the exec of the child, so there is no moment in which the program runs without one;
//!   it signals the group.
//! * A shell on a terminal is started by `portable-pty`, which offers no hook between fork and exec,
//!   so its guardian is made right after the start, from this process; it signals the shell's group
//!   and, on Linux, every process of the shell's session, which is where a job in a group of its own
//!   stays.
//!
//! A guardian is a process forked from a process that has other threads, so it calls only system
//! calls and never allocates. It is made as a grandchild, so a program that waits for all its
//! children never waits for it, and it closes every descriptor but its pipe, so it holds no socket
//! or pipe of the host. A process that left the group (and the session) is out of reach: it is
//! ended only when the host ends the tree itself. This module is one of the files that hold `unsafe`.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::OnceLock;

use libc::c_int;

use crate::config::HostDeath;

/// The name a guardian gives itself (`PR_SET_NAME`), by which a host that adopts it as process 1 recognises it when it
/// ends: nothing else is collected there.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) const NAME: &str = "bohio-guard";
#[cfg(any(target_os = "linux", target_os = "android"))]
const NAME_C: &std::ffi::CStr = c"bohio-guard";

/// The pipe whose write end this process alone holds, and the read end each program's guardian
/// waits on. Both ends close on `exec`, so no program holds either.
struct Lifeline {
    read: OwnedFd,
    /// Held for the life of the process, so the kernel closes it when the process ends.
    _write: OwnedFd,
}

fn lifeline() -> io::Result<&'static Lifeline> {
    static LIFELINE: OnceLock<Lifeline> = OnceLock::new();
    if let Some(lifeline) = LIFELINE.get() {
        return Ok(lifeline);
    }
    let (read, write) = pipe()?;
    Ok(LIFELINE.get_or_init(|| Lifeline {
        read,
        _write: write,
    }))
}

/// A pipe whose ends close on `exec`. Apple's systems have no atomic way to ask for that, so a
/// program started by another thread between the two calls could keep an end open.
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(not(target_vendor = "apple"))]
    {
        Ok(rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?)
    }
    #[cfg(target_vendor = "apple")]
    {
        let (read, write) = rustix::pipe::pipe()?;
        for end in [&read, &write] {
            rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC)?;
        }
        Ok((read, write))
    }
}

/// The signal a guardian sends, or `None` for a host that leaves its programs running.
fn signal(on_death: HostDeath) -> Option<c_int> {
    match on_death {
        HostDeath::Kill => Some(libc::SIGKILL),
        HostDeath::Term => Some(libc::SIGTERM),
        HostDeath::Leave => None,
    }
}

/// What a guardian signals.
#[derive(Clone, Copy)]
enum Target {
    /// The process group the guardian was forked into.
    Group,
    /// The process group, and the processes of the session, named by this id.
    Session(c_int),
}

/// Arranges for `command`'s child to be given a guardian that signals the child's process group
/// when this process dies. The child must lead a group of its own (`process_group(0)` before this
/// call): the guardian refuses to start otherwise, since it would signal the host's group.
pub(super) fn attach(command: &mut Command, on_death: HostDeath) -> io::Result<()> {
    let Some(signal) = signal(on_death) else {
        return Ok(());
    };
    let read = lifeline()?.read.as_raw_fd();
    // SAFETY: the closure runs in the forked child before `exec` and calls only functions that are
    // safe there: system calls, and no allocation or lock (see `fork_guardian`).
    unsafe {
        command.pre_exec(move || {
            lead_own_group()?;
            fork_guardian(read, signal, Target::Group)
        });
    }
    Ok(())
}

/// Makes `command`'s child start with SIGINT, SIGQUIT and SIGHUP at their default action: a host started with `&`
/// or `nohup` has them ignored, and an ignored signal stays ignored across `exec`, so a program would never see an
/// `interrupt` or a `quit`. Runs between the fork and the exec, so it calls only `signal`.
pub(super) fn reset_signals(command: &mut Command) {
    // SAFETY: the closure runs in the forked child before `exec` and calls only `signal`, which is safe there.
    unsafe {
        command.pre_exec(|| {
            for number in [libc::SIGINT, libc::SIGQUIT, libc::SIGHUP] {
                if libc::signal(number, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
            }
            Ok(())
        });
    }
}

/// An error unless this process leads a process group of its own.
fn lead_own_group() -> io::Result<()> {
    // SAFETY: both calls take no arguments and cannot fail.
    if unsafe { libc::getpgrp() != libc::getpid() } {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(())
}

/// A shell's guardian, kept for as long as the shell is the host's to end.
pub(super) struct Watch {
    /// The write end of the guardian's pipe: closing it without a byte says the host died.
    done: OwnedFd,
}

/// Makes a guardian for the session `session`, which `portable-pty` made its leader: `None` when
/// `on_death` leaves the programs running. The guardian exists a moment after the shell does.
pub(super) fn watch_session(session: u32, on_death: HostDeath) -> io::Result<Option<Watch>> {
    let Some(signal) = signal(on_death) else {
        return Ok(None);
    };
    let session = c_int::try_from(session).map_err(|_| io::Error::other("not a process id"))?;
    let (read, done) = pipe()?;
    fork_guardian(read.as_raw_fd(), signal, Target::Session(session))?;
    Ok(Some(Watch { done }))
}

impl Watch {
    /// The shell has been ended and collected: its guardian leaves without signalling, since the
    /// session id may belong to another process from now on.
    pub(super) fn retire(self) {
        let _ = rustix::io::write(&self.done, &[1]);
    }
}

/// Starts a guardian as a grandchild of this process and waits for the process in between.
///
/// Runs in the child between `fork` and `exec` for a program, and in the host for a shell.
fn fork_guardian(read: c_int, signal: c_int, target: Target) -> io::Result<()> {
    // SAFETY: `fork` in a process that may have other threads: the new process calls only system
    // calls (`guardian`), never the allocator or a lock another thread may have held.
    let first = unsafe { libc::fork() };
    if first < 0 {
        return Err(io::Error::last_os_error());
    }
    if first == 0 {
        // SAFETY: as above; the intermediate process only forks and exits.
        let second = unsafe { libc::fork() };
        if second == 0 {
            // SAFETY: `read` is a pipe read end open in this process, which was forked from the
            // process whose group or session the target names.
            unsafe { guardian(read, signal, target) }
        }
        // SAFETY: `_exit` ends the intermediate process without running the parent's exit code.
        unsafe { libc::_exit(i32::from(second < 0)) }
    }
    let mut status: c_int = 0;
    loop {
        // SAFETY: `first` is this process's child and `status` is a valid place for the result.
        let waited = unsafe { libc::waitpid(first, &raw mut status, 0) };
        if waited == first {
            break;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => {}
            // `SIGCHLD` is ignored in this process: the child is gone and its status with it.
            Some(libc::ECHILD) => return Ok(()),
            error => return Err(io::Error::from_raw_os_error(error.unwrap_or(libc::EIO))),
        }
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EAGAIN))
    }
}

/// The guardian: closes everything but `read`, waits for the end of `read`, and signals the target
/// unless a byte arrived first, which says the tree was ended by its owner.
///
/// # Safety
///
/// Called only in a process forked from the one whose group or session `target` names, with `read`
/// open, and never returns. It makes only system calls.
unsafe fn guardian(read: c_int, signal: c_int, target: Target) -> ! {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: `NAME_C` is a NUL-terminated string that outlives the call; the remaining arguments are unused.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, NAME_C.as_ptr(), 0, 0, 0);
    }
    close_all_but(read);
    let mut byte = 0_u8;
    loop {
        // SAFETY: `byte` is one valid byte and `read` is open.
        let count = unsafe { libc::read(read, (&raw mut byte).cast(), 1) };
        if count < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if count > 0 {
            // SAFETY: `_exit` takes a status and never returns.
            unsafe { libc::_exit(0) }
        }
        break;
    }
    match target {
        // The group cannot have been reused: this process is in it until it ends.
        // SAFETY: `getpgrp` takes no arguments; a negative id names a process group.
        Target::Group => unsafe {
            libc::kill(-libc::getpgrp(), signal);
        },
        Target::Session(session) => {
            // SAFETY: a negative id names a process group; `session` is a process id.
            unsafe {
                libc::kill(-session, signal);
                signal_session(session, signal);
            }
        }
    }
    // SAFETY: `_exit` takes a status and never returns.
    unsafe { libc::_exit(0) }
}

/// Signals every process whose session is `session`, found through `/proc`; with SIGKILL it looks
/// again until a pass finds none, since a process may start another while it is ended.
///
/// # Safety
///
/// Makes only system calls and allocates nothing.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn signal_session(session: c_int, signal: c_int) {
    loop {
        let mut found = false;
        // SAFETY: a NUL-terminated path.
        let dir = unsafe {
            libc::open(
                c"/proc".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if dir < 0 {
            return;
        }
        let mut buffer = [0_u8; 4096];
        loop {
            // SAFETY: `buffer` is valid for its length; the call fills it with directory records.
            let count = unsafe {
                libc::syscall(libc::SYS_getdents64, dir, buffer.as_mut_ptr(), buffer.len())
            };
            let Ok(count) = usize::try_from(count) else {
                break;
            };
            if count == 0 {
                break;
            }
            let mut at = 0_usize;
            while at < count {
                // A `linux_dirent64`: inode (8 bytes), offset (8), record length (2), type (1), name.
                let Some(record) = buffer.get(at..count) else {
                    break;
                };
                let Some(length) = record
                    .get(16..18)
                    .map(|bytes| usize::from(u16::from_ne_bytes([bytes[0], bytes[1]])))
                    .filter(|length| *length > 19)
                else {
                    break;
                };
                let name = record.get(19..length).unwrap_or(&[]);
                if let Some(pid) = process_number(name)
                    // SAFETY: only reads `/proc/<pid>/stat` into a stack buffer.
                    && unsafe { in_session(pid, session) }
                    // SAFETY: `kill` takes a process id and a signal.
                    && unsafe { libc::kill(pid, signal) } == 0
                {
                    found = true;
                }
                at += length;
            }
        }
        // SAFETY: `dir` was opened above and is closed once.
        unsafe { libc::close(dir) };
        if !found || signal != libc::SIGKILL {
            return;
        }
    }
}

/// Without `/proc` there is no session to walk: the group alone is signalled.
///
/// # Safety
///
/// Makes no call.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
unsafe fn signal_session(_session: c_int, _signal: c_int) {}

/// The process number a `/proc` entry names, when it is one.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_number(name: &[u8]) -> Option<c_int> {
    let mut number: c_int = 0;
    let mut digits = 0;
    for byte in name {
        match byte {
            b'0'..=b'9' => {
                number = number
                    .checked_mul(10)?
                    .checked_add(c_int::from(byte - b'0'))?;
                digits += 1;
            }
            0 => break,
            _ => return None,
        }
    }
    (digits > 0).then_some(number)
}

/// Whether `pid` is in `session` and is not a zombie: read from `/proc/<pid>/stat`, whose fields
/// after the command name (which may hold spaces and parentheses) are the state, parent, group and
/// session.
///
/// # Safety
///
/// Makes only system calls and allocates nothing.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn in_session(pid: c_int, session: c_int) -> bool {
    let mut path = [0_u8; 32];
    let mut at = 0;
    for byte in b"/proc/" {
        path[at] = *byte;
        at += 1;
    }
    let mut digits = [0_u8; 12];
    let mut count = 0;
    let mut rest = pid;
    while rest > 0 && count < digits.len() {
        digits[count] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
        rest /= 10;
        count += 1;
    }
    while count > 0 && at < path.len() - 6 {
        count -= 1;
        path[at] = digits[count];
        at += 1;
    }
    for byte in b"/stat\0" {
        path[at] = *byte;
        at += 1;
    }
    // SAFETY: `path` is NUL-terminated.
    let file = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if file < 0 {
        return false;
    }
    let mut stat = [0_u8; 512];
    // SAFETY: `stat` is valid for its length.
    let read = unsafe { libc::read(file, stat.as_mut_ptr().cast(), stat.len()) };
    // SAFETY: `file` was opened above and is closed once.
    unsafe { libc::close(file) };
    let Some(stat) = usize::try_from(read).ok().and_then(|len| stat.get(..len)) else {
        return false;
    };
    let Some(after) = stat.iter().rposition(|byte| *byte == b')') else {
        return false;
    };
    let mut fields = stat
        .get(after + 1..)
        .unwrap_or(&[])
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty());
    let state = fields.next().and_then(|field| field.first().copied());
    // The state, then the parent and the group; the session is the fourth field.
    let found = fields.nth(2).and_then(process_number);
    found == Some(session) && !matches!(state, Some(b'Z' | b'X'))
}

/// Closes every descriptor but `keep`, so the guardian holds no pipe, socket or file of the host:
/// a connection the guardian held would not close when the host closed it.
fn close_all_but(keep: c_int) {
    let below = u32::try_from(keep).unwrap_or(0);
    let mut closed = true;
    if below > 0 {
        closed &= close_range(0, below - 1);
    }
    closed &= close_range(below + 1, u32::MAX);
    if closed {
        return;
    }
    // The system has no `close_range`: close one by one up to the descriptor limit.
    // SAFETY: `rlimit` is plain data and `getrlimit` fills it.
    let limit = unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) == 0 {
            limit.rlim_cur
        } else {
            1024
        }
    };
    // An unlimited soft limit is the kernel's own per-process maximum, `nr_open`.
    let top = if limit == libc::RLIM_INFINITY {
        1 << 20
    } else {
        i64::try_from(limit).unwrap_or(1 << 20)
    };
    for fd in 0..top {
        if fd != i64::from(keep) {
            // SAFETY: closing a descriptor that may not be open is harmless here: nothing else in
            // this process uses one.
            unsafe { libc::close(c_int::try_from(fd).unwrap_or(c_int::MAX)) };
        }
    }
}

/// Closes the descriptors `first` to `last`; whether the system call exists and worked.
fn close_range(first: u32, last: u32) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: `close_range` takes two numbers and a flag word and touches only this process's descriptors.
    let result = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) };
    #[cfg(target_os = "freebsd")]
    // SAFETY: as above.
    let result = i64::from(unsafe { libc::close_range(first, last, 0) });
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    let result = {
        let _ = (first, last);
        -1_i64
    };
    result == 0
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    use bohio_proto::exec::Env;

    use crate::platform::reap::{ended_guardians, sweep};
    use crate::platform::{Spec, spawn};

    /// A process that adopts orphans, as process 1 does, collects the guardians that ended and nothing else.
    #[test]
    fn an_adopted_guardian_that_ended_is_collected_by_the_sweep() {
        // SAFETY: `prctl` with `PR_SET_CHILD_SUBREAPER` takes one integer and no pointer.
        let set = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
        assert_eq!(set, 0, "this process cannot adopt orphans");
        let me = std::process::id();
        let started = spawn(Spec {
            program: OsString::from("sleep"),
            args: vec![OsString::from("30")],
            raw_args: None,
            cwd: None,
            env: Env::default(),
            stdin: false,
            on_host_death: crate::config::HostDeath::Kill,
            reset_signals: true,
        })
        .expect("sleep starts");
        started.control.kill().expect("the program ends");
        let until = Instant::now() + Duration::from_secs(10);
        while ended_guardians(me).is_empty() {
            assert!(
                Instant::now() < until,
                "the ended guardian never became this process's zombie"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(sweep(me) >= 1);
        assert!(ended_guardians(me).is_empty());
    }
}
