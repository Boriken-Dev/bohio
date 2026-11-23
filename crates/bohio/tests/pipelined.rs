//! Calls pipelined on one control stream, served in-process over memory channels.

#![forbid(unsafe_code)]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bohio::config::FilesystemConfig;
use bohio::{Grant, Permission};
use bohio_client::Error;
use bohio_client::fs::Fs;
use bohio_proto::ErrorCode;
use support::Link;
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    link: Link,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let grant = Grant::allowing([Permission::FsRead]).with_fs_roots([root.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    Fixture {
        _tmp: tmp,
        root,
        link,
    }
}

#[test]
fn a_thousand_stats_are_answered_in_order_and_a_missing_one_fails_alone() {
    let fixture = fixture();
    let mut paths = Vec::new();
    for index in 0..1000_usize {
        let name = format!("f{index:04}.bin");
        std::fs::write(fixture.root.join(&name), vec![7_u8; index + 1]).unwrap();
        paths.push(name);
    }
    paths[500] = "missing.bin".to_owned();
    let fs = Fs::connect(&fixture.link).unwrap();
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let answers = fs.stat_many(&refs, true);
    assert_eq!(answers.len(), 1000);
    for (index, answer) in answers.iter().enumerate() {
        if index == 500 {
            match answer {
                Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::NotFound),
                other => panic!("expected not_found, got {other:?}"),
            }
        } else {
            assert_eq!(
                answer.as_ref().unwrap().size,
                index as u64 + 1,
                "item {index}"
            );
        }
    }
    // The stream is still good for an ordinary call afterwards.
    assert_eq!(fs.stat("f0009.bin", true).unwrap().size, 10);
}

#[test]
fn a_batch_larger_than_the_stream_window_does_not_deadlock() {
    let fixture = fixture();
    std::fs::write(fixture.root.join("one.bin"), b"x").unwrap();
    let fs = Fs::connect(&fixture.link).unwrap();
    let paths = vec!["one.bin"; 30_000];
    let started = Instant::now();
    let answers = fs.stat_many(&paths, true);
    assert_eq!(answers.len(), 30_000);
    assert!(
        answers
            .iter()
            .all(|answer| answer.as_ref().is_ok_and(|meta| meta.size == 1))
    );
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[test]
fn list_many_follows_every_directorys_pages() {
    let fixture = fixture();
    for dir in ["a", "b"] {
        std::fs::create_dir(fixture.root.join(dir)).unwrap();
    }
    for index in 0..3000 {
        std::fs::write(fixture.root.join("a").join(format!("n{index:05}")), b"").unwrap();
    }
    std::fs::write(fixture.root.join("b").join("only"), b"").unwrap();
    let fs = Fs::connect(&fixture.link).unwrap();
    let listed = fs.list_many(&["a", "b", "nope"]);
    assert_eq!(listed[0].as_ref().unwrap().len(), 3000);
    assert_eq!(listed[1].as_ref().unwrap().len(), 1);
    assert!(matches!(&listed[2], Err(Error::Remote(body)) if body.code == ErrorCode::NotFound));
}
