//! A directory swapped for a junction while the host works inside it, on Windows: no filesystem call
//! (list, read, write, make, remove, rename, link, read a link, set an attribute) reaches outside the
//! root, and none is redirected by a rename of a directory above the one it works in.
//!
//! The root holds `a`, a real directory with `b` inside it. A second thread cycles `a` between that
//! directory and a junction to a directory outside the root that also holds a `b`, using only renames
//! (`a` aside, a prepared junction in as `a`, and back), so every swap is atomic. The host's calls name
//! `a/b/...`. Whatever the host does, the outside `b` must end as it began, and no name the calls make
//! may be seen there while they run.
//!
//! Measured on NTFS: a directory with a handle open anywhere below it cannot be renamed (the handles the
//! confinement layer holds make every rename of `a` fail while a call runs), so a swap lands only between
//! calls and the library's path-based calls were not beaten by it either; this test holds the property
//! for any future change. What the handle-relative calls close that a path-based rename could not is the
//! path itself: `a_rename_deeper_than_the_longest_path_works` fails on the path-based code.

#![cfg(windows)]
#![forbid(unsafe_code)]

mod support;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bohio::config::FilesystemConfig;
use bohio::{Grant, Permission};
use bohio_client::fs::{Fs, Writer};
use bohio_proto::fs::WriteRequest;
use support::{Link, WAIT};
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];
const OUTSIDE: &[u8] = b"OUTSIDE: this file is beyond the root";
const INSIDE: &[u8] = b"INSIDE: this file is under the root";

/// The names the outside `b` starts with.
const OUTSIDE_NAMES: [&str; 5] = ["in.txt", "victim.txt", "mv1", "ro.txt", "only_outside.txt"];

struct Stop<'a>(&'a AtomicBool);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn names(directory: &Path) -> BTreeSet<String> {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Makes a junction (a directory link that needs no privilege); `false` when the system refuses.
fn junction(link: &Path, target: &Path) -> bool {
    std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|out| out.status.success())
}

fn put(link: &Link, path: &str, bytes: &[u8]) {
    let Ok(writer) = Writer::start(link, &WriteRequest::replace(path), WAIT) else {
        return;
    };
    if writer.write(bytes).is_ok() {
        let _ = writer.finish(WAIT);
    }
}

fn run(budget: Duration) -> (u32, u32, u32, Vec<String>) {
    let tmp: TempDir = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    for directory in [root.join("a").join("b"), outside.join("b")] {
        std::fs::create_dir_all(directory).unwrap();
    }
    for name in ["in.txt", "victim.txt", "mv1", "ro.txt"] {
        std::fs::write(root.join("a").join("b").join(name), INSIDE).unwrap();
        std::fs::write(outside.join("b").join(name), OUTSIDE).unwrap();
    }
    std::fs::write(outside.join("b").join("only_outside.txt"), OUTSIDE).unwrap();
    if !junction(&root.join("a_junc"), &outside) {
        eprintln!("SKIPPED: the system refuses to make a junction");
        return (0, 0, 0, Vec::new());
    }
    let grant = Grant::allowing(RW).with_fs_roots([root.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    let fs = Fs::connect(&link).unwrap().with_timeout(WAIT);

    let stop = AtomicBool::new(false);
    let swaps = AtomicU32::new(0);
    let seen_outside = AtomicU32::new(0);
    let host_rounds = AtomicU32::new(0);
    let mut problems = Vec::new();
    let initial: BTreeSet<String> = OUTSIDE_NAMES.iter().map(|n| (*n).to_owned()).collect();
    std::thread::scope(|scope| {
        let swapper = scope.spawn(|| {
            let (a, aside, junc) = (root.join("a"), root.join("a_aside"), root.join("a_junc"));
            while !stop.load(Ordering::Acquire) {
                if std::fs::rename(&a, &aside).is_err() {
                    continue;
                }
                if std::fs::rename(&junc, &a).is_ok() {
                    swaps.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_micros(200));
                    while std::fs::rename(&a, &junc).is_err() && !stop.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                }
                while std::fs::rename(&aside, &a).is_err() && !stop.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
        });
        // Whatever shows in the outside `b` that it did not start with was made by the host.
        let watcher = scope.spawn(|| {
            let outside_b = outside.join("b");
            while !stop.load(Ordering::Acquire) {
                if names(&outside_b) != initial {
                    seen_outside.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let _stop = Stop(&stop);
        let started = Instant::now();
        let mut rounds = 0_u32;
        while started.elapsed() < budget {
            rounds += 1;
            let _ = fs.list_all("a/b");
            let _ = fs.mkdir("a/b/made", false);
            let _ = fs.remove("a/b/made", false);
            let _ = fs.remove("a/b/victim.txt", false);
            put(&link, "a/b/victim.txt", INSIDE);
            let _ = fs.rename("a/b/mv1", "a/b/mv2", false);
            let _ = fs.rename("a/b/mv2", "a/b/mv1", false);
            let _ = fs.symlink("in.txt", "a/b/lnk");
            let _ = fs.readlink("a/b/lnk");
            let _ = fs.remove("a/b/lnk", false);
            let _ = fs.set_readonly("a/b/ro.txt", true);
            let _ = fs.set_readonly("a/b/ro.txt", false);
            let _ = fs.mkdir("a/b/tree/deep", true);
            let _ = fs.remove("a/b/tree", true);
        }
        stop.store(true, Ordering::Release);
        swapper.join().unwrap();
        watcher.join().unwrap();
        host_rounds.store(rounds, Ordering::Relaxed);
    });
    let outside_b = outside.join("b");
    if names(&outside_b) != initial {
        problems.push(format!(
            "the outside directory ends with {:?}",
            names(&outside_b)
        ));
    }
    for name in ["in.txt", "victim.txt", "mv1", "ro.txt", "only_outside.txt"] {
        match std::fs::read(outside_b.join(name)) {
            Ok(bytes) if bytes == OUTSIDE => {}
            other => problems.push(format!("outside {name}: {other:?}")),
        }
    }
    if std::fs::metadata(outside_b.join("ro.txt")).is_ok_and(|meta| meta.permissions().readonly()) {
        problems.push("the outside ro.txt was made read-only".to_owned());
    }
    (
        host_rounds.load(Ordering::Relaxed),
        swaps.load(Ordering::Relaxed),
        seen_outside.load(Ordering::Relaxed),
        problems,
    )
}

#[test]
fn no_call_reaches_outside_while_a_directory_above_is_swapped_for_a_junction() {
    let (rounds, swaps, seen, problems) = run(Duration::from_secs(20));
    eprintln!(
        "junction swap: {rounds} rounds of calls, {swaps} swaps of `a`, {seen} looks that saw the outside change"
    );
    assert!(
        problems.is_empty() && seen == 0,
        "a call reached the outside directory: {problems:?}, seen {seen} times"
    );
}

/// A path-based rename works out the parents' paths and so meets the limit of a path (`MAX_PATH`, 260
/// characters) that a name relative to a handle does not.
#[test]
fn a_rename_deeper_than_the_longest_path_works() {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let mut relative = String::new();
    let mut deep = root.clone();
    for level in 0..14 {
        let part = format!("a_directory_with_a_long_name_{level:02}");
        deep.push(&part);
        relative.push_str(&part);
        relative.push('/');
    }
    std::fs::create_dir_all(&deep).unwrap();
    assert!(deep.as_os_str().len() > 300, "the path must pass MAX_PATH");
    std::fs::write(deep.join("from.txt"), b"moved").unwrap();
    let grant = Grant::allowing(RW).with_fs_roots([root.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    let fs = Fs::connect(&link).unwrap().with_timeout(WAIT);
    fs.stat(&format!("{relative}from.txt"), false)
        .expect("the file is reachable at that depth");
    fs.rename(
        &format!("{relative}from.txt"),
        &format!("{relative}to.txt"),
        false,
    )
    .expect("the rename works at any depth");
    assert_eq!(std::fs::read(deep.join("to.txt")).unwrap(), b"moved");
    assert!(!deep.join("from.txt").exists());
}
