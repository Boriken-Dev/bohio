//! Every message type round-trips; decoding is liberal and never panics.

use std::collections::BTreeMap;

use bohio_proto::compress::{Codec, Compression, Progress, Resume};
use bohio_proto::control::{Call, Reply, Request as ControlRequest, Response};
use bohio_proto::exec::{
    Command, Env, Exit, Input, Output, Request as ExecRequest, Started, Stdin,
};
use bohio_proto::hash::{
    Algorithm, BlockOptions, Digests, HashBlocks, HashDone, HashInput, HashItem, HashOutput,
    HashRequest, HashResult, ItemError,
};
use bohio_proto::info::{
    ExecInfo, FilesystemInfo, HashLimits, Info, Limits, PROTOCOL_VERSION, Service, SessionInfo,
    ShellInfo, TransferInfo,
};
use bohio_proto::shell::ShellKind;
use bohio_proto::{Error, ErrorBody, ErrorCode, decode, decode_with_depth, encode};
use proptest::prelude::*;
use serde::{Serialize, de::DeserializeOwned};

fn round_trips<T: Serialize + DeserializeOwned + PartialEq + core::fmt::Debug>(value: &T) {
    let bytes = encode(value).expect("encodes");
    assert_eq!(&decode::<T>(&bytes).expect("decodes"), value);
}

fn sample_info() -> Info {
    Info {
        protocol: PROTOCOL_VERSION,
        service: Service {
            name: "build-host".into(),
            version: "0.0.0".into(),
        },
        host_id: Some("peer-1".into()),
        os: "linux".into(),
        arch: "aarch64".into(),
        shells: vec![ShellInfo {
            kind: ShellKind::Bash,
            path: "/bin/bash".into(),
        }],
        filesystem: FilesystemInfo {
            enabled: true,
            symlinks: true,
            unix_modes: true,
            windows_attributes: false,
            case_sensitive: true,
            separator: "/".into(),
        },
        exec: ExecInfo {
            enabled: true,
            signals: vec![bohio_proto::signal::Signal::Terminate],
        },
        shell: SessionInfo {
            enabled: false,
            signals: Vec::new(),
        },
        sessions: bohio_proto::info::DetachedInfo::default(),
        limits: Limits {
            max_streams: Some(64),
            max_processes: Some(16),
            max_sessions: None,
        },
        transfer: sample_transfer(),
        forward: bohio_proto::forward::ForwardInfo {
            enabled: true,
            connect: vec!["tcp".into()],
            listen: Vec::new(),
        },
        gui: bohio_proto::gui::GuiInfo::default(),
        access: bohio_proto::info::AccessInfo::default(),
    }
}

fn sample_transfer() -> TransferInfo {
    TransferInfo {
        hashes: vec![Algorithm::Sha256, Algorithm::Blake3],
        hash_limits: HashLimits {
            max_streams: 4,
            rate: 0,
        },
        compression: Vec::new(),
        compression_default: None,
        prepared: false,
        delta: bohio_proto::info::DeltaInfo::default(),
        walk: true,
        walk_limits: bohio_proto::info::WalkLimits {
            max_streams: 4,
            max_entries: 1_000_000,
        },
        search: true,
        search_limits: bohio_proto::info::SearchLimits {
            max_searches: 4,
            max_file_bytes: 16_777_216,
            max_line_bytes: 4096,
            max_matches: 10_000,
            max_files: 100_000,
            max_context: 50,
            deadline_ms: 60_000,
        },
        follow: true,
        bundles: true,
        resume: false,
    }
}

#[test]
fn every_control_message_round_trips() {
    round_trips(&ControlRequest {
        source: None,
        id: 1,
        call: Call::Hello {
            version: 1,
            client: "test".into(),
        },
    });
    round_trips(&ControlRequest {
        source: None,
        id: u64::MAX,
        call: Call::Info,
    });
    round_trips(&Response::ok(
        2,
        Reply::Hello {
            version: 1,
            transfer: sample_transfer(),
        },
    ));
    round_trips(&Response::ok(3, Reply::Info(Box::new(sample_info()))));
    round_trips(&Response::failed(
        4,
        ErrorBody::new(ErrorCode::Denied, "not allowed"),
    ));
}

#[test]
fn every_exec_message_round_trips() {
    let mut full = ExecRequest::argv(["prog", "arg with space", ""]);
    full.cwd = Some("/tmp".into());
    full.env = Env {
        clear: true,
        set: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
        remove: vec!["B".into()],
    };
    full.stdin = Stdin::Open;
    full.timeout_ms = Some(1500);
    round_trips(&full);
    round_trips(&ExecRequest::shell(ShellKind::Pwsh, "Get-Date"));
    assert_eq!(
        ExecRequest::from_metadata(&full.to_metadata().unwrap()).unwrap(),
        full
    );

    let exit = Exit {
        code: Some(3),
        signal: None,
        timed_out: false,
        killed: false,
    };
    for output in [
        Output::Started(Started::new(42)),
        Output::Started(Started {
            pid: 42,
            output_compression: Some(Compression::new(Codec::Zstd).level(3)),
            stdin_compression: Some(Compression::new(Codec::Lz4)),
            session: Some("00112233445566778899aabbccddeeff".into()),
        }),
        Output::Signalled(bohio_proto::signal::Signalled {
            signal: bohio_proto::signal::Signal::Interrupt,
            delivered: false,
            message: Some("no console".into()),
        }),
        Output::StdoutCompressed(vec![1, 2, 3]),
        Output::StderrCompressed(vec![4]),
        Output::Stdout(vec![0, 255, 10]),
        Output::Stderr(Vec::new()),
        Output::Exit(exit),
        Output::Error(ErrorBody::new(ErrorCode::NotFound, "no such program")),
    ] {
        assert_eq!(Output::decode(&output.encode().unwrap()).unwrap(), output);
    }
    for input in [
        Input::Stdin(vec![1, 2, 3]),
        Input::StdinCompressed(vec![4, 5]),
        Input::StdinEnd,
        Input::Kill,
    ] {
        assert_eq!(Input::decode(&input.encode().unwrap()).unwrap(), input);
    }
}

#[test]
fn every_shell_kind_round_trips_and_names_a_program() {
    for kind in ShellKind::KNOWN {
        round_trips(&kind);
        assert!(!kind.program().is_empty());
    }
}

#[test]
fn every_error_code_round_trips() {
    for code in [
        ErrorCode::BadRequest,
        ErrorCode::Denied,
        ErrorCode::NotFound,
        ErrorCode::Exists,
        ErrorCode::Unsupported,
        ErrorCode::Busy,
        ErrorCode::OutsideRoot,
        ErrorCode::Io,
        ErrorCode::Refused,
        ErrorCode::Unreachable,
        ErrorCode::Timeout,
        ErrorCode::Unavailable,
        ErrorCode::InUse,
        ErrorCode::Changed,
        ErrorCode::Internal,
        ErrorCode::Other,
    ] {
        round_trips(&ErrorBody::new(code, "x"));
    }
}

#[test]
fn an_error_code_a_newer_peer_adds_decodes_as_other() {
    #[derive(Serialize)]
    struct Newer {
        code: &'static str,
        message: &'static str,
    }
    let bytes = encode(&Newer {
        code: "quota_exceeded",
        message: "full",
    })
    .unwrap();
    let body: ErrorBody = decode(&bytes).unwrap();
    assert_eq!(body.code, ErrorCode::Other);
    assert_eq!(body.message, "full");
}

#[test]
fn a_client_ignores_info_fields_it_does_not_know_and_defaults_the_missing() {
    #[derive(Serialize)]
    struct Newer {
        os: &'static str,
        gpu_count: u32,
    }
    let bytes = encode(&Newer {
        os: "linux",
        gpu_count: 2,
    })
    .unwrap();
    let info: Info = decode(&bytes).unwrap();
    assert_eq!(info.os, "linux");
    assert_eq!(info.shells, Vec::new());
    assert!(!info.exec.enabled);
}

#[test]
fn a_request_without_optional_keys_takes_their_defaults() {
    #[derive(Serialize)]
    struct Minimal {
        command: Command,
    }
    let bytes = encode(&Minimal {
        command: Command::Argv {
            argv: vec!["x".into()],
        },
    })
    .unwrap();
    let request = ExecRequest::from_metadata(&bytes).unwrap();
    assert_eq!(request, ExecRequest::argv(["x"]));
    assert_eq!(request.stdin, Stdin::Closed);
}

#[test]
fn a_response_with_neither_reply_nor_error_is_an_internal_error() {
    let empty = Response {
        id: 9,
        reply: None,
        error: None,
    };
    assert_eq!(empty.into_result().unwrap_err().code, ErrorCode::Internal);
}

#[test]
fn an_empty_frame_and_an_unknown_tag_are_errors_not_panics() {
    assert_eq!(Output::decode(&[]), Err(Error::Short));
    assert_eq!(Input::decode(&[]), Err(Error::Short));
    assert_eq!(Output::decode(&[0x7f]), Err(Error::Tag(0x7f)));
    assert_eq!(Input::decode(&[0x01]), Err(Error::Tag(0x01)));
    assert_eq!(Output::decode(&[0x01, 0xff]), Err(Error::Decode));
}

#[test]
fn trailing_bytes_after_a_message_are_refused() {
    let mut bytes = encode(&ErrorBody::new(ErrorCode::Io, "x")).unwrap();
    bytes.push(0);
    assert_eq!(decode::<ErrorBody>(&bytes), Err(Error::Decode));
}

#[test]
fn nesting_past_the_depth_is_refused_not_recursed_through() {
    // An array of arrays, a thousand deep: 0x81 is a one-element array.
    let mut bytes = vec![0x81; 1000];
    bytes.push(0x00);
    assert_eq!(
        decode::<serde::de::IgnoredAny>(&bytes).map(|_| ()),
        Err(Error::Decode)
    );
    let mut shallow = vec![0x81; 4];
    shallow.push(0x00);
    assert!(decode_with_depth::<serde::de::IgnoredAny>(&shallow, 8).is_ok());
    assert_eq!(
        decode_with_depth::<serde::de::IgnoredAny>(&shallow, 2).map(|_| ()),
        Err(Error::Decode)
    );
}

#[test]
fn a_declared_length_the_bytes_cannot_fill_is_an_error_not_an_allocation() {
    // A text string declaring 2^60 bytes, with none after it.
    let bytes = [0x7b, 0x10, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(decode::<String>(&bytes), Err(Error::Decode));
    let bytes = [0x5b, 0x10, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        decode::<serde_bytes_like::Bytes>(&bytes).map(|_| ()),
        Err(Error::Decode)
    );
}

mod serde_bytes_like {
    use serde::Deserialize;

    /// A type that asks for a byte string.
    #[derive(Debug)]
    pub struct Bytes;

    impl<'de> Deserialize<'de> for Bytes {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_bytes(serde::de::IgnoredAny).map(|_| Self)
        }
    }
}

/// Every decoder of the wire other than control, info and exec: none may panic on any bytes.
fn decode_every_other_protocol(bytes: &[u8]) {
    use bohio_proto::{forward, fs, gui, local, media, power, search, session, shell, walk};
    let _ = shell::Request::from_metadata(bytes);
    let _ = shell::Output::decode(bytes);
    let _ = shell::Input::decode(bytes);
    let _ = fs::ReadRequest::from_metadata(bytes);
    let _ = fs::ReadOutput::decode(bytes);
    let _ = fs::ReadInput::decode(bytes);
    let _ = fs::WriteRequest::from_metadata(bytes);
    let _ = fs::WriteOutput::decode(bytes);
    let _ = fs::WriteInput::decode(bytes);
    let _ = HashRequest::from_metadata(bytes);
    let _ = HashOutput::decode(bytes);
    let _ = HashInput::decode(bytes);
    let _ = walk::WalkRequest::from_metadata(bytes);
    let _ = walk::WalkOutput::decode(bytes);
    let _ = walk::WalkInput::decode(bytes);
    let _ = search::SearchRequest::from_metadata(bytes);
    let _ = search::SearchOutput::decode(bytes);
    let _ = session::Attach::from_metadata(bytes);
    let _ = session::Attached::decode(bytes);
    let _ = forward::ConnectRequest::from_metadata(bytes);
    let _ = forward::Answer::decode(bytes);
    let _ = forward::ListenRequest::from_metadata(bytes);
    let _ = forward::ListenFrame::decode(bytes);
    let _ = forward::AcceptRequest::from_metadata(bytes);
    let _ = gui::GuiRequest::from_metadata(bytes);
    let _ = gui::GuiOutput::decode(bytes);
    let _ = gui::GuiInput::decode(bytes);
    let _ = gui::ApproveOutput::decode(bytes);
    let _ = gui::ApproveInput::decode(bytes);
    let _ = gui::HelperHello::from_metadata(bytes);
    let _ = gui::HelperOpen::from_metadata(bytes);
    let _ = gui::HelperMessage::decode(bytes);
    let _ = power::Request::from_metadata(bytes);
    let _ = power::Answer::decode(bytes);
    let _ = media::Request::from_metadata(bytes);
    let _ = media::Answer::decode(bytes);
    let _ = local::ViaRequest::from_metadata(bytes);
    let _ = local::LinksRequest::from_metadata(bytes);
    let _ = local::LinksReply::decode(bytes);
    let _ = local::PairRequest::from_metadata(bytes);
    let _ = local::PairReply::decode(bytes);
    let _ = local::ProvidersReply::decode(bytes);
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic_any_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode::<ControlRequest>(&bytes);
        let _ = decode::<Response>(&bytes);
        let _ = decode::<Info>(&bytes);
        let _ = ExecRequest::from_metadata(&bytes);
        let _ = Output::decode(&bytes);
        let _ = Input::decode(&bytes);
        decode_every_other_protocol(&bytes);
    }

    #[test]
    fn exec_frames_round_trip_for_any_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let out = Output::Stdout(bytes.clone());
        prop_assert_eq!(Output::decode(&out.encode().unwrap()).unwrap(), out);
        let input = Input::Stdin(bytes);
        prop_assert_eq!(Input::decode(&input.encode().unwrap()).unwrap(), input);
    }
}

#[test]
fn a_hello_without_a_version_asks_for_the_current_one() {
    let mut bytes = Vec::new();
    let hello = ciborium::Value::Map(vec![
        (
            ciborium::Value::Text("id".into()),
            ciborium::Value::Integer(1.into()),
        ),
        (
            ciborium::Value::Text("call".into()),
            ciborium::Value::Map(vec![(
                ciborium::Value::Text("type".into()),
                ciborium::Value::Text("hello".into()),
            )]),
        ),
    ]);
    ciborium::into_writer(&hello, &mut bytes).unwrap();
    let request: ControlRequest = decode(&bytes).unwrap();
    assert_eq!(
        request.call,
        Call::Hello {
            version: PROTOCOL_VERSION,
            client: String::new(),
        }
    );
}

fn sample_metadata() -> bohio_proto::fs::Metadata {
    use bohio_proto::fs::{FileKind, Metadata, Timestamp};
    Metadata {
        kind: FileKind::File,
        size: 42,
        modified: Some(Timestamp {
            secs: -3,
            nanos: 999_999_999,
        }),
        accessed: Some(Timestamp {
            secs: 1_700_000_000,
            nanos: 5,
        }),
        created: None,
        mode: Some(0o640),
        readonly: false,
        attributes: Some(32),
        device: Some(2049),
        inode: Some(131_077),
        links: Some(1),
    }
}

#[test]
fn every_filesystem_call_and_reply_round_trips() {
    use bohio_proto::fs::{Entry, Listing, Timestamp};
    let stamp = Some(Timestamp { secs: 7, nanos: 8 });
    let calls = [
        Call::Stat {
            path: "a".into(),
            follow: true,
        },
        Call::List {
            path: "d".into(),
            after: Some("m".into()),
            limit: Some(10),
        },
        Call::Mkdir {
            path: "d".into(),
            parents: true,
            mode: Some(0o755),
        },
        Call::Remove {
            path: "d".into(),
            recursive: true,
        },
        Call::Rename {
            from: "a".into(),
            to: "b".into(),
            overwrite: true,
        },
        Call::Copy {
            from: "a".into(),
            to: "b".into(),
            overwrite: false,
            recursive: true,
            preserve: true,
        },
        Call::Move {
            from: "a".into(),
            to: "b".into(),
            overwrite: true,
        },
        Call::Truncate {
            path: "a".into(),
            length: 1 << 40,
        },
        Call::Symlink {
            target: "../t".into(),
            link: "l".into(),
        },
        Call::Readlink { path: "l".into() },
        Call::SetTimes {
            path: "a".into(),
            accessed: stamp,
            modified: None,
        },
        Call::SetMode {
            path: "a".into(),
            mode: 0o600,
        },
        Call::SetAttributes {
            path: "a".into(),
            readonly: Some(true),
        },
    ];
    for (id, call) in (1..).zip(calls) {
        round_trips(&ControlRequest {
            id,
            call,
            source: None,
        });
    }
    let replies = [
        Reply::Stat(Box::new(sample_metadata())),
        Reply::List(Listing {
            entries: vec![Entry {
                name: "x".into(),
                metadata: sample_metadata(),
            }],
            next: Some("x".into()),
            skipped: 2,
        }),
        Reply::Done,
        Reply::Link {
            target: "../t".into(),
        },
    ];
    for (id, reply) in (1..).zip(replies) {
        round_trips(&Response::ok(id, reply));
    }
}

#[test]
fn every_transfer_message_round_trips() {
    use bohio_proto::fs::{
        DeltaBase, DeltaCopy, ReadDelta, ReadDone, ReadHeader, ReadInput, ReadOutput, ReadRequest,
        SignatureBatch, WriteDone, WriteInput, WriteMode, WriteOutput, WriteRequest,
    };
    let read = ReadRequest {
        source: None,
        path: "f".into(),
        offset: 4,
        length: Some(8),
        hashes: vec![Algorithm::Md5, Algorithm::Xxh3],
        compression: Some(Compression::new(Codec::Deflate).level(9)),
        prepare: true,
        resume: Some(Resume {
            token: "ab".into(),
            offset: 7,
        }),
        delta: Some(ReadDelta {
            block_size: 2048,
            weak: "rsync".into(),
            strong: Algorithm::Blake3,
            strong_len: 16,
            base_size: 99,
        }),
        items: true,
        follow: Some(bohio_proto::fs::ReadFollow {
            by: Some(bohio_proto::fs::FollowBy::Name),
            interval_ms: Some(100),
            tail: Some(4096),
        }),
    };
    assert_eq!(
        ReadRequest::from_metadata(&read.to_metadata().unwrap()).unwrap(),
        read
    );
    for mode in [
        WriteMode::Replace,
        WriteMode::Create,
        WriteMode::Append,
        WriteMode::At { offset: 9 },
        WriteMode::Delta {
            block_size: 4096,
            base: DeltaBase {
                size: 5,
                modified: Some(bohio_proto::fs::Timestamp { secs: 3, nanos: 4 }),
                hashes: Digests::from([(Algorithm::Crc32c, "e3069283".into())]),
            },
        },
    ] {
        let write = WriteRequest {
            source: None,
            path: "f".into(),
            mode,
            parents: true,
            file_mode: Some(0o600),
            expect: Digests::from([(Algorithm::Sha256, "cd".repeat(32))]),
            hashes: Some(vec![Algorithm::Sha256, Algorithm::Crc32c]),
            compression: Some(Compression::new(Codec::Lz4)),
            items: true,
            resume: true,
            base: Some(DeltaBase {
                size: 7,
                modified: None,
                hashes: Digests::new(),
            }),
        };
        assert_eq!(
            WriteRequest::from_metadata(&write.to_metadata().unwrap()).unwrap(),
            write
        );
    }
    let error = ErrorBody::new(ErrorCode::OutsideRoot, "no");
    for output in [
        ReadOutput::Header(ReadHeader::raw(5, 3)),
        ReadOutput::Header(ReadHeader {
            size: 5,
            length: 3,
            compression: Some(Compression::new(Codec::Zstd).level(3).window_log(21)),
            compressed_length: Some(4),
            token: Some("ab".into()),
            delta: true,
            follow: true,
            index: Some(3),
            modified: Some(bohio_proto::fs::Timestamp { secs: 3, nanos: 4 }),
            mode: Some(0o644),
        }),
        ReadOutput::Copy(DeltaCopy { block: 7, count: 2 }),
        ReadOutput::Truncated(bohio_proto::fs::Truncated {
            size: 9,
            replaced: true,
        }),
        ReadOutput::Data(vec![0, 1, 255]),
        ReadOutput::Compressed(vec![9, 8]),
        ReadOutput::Progress(Progress { prepared: 3 }),
        ReadOutput::Done(ReadDone {
            hashes: Digests::from([(Algorithm::Crc32c, "e3069283".into())]),
            index: Some(2),
        }),
        ReadOutput::ItemError(ItemError::new(1, error.clone())),
        ReadOutput::End(bohio_proto::fs::BundleEnd { items: 9 }),
        ReadOutput::Error(error.clone()),
    ] {
        assert_eq!(
            ReadOutput::decode(&output.encode().unwrap()).unwrap(),
            output
        );
    }
    for output in [
        WriteOutput::ready(),
        WriteOutput::Ready(bohio_proto::fs::Ready {
            token: Some("ab".into()),
            offset: None,
        }),
        WriteOutput::Ready(bohio_proto::fs::Ready {
            token: None,
            offset: Some(77),
        }),
        WriteOutput::Flushed(bohio_proto::fs::Flushed { written: 5 }),
        WriteOutput::ItemError(ItemError::new(4, error.clone())),
        WriteOutput::End(bohio_proto::fs::BundleEnd { items: 9 }),
        WriteOutput::Done(WriteDone {
            written: 3,
            hashes: Digests::from([(Algorithm::Sha256, "ef".repeat(32))]),
            index: Some(5),
        }),
        WriteOutput::Error(error),
    ] {
        assert_eq!(
            WriteOutput::decode(&output.encode().unwrap()).unwrap(),
            output
        );
    }
    for input in [
        WriteInput::Data(vec![9, 8]),
        WriteInput::Compressed(vec![7]),
        WriteInput::Copy(DeltaCopy {
            block: u64::MAX,
            count: 1,
        }),
        WriteInput::End,
        WriteInput::Flush,
        WriteInput::ItemsEnd,
        WriteInput::Item(bohio_proto::fs::WriteItem {
            path: "a/b".into(),
            mode: WriteMode::Create,
            parents: true,
            file_mode: Some(0o600),
            modified: Some(bohio_proto::fs::Timestamp { secs: 5, nanos: 6 }),
            expect: Digests::from([(Algorithm::Crc32c, "e3069283".into())]),
        }),
    ] {
        assert_eq!(WriteInput::decode(&input.encode()).unwrap(), input);
    }
    for input in [
        ReadInput::Signature(SignatureBatch {
            first: 4,
            weak: vec![1, u32::MAX],
            strong: vec![1, 2, 3, 4],
        }),
        ReadInput::SignatureEnd,
    ] {
        assert_eq!(ReadInput::decode(&input.encode().unwrap()).unwrap(), input);
    }
    assert_eq!(ReadInput::decode(&[0x7f]), Err(Error::Tag(0x7f)));
    assert_eq!(ReadOutput::decode(&[]), Err(Error::Short));
    assert_eq!(ReadOutput::decode(&[0x7f]), Err(Error::Tag(0x7f)));
    assert_eq!(WriteInput::decode(&[0x7f]), Err(Error::Tag(0x7f)));
}

#[test]
fn a_file_kind_a_newer_host_adds_decodes_as_other_and_a_write_mode_defaults_to_replace() {
    use bohio_proto::fs::{FileKind, Metadata, WriteMode, WriteRequest};
    #[derive(Serialize)]
    struct Newer {
        kind: &'static str,
    }
    let bytes = encode(&Newer { kind: "whiteout" }).unwrap();
    let meta: Metadata = decode(&bytes).unwrap();
    assert_eq!(
        (meta.kind, meta.size, meta.mode),
        (FileKind::Other, 0, None)
    );
    #[derive(Serialize)]
    struct Bare {
        path: &'static str,
    }
    let bare = WriteRequest::from_metadata(&encode(&Bare { path: "p" }).unwrap()).unwrap();
    assert_eq!(bare.mode, WriteMode::Replace);
    assert!(!bare.parents && bare.file_mode.is_none());
    assert!(bare.expect.is_empty() && bare.hashes.is_none());
}

#[test]
fn every_shell_message_round_trips() {
    use bohio_proto::shell::{
        DEFAULT_COLS, DEFAULT_ROWS, Exit, Input, Output, Program, Request, Size, Started,
    };
    let mut request = Request::argv(["pwsh", "-NoLogo"]);
    request.cwd = Some("/srv".into());
    request.env.set.insert("A".into(), "b".into());
    request.cols = 132;
    request.rows = 43;
    for request in [request, Request::kind(ShellKind::Bash)] {
        assert_eq!(
            Request::from_metadata(&request.to_metadata().unwrap()).unwrap(),
            request
        );
    }
    let outputs = [
        Output::Started(Started::new(4242)),
        Output::Signalled(bohio_proto::signal::Signalled {
            signal: bohio_proto::signal::Signal::Terminate,
            delivered: true,
            message: None,
        }),
        Output::Data(vec![0x1b, b'[', b'6', b'n', 0, 255]),
        Output::Exit(Exit {
            code: Some(-1),
            signal: Some(9),
            timed_out: true,
            killed: true,
        }),
        Output::Error(ErrorBody::new(ErrorCode::Busy, "full")),
    ];
    for output in outputs {
        assert_eq!(Output::decode(&output.encode().unwrap()).unwrap(), output);
    }
    let inputs = [
        Input::Data(b"ls\r".to_vec()),
        Input::Resize(Size {
            cols: 1,
            rows: 32_767,
        }),
        Input::Kill,
        Input::Signal(bohio_proto::signal::Signal::Hangup),
    ];
    for input in inputs {
        assert_eq!(Input::decode(&input.encode().unwrap()).unwrap(), input);
    }
    assert_eq!(Output::decode(&[]), Err(Error::Short));
    assert_eq!(Output::decode(&[0x7f]), Err(Error::Tag(0x7f)));
    assert_eq!(Input::decode(&[0x7f]), Err(Error::Tag(0x7f)));

    // A request that names no size gets the default one, and a program kind is tagged by type.
    #[derive(Serialize)]
    struct Bare {
        program: Program,
    }
    let bytes = encode(&Bare {
        program: Program::Kind {
            kind: ShellKind::Sh,
        },
    })
    .unwrap();
    let bare = Request::from_metadata(&bytes).unwrap();
    assert_eq!((bare.cols, bare.rows), (DEFAULT_COLS, DEFAULT_ROWS));
    assert!(bare.cwd.is_none() && !bare.env.clear);
}

#[test]
fn every_hash_message_round_trips() {
    let digests = Digests::from([
        (Algorithm::Md5, "d41d8cd98f00b204e9800998ecf8427e".into()),
        (Algorithm::Xxh128, "99aa06d3014798d86001c324468d497f".into()),
    ]);
    let mut request = HashRequest::new([Algorithm::Md5, Algorithm::Blake3]);
    request.blocks = Some(BlockOptions {
        size: Some(2048),
        weak: Some("rsync".into()),
        strong_len: Some(16),
    });
    for request in [request, HashRequest::new([])] {
        assert_eq!(
            HashRequest::from_metadata(&request.to_metadata().unwrap()).unwrap(),
            request
        );
    }
    let error = ErrorBody::new(ErrorCode::Busy, "full");
    let outputs = [
        HashOutput::Result(HashResult {
            index: 0,
            size: 7,
            modified: Some(bohio_proto::fs::Timestamp { secs: 5, nanos: 6 }),
            length: 7,
            unchanged: false,
            hashes: digests,
            block_size: Some(2048),
            strong_len: Some(16),
        }),
        HashOutput::Result(HashResult {
            index: 1,
            size: 9,
            modified: None,
            length: 0,
            unchanged: true,
            hashes: Digests::new(),
            block_size: None,
            strong_len: None,
        }),
        HashOutput::Blocks(HashBlocks {
            index: 1,
            first: 3,
            size: 2048,
            weak: vec![1, u32::MAX],
            strong: vec![0, 1, 255, 7],
        }),
        HashOutput::ItemError(ItemError::new(2, ErrorBody::new(ErrorCode::NotFound, "no"))),
        HashOutput::Done(HashDone { items: 3 }),
        HashOutput::Error(error),
    ];
    for output in outputs {
        assert_eq!(
            HashOutput::decode(&output.encode().unwrap()).unwrap(),
            output
        );
    }
    let inputs = [
        HashInput::Item(HashItem {
            path: "a".into(),
            offset: 1,
            length: Some(2),
            if_size: Some(3),
            if_modified: Some(bohio_proto::fs::Timestamp { secs: 1, nanos: 0 }),
        }),
        HashInput::Item(HashItem::whole("b")),
        HashInput::End,
    ];
    for input in inputs {
        assert_eq!(HashInput::decode(&input.encode().unwrap()).unwrap(), input);
    }
    assert_eq!(HashOutput::decode(&[]), Err(Error::Short));
    assert_eq!(HashOutput::decode(&[0x7f]), Err(Error::Tag(0x7f)));
    assert_eq!(HashInput::decode(&[0x01]), Err(Error::Tag(0x01)));
}

#[test]
fn an_algorithm_is_its_name_and_a_name_this_version_lacks_survives() {
    for algorithm in Algorithm::KNOWN {
        assert_eq!(Algorithm::from_name(algorithm.name()), algorithm);
        assert!(algorithm.is_known() && algorithm.hex_len().is_some());
        let bytes = encode(&algorithm).unwrap();
        assert_eq!(decode::<Algorithm>(&bytes).unwrap(), algorithm);
    }
    let newer = Algorithm::from_name("sha3-256");
    assert_eq!(newer, Algorithm::Other("sha3-256".into()));
    assert!(!newer.is_known() && newer.hex_len().is_none());
    assert_eq!(
        decode::<Algorithm>(&encode(&newer).unwrap()).unwrap(),
        newer
    );
    // A map keyed by algorithm is a map keyed by text, in the order its keys sort.
    let digests = Digests::from([(newer, "00".into()), (Algorithm::Md5, "11".into())]);
    let bytes = encode(&digests).unwrap();
    let by_text: BTreeMap<String, String> = decode(&bytes).unwrap();
    assert_eq!(
        by_text.keys().cloned().collect::<Vec<_>>(),
        ["md5", "sha3-256"]
    );
    assert_eq!(decode::<Digests>(&bytes).unwrap(), digests);
}

#[test]
fn a_hash_stream_decodes_whatever_bytes_it_is_given_without_panicking() {
    proptest!(|(bytes in proptest::collection::vec(any::<u8>(), 0..64))| {
        let _ = HashOutput::decode(&bytes);
        let _ = HashInput::decode(&bytes);
        let _ = HashRequest::from_metadata(&bytes);
    });
}

#[test]
fn a_byte_string_longer_than_the_decoders_scratch_buffer_round_trips() {
    use bohio_proto::fs::SignatureBatch;
    use bohio_proto::hash::{HashBlocks, HashOutput};
    // ciborium reads a byte string only through its 4 KiB buffer unless it is asked for an owned one.
    let strong: Vec<u8> = (0..200_000_u32).map(|n| n.to_le_bytes()[0]).collect();
    let blocks = HashOutput::Blocks(HashBlocks {
        index: 0,
        first: 0,
        size: 2048,
        weak: vec![7; 12_500],
        strong: strong.clone(),
    });
    assert_eq!(
        HashOutput::decode(&blocks.encode().unwrap()).unwrap(),
        blocks
    );
    let batch = bohio_proto::fs::ReadInput::Signature(SignatureBatch {
        first: 9,
        weak: vec![1, 2],
        strong,
    });
    assert_eq!(
        bohio_proto::fs::ReadInput::decode(&batch.encode().unwrap()).unwrap(),
        batch
    );
}

#[test]
fn an_enum_value_a_newer_host_sends_decodes_as_other_and_the_message_survives() {
    use bohio_proto::gui::{GuiInfo, ImageFormat, RectEncoding, Want, WatchEncoding};
    use bohio_proto::signal::{Signal, Signalled};

    #[derive(Serialize)]
    struct Word(&'static str);
    for (name, decoded) in [
        (
            "signal",
            decode::<Signal>(&encode(&Word("suspend")).unwrap()).map(|v| v == Signal::Other),
        ),
        (
            "want",
            decode::<Want>(&encode(&Word("camera")).unwrap()).map(|v| v == Want::Other),
        ),
        (
            "image format",
            decode::<ImageFormat>(&encode(&Word("avif")).unwrap()).map(|v| v == ImageFormat::Other),
        ),
        (
            "rect encoding",
            decode::<RectEncoding>(&encode(&Word("h264")).unwrap())
                .map(|v| v == RectEncoding::Other),
        ),
        (
            "watch encoding",
            decode::<WatchEncoding>(&encode(&Word("av1")).unwrap())
                .map(|v| v == WatchEncoding::Other),
        ),
    ] {
        assert_eq!(decoded.ok(), Some(true), "{name}");
    }

    #[derive(Serialize)]
    struct NewerSignalled {
        signal: &'static str,
        delivered: bool,
    }
    let signalled: Signalled = decode(
        &encode(&NewerSignalled {
            signal: "suspend",
            delivered: false,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(signalled.signal, Signal::Other);

    #[derive(Serialize)]
    struct NewerGui {
        formats: Vec<&'static str>,
    }
    let gui: GuiInfo = decode(
        &encode(&NewerGui {
            formats: vec!["png", "avif"],
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(gui.formats, [ImageFormat::Png, ImageFormat::Other]);

    // A reply type this version lacks is a reply of no known kind, not a message that cannot be read.
    #[derive(Serialize)]
    struct NewerReply {
        id: u64,
        reply: Tagged,
    }
    #[derive(Serialize)]
    struct Tagged {
        r#type: &'static str,
    }
    let response: Response = decode(
        &encode(&NewerReply {
            id: 4,
            reply: Tagged { r#type: "teleport" },
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(response.id, 4);
    assert_eq!(response.reply, Some(Reply::Other));
}

#[test]
fn a_parse_error_names_the_part_and_never_quotes_the_text() {
    use bohio_proto::compress::ParseError;
    use bohio_proto::gui::{BadCombo, InputEvent};
    let error = Compression::parse("zstd:secret-text").unwrap_err();
    assert_eq!(error, ParseError::Level);
    assert!(!error.to_string().contains("secret"));
    assert_eq!("zstd:3".parse::<Compression>().unwrap().level, Some(3));
    let combo = InputEvent::combo("ctrl+secret-key").unwrap_err();
    assert_eq!(combo, BadCombo { part: 2 });
    assert!(!combo.to_string().contains("secret"));
}
