//! Compression of reads, writes and exec streams, and prepared reads, served and driven in-process over
//! memory channels.

mod support;

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bohio::config::{CompressionConfig, ExecutionConfig, FilesystemConfig, Span};
use bohio::{Grant, Permission};
use bohio_client::exec::Exec;
use bohio_client::fs::{PutOptions, ReadOptions, Reader, Writer, put};
use bohio_client::{Error, Open};
use bohio_proto::compress::{Codec, Compression, Resume};
use bohio_proto::exec::{Request, Stdin};
use bohio_proto::fs::{ReadOutput, ReadRequest, WriteRequest};
use bohio_proto::hash::Algorithm;
use bohio_proto::{ErrorCode, protocol};
use bohio_transfer::compress::{Bounds, Decoder};
use bohio_transfer::hash::{Hasher, available};
use coqui_transport::Channel;
use sha2::{Digest, Sha256};
use support::{Link, WAIT, helper};
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    prepared: PathBuf,
    link: Link,
}

fn fixture(compression: CompressionConfig) -> Fixture {
    fixture_with(compression, FilesystemConfig::default())
}

fn fixture_with(mut compression: CompressionConfig, files: FilesystemConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let prepared = tmp.path().join("prepared");
    std::fs::create_dir(&root).unwrap();
    compression
        .prepare_dir
        .get_or_insert_with(|| prepared.clone());
    let grant = Grant::allowing(RW).with_fs_roots([root.clone()]);
    let link = Link::files(grant, files).compressing(compression);
    Fixture {
        _tmp: tmp,
        root,
        prepared,
        link,
    }
}

/// Every algorithm this build compresses with, once at its default.
fn codecs() -> Vec<Compression> {
    let mut all = vec![
        Compression::new(Codec::Lz4),
        Compression::new(Codec::Deflate),
    ];
    if cfg!(feature = "zstd") {
        all.insert(0, Compression::new(Codec::Zstd));
    }
    all
}

fn text(length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length + 80);
    let mut n = 0_u32;
    while out.len() < length {
        out.extend_from_slice(
            format!(
                "line {n}: the quick brown fox {} jumps over the lazy dog {}\n",
                n % 17,
                n.wrapping_mul(2_654_435_761) % 1000
            )
            .as_bytes(),
        );
        n += 1;
    }
    out.truncate(length);
    out
}

fn noise(length: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
    }
}

/// The frames of a read, as they travelled (a reader would have decoded them).
fn frames(link: &Link, request: &ReadRequest) -> Vec<ReadOutput> {
    let channel = link
        .open(protocol::READ, &request.to_metadata().unwrap())
        .unwrap();
    let mut all = Vec::new();
    while let Ok(Some(frame)) = channel.receive(WAIT) {
        let output = ReadOutput::decode(&frame).unwrap();
        let last = matches!(output, ReadOutput::Done(_) | ReadOutput::Error(_));
        all.push(output);
        if last {
            break;
        }
    }
    all
}

fn counts(frames: &[ReadOutput]) -> (usize, usize) {
    let raw = frames
        .iter()
        .filter(|frame| matches!(frame, ReadOutput::Data(_)))
        .count();
    let compressed = frames
        .iter()
        .filter(|frame| matches!(frame, ReadOutput::Compressed(_)))
        .count();
    (raw, compressed)
}

fn ask(compression: Compression, path: &str) -> ReadRequest {
    ReadRequest {
        compression: Some(compression),
        hashes: vec![Algorithm::Sha256],
        ..ReadRequest::whole(path)
    }
}

#[test]
fn a_read_round_trips_in_every_algorithm_and_matches_its_digest() {
    let fixture = fixture(CompressionConfig::default());
    let data = text(1_500_000);
    std::fs::write(fixture.root.join("log.txt"), &data).unwrap();
    for compression in codecs() {
        let reader =
            Reader::start(&fixture.link, &ask(compression.clone(), "log.txt"), WAIT).unwrap();
        let used = reader.compression().cloned().expect("the host used it");
        assert_eq!(used.algorithm, compression.algorithm);
        assert_ne!(used.algorithm, Codec::Auto);
        assert_eq!(reader.header().length, data.len() as u64);
        let bytes = reader.read_to_end(WAIT).unwrap();
        assert_eq!(bytes, data, "{compression:?}");
        assert_eq!(
            reader.digests().unwrap().get(&Algorithm::Sha256),
            Some(&sha256(&data))
        );
        let all = frames(&fixture.link, &ask(compression.clone(), "log.txt"));
        let (_, compressed) = counts(&all);
        assert!(
            compressed >= 20,
            "{compression:?} sent {compressed} compressed frames"
        );
        let wire: usize = all
            .iter()
            .map(|frame| match frame {
                ReadOutput::Compressed(bytes) | ReadOutput::Data(bytes) => bytes.len(),
                _ => 0,
            })
            .sum();
        assert!(
            wire < data.len() / 2,
            "{compression:?} moved {wire} of {}",
            data.len()
        );
    }
}

#[test]
fn auto_is_the_hosts_default_and_the_header_never_says_auto() {
    let default = Compression::new(Codec::Lz4);
    let fixture = fixture(CompressionConfig {
        default: default.clone(),
        ..CompressionConfig::default()
    });
    std::fs::write(fixture.root.join("log.txt"), text(200_000)).unwrap();
    let reader = Reader::start(&fixture.link, &ask(Compression::auto(), "log.txt"), WAIT).unwrap();
    assert_eq!(reader.compression().unwrap().algorithm, Codec::Lz4);
    assert_eq!(reader.read_to_end(WAIT).unwrap().len(), 200_000);
}

#[test]
fn a_stream_can_mix_raw_and_compressed_frames() {
    let fixture = fixture(CompressionConfig {
        // The whole-file trial would send this file raw; the per-frame choice is what is under test.
        sample_bytes: 0,
        ..CompressionConfig::default()
    });
    let mut data = text(300_000);
    data.extend(noise(300_000));
    data.extend(text(300_000));
    std::fs::write(fixture.root.join("mixed.bin"), &data).unwrap();
    for compression in codecs() {
        let all = frames(&fixture.link, &ask(compression.clone(), "mixed.bin"));
        let (raw, compressed) = counts(&all);
        assert!(
            raw >= 4 && compressed >= 8,
            "{compression:?}: {raw} raw, {compressed} compressed"
        );
        let reader =
            Reader::start(&fixture.link, &ask(compression.clone(), "mixed.bin"), WAIT).unwrap();
        assert_eq!(reader.read_to_end(WAIT).unwrap(), data, "{compression:?}");
    }
}

#[cfg(feature = "zstd")]
#[test]
fn a_level_above_the_cap_is_clamped_and_the_header_says_so() {
    let fixture = fixture(CompressionConfig {
        max_level: 5,
        window_log_max: 20,
        ..CompressionConfig::default()
    });
    std::fs::write(fixture.root.join("log.txt"), text(300_000)).unwrap();
    let wish = Compression::new(Codec::Zstd).level(19).window_log(27);
    let reader = Reader::start(&fixture.link, &ask(wish, "log.txt"), WAIT).unwrap();
    let used = reader.compression().unwrap();
    assert_eq!((used.level, used.window_log), (Some(5), Some(20)));
    assert_eq!(reader.read_to_end(WAIT).unwrap().len(), 300_000);
}

#[test]
fn excluded_names_and_skipped_extensions_are_sent_raw() {
    let fixture = fixture(CompressionConfig::default());
    let data = text(200_000);
    for name in [
        "site.key",
        "id_rsa",
        ".env",
        "backup.zip",
        "photo.JPG",
        "dir.pem",
    ] {
        std::fs::write(fixture.root.join(name), &data).unwrap();
        let compression = codecs().remove(0);
        let reader = Reader::start(&fixture.link, &ask(compression.clone(), name), WAIT).unwrap();
        assert_eq!(reader.compression(), None, "{name} was compressed");
        assert_eq!(reader.read_to_end(WAIT).unwrap(), data);
        let (raw, compressed) = counts(&frames(&fixture.link, &ask(compression, name)));
        assert!(
            raw > 0 && compressed == 0,
            "{name}: {raw} raw, {compressed} compressed"
        );
    }
    // The control: the same bytes under an ordinary name are compressed.
    std::fs::write(fixture.root.join("notes.txt"), &data).unwrap();
    let reader = Reader::start(&fixture.link, &ask(codecs().remove(0), "notes.txt"), WAIT).unwrap();
    assert!(reader.compression().is_some());
    // A host may shorten the list: then the extension is compressed again.
    let open = fixture_with_skips(Vec::new());
    std::fs::write(open.root.join("backup.zip"), &data).unwrap();
    let reader = Reader::start(&open.link, &ask(codecs().remove(0), "backup.zip"), WAIT).unwrap();
    assert!(reader.compression().is_some());
}

fn fixture_with_skips(skip_extensions: Vec<String>) -> Fixture {
    fixture(CompressionConfig {
        skip_extensions,
        ..CompressionConfig::default()
    })
}

#[test]
fn sampling_sends_noise_raw_and_text_compressed() {
    let fixture = fixture(CompressionConfig::default());
    std::fs::write(fixture.root.join("random.bin"), noise(500_000)).unwrap();
    std::fs::write(fixture.root.join("words.bin"), text(500_000)).unwrap();
    for compression in codecs() {
        let on_noise =
            Reader::start(&fixture.link, &ask(compression.clone(), "random.bin"), WAIT).unwrap();
        assert_eq!(on_noise.compression(), None, "{compression:?}");
        assert_eq!(on_noise.read_to_end(WAIT).unwrap(), noise(500_000));
        let on_text =
            Reader::start(&fixture.link, &ask(compression.clone(), "words.bin"), WAIT).unwrap();
        assert!(on_text.compression().is_some(), "{compression:?}");
        // Control: with the trial off the noise is tried and goes frame by frame.
        let trial_off = fixture_with_sampling(0);
        std::fs::write(trial_off.root.join("random.bin"), noise(500_000)).unwrap();
        let tried = Reader::start(
            &trial_off.link,
            &ask(compression.clone(), "random.bin"),
            WAIT,
        )
        .unwrap();
        assert!(tried.compression().is_some(), "{compression:?}");
        let (raw, compressed) = counts(&frames(&trial_off.link, &ask(compression, "random.bin")));
        assert!(raw > 0 && compressed == 0);
    }
}

fn fixture_with_sampling(sample_bytes: usize) -> Fixture {
    fixture(CompressionConfig {
        sample_bytes,
        ..CompressionConfig::default()
    })
}

#[test]
fn an_algorithm_the_host_does_not_allow_is_unsupported() {
    let fixture = fixture(CompressionConfig {
        allowed: vec![Codec::Lz4],
        default: Compression::new(Codec::Lz4),
        ..CompressionConfig::default()
    });
    std::fs::write(fixture.root.join("log.txt"), text(1000)).unwrap();
    for refused in [
        Compression::new(Codec::Deflate),
        Compression::new(Codec::Zstd),
        Compression::new(Codec::Other("brotli".into())),
    ] {
        assert_eq!(
            code(Reader::start(
                &fixture.link,
                &ask(refused.clone(), "log.txt"),
                WAIT
            )),
            ErrorCode::Unsupported,
            "{refused:?}"
        );
    }
    let allowed = Reader::start(
        &fixture.link,
        &ask(Compression::new(Codec::Lz4), "log.txt"),
        WAIT,
    )
    .unwrap();
    assert_eq!(allowed.read_to_end(WAIT).unwrap().len(), 1000);
    let write = WriteRequest {
        compression: Some(Compression::new(Codec::Deflate)),
        ..WriteRequest::replace("w.txt")
    };
    assert_eq!(
        code(Writer::start(&fixture.link, &write, WAIT)),
        ErrorCode::Unsupported
    );
}

#[test]
fn a_write_round_trips_in_every_algorithm_and_reports_its_digest() {
    let fixture = fixture(CompressionConfig::default());
    let mut data = text(700_000);
    data.extend(noise(200_000));
    data.extend(text(100_000));
    for (index, compression) in codecs().into_iter().enumerate() {
        let name = format!("out{index}.bin");
        let request = WriteRequest {
            hashes: Some(vec![Algorithm::Sha256]),
            ..WriteRequest::replace(&name)
        };
        let options = PutOptions {
            compression: Some(compression.clone()),
            ..PutOptions::default()
        };
        let done = put(
            &fixture.link,
            &request,
            &mut Cursor::new(data.clone()),
            &options,
            WAIT,
        )
        .unwrap();
        assert_eq!(done.written, data.len() as u64);
        assert_eq!(done.hashes.get(&Algorithm::Sha256), Some(&sha256(&data)));
        assert_eq!(
            std::fs::read(fixture.root.join(&name)).unwrap(),
            data,
            "{compression:?}"
        );
    }
}

#[test]
fn a_prepared_write_sends_one_context_and_the_host_reads_it() {
    let fixture = fixture(CompressionConfig::default());
    let data = text(900_000);
    for (index, compression) in codecs().into_iter().enumerate() {
        let name = format!("prepared{index}.bin");
        let options = PutOptions {
            compression: Some(compression.clone()),
            prepare: true,
            chunk: Some(10_000),
            ..PutOptions::default()
        };
        let done = put(
            &fixture.link,
            &WriteRequest::replace(&name),
            &mut Cursor::new(data.clone()),
            &options,
            WAIT,
        )
        .unwrap();
        assert_eq!(done.written, data.len() as u64);
        assert_eq!(
            std::fs::read(fixture.root.join(&name)).unwrap(),
            data,
            "{compression:?}"
        );
    }
}

#[test]
fn compressed_frames_without_a_declared_compression_are_refused() {
    let fixture = fixture(CompressionConfig::default());
    let writer = Writer::start(&fixture.link, &WriteRequest::replace("x.bin"), WAIT).unwrap();
    writer.write_compressed(&[1, 2, 3]).unwrap();
    assert_eq!(code(writer.finish(WAIT)), ErrorCode::BadRequest);
}

#[test]
fn a_write_of_zeros_past_the_expansion_bound_ends_and_zero_turns_the_bound_off() {
    let zeros = 64 * 1024 * 1024;
    let compression = codecs().remove(0);
    let request = WriteRequest::replace("zeros.bin");
    let options = PutOptions {
        compression: Some(compression),
        ..PutOptions::default()
    };
    let bounded = fixture(bounded_config());
    let result = put(
        &bounded.link,
        &request,
        &mut std::io::repeat(0).take(zeros as u64),
        &options,
        WAIT,
    );
    assert_eq!(code(result), ErrorCode::BadRequest);
    assert!(
        !bounded.root.join("zeros.bin").exists(),
        "a refused replace puts nothing in place"
    );
    let open = fixture(CompressionConfig {
        max_expansion: 0,
        ..CompressionConfig::default()
    });
    let done = put(
        &open.link,
        &request,
        &mut std::io::repeat(0).take(zeros as u64),
        &options,
        WAIT,
    )
    .unwrap();
    assert_eq!(done.written, zeros as u64);
    assert_eq!(
        std::fs::metadata(open.root.join("zeros.bin"))
            .unwrap()
            .len(),
        zeros as u64
    );
}

use std::io::Read;

#[cfg(feature = "zstd")]
#[test]
fn a_frame_with_a_window_over_the_hosts_bound_is_refused() {
    let fixture = fixture(CompressionConfig {
        window_log_max: 17,
        ..CompressionConfig::default()
    });
    let wish = Compression::new(Codec::Zstd).window_log(27);
    let request = WriteRequest {
        compression: Some(wish),
        ..WriteRequest::replace("w.bin")
    };
    let writer = Writer::start(&fixture.link, &request, WAIT).unwrap();
    let sent = writer.write(&text(200_000));
    let result = sent.and_then(|()| writer.finish(WAIT));
    assert_eq!(code(result), ErrorCode::BadRequest);
    assert!(!fixture.root.join("w.bin").exists());
}

/// A host that sends what a test wrote, frame by frame, whatever the request.
struct Crafted {
    frames: Vec<Vec<u8>>,
}

impl Open for Crafted {
    fn open(&self, _protocol: &str, _metadata: &[u8]) -> Result<Box<dyn Channel>, Error> {
        use coqui_transport::frame::LengthPrefix;
        use coqui_transport::{StreamChannelBuilder, memory};
        let build = |stream| {
            StreamChannelBuilder::new()
                .codec(LengthPrefix::u32_be())
                .stream(stream)
                .pending(4)
                .queued_bytes(1024 * 1024)
                .build()
                .unwrap()
        };
        let (host, client) = memory::pipe(64 * 1024);
        let host = build(host);
        let frames = self.frames.clone();
        std::thread::spawn(move || {
            for frame in frames {
                if host
                    .send_timeout(&frame, Some(Duration::from_secs(5)))
                    .is_err()
                {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        });
        Ok(Box::new(build(client)))
    }
}

#[test]
fn a_reader_stops_at_the_announced_length_on_a_crafted_overlong_stream() {
    use bohio_proto::fs::{ReadDone, ReadHeader};
    let lie = |used: Compression, piece: Vec<u8>, announced: u64| Crafted {
        frames: vec![
            ReadOutput::Header(ReadHeader {
                compression: Some(used),
                ..ReadHeader::raw(announced, announced)
            })
            .encode()
            .unwrap(),
            ReadOutput::Compressed(piece).encode().unwrap(),
            ReadOutput::Done(ReadDone::default()).encode().unwrap(),
        ],
    };
    for compression in codecs() {
        // 300 000 bytes of text, announced as 1000.
        let big = bohio_transfer::compress::compress_all(
            &compression,
            &bohio_transfer::compress::Caps::default(),
            &text(300_000),
        )
        .unwrap();
        let host = lie(compression.clone(), big, 1000);
        let reader = Reader::start(&host, &ReadRequest::whole("x"), WAIT).unwrap();
        let mut sink = Vec::new();
        let result = reader.copy_to(&mut sink, WAIT);
        assert!(
            matches!(result, Err(Error::Protocol(_))),
            "{compression:?}: {result:?}"
        );
        assert!(
            sink.len() <= 1000,
            "{compression:?} wrote {} bytes",
            sink.len()
        );
    }
    // A raw frame past the announced length is held to it too.
    let host = Crafted {
        frames: vec![
            ReadOutput::Header(ReadHeader::raw(10, 4)).encode().unwrap(),
            ReadOutput::Data(vec![0; 9]).encode().unwrap(),
        ],
    };
    let reader = Reader::start(&host, &ReadRequest::whole("x"), WAIT).unwrap();
    assert!(matches!(reader.read_to_end(WAIT), Err(Error::Protocol(_))));
}

#[test]
fn exec_output_and_input_round_trip_in_every_algorithm() {
    let link = Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    );
    for compression in codecs() {
        // Output on both streams, each its own context.
        let mut request = Request::argv([helper().as_str(), "text", "400000"]);
        request.output_compression = Some(compression.clone());
        let exec = Exec::start(&link, &request).unwrap();
        let done = exec.wait(WAIT).unwrap();
        assert_eq!(done.stdout, text(400_000), "{compression:?} stdout");
        assert_eq!(done.stderr, text(400_000), "{compression:?} stderr");
        let (used, _) = exec.compression();
        assert_eq!(used.unwrap().algorithm, compression.algorithm);

        // Input, echoed back through `cat` compressed too.
        let mut request = Request::argv([helper().as_str(), "cat"]);
        request.stdin = Stdin::Open;
        request.stdin_compression = Some(compression.clone());
        request.output_compression = Some(compression.clone());
        let exec = Exec::start(&link, &request).unwrap();
        let data = text(300_000);
        exec.write_stdin(&data).unwrap();
        exec.write_stdin(&noise(50_000)).unwrap();
        exec.write_stdin(b"tail").unwrap();
        exec.finish_stdin().unwrap();
        let done = exec.wait(WAIT).unwrap();
        let mut wanted = data;
        wanted.extend(noise(50_000));
        wanted.extend_from_slice(b"tail");
        assert_eq!(done.stdout, wanted, "{compression:?} cat");
        let (out, stdin) = exec.compression();
        assert_eq!(out.unwrap().algorithm, compression.algorithm);
        assert_eq!(stdin.unwrap().algorithm, compression.algorithm);
    }
}

#[test]
fn exec_output_that_does_not_compress_goes_raw() {
    let link = Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    );
    let mut request = Request::argv([helper().as_str(), "noise", "200000"]);
    request.output_compression = Some(codecs().remove(0));
    let done = Exec::start(&link, &request).unwrap().wait(WAIT).unwrap();
    assert_eq!(done.stdout, noise(200_000));
}

#[test]
fn exec_compression_is_unsupported_when_not_allowed_and_auto_input_is_refused() {
    let link = Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    )
    .compressing(CompressionConfig {
        allowed: vec![Codec::Lz4],
        default: Compression::new(Codec::Lz4),
        ..CompressionConfig::default()
    });
    let mut request = Request::argv([helper().as_str(), "echo", "x"]);
    request.output_compression = Some(Compression::new(Codec::Deflate));
    let exec = Exec::start(&link, &request).unwrap();
    assert_eq!(code(exec.wait(WAIT)), ErrorCode::Unsupported);

    let mut request = Request::argv([helper().as_str(), "cat"]);
    request.stdin = Stdin::Open;
    request.stdin_compression = Some(Compression::auto());
    assert!(matches!(
        Exec::start(&link, &request),
        Err(Error::Compression(_))
    ));
    // The host refuses it too, for a client that does not check.
    let mut raw = request.clone();
    raw.stdin_compression = Some(Compression::auto());
    let channel = link
        .open(protocol::EXEC, &raw.to_metadata().unwrap())
        .unwrap();
    let frame = channel.receive(WAIT).unwrap().unwrap();
    assert!(matches!(
        bohio_proto::exec::Output::decode(&frame).unwrap(),
        bohio_proto::exec::Output::Error(body) if body.code == ErrorCode::BadRequest
    ));
}

#[test]
fn compressed_standard_input_past_the_expansion_bound_ends_the_process() {
    let link = Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    )
    .compressing(bounded_config());

    let mut request = Request::argv([helper().as_str(), "cat"]);
    request.stdin = Stdin::Open;
    request.stdin_compression = Some(codecs().remove(0));
    let exec = Exec::start(&link, &request).unwrap();
    let zeros = vec![0_u8; 1024 * 1024];
    let mut sent = Ok(());
    for _ in 0..64 {
        sent = exec.write_stdin(&zeros);
        if sent.is_err() {
            break;
        }
    }
    let _ = sent;
    assert_eq!(code(exec.wait(WAIT)), ErrorCode::BadRequest);
}

fn info_of(link: &Link) -> bohio_proto::info::Info {
    let config = bohio::config::NodeConfig {
        compression: (*link.compression).clone(),
        ..bohio::config::NodeConfig::default()
    };
    bohio::info::collect(&config, std::ffi::OsStr::new(""))
}

#[test]
fn info_lists_the_algorithms_levels_default_and_prepared() {
    let fixture = fixture(CompressionConfig::default());
    let info = info_of(&fixture.link).transfer;
    let names: Vec<_> = info
        .compression
        .iter()
        .map(|item| item.algorithm.clone())
        .collect();
    assert_eq!(names.contains(&Codec::Zstd), cfg!(feature = "zstd"));
    assert!(names.contains(&Codec::Lz4) && names.contains(&Codec::Deflate));
    let deflate = info
        .compression
        .iter()
        .find(|item| item.algorithm == Codec::Deflate)
        .unwrap();
    assert_eq!(
        (
            deflate.levels.min,
            deflate.levels.max,
            deflate.default_level
        ),
        (0, 9, 6)
    );
    assert!(info.compression_default.is_some());
    assert!(info.prepared);
    // No prepare directory: no prepared reads.
    let info = info_of(&Link::files(
        Grant::allowing(RW),
        FilesystemConfig::default(),
    ))
    .transfer;
    assert!(!info.prepared);
}

#[cfg(not(feature = "zstd"))]
#[test]
fn without_zstd_it_is_not_offered_and_a_request_for_it_is_unsupported() {
    let fixture = fixture(CompressionConfig::default());
    let info = info_of(&fixture.link).transfer;
    assert!(
        info.compression
            .iter()
            .all(|item| item.algorithm != Codec::Zstd)
    );
    std::fs::write(fixture.root.join("log.txt"), text(1000)).unwrap();
    assert_eq!(
        code(Reader::start(
            &fixture.link,
            &ask(Compression::new(Codec::Zstd), "log.txt"),
            WAIT
        )),
        ErrorCode::Unsupported
    );
    assert!(
        CompressionConfig::default()
            .allowed
            .iter()
            .all(|codec| *codec != Codec::Zstd)
    );
}

fn prepared_request(path: &str) -> ReadRequest {
    ReadRequest {
        compression: Some(
            codecs()
                .remove(0)
                .level(if cfg!(feature = "zstd") { 9 } else { 6 }),
        ),
        prepare: true,
        hashes: vec![Algorithm::Sha256],
        ..ReadRequest::whole(path)
    }
}

#[test]
fn a_prepared_read_reports_progress_and_its_lengths_and_matches_the_file() {
    let fixture = fixture_with(
        CompressionConfig::default(),
        FilesystemConfig {
            // Progress at least every quarter of this.
            idle_timeout: Span(Duration::from_millis(40)),
            ..FilesystemConfig::default()
        },
    );
    let data = text(12_000_000);
    std::fs::write(fixture.root.join("big.log"), &data).unwrap();
    let progress = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&progress);
    let reader = Reader::start_with(
        &fixture.link,
        &prepared_request("big.log"),
        WAIT,
        ReadOptions {
            on_progress: Some(Box::new(move |done| seen.lock().unwrap().push(done))),
            ..ReadOptions::default()
        },
    )
    .unwrap();
    let header = reader.header();
    assert!(header.token.is_some());
    assert!(header.compressed_length.unwrap() < data.len() as u64 / 2);
    assert_eq!(header.length, data.len() as u64);
    let bytes = reader.read_to_end(WAIT).unwrap();
    assert_eq!(bytes, data);
    assert_eq!(
        reader.digests().unwrap().get(&Algorithm::Sha256),
        Some(&sha256(&data))
    );
    let progress = progress.lock().unwrap();
    assert!(!progress.is_empty(), "a long preparation sent no progress");
    assert!(progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(progress.iter().all(|done| *done <= data.len() as u64));
}

#[test]
fn a_prepared_read_resumes_from_a_compressed_offset_after_the_client_closes() {
    let fixture = fixture(CompressionConfig::default());
    let data = text(3_000_000);
    std::fs::write(fixture.root.join("big.log"), &data).unwrap();
    let raw = || ReadOptions {
        raw: true,
        ..ReadOptions::default()
    };
    let first =
        Reader::start_with(&fixture.link, &prepared_request("big.log"), WAIT, raw()).unwrap();
    let header = first.header();
    let (token, all) = (
        header.token.clone().unwrap(),
        header.compressed_length.unwrap(),
    );
    // Take a part of the compressed stream, then go away.
    let mut held = Vec::new();
    while (held.len() as u64) < all / 2 {
        held.extend(first.next_chunk(WAIT).unwrap().expect("more to come"));
    }
    drop(first);
    let offset = held.len() as u64;

    let request = ReadRequest {
        resume: Some(Resume {
            token: token.clone(),
            offset,
        }),
        ..ReadRequest::whole("big.log")
    };
    let second = Reader::start_with(&fixture.link, &request, WAIT, ReadOptions::default()).unwrap();
    assert_eq!(second.header().compressed_length, Some(all));
    assert_eq!(second.header().token.as_deref(), Some(token.as_str()));
    let rest = second.read_to_end(WAIT).unwrap();
    assert_eq!(held.len() + rest.len(), all as usize);
    held.extend(rest);
    assert_eq!(
        second.digests().unwrap().get(&Algorithm::Sha256),
        Some(&sha256(&data))
    );
    // The two halves are one stream.
    let mut decoder = Decoder::new(&header.compression.unwrap(), &Bounds::default()).unwrap();
    let mut out = Vec::new();
    decoder.feed(&held, &mut out).unwrap();
    assert_eq!(out, data);
}
fn resume_of(token: &str, path: &str) -> ReadRequest {
    ReadRequest {
        resume: Some(Resume {
            token: token.to_owned(),
            offset: 0,
        }),
        ..ReadRequest::whole(path)
    }
}

#[test]
fn a_prepared_file_belongs_to_one_peer_and_one_path() {
    let fixture = fixture(CompressionConfig::default());
    std::fs::write(fixture.root.join("a.log"), text(200_000)).unwrap();
    std::fs::write(fixture.root.join("b.log"), text(200_000)).unwrap();
    let reader = Reader::start(&fixture.link, &prepared_request("a.log"), WAIT).unwrap();
    let token = reader.header().token.unwrap();
    reader.read_to_end(WAIT).unwrap();
    assert!(fixture.prepared.join(format!("{token}.bin")).exists());
    assert!(fixture.prepared.join(format!("{token}.meta")).exists());

    // A token that is not one, and one that names nothing, are the same answer.
    for bad in ["../../etc/passwd", "zz", &"0".repeat(32)] {
        assert_eq!(
            code(Reader::start(&fixture.link, &resume_of(bad, "a.log"), WAIT)),
            ErrorCode::NotFound,
            "{bad}"
        );
    }
    // Another path than the one it was made for.
    assert_eq!(
        code(Reader::start(
            &fixture.link,
            &resume_of(&token, "b.log"),
            WAIT
        )),
        ErrorCode::BadRequest
    );
    // Another peer cannot use it, and is told what it is told of a token that names nothing.
    let mut other = Link::files(
        Grant::allowing(RW).with_fs_roots([fixture.root.clone()]),
        FilesystemConfig::default(),
    )
    .compressing((*fixture.link.compression).clone());
    other.peer = "another-peer".to_owned();
    assert_eq!(
        code(Reader::start(&other, &resume_of(&token, "a.log"), WAIT)),
        ErrorCode::NotFound
    );
    // The control: its own peer, its own path.
    Reader::start(&fixture.link, &resume_of(&token, "a.log"), WAIT)
        .unwrap()
        .read_to_end(WAIT)
        .unwrap();
}

#[test]
fn a_prepared_file_is_removed_once_its_time_has_passed() {
    let fixture = fixture(CompressionConfig {
        prepare_ttl: Span(Duration::from_millis(300)),
        ..CompressionConfig::default()
    });
    std::fs::write(fixture.root.join("a.log"), text(200_000)).unwrap();
    std::fs::write(fixture.root.join("b.log"), text(200_000)).unwrap();
    let reader = Reader::start(&fixture.link, &prepared_request("a.log"), WAIT).unwrap();
    let token = reader.header().token.unwrap();
    reader.read_to_end(WAIT).unwrap();
    std::thread::sleep(Duration::from_millis(700));
    // The next prepared read removes what has expired.
    Reader::start(&fixture.link, &prepared_request("b.log"), WAIT)
        .unwrap()
        .read_to_end(WAIT)
        .unwrap();
    assert!(!fixture.prepared.join(format!("{token}.bin")).exists());
    assert!(!fixture.prepared.join(format!("{token}.meta")).exists());
    assert_eq!(
        code(Reader::start(
            &fixture.link,
            &resume_of(&token, "a.log"),
            WAIT
        )),
        ErrorCode::NotFound
    );
}

#[test]
fn the_nodes_start_removes_what_expired_before_it() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("old.bin"), b"x").unwrap();
    std::fs::write(tmp.path().join("old.meta"), b"x").unwrap();
    std::fs::write(tmp.path().join("keep.txt"), b"x").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    bohio::fs::sweep_prepared(tmp.path(), Duration::from_millis(100));
    assert!(!tmp.path().join("old.bin").exists() && !tmp.path().join("old.meta").exists());
    assert!(
        tmp.path().join("keep.txt").exists(),
        "only the files a prepared read makes are removed"
    );
}

#[test]
fn a_prepared_result_past_the_limit_is_busy_and_leaves_nothing() {
    let fixture = fixture(CompressionConfig {
        prepare_max_bytes: 1000,
        ..CompressionConfig::default()
    });
    std::fs::write(fixture.root.join("n.bin"), noise(500_000)).unwrap();
    assert_eq!(
        code(Reader::start(
            &fixture.link,
            &prepared_request("n.bin"),
            WAIT
        )),
        ErrorCode::Busy
    );
    let left = std::fs::read_dir(&fixture.prepared).map_or(0, Iterator::count);
    assert_eq!(left, 0, "a refused preparation left files behind");
}

#[test]
fn a_host_without_a_prepare_directory_does_not_prepare() {
    let mut fixture = fixture(CompressionConfig::default());
    fixture.link = Link::files(
        Grant::allowing(RW).with_fs_roots([fixture.root.clone()]),
        FilesystemConfig::default(),
    );
    std::fs::write(fixture.root.join("a.log"), text(1000)).unwrap();
    assert_eq!(
        code(Reader::start(
            &fixture.link,
            &prepared_request("a.log"),
            WAIT
        )),
        ErrorCode::Unsupported
    );
    // And a prepare with no compression is a malformed request.
    let fixture = self::fixture(CompressionConfig::default());
    std::fs::write(fixture.root.join("a.log"), text(1000)).unwrap();
    let request = ReadRequest {
        prepare: true,
        ..ReadRequest::whole("a.log")
    };
    assert_eq!(
        code(Reader::start(&fixture.link, &request, WAIT)),
        ErrorCode::BadRequest
    );
}

#[test]
fn every_hash_the_host_offers_matches_for_a_compressed_read() {
    let fixture = fixture(CompressionConfig::default());
    let data = text(400_000);
    std::fs::write(fixture.root.join("a.log"), &data).unwrap();
    let all = available();
    let request = ReadRequest {
        compression: Some(codecs().remove(0)),
        hashes: all.clone(),
        ..ReadRequest::whole("a.log")
    };
    let reader = Reader::start(&fixture.link, &request, WAIT).unwrap();
    reader.read_to_end(WAIT).unwrap();
    let mut hasher = Hasher::new(&all).unwrap();
    hasher.update(&data);
    assert_eq!(reader.digests().unwrap(), hasher.finish());
}

/// The default bound (1024), which zstd's ratio on zeros passes. Without zstd the best ratio is LZ4's, about 255,
/// so the same test runs with a bound of 100.
fn bounded_config() -> CompressionConfig {
    if cfg!(feature = "zstd") {
        CompressionConfig::default()
    } else {
        CompressionConfig {
            max_expansion: 100,
            ..CompressionConfig::default()
        }
    }
}
