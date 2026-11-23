//! `sync_tree`: a local tree and a tree of a host made the same, served and driven in-process.

#![forbid(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bohio::config::FilesystemConfig;
use bohio::{Grant, Permission};
use bohio_client::sync::{
    Compare, Direction, Step, SyncOptions, TreeOptions, TreeReport, sync_tree,
};
use bohio_proto::compress::{Codec, Compression};
use support::Link;
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];

struct Fixture {
    _tmp: TempDir,
    local: PathBuf,
    remote: PathBuf,
    link: Link,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let local = tmp.path().join("local");
    let remote = tmp.path().join("remote");
    std::fs::create_dir(&local).unwrap();
    std::fs::create_dir(&remote).unwrap();
    let grant = Grant::allowing(RW).with_fs_roots([remote.clone()]);
    let mut link = Link::files(grant, FilesystemConfig::default());
    // What the host reports, as the node would: the client reads what it offers from it.
    link.info = std::sync::Arc::new(bohio::info::collect(
        &bohio::config::NodeConfig::default(),
        std::ffi::OsStr::new(""),
    ));
    Fixture {
        _tmp: tmp,
        local,
        remote,
        link,
    }
}

fn pseudo(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn put(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn remote_text(f: &Fixture) -> String {
    f.remote.to_string_lossy().into_owned()
}

/// Every file below `root` with its bytes, by path with `/`; directories as an entry with no bytes.
fn snapshot(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".bohio-") {
                panic!("a temporary file stays behind: {name}");
            }
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if entry.file_type().unwrap().is_dir() {
                found.push((relative.clone(), None));
                stack.push((entry.path(), relative));
            } else {
                found.push((relative, Some(std::fs::read(entry.path()).unwrap())));
            }
        }
    }
    found.sort();
    found
}

fn build_tree(root: &Path) {
    for dir in 0..5 {
        for file in 0..40 {
            put(
                root,
                &format!("pkg{dir}/src/m{file:02}.rs"),
                format!("// file {dir}/{file}\nfn main() {{}}\n").as_bytes(),
            );
        }
    }
    put(root, "assets/big.bin", &pseudo(3 << 20, 4));
    put(root, "assets/medium.bin", &pseudo(300_000, 5));
    put(root, "empty/none/.keep", b"");
    std::fs::create_dir_all(root.join("emptydir/inside")).unwrap();
    put(root, "top.txt", b"top level\n");
}

fn options(direction: Direction) -> TreeOptions {
    TreeOptions {
        direction,
        ..TreeOptions::default()
    }
}

fn assert_clean(report: &TreeReport) {
    assert!(report.is_clean(), "{:?}", report.failures);
}

#[test]
fn a_push_makes_the_tree_and_a_second_push_moves_nothing_and_sends_no_file_bytes() {
    let f = fixture();
    build_tree(&f.local);
    let first = sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &options(Direction::Push),
    )
    .unwrap();
    assert_clean(&first);
    assert_eq!(snapshot(&f.local), snapshot(&f.remote));
    // 200 small files in one bundle (or a few), the large ones one by one.
    assert_eq!(first.bundled, 200 + 3);
    assert!(first.bundles >= 1);
    assert_eq!(first.singles, 1);
    assert_eq!(first.unchanged, 0);

    // The times came across, so a comparison by size and time finds everything the same.
    let again = sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &options(Direction::Push),
    )
    .unwrap();
    assert_clean(&again);
    assert_eq!((again.bundled, again.singles, again.deleted), (0, 0, 0));
    assert_eq!(again.unchanged, 200 + 3 + 1);
    assert_eq!((again.bytes, again.wire_bytes, again.bundles), (0, 0, 0));
    assert!(again.actions.is_empty());
    assert!(again.walk_entries > 200);
}

#[test]
fn a_changed_file_goes_by_delta_when_large_and_in_a_bundle_when_small() {
    let f = fixture();
    build_tree(&f.local);
    sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &options(Direction::Push),
    )
    .unwrap();
    // One byte of the large file and one small file change; times must differ to be seen by a size-and-time compare.
    let mut big = std::fs::read(f.local.join("assets/big.bin")).unwrap();
    big[1_000_000] ^= 0xff;
    std::fs::write(f.local.join("assets/big.bin"), &big).unwrap();
    std::fs::write(f.local.join("top.txt"), b"top level changed\n").unwrap();
    let later = SystemTime::now() + Duration::from_secs(5);
    for name in ["assets/big.bin", "top.txt"] {
        std::fs::File::options()
            .write(true)
            .open(f.local.join(name))
            .unwrap()
            .set_modified(later)
            .unwrap();
    }
    let report = sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &options(Direction::Push),
    )
    .unwrap();
    assert_clean(&report);
    assert_eq!((report.bundled, report.singles, report.deltas), (1, 1, 1));
    assert!(
        report.wire_bytes * 10 < 3 << 20,
        "a delta of one byte sent {} bytes",
        report.wire_bytes
    );
    assert_eq!(snapshot(&f.local), snapshot(&f.remote));
    let steps: Vec<(Step, &str)> = report
        .actions
        .iter()
        .map(|action| (action.step, action.path.as_str()))
        .collect();
    assert_eq!(
        steps,
        [(Step::Update, "assets/big.bin"), (Step::Update, "top.txt")]
    );
}

#[test]
fn delete_and_dry_run_push_and_pull_make_what_they_say_and_nothing_with_a_dry_run() {
    let f = fixture();
    build_tree(&f.local);
    put(&f.remote, "stale/old.txt", b"old");
    put(&f.remote, "top.txt", b"different");
    put(&f.remote, "pkg0/src/extra.rs", b"extra");
    let dry = TreeOptions {
        dry_run: true,
        delete: true,
        ..options(Direction::Push)
    };
    let planned = sync_tree(&f.link, &f.local, &remote_text(&f), &dry).unwrap();
    assert_clean(&planned);
    let deletes: Vec<&str> = planned
        .actions
        .iter()
        .filter(|action| action.step == Step::Delete)
        .map(|action| action.path.as_str())
        .collect();
    assert_eq!(deletes, ["pkg0/src/extra.rs", "stale"]);
    assert!(
        planned
            .actions
            .iter()
            .any(|action| action.step == Step::Update && action.path == "top.txt")
    );
    // Nothing changed.
    assert!(f.remote.join("stale/old.txt").exists());
    assert_eq!(
        std::fs::read(f.remote.join("top.txt")).unwrap(),
        b"different"
    );
    assert!(!f.remote.join("assets").exists());

    // Without `delete` extras stay; with it they go.
    let kept = sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &options(Direction::Push),
    )
    .unwrap();
    assert_clean(&kept);
    assert!(f.remote.join("stale/old.txt").exists());
    let deleting = TreeOptions {
        delete: true,
        ..options(Direction::Push)
    };
    let done = sync_tree(&f.link, &f.local, &remote_text(&f), &deleting).unwrap();
    assert_clean(&done);
    assert_eq!(done.deleted, 2);
    assert_eq!(snapshot(&f.local), snapshot(&f.remote));

    // The same the other way: a pull into a tree with extras and stale files.
    let other = f.local.parent().unwrap().join("pulled");
    put(&other, "junk/x", b"x");
    put(&other, "top.txt", b"stale");
    let pull = TreeOptions {
        delete: true,
        ..options(Direction::Pull)
    };
    let dry_pull = TreeOptions {
        dry_run: true,
        ..pull.clone()
    };
    let planned = sync_tree(&f.link, &other, &remote_text(&f), &dry_pull).unwrap();
    assert!(
        planned
            .actions
            .iter()
            .any(|a| a.step == Step::Delete && a.path == "junk")
    );
    assert!(other.join("junk/x").exists());
    let pulled = sync_tree(&f.link, &other, &remote_text(&f), &pull).unwrap();
    assert_clean(&pulled);
    assert_eq!(snapshot(&other), snapshot(&f.remote));
    let again = sync_tree(&f.link, &other, &remote_text(&f), &pull).unwrap();
    assert_eq!(
        (again.bundled, again.singles, again.deleted, again.bytes),
        (0, 0, 0, 0)
    );
}

#[test]
fn a_pull_takes_the_hosts_times_so_the_next_compare_by_size_and_time_is_quiet_and_a_hash_compare_agrees()
 {
    let f = fixture();
    build_tree(&f.remote);
    let missing = f.local.parent().unwrap().join("new-local");
    let pulled = sync_tree(
        &f.link,
        &missing,
        &remote_text(&f),
        &options(Direction::Pull),
    )
    .unwrap();
    assert_clean(&pulled);
    assert_eq!(snapshot(&missing), snapshot(&f.remote));
    for compare in [
        Compare::Meta,
        Compare::Hash(bohio_proto::hash::Algorithm::Blake3),
        Compare::Blocks,
    ] {
        let report = sync_tree(
            &f.link,
            &missing,
            &remote_text(&f),
            &TreeOptions {
                compare: compare.clone(),
                ..options(Direction::Pull)
            },
        )
        .unwrap();
        assert_clean(&report);
        assert_eq!((report.bundled, report.singles), (0, 0), "{compare:?}");
    }
    // By hash, a file whose size and bytes agree is the same whatever its time; one byte off is not.
    std::fs::write(f.remote.join("top.txt"), b"top LEVEL\n").unwrap();
    let by_hash = TreeOptions {
        compare: Compare::Hash(bohio_proto::hash::Algorithm::Blake3),
        ..options(Direction::Pull)
    };
    let report = sync_tree(&f.link, &missing, &remote_text(&f), &by_hash).unwrap();
    assert_clean(&report);
    assert_eq!(report.bundled, 1);
    assert_eq!(
        std::fs::read(missing.join("top.txt")).unwrap(),
        b"top LEVEL\n"
    );
    // A host tree that does not exist is an error for a pull, and a root made for a push.
    let error = sync_tree(
        &f.link,
        &missing,
        &format!("{}/nowhere", remote_text(&f)),
        &options(Direction::Pull),
    );
    assert!(error.is_err());
    let made = sync_tree(
        &f.link,
        &f.local,
        &format!("{}/created/here", remote_text(&f)),
        &options(Direction::Push),
    )
    .unwrap();
    assert_clean(&made);
    assert!(f.remote.join("created/here").is_dir());
}

#[test]
fn compressed_bundles_and_parallel_streams_move_the_same_tree() {
    let f = fixture();
    build_tree(&f.local);
    let compression = Compression::new(Codec::Zstd).level(3);
    let report = sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &TreeOptions {
            parallel: 3,
            bundle_items: 25,
            sync: SyncOptions {
                compression: Some(compression.clone()),
                ..SyncOptions::default()
            },
            bundle: bohio_client::bundle::BundleOptions {
                compression: Some(compression),
                ..bohio_client::bundle::BundleOptions::default()
            },
            ..options(Direction::Push)
        },
    )
    .unwrap();
    assert_clean(&report);
    assert_eq!(report.bundles, 9, "203 small files in bundles of 25");
    assert_eq!(snapshot(&f.local), snapshot(&f.remote));
}

#[test]
fn a_file_and_a_directory_in_each_others_way_fail_alone_unless_delete_replaces_them() {
    let f = fixture();
    put(&f.local, "thing", b"a file here");
    put(&f.local, "fine.txt", b"fine");
    put(&f.remote, "thing/inside", b"a directory there");
    let report = sync_tree(
        &f.link,
        &f.local,
        &remote_text(&f),
        &options(Direction::Push),
    )
    .unwrap();
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].0, "thing");
    assert_eq!(std::fs::read(f.remote.join("fine.txt")).unwrap(), b"fine");
    assert!(f.remote.join("thing/inside").exists());
    let deleting = TreeOptions {
        delete: true,
        ..options(Direction::Push)
    };
    let report = sync_tree(&f.link, &f.local, &remote_text(&f), &deleting).unwrap();
    assert_clean(&report);
    assert_eq!(
        std::fs::read(f.remote.join("thing")).unwrap(),
        b"a file here"
    );
}
