//! Every message type round-trips; decoding is liberal and never panics.

use std::collections::BTreeMap;

use bohio_proto::control::{Call, Reply, Request as ControlRequest, Response};
use bohio_proto::exec::{
    Command, Env, Exit, Input, Output, Request as ExecRequest, Started, Stdin,
};
use bohio_proto::info::{
    ExecInfo, FilesystemInfo, Info, Limits, PROTOCOL_VERSION, Service, SessionInfo, ShellInfo,
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
        exec: ExecInfo { enabled: true },
        shell: SessionInfo { enabled: false },
        limits: Limits {
            max_streams: Some(64),
            max_processes: Some(16),
            max_sessions: None,
        },
    }
}

#[test]
fn every_control_message_round_trips() {
    round_trips(&ControlRequest {
        id: 1,
        call: Call::Hello {
            version: 1,
            client: "test".into(),
        },
    });
    round_trips(&ControlRequest {
        id: u64::MAX,
        call: Call::Info,
    });
    round_trips(&Response::ok(2, Reply::Hello { version: 1 }));
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
        Output::Started(Started { pid: 42 }),
        Output::Stdout(vec![0, 255, 10]),
        Output::Stderr(Vec::new()),
        Output::Exit(exit),
        Output::Error(ErrorBody::new(ErrorCode::NotFound, "no such program")),
    ] {
        assert_eq!(Output::decode(&output.encode().unwrap()).unwrap(), output);
    }
    for input in [Input::Stdin(vec![1, 2, 3]), Input::StdinEnd, Input::Kill] {
        assert_eq!(Input::decode(&input.encode()).unwrap(), input);
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

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic_any_decoder(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = decode::<ControlRequest>(&bytes);
        let _ = decode::<Response>(&bytes);
        let _ = decode::<Info>(&bytes);
        let _ = ExecRequest::from_metadata(&bytes);
        let _ = Output::decode(&bytes);
        let _ = Input::decode(&bytes);
    }

    #[test]
    fn exec_frames_round_trip_for_any_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let out = Output::Stdout(bytes.clone());
        prop_assert_eq!(Output::decode(&out.encode().unwrap()).unwrap(), out);
        let input = Input::Stdin(bytes);
        prop_assert_eq!(Input::decode(&input.encode()).unwrap(), input);
    }
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
        Call::Checksum {
            path: "a".into(),
            offset: 5,
            length: Some(9),
        },
    ];
    for (id, call) in (1..).zip(calls) {
        round_trips(&ControlRequest { id, call });
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
        Reply::Checksum {
            sha256: "ab".repeat(32),
            length: 3,
        },
    ];
    for (id, reply) in (1..).zip(replies) {
        round_trips(&Response::ok(id, reply));
    }
}

#[test]
fn every_transfer_message_round_trips() {
    use bohio_proto::fs::{
        ReadHeader, ReadOutput, ReadRequest, WriteDone, WriteInput, WriteMode, WriteOutput,
        WriteRequest,
    };
    let read = ReadRequest {
        path: "f".into(),
        offset: 4,
        length: Some(8),
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
    ] {
        let write = WriteRequest {
            path: "f".into(),
            mode,
            parents: true,
            file_mode: Some(0o600),
            expect_sha256: Some("cd".repeat(32)),
        };
        assert_eq!(
            WriteRequest::from_metadata(&write.to_metadata().unwrap()).unwrap(),
            write
        );
    }
    let error = ErrorBody::new(ErrorCode::OutsideRoot, "no");
    for output in [
        ReadOutput::Header(ReadHeader { size: 5, length: 3 }),
        ReadOutput::Data(vec![0, 1, 255]),
        ReadOutput::Done,
        ReadOutput::Error(error.clone()),
    ] {
        assert_eq!(
            ReadOutput::decode(&output.encode().unwrap()).unwrap(),
            output
        );
    }
    for output in [
        WriteOutput::Ready,
        WriteOutput::Done(WriteDone {
            written: 3,
            sha256: "ef".repeat(32),
        }),
        WriteOutput::Error(error),
    ] {
        assert_eq!(
            WriteOutput::decode(&output.encode().unwrap()).unwrap(),
            output
        );
    }
    for input in [WriteInput::Data(vec![9, 8]), WriteInput::End] {
        assert_eq!(WriteInput::decode(&input.encode()).unwrap(), input);
    }
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
    assert!(!bare.parents && bare.file_mode.is_none() && bare.expect_sha256.is_none());
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
        Output::Started(Started { pid: 4242 }),
        Output::Data(vec![0x1b, b'[', b'6', b'n', 0, 255]),
        Output::Exit(Exit {
            code: Some(-1),
            signal: Some(9),
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
