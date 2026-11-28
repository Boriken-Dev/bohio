//! Ending a process together with everything it started, which std cannot do: a job object on
//! Windows, a process group on Unix.

#![forbid(unsafe_code)]

#[cfg(windows)]
use super::winproc::Child;
use std::io;
#[cfg(unix)]
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
pub(super) use self::unix::{Tree, contain, end, end_group, poll, poll_id, prepare, send_group};
#[cfg(windows)]
pub(super) use self::windows::{Tree, contain_handle, end, end_tree, job, poll};

#[cfg(unix)]
mod unix {
    use super::{Child, Command, Poll, io};
    use crate::knobs::HostDeath;
    use crate::platform::ProcessExit;

    /// A process group needs no object of its own: the child leads it.
    pub(in crate::platform) struct Tree;

    /// The child leads a process group of its own, which `end` signals, and gets a guardian that
    /// signals it if this process dies first.
    pub(in crate::platform) fn prepare(
        command: &mut Command,
        on_host_death: HostDeath,
    ) -> io::Result<()> {
        std::os::unix::process::CommandExt::process_group(command, 0);
        crate::platform::guard::attach(command, on_host_death)
    }

    pub(in crate::platform) fn contain(_child: &Child) -> io::Result<Tree> {
        Ok(Tree)
    }

    /// Kills the group, and first stops and then kills every process
    /// descended from the child, those that left the group or session included. Called while the
    /// child is not yet collected: a collected child's id, and its group's, may belong to another
    /// process.
    pub(in crate::platform) fn end(_tree: &Tree, child: &Child) {
        end_group(child.id());
    }

    /// Sends `signal` to the process group `leader` leads, which must not have been collected yet. `false` when
    /// no process is left in it or the signal is refused.
    pub(in crate::platform) fn send_group(
        leader: u32,
        signal: bohio_proto::signal::Signal,
    ) -> bool {
        use bohio_proto::signal::Signal;
        use rustix::process::{Pid, Signal as Sys, kill_process_group};
        let Some(group) = i32::try_from(leader)
            .ok()
            .filter(|pid| *pid > 1)
            .and_then(Pid::from_raw)
        else {
            return false;
        };
        let sys = match signal {
            Signal::Interrupt => Sys::INT,
            Signal::Terminate => Sys::TERM,
            Signal::Hangup => Sys::HUP,
            Signal::Quit => Sys::QUIT,
            Signal::User1 => Sys::USR1,
            Signal::User2 => Sys::USR2,
            _ => Sys::KILL,
        };
        kill_process_group(group, sys).is_ok()
    }

    /// As [`end`], for a process this module did not start (it only needs the id of the leader
    /// of a group of its own, which must not have been collected yet).
    pub(in crate::platform) fn end_group(leader: u32) {
        use rustix::process::{Pid, Signal, kill_process, kill_process_group};
        let Some(group) = i32::try_from(leader)
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
        Ok(poll_id(child.id())?.map_or(Poll::Running, Poll::Exited))
    }

    /// As [`poll`], for a process this module did not start: how it ended, without collecting
    /// it, or `None` while it runs.
    pub(in crate::platform) fn poll_id(id: u32) -> io::Result<Option<ProcessExit>> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let pid = i32::try_from(id)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::other("not a process id"))?;
        let status = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?;
        Ok(status.map(|status| ProcessExit {
            code: status.exit_status(),
            signal: status.terminating_signal(),
        }))
    }

    /// The processes descended from one, found through `/proc` by their parents.
    mod descendants {
        /// How many times the tree is looked at again for descendants a stopped parent had just started: each round
        /// finds the children of the last round's, so this is the depth of a chain of forks caught in one stop. Not a
        /// limit on the tree's size; a deeper chain is ended by the process group's own kill.
        const ROUNDS: usize = 16;

        use rustix::process::Pid;

        /// Every process descended from `root`, each stopped so it starts no more, found again
        /// until no new one appears. Parents come from `/proc` on Linux and Android and from `ps`
        /// elsewhere.
        pub(super) fn stopped(root: Pid) -> Vec<Pid> {
            use std::collections::BTreeSet;

            use rustix::process::{Signal, kill_process};
            let mut found = BTreeSet::new();
            for _ in 0..ROUNDS {
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

        /// Each process's parent, from `ps` where there is no `/proc` (macOS and the BSDs).
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        fn parents() -> std::collections::BTreeMap<i32, i32> {
            let mut parents = std::collections::BTreeMap::new();
            let Ok(output) = std::process::Command::new("/bin/ps")
                .args(["-A", "-o", "pid=,ppid="])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
            else {
                return parents;
            };
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let mut fields = line.split_whitespace();
                if let (Some(pid), Some(parent)) = (
                    fields.next().and_then(|field| field.parse::<i32>().ok()),
                    fields.next().and_then(|field| field.parse::<i32>().ok()),
                ) {
                    parents.insert(pid, parent);
                }
            }
            parents
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

    use super::{Child, Poll, exit_of, io};

    /// A job object that holds the child and everything it starts and ends them all when its
    /// handle closes, which the operating system does too when this process ends.
    pub(in crate::platform) struct Tree(Mutex<Option<win32job::Job>>);

    /// A job for a process of this module's own to start in (`winproc::start`): nothing it
    /// starts at once is outside it.
    pub(in crate::platform) fn job() -> io::Result<win32job::Job> {
        let mut limits = win32job::ExtendedLimitInfo::new();
        limits.limit_kill_on_job_close();
        win32job::Job::create_with_limit_info(&limits).map_err(io::Error::other)
    }

    impl Tree {
        /// The tree of the process that started in `job`.
        pub(in crate::platform) fn holding(job: win32job::Job) -> Self {
            Self(Mutex::new(Some(job)))
        }
    }

    /// Puts a process this module did not start, named by its handle, in a job of its own. A
    /// process it starts in the instant before it joins is outside the job.
    pub(in crate::platform) fn contain_handle(process: isize) -> io::Result<Tree> {
        let job = job()?;
        job.assign_process(process).map_err(io::Error::other)?;
        Ok(Tree::holding(job))
    }

    /// Closing the job ends everything in it.
    pub(in crate::platform) fn end(tree: &Tree, _child: &Child) {
        end_tree(tree);
    }

    /// Closing the job ends everything in it; repeatable.
    pub(in crate::platform) fn end_tree(tree: &Tree) {
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
