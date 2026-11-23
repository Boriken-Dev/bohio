//! `stats`, `which` and the followed read, served and driven in-process over memory channels.

#![forbid(unsafe_code)]

mod support;

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bohio::config::FilesystemConfig;
use bohio::{Grant, Permission};
use bohio_client::control::Control;
use bohio_client::follow::{Follow, FollowEvent, FollowOptions, follow};
use bohio_client::{Error, Open};
use bohio_proto::ErrorCode;
use bohio_proto::fs::{ReadFollow, ReadOutput, ReadRequest};
use bohio_proto::protocol;
use support::{Link, WAIT, helper};
use tempfile::TempDir;

const TICK: Option<Duration> = Some(Duration::from_millis(20));

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
    }
}

#[test]
fn stats_report_a_machine_with_memory_processors_and_a_disk() {
    let link = Link::files(
        Grant::allowing([Permission::Info]),
        FilesystemConfig::default(),
    );
    let control = Control::open(&link).unwrap();
    let stats = control.stats(WAIT).unwrap();
    assert!(stats.memory.total > 0, "{stats:?}");
    assert!(stats.memory.available <= stats.memory.total);
    assert!(stats.cpus >= 1);
    assert!(stats.uptime_s > 0);
    assert!(!stats.disks.is_empty(), "{stats:?}");
    assert!(stats.disks.iter().all(|disk| !disk.mount.is_empty()));
    assert!(stats.disks.iter().any(|disk| disk.total > 0));
    assert_eq!(stats.load.is_some(), cfg!(unix));
}

#[test]
fn stats_and_which_need_info() {
    let link = Link::files(
        Grant::allowing([Permission::FsRead]),
        FilesystemConfig::default(),
    );
    let control = Control::open(&link).unwrap();
    assert_eq!(code(control.stats(WAIT)), ErrorCode::Denied);
    assert_eq!(code(control.which(&["sh"], WAIT)), ErrorCode::Denied);
}

#[test]
fn which_finds_a_program_on_the_host_s_own_path_and_says_null_for_the_rest() {
    let mut link = Link::files(
        Grant::allowing([Permission::Info]),
        FilesystemConfig::default(),
    );
    let program = PathBuf::from(helper());
    let directory = program.parent().unwrap().to_owned();
    link.path = Arc::new(OsString::from(directory.as_os_str()));
    let name = program.file_stem().unwrap().to_string_lossy().into_owned();
    let control = Control::open(&link).unwrap();
    let found = control
        .which(
            &[
                name.as_str(),
                "no-such-program-here",
                // A name with a directory in it is not looked up.
                "./exec_helper",
                "sub/exec_helper",
            ],
            WAIT,
        )
        .unwrap();
    assert_eq!(found.len(), 4);
    let located = found[&name].as_deref().expect("the helper is found");
    assert!(Path::new(located).is_absolute(), "{located}");
    assert!(
        Path::new(located).file_stem().unwrap().to_string_lossy() == name,
        "{located}"
    );
    assert_eq!(found["no-such-program-here"], None);
    assert_eq!(found["./exec_helper"], None);
    assert_eq!(found["sub/exec_helper"], None);
    assert_eq!(code(control.which(&[], WAIT)), ErrorCode::BadRequest);
}

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

fn quick(by_name: bool) -> FollowOptions {
    FollowOptions {
        by_name,
        interval: Some(Duration::from_millis(10)),
        ..FollowOptions::default()
    }
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

/// Reads data until `text` has arrived in all (events that are not data end the wait with a panic).
fn data_until(following: &mut Follow, text: &str) -> String {
    let mut got = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !got.ends_with(text) {
        assert!(
            std::time::Instant::now() < deadline,
            "waiting for {text:?}, got {got:?}"
        );
        match following.next_within(TICK).unwrap() {
            Some(FollowEvent::Data(bytes)) => got.push_str(&String::from_utf8_lossy(&bytes)),
            Some(other) => panic!("expected data, got {other:?} after {got:?}"),
            None => {}
        }
    }
    got
}

#[test]
fn a_follow_sends_the_file_and_then_every_line_appended() {
    let f = fixture();
    let log = f.root.join("app.log");
    std::fs::write(&log, "one\ntwo\n").unwrap();
    let mut following = follow(&f.link, "app.log", &quick(false), WAIT).unwrap();
    assert_eq!(following.size(), 8);
    assert_eq!(following.initial_length(), 8);
    assert_eq!(data_until(&mut following, "two\n"), "one\ntwo\n");
    // Nothing is sent while nothing is written.
    assert!(following.next_within(TICK).unwrap().is_none());
    append(&log, "three\n");
    assert_eq!(data_until(&mut following, "three\n"), "three\n");
    for number in 4..9 {
        append(&log, &format!("line {number}\n"));
    }
    assert_eq!(
        data_until(&mut following, "line 8\n"),
        "line 4\nline 5\nline 6\nline 7\nline 8\n"
    );
    drop(following);
    // The host's follow ends with the stream: nothing is left running.
    let served = f.link.finished_fs();
    assert_eq!(served.len(), 1);
    assert!(served[0].is_ok(), "{served:?}");
}

#[test]
fn a_follow_starts_at_an_offset_or_this_many_bytes_before_the_end() {
    let f = fixture();
    let log = f.root.join("app.log");
    std::fs::write(&log, "0123456789").unwrap();
    let mut from_four = follow(
        &f.link,
        "app.log",
        &FollowOptions {
            offset: 4,
            ..quick(false)
        },
        WAIT,
    )
    .unwrap();
    assert_eq!(data_until(&mut from_four, "9"), "456789");
    let mut last_three = follow(
        &f.link,
        "app.log",
        &FollowOptions {
            tail: Some(3),
            ..quick(false)
        },
        WAIT,
    )
    .unwrap();
    assert_eq!(last_three.initial_length(), 3);
    assert_eq!(data_until(&mut last_three, "9"), "789");
    // `tail` of zero sends only what comes after, and a tail longer than the file sends all of it.
    let mut after = follow(
        &f.link,
        "app.log",
        &FollowOptions {
            tail: Some(0),
            ..quick(false)
        },
        WAIT,
    )
    .unwrap();
    assert_eq!(after.initial_length(), 0);
    assert!(after.next_within(TICK).unwrap().is_none());
    append(&log, "AB");
    assert_eq!(data_until(&mut after, "AB"), "AB");
    let mut all = follow(
        &f.link,
        "app.log",
        &FollowOptions {
            tail: Some(1000),
            ..quick(false)
        },
        WAIT,
    )
    .unwrap();
    assert_eq!(data_until(&mut all, "AB"), "0123456789AB");
    assert_eq!(
        code(follow(
            &f.link,
            "app.log",
            &FollowOptions {
                offset: 100,
                ..quick(false)
            },
            WAIT
        )),
        ErrorCode::BadRequest
    );
}

#[test]
fn a_shorter_file_is_reported_and_followed_from_its_new_end() {
    let f = fixture();
    let log = f.root.join("app.log");
    std::fs::write(&log, "a long first line\n").unwrap();
    let mut following = follow(&f.link, "app.log", &quick(false), WAIT).unwrap();
    data_until(&mut following, "line\n");
    // A copy-truncate rotation: the same file, shorter.
    std::fs::write(&log, "new\n").unwrap();
    let truncated = loop {
        match following.next_within(TICK).unwrap() {
            Some(FollowEvent::Truncated(truncated)) => break truncated,
            Some(FollowEvent::Data(bytes)) => {
                panic!("data before the truncation: {bytes:?}")
            }
            None => {}
        }
    };
    assert_eq!(truncated.size, 4);
    assert!(!truncated.replaced);
    append(&log, "after\n");
    assert_eq!(data_until(&mut following, "after\n"), "after\n");
}

#[test]
fn by_name_a_file_that_takes_the_name_is_followed_from_its_start_and_by_handle_it_is_not() {
    let f = fixture();
    let log = f.root.join("app.log");
    std::fs::write(&log, "old 1\n").unwrap();
    let mut by_name = follow(&f.link, "app.log", &quick(true), WAIT).unwrap();
    let mut by_handle = follow(&f.link, "app.log", &quick(false), WAIT).unwrap();
    data_until(&mut by_name, "old 1\n");
    data_until(&mut by_handle, "old 1\n");

    // The rotation: the log moves aside and a new one takes the name. The old file gets one more line, which a follow
    // by name reads before it moves on.
    std::fs::rename(&log, f.root.join("app.log.1")).unwrap();
    append(&f.root.join("app.log.1"), "old 2\n");
    // A name that is free for a moment is waited out, not an error.
    assert!(
        by_name
            .next_within(Some(Duration::from_millis(100)))
            .unwrap()
            .is_some()
    );
    std::fs::write(&log, "fresh 1\nfresh 2\n").unwrap();
    let mut events = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !matches!(events.last(), Some(FollowEvent::Data(bytes)) if bytes.ends_with(b"fresh 2\n"))
    {
        assert!(std::time::Instant::now() < deadline, "{events:?}");
        if let Some(event) = by_name.next_within(TICK).unwrap() {
            events.push(event);
        }
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            FollowEvent::Truncated(truncated) if truncated.replaced && truncated.size == 16
        )),
        "{events:?}"
    );
    // By handle the follow stays on the file that was opened, now renamed.
    assert_eq!(data_until(&mut by_handle, "old 2\n"), "old 2\n");
    append(&f.root.join("app.log.1"), "old 3\n");
    assert_eq!(data_until(&mut by_handle, "old 3\n"), "old 3\n");
    assert!(by_handle.next_within(TICK).unwrap().is_none());
}

#[test]
fn a_follow_is_refused_what_a_read_is_and_what_cannot_follow() {
    let f = fixture();
    std::fs::write(f.root.join("app.log"), "x").unwrap();
    assert_eq!(
        code(follow(&f.link, "missing.log", &quick(false), WAIT)),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(follow(&f.link, "../outside", &quick(false), WAIT)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(follow(
            &f.link,
            "app.log",
            &FollowOptions {
                interval: Some(Duration::ZERO),
                ..FollowOptions::default()
            },
            WAIT
        )),
        // An interval of zero milliseconds is carried as zero: meaningless.
        ErrorCode::BadRequest
    );
    // The requests a follow cannot serve: a length, hashes, compression, a delta, a bundle.
    for edit in [
        |request: &mut ReadRequest| request.length = Some(1),
        |request: &mut ReadRequest| {
            request.hashes = vec![bohio_proto::hash::Algorithm::Sha256];
        },
        |request: &mut ReadRequest| {
            request.compression = Some(bohio_proto::compress::Compression::new(
                bohio_proto::compress::Codec::Lz4,
            ));
        },
        |request: &mut ReadRequest| request.items = true,
    ] {
        let mut request = ReadRequest::whole("app.log");
        request.follow = Some(ReadFollow::default());
        edit(&mut request);
        let channel = f
            .link
            .open(protocol::READ, &request.to_metadata().unwrap())
            .unwrap();
        let frame = channel.receive(WAIT).unwrap().unwrap();
        match ReadOutput::decode(&frame).unwrap() {
            ReadOutput::Error(body) => assert_eq!(body.code, ErrorCode::BadRequest),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    // Without `fs.read` nothing follows.
    let denied = Link::files(
        Grant::allowing([Permission::Info]).with_fs_roots([f.root.clone()]),
        FilesystemConfig::default(),
    );
    assert_eq!(
        code(follow(&denied, "app.log", &quick(false), WAIT)),
        ErrorCode::Denied
    );
}

#[test]
fn a_follow_holds_a_follow_slot_and_no_transfer_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("app.log"), "x").unwrap();
    let grant = Grant::allowing([Permission::FsRead]).with_fs_roots([root.clone()]);
    let link = Link::files(
        grant,
        FilesystemConfig {
            max_follows: 2,
            max_transfers: 1,
            ..FilesystemConfig::default()
        },
    );
    let first = follow(&link, "app.log", &quick(false), WAIT).unwrap();
    let second = follow(&link, "app.log", &quick(false), WAIT).unwrap();
    // The third is over the limit.
    assert_eq!(
        code(follow(&link, "app.log", &quick(false), WAIT)),
        ErrorCode::Busy
    );
    // Two follows and a transfer limit of one: a plain read still runs.
    let read = bohio_client::fs::Reader::start(&link, &ReadRequest::whole("app.log"), WAIT)
        .unwrap()
        .next_chunk(WAIT)
        .unwrap();
    assert_eq!(read.as_deref(), Some(&b"x"[..]));
    drop(first);
    // A slot is free again once a follow's stream is closed.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match follow(&link, "app.log", &quick(false), WAIT) {
            Ok(_third) => break,
            Err(Error::Remote(body)) if body.code == ErrorCode::Busy => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the slot stayed taken"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(other) => panic!("{other:?}"),
        }
    }
    drop(second);
}
