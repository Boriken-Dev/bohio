//! The wire as data: `vectors/*.json` at the repository root hold the hex of encoded messages, and
//! every client in any language decodes them.
//!
//! The test builds the vectors from the types, writes a file whose content differs (so a tree that
//! is clean after `cargo test` holds current vectors), reads every file back and decodes each
//! entry the way a foreign client would: from the hex alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bohio_proto::control::{Call, Reply, Request as ControlRequest, Response};
use bohio_proto::exec::{self, Env, Stdin};
use bohio_proto::fs::{
    Entry, FileKind, Listing, Metadata, ReadHeader, ReadOutput, ReadRequest, Timestamp, WriteDone,
    WriteInput, WriteMode, WriteOutput, WriteRequest,
};
use bohio_proto::info::{
    ExecInfo, FilesystemInfo, Info, Limits, PROTOCOL_VERSION, Service, SessionInfo, ShellInfo,
};
use bohio_proto::shell::{self, ShellKind, Size};
use bohio_proto::{Error, ErrorBody, ErrorCode, decode, encode};
use ciborium::Value as Cbor;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len() & 1 == 0, "odd hex length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digit"))
        .collect()
}

/// One vector: the bytes, and what they mean.
struct Case {
    name: String,
    direction: &'static str,
    kind: &'static str,
    ty: &'static str,
    bytes: Vec<u8>,
    value: Option<Value>,
    /// The bytes are not what the type encodes to (a message with a key this version does not
    /// know): a decoder must accept them, but they are not compared with a re-encoding.
    decode_only: bool,
    /// A decoder must refuse the bytes with this error.
    error: Option<&'static str>,
}

impl Case {
    fn entry(&self) -> Value {
        let mut entry = json!({
            "name": self.name,
            "direction": self.direction,
            "kind": self.kind,
            "type": self.ty,
            "hex": hex(&self.bytes),
        });
        let map = entry.as_object_mut().unwrap();
        if let Some(value) = &self.value {
            map.insert("value".into(), value.clone());
        }
        if self.decode_only {
            map.insert("decode_only".into(), json!(true));
        }
        if let Some(error) = self.error {
            map.insert("error".into(), json!(error));
        }
        entry
    }
}

fn message<T: Serialize>(
    name: &str,
    direction: &'static str,
    kind: &'static str,
    ty: &'static str,
    value: &T,
) -> Case {
    Case {
        name: name.to_owned(),
        direction,
        kind,
        ty,
        bytes: encode(value).unwrap(),
        value: Some(serde_json::to_value(value).unwrap()),
        decode_only: false,
        error: None,
    }
}

/// A frame: a tag byte, then the body. `body` describes it for a reader of the file.
fn frame(
    name: &str,
    direction: &'static str,
    ty: &'static str,
    bytes: Vec<u8>,
    body: Value,
) -> Case {
    let mut value = json!({ "tag": bytes[0] });
    match body {
        Value::Null => {}
        Value::String(raw) => {
            value["data_hex"] = json!(raw);
        }
        message => {
            value["message"] = message;
        }
    }
    Case {
        name: name.to_owned(),
        direction,
        kind: "frame",
        ty,
        bytes,
        value: Some(value),
        decode_only: false,
        error: None,
    }
}

fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

fn raw(bytes: &[u8]) -> Value {
    json!(hex(bytes))
}

/// A message written from a CBOR map by hand-picked keys, for what the types never produce.
fn map(pairs: Vec<(&str, Cbor)>) -> Vec<u8> {
    let value = Cbor::Map(
        pairs
            .into_iter()
            .map(|(key, value)| (Cbor::Text(key.to_owned()), value))
            .collect(),
    );
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).unwrap();
    bytes
}

fn text(value: &str) -> Cbor {
    Cbor::Text(value.to_owned())
}

fn sample_info() -> Info {
    Info {
        protocol: PROTOCOL_VERSION,
        service: Service {
            name: "bohio".into(),
            version: "0.0.0".into(),
        },
        host_id: Some("example-host".into()),
        os: "linux".into(),
        arch: "x86_64".into(),
        shells: vec![
            ShellInfo {
                kind: ShellKind::Sh,
                path: "/bin/sh".into(),
            },
            ShellInfo {
                kind: ShellKind::Bash,
                path: "/usr/bin/bash".into(),
            },
        ],
        filesystem: FilesystemInfo {
            enabled: true,
            symlinks: true,
            unix_modes: true,
            windows_attributes: false,
            case_sensitive: true,
            separator: "/".into(),
        },
        exec: ExecInfo { enabled: true },
        shell: SessionInfo { enabled: true },
        limits: Limits {
            max_streams: Some(64),
            max_processes: Some(16),
            max_sessions: None,
        },
    }
}

fn sample_metadata() -> Metadata {
    Metadata {
        kind: FileKind::File,
        size: 42,
        modified: Some(Timestamp {
            secs: 1_700_000_000,
            nanos: 5,
        }),
        accessed: None,
        created: None,
        mode: Some(0o640),
        readonly: false,
        attributes: None,
    }
}

const TO_HOST: &str = "client_to_host";
const TO_CLIENT: &str = "host_to_client";
const OPEN: &str = "client_to_host";

fn control() -> Vec<Case> {
    let request = |name: &str, id: u64, call: Call| {
        message(
            name,
            TO_HOST,
            "message",
            "control.Request",
            &ControlRequest { id, call },
        )
    };
    let response = |name: &str, id: u64, reply: Reply| {
        message(
            name,
            TO_CLIENT,
            "message",
            "control.Response",
            &Response::ok(id, reply),
        )
    };
    let mut cases = vec![
        request(
            "hello",
            1,
            Call::Hello {
                version: 1,
                client: "example".into(),
            },
        ),
        request("info", 2, Call::Info),
        request(
            "stat",
            3,
            Call::Stat {
                path: "/etc/hostname".into(),
                follow: true,
            },
        ),
        request(
            "list_page",
            4,
            Call::List {
                path: "/var/log".into(),
                after: Some("syslog".into()),
                limit: Some(100),
            },
        ),
        request(
            "mkdir",
            5,
            Call::Mkdir {
                path: "/srv/work/a/b".into(),
                parents: true,
                mode: Some(0o755),
            },
        ),
        request(
            "remove",
            6,
            Call::Remove {
                path: "/srv/work/a".into(),
                recursive: true,
            },
        ),
        request(
            "rename",
            7,
            Call::Rename {
                from: "/srv/a".into(),
                to: "/srv/b".into(),
                overwrite: false,
            },
        ),
        request(
            "copy",
            8,
            Call::Copy {
                from: "/srv/a".into(),
                to: "/srv/c".into(),
                overwrite: true,
            },
        ),
        request(
            "symlink",
            9,
            Call::Symlink {
                target: "a".into(),
                link: "/srv/l".into(),
            },
        ),
        request(
            "readlink",
            10,
            Call::Readlink {
                path: "/srv/l".into(),
            },
        ),
        request(
            "set_times",
            11,
            Call::SetTimes {
                path: "/srv/a".into(),
                accessed: None,
                modified: Some(Timestamp {
                    secs: -3,
                    nanos: 999_999_999,
                }),
            },
        ),
        request(
            "set_mode",
            12,
            Call::SetMode {
                path: "/srv/a".into(),
                mode: 0o600,
            },
        ),
        request(
            "set_attributes",
            13,
            Call::SetAttributes {
                path: "C:\\work\\a.txt".into(),
                readonly: Some(true),
            },
        ),
        request(
            "checksum_range",
            14,
            Call::Checksum {
                path: "/srv/a".into(),
                offset: 1024,
                length: Some(4096),
            },
        ),
        request("largest_id", u64::MAX, Call::Info),
        response("hello_reply", 1, Reply::Hello { version: 1 }),
        response("info_reply", 2, Reply::Info(Box::new(sample_info()))),
        response("stat_reply", 3, Reply::Stat(Box::new(sample_metadata()))),
        response(
            "list_reply",
            4,
            Reply::List(Listing {
                entries: vec![
                    Entry {
                        name: "a.txt".into(),
                        metadata: sample_metadata(),
                    },
                    Entry {
                        name: "dir".into(),
                        metadata: Metadata {
                            kind: FileKind::Directory,
                            size: 0,
                            mode: Some(0o755),
                            ..sample_metadata()
                        },
                    },
                ],
                next: Some("dir".into()),
                skipped: 1,
            }),
        ),
        response("done_reply", 5, Reply::Done),
        response("link_reply", 10, Reply::Link { target: "a".into() }),
        response(
            "checksum_reply",
            14,
            Reply::Checksum {
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                length: 0,
            },
        ),
    ];
    for (name, code) in [
        ("bad_request", ErrorCode::BadRequest),
        ("denied", ErrorCode::Denied),
        ("not_found", ErrorCode::NotFound),
        ("exists", ErrorCode::Exists),
        ("unsupported", ErrorCode::Unsupported),
        ("busy", ErrorCode::Busy),
        ("outside_root", ErrorCode::OutsideRoot),
        ("io", ErrorCode::Io),
        ("internal", ErrorCode::Internal),
    ] {
        cases.push(message(
            &format!("error_{name}"),
            TO_CLIENT,
            "message",
            "control.Response",
            &Response::failed(20, ErrorBody::new(code, "message text")),
        ));
    }
    // A message from a newer peer: an error code and keys this version does not know.
    cases.push(Case {
        name: "error_code_from_a_newer_peer".into(),
        direction: TO_CLIENT,
        kind: "message",
        ty: "control.Response",
        bytes: map(vec![
            ("id", Cbor::Integer(21.into())),
            (
                "error",
                Cbor::Map(vec![
                    (text("code"), text("quota_exceeded")),
                    (text("message"), text("full")),
                    (text("retry_after"), Cbor::Integer(30.into())),
                ]),
            ),
        ]),
        value: Some(to_json(&Response::failed(
            21,
            ErrorBody::new(ErrorCode::Other, "full"),
        ))),
        decode_only: true,
        error: None,
    });
    cases.push(Case {
        name: "info_with_unknown_and_missing_fields".into(),
        direction: TO_CLIENT,
        kind: "message",
        ty: "control.Response",
        bytes: map(vec![
            ("id", Cbor::Integer(22.into())),
            (
                "reply",
                Cbor::Map(vec![
                    (text("type"), text("info")),
                    (text("os"), text("linux")),
                    (text("gpu_count"), Cbor::Integer(2.into())),
                ]),
            ),
        ]),
        value: Some(to_json(&Response::ok(
            22,
            Reply::Info(Box::new(Info {
                os: "linux".into(),
                ..Info::default()
            })),
        ))),
        decode_only: true,
        error: None,
    });
    cases
}

fn exec_cases() -> Vec<Case> {
    let mut full = exec::Request::argv(["prog", "arg with space", ""]);
    full.cwd = Some("/tmp".into());
    full.env = Env {
        clear: false,
        set: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
        remove: vec!["B".into()],
    };
    full.stdin = Stdin::Open;
    full.timeout_ms = Some(1500);
    let exit = exec::Exit {
        code: Some(3),
        signal: None,
        timed_out: false,
        killed: false,
    };
    let signalled = exec::Exit {
        code: None,
        signal: Some(9),
        timed_out: true,
        killed: true,
    };
    let output = |name: &str, value: exec::Output, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "exec.Output",
            value.encode().unwrap(),
            body,
        )
    };
    let input = |name: &str, value: exec::Input, body: Value| {
        frame(name, TO_HOST, "exec.Input", value.encode(), body)
    };
    vec![
        message(
            "open_argv",
            OPEN,
            "open",
            "exec.Request",
            &exec::Request::argv(["echo", "hello"]),
        ),
        message("open_full", OPEN, "open", "exec.Request", &full),
        message(
            "open_shell_script",
            OPEN,
            "open",
            "exec.Request",
            &exec::Request::shell(ShellKind::Sh, "echo $HOME"),
        ),
        Case {
            name: "open_minimal_takes_defaults".into(),
            direction: OPEN,
            kind: "open",
            ty: "exec.Request",
            bytes: map(vec![(
                "command",
                Cbor::Map(vec![
                    (text("type"), text("argv")),
                    (text("argv"), Cbor::Array(vec![text("x")])),
                ]),
            )]),
            value: Some(to_json(&exec::Request::argv(["x"]))),
            decode_only: true,
            error: None,
        },
        output(
            "started",
            exec::Output::Started(exec::Started { pid: 4242 }),
            to_json(&exec::Started { pid: 4242 }),
        ),
        output(
            "stdout",
            exec::Output::Stdout(b"hello\n".to_vec()),
            raw(b"hello\n"),
        ),
        output(
            "stderr",
            exec::Output::Stderr(vec![0, 255, 10]),
            raw(&[0, 255, 10]),
        ),
        output("exit_code", exec::Output::Exit(exit), to_json(&exit)),
        output(
            "exit_signalled_at_the_deadline",
            exec::Output::Exit(signalled),
            to_json(&signalled),
        ),
        output(
            "error_not_found",
            exec::Output::Error(ErrorBody::new(ErrorCode::NotFound, "no such program")),
            to_json(&ErrorBody::new(ErrorCode::NotFound, "no such program")),
        ),
        input("stdin", exec::Input::Stdin(b"data".to_vec()), raw(b"data")),
        input("stdin_end", exec::Input::StdinEnd, Value::Null),
        input("kill", exec::Input::Kill, Value::Null),
        Case {
            name: "unknown_tag_from_the_host".into(),
            direction: TO_CLIENT,
            kind: "frame",
            ty: "exec.Output",
            bytes: vec![0x7f, 1, 2],
            value: None,
            decode_only: true,
            error: Some("tag"),
        },
        Case {
            name: "empty_frame".into(),
            direction: TO_CLIENT,
            kind: "frame",
            ty: "exec.Output",
            bytes: Vec::new(),
            value: None,
            decode_only: true,
            error: Some("short"),
        },
    ]
}

fn shell_cases() -> Vec<Case> {
    let mut by_argv = shell::Request::argv(["/bin/zsh", "-l"]);
    by_argv.cwd = Some("/srv".into());
    by_argv.cols = 132;
    by_argv.rows = 43;
    let exit = shell::Exit {
        code: Some(0),
        signal: None,
        killed: false,
    };
    let output = |name: &str, value: shell::Output, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "shell.Output",
            value.encode().unwrap(),
            body,
        )
    };
    let input = |name: &str, value: shell::Input, body: Value| {
        frame(name, TO_HOST, "shell.Input", value.encode().unwrap(), body)
    };
    vec![
        message(
            "open_kind",
            OPEN,
            "open",
            "shell.Request",
            &shell::Request::kind(ShellKind::Bash),
        ),
        message("open_argv_sized", OPEN, "open", "shell.Request", &by_argv),
        output(
            "started",
            shell::Output::Started(shell::Started { pid: 77 }),
            to_json(&shell::Started { pid: 77 }),
        ),
        output(
            "data",
            shell::Output::Data(b"\x1b[32mok\x1b[0m\r\n".to_vec()),
            raw(b"\x1b[32mok\x1b[0m\r\n"),
        ),
        output("exit", shell::Output::Exit(exit), to_json(&exit)),
        output(
            "error_denied",
            shell::Output::Error(ErrorBody::new(ErrorCode::Denied, "not allowed")),
            to_json(&ErrorBody::new(ErrorCode::Denied, "not allowed")),
        ),
        input("data", shell::Input::Data(b"ls\r".to_vec()), raw(b"ls\r")),
        input(
            "resize",
            shell::Input::Resize(Size {
                cols: 100,
                rows: 30,
            }),
            to_json(&Size {
                cols: 100,
                rows: 30,
            }),
        ),
        input("kill", shell::Input::Kill, Value::Null),
    ]
}

fn read_cases() -> Vec<Case> {
    let output = |name: &str, value: ReadOutput, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "fs.ReadOutput",
            value.encode().unwrap(),
            body,
        )
    };
    vec![
        message(
            "open_whole",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest::whole("/etc/hostname"),
        ),
        message(
            "open_range",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                path: "/srv/big.bin".into(),
                offset: 1_048_576,
                length: Some(65_536),
            },
        ),
        output(
            "header",
            ReadOutput::Header(ReadHeader { size: 9, length: 5 }),
            to_json(&ReadHeader { size: 9, length: 5 }),
        ),
        output("data", ReadOutput::Data(b"hello".to_vec()), raw(b"hello")),
        output("done", ReadOutput::Done, Value::Null),
        output(
            "error_outside_root",
            ReadOutput::Error(ErrorBody::new(ErrorCode::OutsideRoot, "a path led outside")),
            to_json(&ErrorBody::new(
                ErrorCode::OutsideRoot,
                "a path led outside",
            )),
        ),
    ]
}

fn write_cases() -> Vec<Case> {
    let output = |name: &str, value: WriteOutput, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "fs.WriteOutput",
            value.encode().unwrap(),
            body,
        )
    };
    let input = |name: &str, value: WriteInput, body: Value| {
        frame(name, TO_HOST, "fs.WriteInput", value.encode(), body)
    };
    let done = WriteDone {
        written: 5,
        sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
    };
    let mut checked = WriteRequest::replace("/srv/out.txt");
    checked.parents = true;
    checked.file_mode = Some(0o600);
    checked.expect_sha256 = Some(done.sha256.clone());
    vec![
        message(
            "open_replace",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest::replace("/srv/out.txt"),
        ),
        message("open_checked", OPEN, "open", "fs.WriteRequest", &checked),
        message(
            "open_append",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                mode: WriteMode::Append,
                ..WriteRequest::replace("/srv/log.txt")
            },
        ),
        message(
            "open_at_offset",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                mode: WriteMode::At { offset: 512 },
                ..WriteRequest::replace("/srv/img.bin")
            },
        ),
        message(
            "open_create",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                mode: WriteMode::Create,
                ..WriteRequest::replace("/srv/new.txt")
            },
        ),
        output("ready", WriteOutput::Ready, Value::Null),
        output("done", WriteOutput::Done(done.clone()), to_json(&done)),
        output(
            "error_exists",
            WriteOutput::Error(ErrorBody::new(ErrorCode::Exists, "exists")),
            to_json(&ErrorBody::new(ErrorCode::Exists, "exists")),
        ),
        input("data", WriteInput::Data(b"hello".to_vec()), raw(b"hello")),
        input("end", WriteInput::End, Value::Null),
    ]
}

fn files() -> Vec<(&'static str, &'static str, Vec<Case>)> {
    vec![
        ("control.json", "bohio/1", control()),
        ("exec.json", "bohio-exec/1", exec_cases()),
        ("shell.json", "bohio-shell/1", shell_cases()),
        ("read.json", "bohio-read/1", read_cases()),
        ("write.json", "bohio-write/1", write_cases()),
    ]
}

fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors")
}

fn render(protocol: &str, cases: &[Case]) -> String {
    let document = json!({
        "protocol": protocol,
        "encoding": "hex is the stream message exactly as it travels: open metadata and control messages are bare CBOR, a frame is one tag byte then its body",
        "vectors": cases.iter().map(Case::entry).collect::<Vec<_>>(),
    });
    let mut out = serde_json::to_string_pretty(&document).unwrap();
    out.push('\n');
    out
}

fn decode_error(ty: &str, bytes: &[u8]) -> Result<(), Error> {
    match ty {
        "control.Request" => decode::<ControlRequest>(bytes).map(|_| ()),
        "control.Response" => decode::<Response>(bytes).map(|_| ()),
        "exec.Request" => exec::Request::from_metadata(bytes).map(|_| ()),
        "exec.Output" => exec::Output::decode(bytes).map(|_| ()),
        "exec.Input" => exec::Input::decode(bytes).map(|_| ()),
        "shell.Request" => shell::Request::from_metadata(bytes).map(|_| ()),
        "shell.Output" => shell::Output::decode(bytes).map(|_| ()),
        "shell.Input" => shell::Input::decode(bytes).map(|_| ()),
        "fs.ReadRequest" => ReadRequest::from_metadata(bytes).map(|_| ()),
        "fs.ReadOutput" => ReadOutput::decode(bytes).map(|_| ()),
        "fs.WriteRequest" => WriteRequest::from_metadata(bytes).map(|_| ()),
        "fs.WriteOutput" => WriteOutput::decode(bytes).map(|_| ()),
        "fs.WriteInput" => WriteInput::decode(bytes).map(|_| ()),
        other => panic!("a vector names a type this test does not know: {other}"),
    }
}

/// Decodes `bytes` as a CBOR message, then checks what it means and (unless the vector is
/// decode-only) that encoding it again gives the same bytes.
fn check_message<T: Serialize + DeserializeOwned>(entry: &Value, bytes: &[u8]) {
    let decoded: T = decode(bytes).expect("a vector decodes");
    assert_eq!(Some(&to_json(&decoded)), entry.get("value"), "{entry}");
    if entry.get("decode_only").is_none() {
        assert_eq!(encode(&decoded).unwrap(), bytes, "{entry}");
    }
}

fn check_frame<T>(
    entry: &Value,
    bytes: &[u8],
    decode: fn(&[u8]) -> Result<T, Error>,
    encode: impl Fn(&T) -> Vec<u8>,
) {
    let decoded = decode(bytes).expect("a vector decodes");
    assert_eq!(encode(&decoded), bytes, "{entry}");
    assert_eq!(entry["value"]["tag"], json!(bytes[0]), "{entry}");
}

fn check(entry: &Value) {
    let ty = entry["type"].as_str().unwrap();
    let bytes = unhex(entry["hex"].as_str().unwrap());
    if let Some(error) = entry.get("error") {
        let got = decode_error(ty, &bytes).expect_err("a vector that must be refused");
        let name = match got {
            Error::Tag(_) => "tag",
            Error::Short => "short",
            Error::Decode => "decode",
            Error::Encode => "encode",
            _ => "other",
        };
        assert_eq!(error, name, "{entry}");
        return;
    }
    match ty {
        "control.Request" => check_message::<ControlRequest>(entry, &bytes),
        "control.Response" => check_message::<Response>(entry, &bytes),
        "exec.Request" => check_message::<exec::Request>(entry, &bytes),
        "shell.Request" => check_message::<shell::Request>(entry, &bytes),
        "fs.ReadRequest" => check_message::<ReadRequest>(entry, &bytes),
        "fs.WriteRequest" => check_message::<WriteRequest>(entry, &bytes),
        "exec.Output" => check_frame(entry, &bytes, exec::Output::decode, |v| v.encode().unwrap()),
        "exec.Input" => check_frame(entry, &bytes, exec::Input::decode, exec::Input::encode),
        "shell.Output" => check_frame(entry, &bytes, shell::Output::decode, |v| {
            v.encode().unwrap()
        }),
        "shell.Input" => check_frame(entry, &bytes, shell::Input::decode, |v| v.encode().unwrap()),
        "fs.ReadOutput" => check_frame(entry, &bytes, ReadOutput::decode, |v| v.encode().unwrap()),
        "fs.WriteOutput" => {
            check_frame(entry, &bytes, WriteOutput::decode, |v| v.encode().unwrap())
        }
        "fs.WriteInput" => check_frame(entry, &bytes, WriteInput::decode, WriteInput::encode),
        other => panic!("a vector names a type this test does not know: {other}"),
    }
}

#[test]
fn the_vectors_are_current_and_every_one_decodes_from_its_hex() {
    let dir = vectors_dir();
    // A packaged crate has no repository around it: the vectors are checked where they exist.
    let in_repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../Cargo.toml")
        .exists();
    if in_repository {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let mut seen = 0;
    for (file, protocol, cases) in files() {
        let rendered = render(protocol, &cases);
        let path = dir.join(file);
        if in_repository
            && std::fs::read_to_string(&path).ok().as_deref() != Some(rendered.as_str())
        {
            std::fs::write(&path, rendered.as_bytes()).unwrap();
        }
        let Ok(on_disk) = std::fs::read_to_string(&path) else {
            continue;
        };
        let document: Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(document["protocol"], protocol);
        for entry in document["vectors"].as_array().unwrap() {
            check(entry);
            seen += 1;
        }
    }
    assert!(
        !in_repository || seen >= 60,
        "only {seen} vectors were checked"
    );
}
