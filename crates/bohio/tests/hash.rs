//! `bohio-hash/1`, and the digests of reads and writes, served and driven in-process over memory channels.

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bohio::config::{FilesystemConfig, HashConfig};
use bohio::{Grant, Permission};
use bohio_client::fs::{Reader, Writer};
use bohio_client::hash::{hash_file, hash_items, hash_many};
use bohio_client::{Error, Open};
use bohio_proto::fs::{ReadRequest, WriteRequest};
use bohio_proto::hash::{
    Algorithm, BlockOptions, Digests, HashDone, HashInput, HashItem, HashOutput, HashRequest,
};
use bohio_proto::{ErrorBody, ErrorCode, protocol};
use bohio_transfer::hash::{Hasher, available};
use support::{Link, WAIT};
use tempfile::TempDir;

const READ: [Permission; 1] = [Permission::FsRead];
const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    link: Link,
}

fn fixture(allow: &[Permission]) -> Fixture {
    fixture_with(allow, HashConfig::default())
}

fn fixture_with(allow: &[Permission], hash: HashConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(tmp.path().join("secret.txt"), b"beyond the root").unwrap();
    let grant = Grant::allowing(allow.iter().copied()).with_fs_roots([root.clone()]);
    let link = Link::hashing(grant, FilesystemConfig::default(), hash);
    Fixture {
        _tmp: tmp,
        root,
        link,
    }
}

fn digest(algorithms: &[Algorithm], bytes: &[u8]) -> Digests {
    let mut hasher = Hasher::new(algorithms).unwrap();
    hasher.update(bytes);
    hasher.finish()
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
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

#[test]
fn every_algorithm_gives_what_a_local_hasher_gives_for_a_whole_file_and_a_range() {
    let fixture = fixture(&READ);
    let data = pseudo(300_000, 7);
    std::fs::write(fixture.root.join("data.bin"), &data).unwrap();
    let all = available();
    let whole = hash_file(&fixture.link, "data.bin", &all).unwrap();
    assert_eq!(whole, digest(&all, &data));
    let item = HashItem {
        offset: 1234,
        length: Some(100_000),
        ..HashItem::whole("data.bin")
    };
    let part = hash_items(
        &fixture.link,
        &HashRequest::new(all.clone()),
        std::slice::from_ref(&item),
    )
    .unwrap();
    let result = part[0].as_ref().unwrap();
    assert_eq!(result.hashes, digest(&all, &data[1234..101_234]));
    assert_eq!(
        (result.index, result.size, result.length),
        (0, 300_000, 100_000)
    );
    assert!(!result.unchanged && result.modified.is_some());
}

#[test]
fn a_batch_of_a_thousand_items_is_answered_in_order_on_one_stream() {
    let fixture = fixture(&READ);
    let algorithms = [Algorithm::Md5, Algorithm::Crc32c];
    let mut items = Vec::new();
    let mut wanted = Vec::new();
    for index in 0..1000_u32 {
        let bytes = pseudo(1 + (index as usize * 37) % 4000, u64::from(index) + 1);
        let name = format!("f{index:04}.bin");
        std::fs::write(fixture.root.join(&name), &bytes).unwrap();
        items.push(HashItem::whole(name));
        wanted.push(digest(&algorithms, &bytes));
    }
    let answers = hash_many(&fixture.link, &items, &algorithms).unwrap();
    assert_eq!(answers.len(), 1000);
    for (index, (answer, want)) in answers.iter().zip(&wanted).enumerate() {
        assert_eq!(answer.as_ref().unwrap(), want, "item {index}");
    }
    // One stream carried them all.
    assert_eq!(fixture.link.finished_fs().len(), 1);
}

#[test]
fn an_item_that_fails_is_an_item_error_and_the_stream_goes_on() {
    let fixture = fixture(&READ);
    std::fs::write(fixture.root.join("good.txt"), b"good").unwrap();
    std::fs::create_dir(fixture.root.join("dir")).unwrap();
    let items: Vec<HashItem> = ["good.txt", "absent.txt", "../secret.txt", "dir", "good.txt"]
        .into_iter()
        .map(HashItem::whole)
        .collect();
    let want = digest(&[Algorithm::Sha256], b"good");
    let answers = hash_many(&fixture.link, &items, &[Algorithm::Sha256]).unwrap();
    let codes: Vec<Result<(), ErrorCode>> = answers
        .iter()
        .map(|answer| answer.as_ref().map(|_| ()).map_err(|body| body.code))
        .collect();
    assert_eq!(
        codes,
        [
            Ok(()),
            Err(ErrorCode::NotFound),
            Err(ErrorCode::OutsideRoot),
            Err(ErrorCode::BadRequest),
            Ok(())
        ]
    );
    assert_eq!(answers[0].as_ref().unwrap(), &want);
    assert_eq!(answers[4].as_ref().unwrap(), &want);
    // The file outside the root was not read: its message names no path and no content.
    let body: &ErrorBody = answers[2].as_ref().unwrap_err();
    assert!(!body.message.contains("secret"), "{}", body.message);
}

#[test]
fn an_item_whose_size_and_time_match_is_unchanged_and_reads_nothing() {
    let fixture = fixture(&READ);
    let data = pseudo(70_000, 3);
    std::fs::write(fixture.root.join("a.bin"), &data).unwrap();
    let request = HashRequest::new([Algorithm::Sha256]);
    let first = hash_items(&fixture.link, &request, &[HashItem::whole("a.bin")]).unwrap();
    let first = first[0].as_ref().unwrap();
    let (size, modified) = (first.size, first.modified);

    let same = HashItem {
        if_size: Some(size),
        if_modified: modified,
        ..HashItem::whole("a.bin")
    };
    let other_size = HashItem {
        if_size: Some(size + 1),
        ..same.clone()
    };
    let only_size = HashItem {
        if_modified: None,
        ..same.clone()
    };
    let answers = hash_items(&fixture.link, &request, &[same, other_size, only_size]).unwrap();
    let [unchanged, hashed, also_hashed] = [0, 1, 2].map(|at| answers[at].as_ref().unwrap());
    assert!(unchanged.unchanged && unchanged.hashes.is_empty() && unchanged.length == 0);
    assert_eq!((unchanged.size, unchanged.modified), (size, modified));
    // A mismatch, or only one of the two conditions, hashes as usual.
    for result in [hashed, also_hashed] {
        assert!(!result.unchanged);
        assert_eq!(result.hashes, first.hashes);
    }
}

#[test]
fn a_grant_narrowed_while_the_stream_is_open_refuses_the_next_item() {
    let fixture = fixture(&READ);
    let sub = fixture.root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(fixture.root.join("top.txt"), b"top").unwrap();
    std::fs::write(sub.join("low.txt"), b"low").unwrap();
    let path = |p: &PathBuf| p.to_string_lossy().into_owned();
    let top = path(&fixture.root.join("top.txt"));
    let low = path(&sub.join("low.txt"));

    let channel = fixture
        .link
        .open(
            protocol::HASH,
            &HashRequest::new([Algorithm::Sha256]).to_metadata().unwrap(),
        )
        .unwrap();
    let send = |input: HashInput| {
        channel
            .send_timeout(&input.encode().unwrap(), WAIT)
            .expect("the item is sent");
    };
    let next = || {
        let frame = channel.receive(WAIT).unwrap().expect("a frame");
        HashOutput::decode(&frame).unwrap()
    };
    send(HashInput::Item(HashItem::whole(top.clone())));
    assert!(matches!(next(), HashOutput::Result(result) if result.index == 0));

    // The peer's grant now covers only the subdirectory.
    fixture
        .link
        .narrow(Grant::allowing(READ).with_fs_roots([sub.clone()]));
    send(HashInput::Item(HashItem::whole(top)));
    send(HashInput::Item(HashItem::whole(low)));
    send(HashInput::End);
    match next() {
        HashOutput::ItemError(error) => {
            assert_eq!((error.index, error.code), (1, ErrorCode::OutsideRoot));
        }
        other => panic!("the narrowed item was answered: {other:?}"),
    }
    assert!(matches!(next(), HashOutput::Result(result) if result.index == 2));
    assert_eq!(next(), HashOutput::Done(HashDone { items: 3 }));
}

#[test]
fn a_stream_past_max_streams_is_busy_until_one_ends() {
    let fixture = fixture_with(
        &READ,
        HashConfig {
            max_streams: 1,
            ..HashConfig::default()
        },
    );
    std::fs::write(fixture.root.join("f.txt"), b"data").unwrap();
    let holder = fixture
        .link
        .open(
            protocol::HASH,
            &HashRequest::new([Algorithm::Md5]).to_metadata().unwrap(),
        )
        .unwrap();
    // An answer to an item proves the stream holds its slot.
    holder
        .send_timeout(
            &HashInput::Item(HashItem::whole("f.txt")).encode().unwrap(),
            WAIT,
        )
        .unwrap();
    let frame = holder.receive(WAIT).unwrap().expect("a frame");
    assert!(matches!(
        HashOutput::decode(&frame).unwrap(),
        HashOutput::Result(_)
    ));

    assert_eq!(
        code(hash_file(&fixture.link, "f.txt", &[Algorithm::Md5])),
        ErrorCode::Busy
    );

    holder
        .send_timeout(&HashInput::End.encode().unwrap(), WAIT)
        .unwrap();
    let frame = holder.receive(WAIT).unwrap().expect("done");
    assert!(matches!(
        HashOutput::decode(&frame).unwrap(),
        HashOutput::Done(_)
    ));
    // The slot is returned when the engine's thread ends, just after the last frame.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match hash_file(&fixture.link, "f.txt", &[Algorithm::Md5]) {
            Ok(digests) => {
                assert_eq!(digests, digest(&[Algorithm::Md5], b"data"));
                break;
            }
            Err(Error::Remote(body))
                if body.code == ErrorCode::Busy && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(other) => panic!("the freed slot was not given: {other:?}"),
        }
    }
}

/// Seconds `hash_file` takes for `path`.
fn seconds(link: &Link, path: &str) -> f64 {
    let start = Instant::now();
    hash_file(link, path, &[Algorithm::Crc32c]).unwrap();
    start.elapsed().as_secs_f64()
}

#[test]
fn rate_holds_a_stream_to_the_configured_bytes_a_second() {
    const SIZE: usize = 16 * 1024 * 1024;
    let data = pseudo(SIZE, 11);
    let free = fixture(&READ);
    std::fs::write(free.root.join("big.bin"), &data).unwrap();
    // A shared runner's timer can overshoot a sleep, so the limit is held from below and above only loosely.
    // How long the work takes unlimited, here: the limit is chosen well below that speed (a quarter of it, and
    // at most 8 MiB a second), so it and not the machine decides how long the hash takes.
    let unlimited = seconds(&free.link, "big.bin");
    let expected = (unlimited * 4.0).max(2.0);
    let rate = (SIZE as f64 / expected) as u64;
    let limited = fixture_with(
        &READ,
        HashConfig {
            rate,
            ..HashConfig::default()
        },
    );
    std::fs::write(limited.root.join("big.bin"), &data).unwrap();
    let took = seconds(&limited.link, "big.bin");
    assert!(
        (expected * 0.8..=expected * 2.0).contains(&took),
        "16 MiB at {rate} B/s should take about {expected} s (unlimited: {unlimited} s), took {took} s"
    );
    // The control: the same hash without the rate was far quicker, so the rate is what slowed it.
    assert!(
        took > unlimited * 2.0,
        "the limit changed nothing: {took} s against {unlimited} s"
    );
}

#[test]
fn an_algorithm_the_host_does_not_offer_is_unsupported_before_any_item() {
    let fixture = fixture_with(
        &READ,
        HashConfig {
            algorithms: vec![Algorithm::Sha256, Algorithm::Blake3],
            ..HashConfig::default()
        },
    );
    std::fs::write(fixture.root.join("f.txt"), b"data").unwrap();
    assert!(hash_file(&fixture.link, "f.txt", &[Algorithm::Blake3]).is_ok());
    assert_eq!(
        code(hash_file(
            &fixture.link,
            "f.txt",
            &[Algorithm::Sha256, Algorithm::Md5]
        )),
        ErrorCode::Unsupported
    );
    assert_eq!(
        code(hash_file(
            &fixture.link,
            "f.txt",
            &[Algorithm::Other("whirlpool".into())]
        )),
        ErrorCode::Unsupported
    );
    assert_eq!(
        code(hash_file(&fixture.link, "f.txt", &[])),
        ErrorCode::BadRequest
    );
    let blocks = HashRequest {
        blocks: Some(BlockOptions {
            weak: Some("adler".into()),
            ..BlockOptions::default()
        }),
        ..HashRequest::new([Algorithm::Sha256])
    };
    assert_eq!(
        code(hash_items(
            &fixture.link,
            &blocks,
            &[HashItem::whole("f.txt")]
        )),
        ErrorCode::Unsupported
    );
    // Reads and writes are held to the same list.
    let read = ReadRequest {
        hashes: vec![Algorithm::Md5],
        ..ReadRequest::whole("f.txt")
    };
    assert_eq!(
        code(Reader::start(&fixture.link, &read, WAIT)),
        ErrorCode::Unsupported
    );
}

#[test]
fn hashing_needs_fs_read_and_a_root() {
    let write_only = fixture(&[Permission::FsWrite]);
    std::fs::write(write_only.root.join("f.txt"), b"data").unwrap();
    assert_eq!(
        code(hash_file(&write_only.link, "f.txt", &[Algorithm::Md5])),
        ErrorCode::Denied
    );
    let rootless = Link::hashing(
        Grant::allowing(READ),
        FilesystemConfig::default(),
        HashConfig::default(),
    );
    assert_eq!(
        code(hash_file(&rootless, "f.txt", &[Algorithm::Md5])),
        ErrorCode::Denied
    );
    let off = Link::hashing(
        Grant::allowing(READ).with_fs_roots([std::env::temp_dir()]),
        FilesystemConfig {
            enabled: false,
            ..FilesystemConfig::default()
        },
        HashConfig::default(),
    );
    assert_eq!(
        code(hash_file(&off, "f.txt", &[Algorithm::Md5])),
        ErrorCode::Unsupported
    );
}

#[test]
fn a_read_answers_the_digests_of_the_bytes_it_sent() {
    let fixture = fixture(&READ);
    let data = pseudo(250_000, 5);
    std::fs::write(fixture.root.join("r.bin"), &data).unwrap();
    let algorithms = vec![Algorithm::Blake3, Algorithm::Xxh64];
    let request = ReadRequest {
        offset: 1000,
        length: Some(200_000),
        hashes: algorithms.clone(),
        ..ReadRequest::whole("r.bin")
    };
    let reader = Reader::start(&fixture.link, &request, WAIT).unwrap();
    assert!(
        reader.digests().is_none(),
        "digests come with the last frame"
    );
    let got = reader.read_to_end(WAIT).unwrap();
    assert_eq!(got, data[1000..201_000]);
    assert_eq!(reader.digests().unwrap(), digest(&algorithms, &got));
    // A read that names none is answered with none.
    let plain = Reader::start(&fixture.link, &ReadRequest::whole("r.bin"), WAIT).unwrap();
    plain.read_to_end(WAIT).unwrap();
    assert_eq!(plain.digests().unwrap(), Digests::new());
}

#[test]
fn a_write_answers_write_hashes_by_default_and_what_it_asks_for_otherwise() {
    let fixture = fixture(&RW);
    let write = |request: WriteRequest| {
        let writer = Writer::start(&fixture.link, &request, WAIT).unwrap();
        writer.write(b"new content").unwrap();
        writer.finish(WAIT).unwrap().hashes
    };
    // `[filesystem] write_hashes` is sha256.
    assert_eq!(
        write(WriteRequest::replace("a.txt")),
        digest(&[Algorithm::Sha256], b"new content")
    );
    let asked = vec![Algorithm::Md5, Algorithm::Crc32c];
    assert_eq!(
        write(WriteRequest {
            hashes: Some(asked.clone()),
            ..WriteRequest::replace("a.txt")
        }),
        digest(&asked, b"new content")
    );
    assert!(
        write(WriteRequest {
            hashes: Some(Vec::new()),
            ..WriteRequest::replace("a.txt")
        })
        .is_empty()
    );
}

#[test]
fn a_write_checks_every_expected_digest_and_a_mismatch_leaves_the_target_as_it_was() {
    let fixture = fixture(&RW);
    std::fs::write(fixture.root.join("doc.txt"), b"old").unwrap();
    let right = digest(
        &[Algorithm::Sha256, Algorithm::Blake3, Algorithm::Xxh3],
        b"new",
    );
    let attempt = |expect: Digests| {
        let request = WriteRequest {
            expect,
            // Nothing is asked back: the check alone computes what it needs.
            hashes: Some(Vec::new()),
            ..WriteRequest::replace("doc.txt")
        };
        let writer = Writer::start(&fixture.link, &request, WAIT)?;
        writer.write(b"new")?;
        writer.finish(WAIT)
    };
    // Each algorithm in turn is the wrong one: the others match, and nothing is replaced.
    for wrong in [Algorithm::Sha256, Algorithm::Blake3, Algorithm::Xxh3] {
        let mut expect = right.clone();
        let flipped = if expect[&wrong].starts_with('0') {
            '1'
        } else {
            '0'
        };
        expect
            .get_mut(&wrong)
            .unwrap()
            .replace_range(0..1, &flipped.to_string());
        assert_eq!(code(attempt(expect)), ErrorCode::Io, "{wrong} wrong");
        assert_eq!(std::fs::read(fixture.root.join("doc.txt")).unwrap(), b"old");
    }
    let leftovers: Vec<_> = std::fs::read_dir(&fixture.root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".bohio-"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    // All three right: put in place, and nothing came back that was not asked for.
    let done = attempt(right).unwrap();
    assert!(done.hashes.is_empty());
    assert_eq!(std::fs::read(fixture.root.join("doc.txt")).unwrap(), b"new");
    // An expected digest of an algorithm the host does not have is refused up front.
    let unknown = Digests::from([(Algorithm::Other("whirlpool".into()), "00".into())]);
    assert_eq!(code(attempt(unknown)), ErrorCode::Unsupported);
    // The digest's length is the algorithm's.
    let short = Digests::from([(Algorithm::Blake3, "abcd".into())]);
    assert_eq!(code(attempt(short)), ErrorCode::BadRequest);
}

#[test]
fn the_control_stream_no_longer_has_a_checksum_call() {
    // A request a newer or older client could still send is answered, not fatal.
    #[derive(serde::Serialize)]
    struct Call {
        r#type: String,
        path: &'static str,
    }
    #[derive(serde::Serialize)]
    struct Request {
        id: u64,
        call: Call,
    }
    let fixture = fixture(&READ);
    let channel = fixture.link.open(protocol::CONTROL, &[]).unwrap();
    let bytes = bohio_proto::encode(&Request {
        id: 5,
        call: Call {
            // The name of the call this stream used to have.
            r#type: "check_sum".replace('_', ""),
            path: "f.txt",
        },
    })
    .unwrap();
    channel.send_timeout(&bytes, WAIT).unwrap();
    let frame = channel.receive(WAIT).unwrap().expect("an answer");
    let response: bohio_proto::control::Response = bohio_proto::decode(&frame).unwrap();
    assert_eq!(
        response.error.map(|body| body.code),
        Some(ErrorCode::BadRequest)
    );
}
