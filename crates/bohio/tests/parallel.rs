//! Parallel ranged transfers, served and driven in-process over memory channels.

#![forbid(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};

use bohio::config::FilesystemConfig;
use bohio::{Grant, Permission};
use bohio_client::parallel::{ParallelOptions, get_parallel, put_parallel, read_parallel};
use bohio_client::{Error, Open};
use bohio_proto::ErrorCode;
use bohio_proto::hash::Algorithm;
use bohio_transfer::hash::Hasher;
use support::Link;
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];
const READ: [Permission; 1] = [Permission::FsRead];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    local: PathBuf,
    link: Link,
}

fn link_over(root: &Path, allow: &[Permission]) -> Link {
    let grant = Grant::allowing(allow.iter().copied()).with_fs_roots([root.to_path_buf()]);
    Link::files(grant, FilesystemConfig::default())
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let local = tmp.path().join("local");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&local).unwrap();
    let link = link_over(&root, &RW);
    Fixture {
        _tmp: tmp,
        root,
        local,
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

fn sha256(bytes: &[u8]) -> bohio_proto::hash::Digests {
    let mut hasher = Hasher::new(&[Algorithm::Sha256]).unwrap();
    hasher.update(bytes);
    hasher.finish()
}

fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    found
}

fn small_ranges() -> ParallelOptions {
    ParallelOptions {
        streams: 4,
        range: Some(37_000),
        ..ParallelOptions::default()
    }
}

#[test]
fn get_parallel_gives_the_hosts_bytes_over_one_link_and_over_two() {
    let fixture = fixture();
    let data = pseudo(1_000_003, 5);
    std::fs::write(fixture.root.join("big.bin"), &data).unwrap();
    let second = link_over(&fixture.root, &RW);
    for (label, links) in [
        ("one", vec![&fixture.link as &dyn Open]),
        (
            "two",
            vec![&fixture.link as &dyn Open, &second as &dyn Open],
        ),
    ] {
        let target = fixture.local.join(format!("got-{label}.bin"));
        let moved = get_parallel(&links, "big.bin", &target, &small_ranges()).unwrap();
        let got = std::fs::read(&target).unwrap();
        assert_eq!(sha256(&got), sha256(&data), "{label} link");
        assert_eq!(moved.size, data.len() as u64);
    }
    // Nothing but the two results is left beside them.
    assert_eq!(names(&fixture.local), ["got-one.bin", "got-two.bin"]);
}

#[test]
fn get_parallel_with_one_range_per_stream_and_an_empty_file() {
    let fixture = fixture();
    let data = pseudo(300_001, 9);
    std::fs::write(fixture.root.join("a.bin"), &data).unwrap();
    std::fs::write(fixture.root.join("empty.bin"), b"").unwrap();
    let links: [&dyn Open; 1] = [&fixture.link];
    let options = ParallelOptions::default();
    get_parallel(&links, "a.bin", &fixture.local.join("a.bin"), &options).unwrap();
    assert_eq!(std::fs::read(fixture.local.join("a.bin")).unwrap(), data);
    get_parallel(
        &links,
        "empty.bin",
        &fixture.local.join("empty.bin"),
        &options,
    )
    .unwrap();
    assert_eq!(std::fs::read(fixture.local.join("empty.bin")).unwrap(), b"");
}

#[test]
fn a_failed_get_parallel_leaves_nothing_and_keeps_the_old_file() {
    let fixture = fixture();
    std::fs::write(fixture.root.join("big.bin"), pseudo(400_000, 3)).unwrap();
    std::fs::write(fixture.local.join("big.bin"), b"old").unwrap();
    // The second link may not read, so its stream fails while the first one's succeeds.
    let blind = link_over(&fixture.root, &[]);
    let links: [&dyn Open; 2] = [&fixture.link, &blind];
    let failed = get_parallel(
        &links,
        "big.bin",
        &fixture.local.join("big.bin"),
        &small_ranges(),
    );
    assert!(
        matches!(&failed, Err(Error::Remote(body)) if body.code == ErrorCode::Denied),
        "{failed:?}"
    );
    assert_eq!(
        std::fs::read(fixture.local.join("big.bin")).unwrap(),
        b"old"
    );
    assert_eq!(names(&fixture.local), ["big.bin"]);
    let missing = get_parallel(
        &links[..1],
        "nope.bin",
        &fixture.local.join("nope.bin"),
        &small_ranges(),
    );
    assert!(matches!(&missing, Err(Error::Remote(body)) if body.code == ErrorCode::NotFound));
    assert_eq!(names(&fixture.local), ["big.bin"]);
}

#[test]
fn put_parallel_gives_the_hosts_bytes_over_one_link_and_over_two_and_replaces_a_file() {
    let fixture = fixture();
    let data = pseudo(1_000_003, 11);
    let source = fixture.local.join("source.bin");
    std::fs::write(&source, &data).unwrap();
    std::fs::write(fixture.root.join("two.bin"), b"the old contents").unwrap();
    let second = link_over(&fixture.root, &RW);
    let one: [&dyn Open; 1] = [&fixture.link];
    let two: [&dyn Open; 2] = [&fixture.link, &second];
    let moved = put_parallel(&one, &source, "one.bin", &small_ranges()).unwrap();
    assert_eq!(moved.size, data.len() as u64);
    put_parallel(&two, &source, "two.bin", &small_ranges()).unwrap();
    for name in ["one.bin", "two.bin"] {
        assert_eq!(
            sha256(&std::fs::read(fixture.root.join(name)).unwrap()),
            sha256(&data),
            "{name}"
        );
    }
    assert_eq!(names(&fixture.root), ["one.bin", "two.bin"]);
}

#[test]
fn a_failed_put_parallel_removes_its_staged_file_and_keeps_the_target() {
    let fixture = fixture();
    let source = fixture.local.join("source.bin");
    std::fs::write(&source, pseudo(500_000, 13)).unwrap();
    std::fs::write(fixture.root.join("target.bin"), b"keep me").unwrap();
    // The second link may not write: the first one's staged file exists when the second one's stream is refused.
    let read_only = link_over(&fixture.root, &READ);
    let links: [&dyn Open; 2] = [&fixture.link, &read_only];
    let failed = put_parallel(&links, &source, "target.bin", &small_ranges());
    assert!(
        matches!(&failed, Err(Error::Remote(body)) if body.code == ErrorCode::Denied),
        "{failed:?}"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("target.bin")).unwrap(),
        b"keep me"
    );
    assert_eq!(names(&fixture.root), ["target.bin"]);
    // A target in a directory that does not exist fails before anything is staged.
    let links: [&dyn Open; 1] = [&fixture.link];
    assert!(put_parallel(&links, &source, "nodir/target.bin", &small_ranges()).is_err());
    assert_eq!(names(&fixture.root), ["target.bin"]);
}

#[test]
fn read_parallel_writes_the_ranges_in_order() {
    let fixture = fixture();
    let data = pseudo(777_777, 21);
    std::fs::write(fixture.root.join("r.bin"), &data).unwrap();
    let second = link_over(&fixture.root, &RW);
    let links: [&dyn Open; 2] = [&fixture.link, &second];
    let mut out = Vec::new();
    let written = read_parallel(&links, "r.bin", &mut out, &small_ranges()).unwrap();
    assert_eq!(written, data.len() as u64);
    assert!(out == data, "the bytes differ");
    let mut again = Vec::new();
    read_parallel(
        &links[..1],
        "r.bin",
        &mut again,
        &ParallelOptions::default(),
    )
    .unwrap();
    assert!(again == data);
}
