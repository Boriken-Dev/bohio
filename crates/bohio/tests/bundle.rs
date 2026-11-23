//! Bundles of many files on one stream, streaming writes with `flush`, and resumable writes, served and driven
//! in-process over memory channels.

#![forbid(unsafe_code)]

mod support;

use std::io::Cursor;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bohio::config::{FilesystemConfig, Span};
use bohio::{Grant, Permission};
use bohio_client::bundle::{BundleFile, BundleOptions, Fetch, get_many, put_many, read_bundle};
use bohio_client::fs::{Writer, resolve_auto};
use bohio_client::{Error, Open};
use bohio_proto::compress::{Codec, Compression};
use bohio_proto::fs::{
    ReadInput, ReadItem, ReadOutput, ReadRequest, WriteInput, WriteItem, WriteMode, WriteOutput,
    WriteRequest,
};
use bohio_proto::hash::{Algorithm, Digests};
use bohio_proto::{ErrorCode, protocol};
use bohio_transfer::hash::Hasher;
use support::{Link, WAIT};
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    link: Link,
}

fn fixture_with(config: FilesystemConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let grant = Grant::allowing(RW).with_fs_roots([root.clone()]);
    let link = Link::files(grant, config);
    Fixture {
        _tmp: tmp,
        root,
        link,
    }
}

fn fixture() -> Fixture {
    fixture_with(FilesystemConfig::default())
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

/// Text that compresses well and differs between files.
fn text(index: u32, len: usize) -> Vec<u8> {
    let line = format!("line {index}: the quick brown fox jumps over the lazy dog\n");
    line.bytes().cycle().take(len).collect()
}

fn digest(algorithm: &Algorithm, bytes: &[u8]) -> String {
    let mut hasher = Hasher::new(std::slice::from_ref(algorithm)).unwrap();
    hasher.update(bytes);
    hasher.finish().remove(algorithm).unwrap()
}

#[test]
fn a_bundle_of_a_thousand_files_is_read_on_one_stream_with_each_files_header_and_digest() {
    let f = fixture();
    let mut files = Vec::new();
    for index in 0..1000_u32 {
        let bytes = pseudo(1 + (index as usize * 37) % 4096, u64::from(index) + 1);
        std::fs::write(f.root.join(format!("f{index:04}.bin")), &bytes).unwrap();
        files.push(bytes);
    }
    let items: Vec<ReadItem> = (0..1000_u32)
        .map(|index| ReadItem::whole(format!("f{index:04}.bin")))
        .collect();
    let options = BundleOptions {
        hashes: Some(vec![Algorithm::Sha256]),
        ..BundleOptions::default()
    };
    let (fetched, stats) = get_many(&f.link, &items, &options).unwrap();
    assert_eq!(stats.items, 1000);
    for (index, (got, want)) in fetched.iter().zip(&files).enumerate() {
        let got = got.as_ref().unwrap();
        assert_eq!(&got.data, want, "file {index}");
        assert_eq!(got.header.index, Some(index as u64));
        assert_eq!(got.header.size, want.len() as u64);
        assert!(got.header.modified.is_some());
        assert_eq!(
            got.hashes.get(&Algorithm::Sha256).unwrap(),
            &digest(&Algorithm::Sha256, want)
        );
    }
    // One stream, one transfer slot.
    let served = f.link.finished_fs();
    assert_eq!(served.len(), 1);
    assert!(served[0].is_ok());
}

#[test]
fn a_file_that_fails_does_not_end_a_bundle_read_and_a_range_is_honoured() {
    let f = fixture();
    std::fs::write(f.root.join("a"), b"0123456789").unwrap();
    std::fs::write(f.root.join("b"), b"bee").unwrap();
    std::fs::create_dir(f.root.join("dir")).unwrap();
    let items = vec![
        ReadItem::whole("a"),
        ReadItem::whole("missing"),
        ReadItem {
            path: "a".into(),
            offset: 3,
            length: Some(4),
        },
        ReadItem::whole("../outside"),
        ReadItem::whole("dir"),
        ReadItem::whole("b"),
    ];
    let (fetched, _) = get_many(&f.link, &items, &BundleOptions::default()).unwrap();
    assert_eq!(fetched[0].as_ref().unwrap().data, b"0123456789");
    assert_eq!(fetched[1].as_ref().unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(fetched[2].as_ref().unwrap().data, b"3456");
    assert_eq!(
        fetched[3].as_ref().unwrap_err().code,
        ErrorCode::OutsideRoot
    );
    assert_eq!(fetched[4].as_ref().unwrap_err().code, ErrorCode::BadRequest);
    assert_eq!(fetched[5].as_ref().unwrap().data, b"bee");
}

#[test]
fn a_compressed_bundle_read_with_incompressible_and_skipped_files_between_decodes_every_file() {
    let f = fixture();
    let mut expected: Vec<(String, Vec<u8>)> = Vec::new();
    for index in 0..60_u32 {
        let (name, bytes) = match index % 3 {
            // Compresses well.
            0 => (format!("t{index}.txt"), text(index, 20_000)),
            // Does not compress: sent raw, which ends the context.
            1 => (format!("r{index}.bin"), pseudo(5000, u64::from(index))),
            // Never compressed by name, though it would compress.
            _ => (format!("z{index}.zip"), text(index, 8000)),
        };
        std::fs::write(f.root.join(&name), &bytes).unwrap();
        expected.push((name, bytes));
    }
    expected.push(("empty".into(), Vec::new()));
    std::fs::write(f.root.join("empty"), b"").unwrap();
    let items: Vec<ReadItem> = expected
        .iter()
        .map(|(name, _)| ReadItem::whole(name))
        .collect();
    for codec in [Codec::Zstd, Codec::Lz4, Codec::Deflate] {
        let options = BundleOptions {
            compression: Some(Compression::new(codec.clone())),
            ..BundleOptions::default()
        };
        let (fetched, stats) = get_many(&f.link, &items, &options).unwrap();
        for ((name, bytes), got) in expected.iter().zip(&fetched) {
            assert_eq!(&got.as_ref().unwrap().data, bytes, "{codec:?} {name}");
        }
        let total: usize = expected.iter().map(|(_, bytes)| bytes.len()).sum();
        assert!(
            (stats.wire_bytes as usize) < total,
            "{codec:?}: {} of {total}",
            stats.wire_bytes
        );
    }
}

fn put_all(
    link: &Link,
    names: &[String],
    contents: &[Vec<u8>],
    options: &BundleOptions,
    modified: Option<bohio_proto::fs::Timestamp>,
) -> bohio_client::bundle::Put {
    let mut cursors: Vec<Cursor<Vec<u8>>> = contents.iter().cloned().map(Cursor::new).collect();
    let mut files: Vec<BundleFile<'_>> = names
        .iter()
        .zip(cursors.iter_mut())
        .map(|(name, cursor)| {
            let mut file = BundleFile::replace(name.clone(), cursor);
            file.item.parents = true;
            file.item.modified = modified;
            file
        })
        .collect();
    put_many(link, &mut files, options).unwrap()
}

#[test]
fn a_bundle_of_a_thousand_files_is_written_on_one_stream_each_in_place_with_its_modified_time() {
    let f = fixture();
    let names: Vec<String> = (0..1000_u32)
        .map(|index| format!("tree/d{}/f{index:04}.txt", index % 10))
        .collect();
    let contents: Vec<Vec<u8>> = (0..1000_u32).map(|index| text(index, 4096)).collect();
    let stamp = bohio_proto::fs::Timestamp {
        secs: 1_600_000_000,
        nanos: 123_456_000,
    };
    let options = BundleOptions {
        hashes: Some(vec![Algorithm::Crc32c]),
        ..BundleOptions::default()
    };
    let put = put_all(&f.link, &names, &contents, &options, Some(stamp));
    assert_eq!(put.stats.items, 1000);
    for (index, (name, want)) in names.iter().zip(&contents).enumerate() {
        let done = put.results[index].as_ref().unwrap();
        assert_eq!(done.index, Some(index as u64));
        assert_eq!(done.written, want.len() as u64);
        assert_eq!(
            done.hashes.get(&Algorithm::Crc32c).unwrap(),
            &digest(&Algorithm::Crc32c, want)
        );
        let path = f.root.join(name);
        assert_eq!(&std::fs::read(&path).unwrap(), want, "{name}");
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let wanted = std::time::UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_000);
        // File systems keep times to their own precision.
        let off = modified
            .duration_since(wanted)
            .unwrap_or_else(|error| error.duration());
        assert!(off < Duration::from_micros(1), "{name}: {off:?}");
    }
    // No temporary file is left beside them.
    let leftovers = std::fs::read_dir(f.root.join("tree/d0"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        })
        .count();
    assert_eq!(leftovers, 0);
    let served = f.link.finished_fs();
    assert_eq!(served.len(), 1);
    assert!(served[0].is_ok());
}

#[test]
fn a_failing_item_leaves_its_target_alone_and_does_not_end_the_bundle_write_even_compressed() {
    let f = fixture();
    std::fs::write(f.root.join("exists"), b"old").unwrap();
    std::fs::write(f.root.join("checked"), b"old").unwrap();
    let names = [
        "one",
        "exists",
        "../outside",
        "two",
        "checked",
        "three",
        "dir/sub/four",
    ];
    let contents: Vec<Vec<u8>> = (0..names.len() as u32).map(|i| text(i, 30_000)).collect();
    for compression in [None, Some(Compression::new(Codec::Zstd))] {
        for name in ["one", "two", "three", "dir"] {
            let _ = std::fs::remove_dir_all(f.root.join(name));
            let _ = std::fs::remove_file(f.root.join(name));
        }
        let mut cursors: Vec<Cursor<Vec<u8>>> = contents.iter().cloned().map(Cursor::new).collect();
        let mut files: Vec<BundleFile<'_>> = names
            .iter()
            .zip(cursors.iter_mut())
            .map(|(name, cursor)| {
                let mut file = BundleFile::replace((*name).to_owned(), cursor);
                file.item.parents = true;
                match *name {
                    "exists" => file.item.mode = WriteMode::Create,
                    "checked" => {
                        file.item.expect = Digests::from([(Algorithm::Crc32c, "00000000".into())]);
                    }
                    _ => {}
                }
                file
            })
            .collect();
        let options = BundleOptions {
            compression: compression.clone(),
            ..BundleOptions::default()
        };
        let put = put_many(&f.link, &mut files, &options).unwrap();
        let codes: Vec<Option<ErrorCode>> = put
            .results
            .iter()
            .map(|result| result.as_ref().err().map(|body| body.code))
            .collect();
        assert_eq!(
            codes,
            [
                None,
                Some(ErrorCode::Exists),
                Some(ErrorCode::OutsideRoot),
                None,
                Some(ErrorCode::Io),
                None,
                None
            ],
            "{compression:?}"
        );
        // What failed left the old file; what came after it is whole, whatever the context did meanwhile.
        assert_eq!(std::fs::read(f.root.join("exists")).unwrap(), b"old");
        assert_eq!(std::fs::read(f.root.join("checked")).unwrap(), b"old");
        for (index, name) in names.iter().enumerate() {
            if codes[index].is_none() {
                assert_eq!(std::fs::read(f.root.join(name)).unwrap(), contents[index]);
            }
        }
    }
}

/// A source file: a boilerplate header every file of a tree shares, then what is its own.
fn sourced(index: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in 0..40 {
        bytes.extend_from_slice(
            format!(
                "// Licensed under the Example Public License, version 2, clause {line}.
"
            )
            .as_bytes(),
        );
    }
    bytes.extend_from_slice(&text(index, 1000));
    bytes
}

#[test]
fn one_compression_context_over_a_bundle_sends_fewer_bytes_a_file_than_compressed_single_writes() {
    let f = fixture();
    let names: Vec<String> = (0..1000_u32).map(|i| format!("b{i:04}.rs")).collect();
    let contents: Vec<Vec<u8>> = (0..1000_u32).map(sourced).collect();
    let compression = Compression::new(Codec::Zstd).level(3);
    let options = BundleOptions {
        compression: Some(compression.clone()),
        ..BundleOptions::default()
    };
    let put = put_all(&f.link, &names, &contents, &options, None);
    assert!(put.results.iter().all(Result::is_ok));
    let per_bundled_file = put.stats.wire_bytes / 1000;
    // A hundred single writes: each costs its open, its frames and its end.
    let mut single_bytes = 0_u64;
    for (index, bytes) in contents.iter().enumerate().take(100) {
        let mut request = WriteRequest::replace(format!("s{index:04}.rs"));
        request.compression = Some(compression.clone());
        single_bytes += request.to_metadata().unwrap().len() as u64;
        let writer = Writer::start(&f.link, &request, WAIT).unwrap();
        writer.write(bytes).unwrap();
        writer.finish(WAIT).unwrap();
        single_bytes += writer.wire_bytes();
    }
    let per_single_file = single_bytes / 100;
    assert!(
        per_bundled_file * 2 < per_single_file,
        "{per_bundled_file} bytes a file in a bundle against {per_single_file} in single writes"
    );
    // A hundred and one streams (the bundle's and each write's) against one for the thousand.
    assert_eq!(f.link.finished_fs().len(), 101);
}

#[test]
fn a_grant_narrowed_while_a_bundle_is_open_refuses_the_next_item_of_each_direction() {
    let f = fixture();
    std::fs::write(f.root.join("a"), b"aaa").unwrap();
    std::fs::write(f.root.join("b"), b"bbb").unwrap();

    let stream = f
        .link
        .open(
            protocol::READ,
            &ReadRequest::bundle().to_metadata().unwrap(),
        )
        .unwrap();
    let send = |input: ReadInput| stream.send_timeout(&input.encode().unwrap(), WAIT).unwrap();
    let next = || ReadOutput::decode(&stream.receive(WAIT).unwrap().unwrap()).unwrap();
    assert!(matches!(next(), ReadOutput::Header(_)));
    send(ReadInput::Item(ReadItem::whole("a")));
    loop {
        if matches!(next(), ReadOutput::Done(_)) {
            break;
        }
    }
    f.link.narrow(Grant::allowing(RW));
    send(ReadInput::Item(ReadItem::whole("b")));
    let ReadOutput::ItemError(error) = next() else {
        panic!("the second item is refused");
    };
    assert_eq!((error.index, error.code), (1, ErrorCode::Denied));
    send(ReadInput::ItemsEnd);
    assert!(matches!(next(), ReadOutput::End(end) if end.items == 2));
    f.link
        .narrow(Grant::allowing(RW).with_fs_roots([f.root.clone()]));

    let stream = f
        .link
        .open(
            protocol::WRITE,
            &WriteRequest::bundle().to_metadata().unwrap(),
        )
        .unwrap();
    let send = |input: WriteInput| stream.send_timeout(&input.encode(), WAIT).unwrap();
    let next = || WriteOutput::decode(&stream.receive(WAIT).unwrap().unwrap()).unwrap();
    assert!(matches!(next(), WriteOutput::Ready(_)));
    send(WriteInput::Item(WriteItem::replace("w1")));
    send(WriteInput::Data(b"one".to_vec()));
    send(WriteInput::End);
    assert!(matches!(next(), WriteOutput::Done(done) if done.index == Some(0)));
    f.link.narrow(Grant::allowing(RW));
    send(WriteInput::Item(WriteItem::replace("w2")));
    send(WriteInput::Data(b"two".to_vec()));
    send(WriteInput::End);
    let WriteOutput::ItemError(error) = next() else {
        panic!("the second item is refused");
    };
    assert_eq!((error.index, error.code), (1, ErrorCode::Denied));
    send(WriteInput::ItemsEnd);
    assert!(matches!(next(), WriteOutput::End(end) if end.items == 2));
    assert_eq!(std::fs::read(f.root.join("w1")).unwrap(), b"one");
    assert!(!f.root.join("w2").exists());
}

#[test]
fn a_bundle_past_max_bundle_items_ends_with_busy() {
    let f = fixture_with(FilesystemConfig {
        max_bundle_items: 2,
        ..FilesystemConfig::default()
    });
    for name in ["a", "b", "c"] {
        std::fs::write(f.root.join(name), name.as_bytes()).unwrap();
    }
    let items: Vec<ReadItem> = ["a", "b", "c"].into_iter().map(ReadItem::whole).collect();
    let mut seen = 0;
    let result = read_bundle(&f.link, &items, &BundleOptions::default(), &mut |event| {
        if matches!(event, Fetch::End { .. }) {
            seen += 1;
        }
        Ok(())
    });
    assert_eq!(code(result), ErrorCode::Busy);
    assert_eq!(seen, 2);
}

#[test]
fn flush_makes_what_was_written_safe_and_says_how_much_for_append_and_at_only() {
    let f = fixture();
    let mut request = WriteRequest::replace("log.txt");
    request.mode = WriteMode::Append;
    let writer = Writer::start(&f.link, &request, WAIT).unwrap();
    writer.write(b"hello ").unwrap();
    assert_eq!(writer.flush_remote(WAIT).unwrap(), 6);
    // Before the stream ends the host already has the bytes on disk.
    assert_eq!(std::fs::read(f.root.join("log.txt")).unwrap(), b"hello ");
    writer.write(b"world").unwrap();
    assert_eq!(writer.flush_remote(WAIT).unwrap(), 11);
    assert_eq!(
        std::fs::read(f.root.join("log.txt")).unwrap(),
        b"hello world"
    );
    writer.finish(WAIT).unwrap();

    let mut request = WriteRequest::replace("log.txt");
    request.mode = WriteMode::At { offset: 6 };
    let writer = Writer::start(&f.link, &request, WAIT).unwrap();
    writer.write(b"WORLD").unwrap();
    assert_eq!(writer.flush_remote(WAIT).unwrap(), 5);
    assert_eq!(
        std::fs::read(f.root.join("log.txt")).unwrap(),
        b"hello WORLD"
    );
    writer.finish(WAIT).unwrap();

    // A replace has nothing in place to make safe: a flush is a bad request and ends the stream.
    let writer = Writer::start(&f.link, &WriteRequest::replace("r.txt"), WAIT).unwrap();
    writer.write(b"x").unwrap();
    assert_eq!(code(writer.flush_remote(WAIT)), ErrorCode::BadRequest);
    assert!(!f.root.join("r.txt").exists());
}

fn kept_temporaries(root: &std::path::Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".bohio-resume-"))
        })
        .collect()
}

fn resuming() -> Fixture {
    fixture_with(FilesystemConfig {
        resume_ttl: Span(Duration::from_secs(60)),
        ..FilesystemConfig::default()
    })
}

#[test]
fn a_resumable_write_dropped_midway_continues_from_what_the_host_holds_and_ends_with_the_whole_hash()
 {
    let f = resuming();
    let data = pseudo(3 * 1024 * 1024, 21);
    let whole = digest(&Algorithm::Sha256, &data);
    let mut request = WriteRequest::replace("big.bin");
    request.resume = true;
    let writer = Writer::start(&f.link, &request, WAIT)
        .unwrap()
        .with_chunk(32 * 1024);
    let token = writer.token().expect("the host names a token").to_owned();
    writer.write(&data[..1024 * 1024]).unwrap();
    // The host has taken what it has taken when the stream ends; the rest is lost.
    writer.abort();
    drop(writer);
    let ended = f.link.finished_fs();
    assert!(ended[0].is_err());
    assert!(
        !f.root.join("big.bin").exists(),
        "nothing replaced the target"
    );
    let kept = kept_temporaries(&f.root);
    assert_eq!(kept.len(), 1);
    let held = std::fs::metadata(&kept[0]).unwrap().len();
    assert!(held <= 1024 * 1024);

    let mut again = WriteRequest::replace("big.bin");
    again.mode = WriteMode::Resume { token };
    again.expect = Digests::from([(Algorithm::Sha256, whole.clone())]);
    let writer = Writer::start(&f.link, &again, WAIT).unwrap();
    let offset = writer.resumed_at().expect("the host says where to go on");
    assert_eq!(offset, held);
    writer
        .write(&data[usize::try_from(offset).unwrap()..])
        .unwrap();
    let done = writer.finish(WAIT).unwrap();
    assert_eq!(done.written, data.len() as u64);
    assert_eq!(done.hashes.get(&Algorithm::Sha256), Some(&whole));
    assert_eq!(std::fs::read(f.root.join("big.bin")).unwrap(), data);
    assert!(kept_temporaries(&f.root).is_empty());
}

#[test]
fn a_resume_is_unsupported_at_ttl_zero_and_for_a_token_another_peer_or_path_holds_or_one_that_expired()
 {
    // The default host keeps nothing.
    let f = fixture();
    let mut request = WriteRequest::replace("x");
    request.resume = true;
    assert_eq!(
        code(Writer::start(&f.link, &request, WAIT).map(|_| ())),
        ErrorCode::Unsupported
    );
    let mut again = WriteRequest::replace("x");
    again.mode = WriteMode::Resume {
        token: "0".repeat(32),
    };
    assert_eq!(
        code(Writer::start(&f.link, &again, WAIT).map(|_| ())),
        ErrorCode::Unsupported
    );

    let f = resuming();
    let mut request = WriteRequest::replace("file");
    request.resume = true;
    let writer = Writer::start(&f.link, &request, WAIT).unwrap();
    let token = writer.token().unwrap().to_owned();
    writer.write(b"partial").unwrap();
    writer.abort();
    drop(writer);
    let _ = f.link.finished_fs();
    let resume = |path: &str| {
        let mut again = WriteRequest::replace(path);
        again.mode = WriteMode::Resume {
            token: token.clone(),
        };
        again
    };
    // Another path, and a token nobody holds.
    assert_eq!(
        code(Writer::start(&f.link, &resume("other"), WAIT).map(|_| ())),
        ErrorCode::NotFound
    );
    let mut bogus = resume("file");
    bogus.mode = WriteMode::Resume {
        token: "f".repeat(32),
    };
    assert_eq!(
        code(Writer::start(&f.link, &bogus, WAIT).map(|_| ())),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(
            Writer::start(
                &f.link,
                &WriteRequest {
                    mode: WriteMode::Resume {
                        token: "../x".into()
                    },
                    ..WriteRequest::replace("file")
                },
                WAIT
            )
            .map(|_| ())
        ),
        ErrorCode::NotFound
    );
    // Another peer with the same grant.
    let mut stranger = Link::files(
        Grant::allowing(RW).with_fs_roots([f.root.clone()]),
        FilesystemConfig {
            resume_ttl: Span(Duration::from_secs(60)),
            ..FilesystemConfig::default()
        },
    );
    stranger.peer = "someone-else".to_owned();
    assert_eq!(
        code(Writer::start(&stranger, &resume("file"), WAIT).map(|_| ())),
        ErrorCode::NotFound
    );
    // The owner still can.
    let writer = Writer::start(&f.link, &resume("file"), WAIT).unwrap();
    assert_eq!(writer.resumed_at(), Some(7));
    writer.write(b" and the rest").unwrap();
    writer.finish(WAIT).unwrap();
    assert_eq!(
        std::fs::read(f.root.join("file")).unwrap(),
        b"partial and the rest"
    );

    // A kept file expires.
    let short = fixture_with(FilesystemConfig {
        resume_ttl: Span(Duration::from_millis(300)),
        ..FilesystemConfig::default()
    });
    let mut request = WriteRequest::replace("old");
    request.resume = true;
    let writer = Writer::start(&short.link, &request, WAIT).unwrap();
    let token = writer.token().unwrap().to_owned();
    writer.write(b"abc").unwrap();
    writer.abort();
    drop(writer);
    let _ = short.link.finished_fs();
    let deadline = Instant::now() + Duration::from_secs(5);
    while kept_temporaries(&short.root).is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(600));
    let mut again = WriteRequest::replace("old");
    again.mode = WriteMode::Resume { token };
    assert_eq!(
        code(Writer::start(&short.link, &again, WAIT).map(|_| ())),
        ErrorCode::NotFound
    );
    assert!(
        kept_temporaries(&short.root).is_empty(),
        "the expired file is swept"
    );
}

#[test]
fn a_resumed_write_whose_digest_does_not_match_replaces_nothing_and_a_finished_one_keeps_no_temporary()
 {
    let f = resuming();
    std::fs::write(f.root.join("target"), b"old").unwrap();
    let mut request = WriteRequest::replace("target");
    request.resume = true;
    let writer = Writer::start(&f.link, &request, WAIT).unwrap();
    let token = writer.token().unwrap().to_owned();
    writer.write(b"first half ").unwrap();
    writer.abort();
    drop(writer);
    let _ = f.link.finished_fs();
    let mut again = WriteRequest::replace("target");
    again.mode = WriteMode::Resume { token };
    again.expect = Digests::from([(Algorithm::Crc32c, "00000000".into())]);
    let writer = Writer::start(&f.link, &again, WAIT).unwrap();
    writer.write(b"second half").unwrap();
    assert_eq!(code(writer.finish(WAIT)), ErrorCode::Io);
    assert_eq!(std::fs::read(f.root.join("target")).unwrap(), b"old");
    assert!(kept_temporaries(&f.root).is_empty());
    // A write that asked to resume and finished leaves nothing kept either.
    let mut request = WriteRequest::replace("fresh");
    request.resume = true;
    let writer = Writer::start(&f.link, &request, WAIT).unwrap();
    writer.write(b"all of it").unwrap();
    writer.finish(WAIT).unwrap();
    assert_eq!(std::fs::read(f.root.join("fresh")).unwrap(), b"all of it");
    assert!(kept_temporaries(&f.root).is_empty());
}

#[test]
fn an_auto_compression_of_a_bundle_write_is_refused_and_a_named_one_is_taken() {
    let f = fixture();
    let names = ["a".to_owned()];
    let contents = [text(1, 1000)];
    let mut cursors: Vec<Cursor<Vec<u8>>> = contents.iter().cloned().map(Cursor::new).collect();
    let mut files: Vec<BundleFile<'_>> = names
        .iter()
        .zip(cursors.iter_mut())
        .map(|(name, cursor)| BundleFile::replace(name.clone(), cursor))
        .collect();
    let auto = BundleOptions {
        compression: Some(Compression::auto()),
        ..BundleOptions::default()
    };
    assert!(matches!(
        put_many(&f.link, &mut files, &auto),
        Err(Error::Compression(_))
    ));
    let info = bohio_proto::info::Info::default();
    assert!(resolve_auto(&info, &Compression::auto()).is_none());
}
