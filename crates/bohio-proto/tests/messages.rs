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
