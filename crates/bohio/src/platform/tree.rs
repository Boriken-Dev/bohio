//! Ending a process together with everything it started, which std cannot do: a job object on
//! Windows, a process group on Unix.

use std::io;
use std::process::{Child, Command};

/// How a poll of the child found it.
pub(super) enum Poll {
    Running,
    /// Exited and not yet collected, so its id and its group's are still its own.
    #[cfg(unix)]
    Exited(super::ProcessExit),
    /// Exited and collected: the platform could not look without collecting.
    #[cfg(windows)]
    Collected(super::ProcessExit),
}

pub(super) fn exit_of(status: std::process::ExitStatus) -> super::ProcessExit {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    super::ProcessExit {
        code: status.code(),
        signal,
    }
}

#[cfg(unix)]
pub(super) use self::unix::{Tree, contain, end, poll, prepare};
#[cfg(windows)]
pub(super) use self::windows::{Tree, contain, end, poll, prepare};

#[cfg(unix)]
mod unix {
    use super::{Child, Command, Poll, io};
    use crate::platform::ProcessExit;

    /// A process group needs no object of its own: the child leads it.
    pub(in crate::platform) struct Tree;

    /// The child leads a process group of its own, which `end` signals.
    pub(in crate::platform) fn prepare(command: &mut Command) {
        std::os::unix::process::CommandExt::process_group(command, 0);
    }

    pub(in crate::platform) fn contain(_child: &Child) -> io::Result<Tree> {
        Ok(Tree)
    }

    /// Kills the group, and on Linux and Android first stops and then kills every process
    /// descended from the child, those that left the group or session included. Called while the
    /// child is not yet collected: a collected child's id, and its group's, may belong to another
    /// process.
    pub(in crate::platform) fn end(_tree: &Tree, child: &Child) {
        use rustix::process::{Pid, Signal, kill_process, kill_process_group};
        let Some(group) = i32::try_from(child.id())
            .ok()
            .filter(|pid| *pid > 1)
            .and_then(Pid::from_raw)
        else {
            return;
        };
        let escaped = descendants::stopped(group);
        let _ = kill_process_group(group, Signal::KILL);
        for pid in escaped {
            let _ = kill_process(pid, Signal::KILL);
        }
    }

    /// Looks at the child without collecting it, so its group can still be signalled.
    pub(in crate::platform) fn poll(child: &mut Child) -> io::Result<Poll> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let pid = i32::try_from(child.id())
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::other("not a process id"))?;
        let status = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?;
        Ok(status.map_or(Poll::Running, |status| {
            Poll::Exited(ProcessExit {
                code: status.exit_status(),
                signal: status.terminating_signal(),
            })
        }))
    }

    /// The processes descended from one, found through `/proc` by their parents.
    mod descendants {
        use rustix::process::Pid;

        /// Every process descended from `root`, each stopped so it starts no more, found again
        /// until no new one appears. Empty where there is no `/proc` to read.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        pub(super) fn stopped(root: Pid) -> Vec<Pid> {
            use std::collections::BTreeSet;

            use rustix::process::{Signal, kill_process};
            let mut found = BTreeSet::new();
            for _ in 0..16 {
                let parents = parents();
                let mut grown = false;
                let mut queue = Vec::from([root.as_raw_nonzero().get()]);
                while let Some(parent) = queue.pop() {
                    for (child, _) in parents.iter().filter(|(_, p)| **p == parent) {
                        if found.insert(*child) {
                            grown = true;
                            if let Some(pid) = Pid::from_raw(*child) {
                                let _ = kill_process(pid, Signal::STOP);
                            }
                        }
                        queue.push(*child);
                    }
                }
                if !grown {
                    break;
                }
            }
            found.into_iter().filter_map(Pid::from_raw).collect()
        }

        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        pub(super) fn stopped(_root: Pid) -> Vec<Pid> {
            Vec::new()
        }

        /// Each process's parent, as `/proc/<pid>/stat` says.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        fn parents() -> std::collections::BTreeMap<i32, i32> {
            let mut parents = std::collections::BTreeMap::new();
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return parents;
            };
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<i32>().ok())
                else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                // `pid (name) state ppid ...`: the name may hold spaces and parentheses, so read
                // from its last `)`.
                let parent = stat
                    .rfind(')')
                    .and_then(|end| stat[end + 1..].split_whitespace().nth(1))
                    .and_then(|ppid| ppid.parse::<i32>().ok());
                if let Some(parent) = parent {
                    parents.insert(pid, parent);
                }
            }
            parents
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::sync::Mutex;

    use super::{Child, Command, Poll, exit_of, io};

    /// A job object that holds the child and everything it starts and ends them all when its
    /// handle closes, which the operating system does too when this process ends.
    pub(in crate::platform) struct Tree(Mutex<Option<win32job::Job>>);

    /// Without a console of its own the child opens no window, whoever started this process.
    pub(in crate::platform) fn prepare(command: &mut Command) {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::os::windows::process::CommandExt::creation_flags(command, CREATE_NO_WINDOW);
    }

    /// Puts the child in a job of its own. A process it starts in the instant before it joins is
    /// outside the job.
    pub(in crate::platform) fn contain(child: &Child) -> io::Result<Tree> {
        use std::os::windows::io::AsRawHandle;
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        let job = win32job::Job::create_with_limit_info(&limits).map_err(io::Error::other)?;
        job.assign_process(child.as_raw_handle() as isize)
            .map_err(io::Error::other)?;
        Ok(Tree(Mutex::new(Some(job))))
    }

    /// Closing the job ends everything in it.
    pub(in crate::platform) fn end(tree: &Tree, _child: &Child) {
        let mut job = tree
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(job.take());
    }

    pub(in crate::platform) fn poll(child: &mut Child) -> io::Result<Poll> {
        Ok(child
            .try_wait()?
            .map_or(Poll::Running, |status| Poll::Collected(exit_of(status))))
    }
}
