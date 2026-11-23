//! The wire as data: `vectors/*.json` at the repository root hold the hex of encoded messages, and
//! every client in any language decodes them.
//!
//! The test builds the vectors from the types, writes a file whose content differs (so a tree that
//! is clean after `cargo test` holds current vectors), reads every file back and decodes each
//! entry the way a foreign client would: from the hex alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bohio_proto::compress::{Codec, Compression, Progress, Resume};
use bohio_proto::control::{
    Call, Disk, Memory, Reply, Request as ControlRequest, Response, Stats, Swap,
};
use bohio_proto::exec::{self, Env, Stdin};
use bohio_proto::forward::{
    AcceptRequest, Answer, Arrival, Bind, ConnectRequest, Connected, ForwardInfo, ListenFrame,
    ListenRequest, Listening, Target,
};
use bohio_proto::fs::{
    BundleEnd, DeltaBase, DeltaCopy, Entry, FileKind, Flushed, FollowBy, Listing, Metadata,
    ReadDelta, ReadDone, ReadFollow, ReadHeader, ReadInput, ReadItem, ReadOutput, ReadRequest,
    Ready as WriteReady, SignatureBatch, Timestamp, Truncated, WriteDone, WriteInput, WriteItem,
    WriteMode, WriteOutput, WriteRequest,
};
use bohio_proto::gui::{
    Ack, ApproveInput, ApproveOutput, Button, ButtonAction, ClipboardGet, ClipboardSet,
    ClipboardText, CursorFrame, Display, DisplayId, GuiInfo, GuiInput, GuiOutput, GuiRequest,
    GuiSession, ImageFormat, ImageHeader, Input, InputDone, InputEvent, KeyAction, PendingRequest,
    Point, Ready, Rect, RectEncoding, RequestError, Settle, SettleOptions, Settled, SettledFrame,
    Snapshot, State, Unwatch, UpdateHeader, UpdateRect, Want, Watch, WatchEncoding,
};
use bohio_proto::hash::{
    Algorithm, BlockOptions, Digests, HashBlocks, HashDone, HashInput, HashItem, HashOutput,
    HashRequest, HashResult, ItemError,
};
use bohio_proto::info::{
    CompressionInfo, DeltaInfo, DetachedInfo, ExecInfo, FilesystemInfo, HashLimits, Info, Levels,
    Limits, PROTOCOL_VERSION, SearchLimits, Service, SessionInfo, ShellInfo, TransferInfo,
    WalkLimits,
};
use bohio_proto::search::{
    SearchDone, SearchFile, SearchItemError, SearchMatch, SearchOutput, SearchRequest, SearchScope,
    Skipped,
};
use bohio_proto::session::{Attach, Attached, Kind, Listed, OnFull, SessionRequest};
use bohio_proto::shell::{self, ShellKind, Size};
use bohio_proto::signal::Signal;
use bohio_proto::walk::{
    WalkDone, WalkEntries, WalkEntry, WalkHeader, WalkInput, WalkItemError, WalkOutput,
    WalkRequest, WalkRoot,
};
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
        exec: ExecInfo {
            enabled: true,
            signals: Signal::ALL.to_vec(),
        },
        shell: SessionInfo {
            enabled: true,
            signals: vec![Signal::Interrupt, Signal::Terminate, Signal::Kill],
        },
        sessions: DetachedInfo {
            enabled: true,
            scrollback: 1_048_576,
            max_scrollback: 16_777_216,
            linger_ms: 86_400_000,
        },
        limits: Limits {
            max_streams: Some(64),
            max_processes: Some(16),
            max_sessions: None,
        },
        transfer: sample_transfer(),
        forward: ForwardInfo {
            enabled: true,
            connect: vec!["tcp".into(), "unix".into()],
            listen: vec!["tcp".into(), "unix".into()],
        },
        gui: sample_gui_info(),
    }
}

fn sample_display() -> Display {
    Display {
        id: DisplayId::from("0"),
        name: Some("Built-in".into()),
        primary: true,
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        scale: 1.5,
        rotation: Some(0),
    }
}

fn sample_state() -> State {
    State {
        active: true,
        locked: Some(false),
        displays: vec![sample_display()],
    }
}

fn sample_gui_info() -> GuiInfo {
    GuiInfo {
        enabled: true,
        available: true,
        reason: None,
        backend: Some("x11".into()),
        input: true,
        clipboard: true,
        lock_detection: false,
        formats: vec![ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp],
        sessions: vec![GuiSession {
            id: "default".into(),
            user: Some("alice".into()),
            active: true,
            locked: Some(false),
            displays: vec![sample_display()],
        }],
    }
}

fn sample_transfer() -> TransferInfo {
    TransferInfo {
        hashes: vec![Algorithm::Md5, Algorithm::Sha256, Algorithm::Blake3],
        hash_limits: HashLimits {
            max_streams: 4,
            rate: 0,
        },
        compression: vec![
            CompressionInfo {
                algorithm: Codec::Zstd,
                levels: Levels {
                    min: -131_072,
                    max: 19,
                },
                default_level: 3,
                window_log_max: 23,
            },
            CompressionInfo {
                algorithm: Codec::Lz4,
                levels: Levels::default(),
                default_level: 0,
                window_log_max: 0,
            },
            CompressionInfo {
                algorithm: Codec::Deflate,
                levels: Levels { min: 0, max: 9 },
                default_level: 6,
                window_log_max: 0,
            },
        ],
        compression_default: Some(Compression::new(Codec::Zstd).level(3)),
        prepared: true,
        delta: DeltaInfo {
            weak: vec!["rsync".into()],
            min_block: 2048,
            max_block: 1_048_576,
            min_size: 1_048_576,
            max_signature_bytes: 67_108_864,
        },
        walk: true,
        walk_limits: WalkLimits {
            max_streams: 4,
            max_entries: 1_000_000,
        },
        search: true,
        search_limits: SearchLimits {
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
        resume: true,
    }
}

/// Pieces of the compressed stream of `hello hello hello hello hello hello\n`, one flushed piece of
/// each algorithm; `bohio-transfer`'s tests decode these same bytes.
const ZSTD_PIECE: &str = "28b52ffd00586c00003868656c6c6f200a0100b94b11";
const LZ4_PIECE: &str = "04224d18607073110000006f68656c6c6f200600056068656c6c6f0a";
const DEFLATE_PIECE: &str = "789cca48cdc9c957c8c0477201000000ffff";

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
        device: Some(2049),
        inode: Some(131_077),
        links: Some(1),
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
                recursive: false,
                preserve: false,
            },
        ),
        request(
            "copy_tree",
            80,
            Call::Copy {
                from: "/srv/tree".into(),
                to: "/srv/tree.bak".into(),
                overwrite: false,
                recursive: true,
                preserve: true,
            },
        ),
        request(
            "move",
            81,
            Call::Move {
                from: "/srv/a".into(),
                to: "/mnt/b".into(),
                overwrite: true,
            },
        ),
        request(
            "truncate",
            82,
            Call::Truncate {
                path: "/srv/a".into(),
                length: 4096,
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
        request("largest_id", u64::MAX, Call::Info),
        response(
            "hello_reply",
            1,
            Reply::Hello {
                version: 1,
                transfer: sample_transfer(),
            },
        ),
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
        request("stats", 50, Call::Stats),
        response(
            "stats_reply",
            51,
            Reply::Stats(Box::new(Stats {
                hostname: Some("build-host".into()),
                uptime_s: 86_400,
                cpus: 8,
                load: Some([0.5, 0.25, 0.125]),
                memory: Memory {
                    total: 17_179_869_184,
                    available: 8_589_934_592,
                },
                swap: Swap {
                    total: 2_147_483_648,
                    free: 2_147_483_648,
                },
                disks: vec![
                    Disk {
                        mount: "/".into(),
                        filesystem: "ext4".into(),
                        total: 107_374_182_400,
                        available: 53_687_091_200,
                    },
                    Disk {
                        mount: "C:\\".into(),
                        filesystem: "NTFS".into(),
                        total: 536_870_912_000,
                        available: 0,
                    },
                ],
            })),
        ),
        response("stats_reply_bare", 52, Reply::Stats(Box::default())),
        request(
            "which",
            53,
            Call::Which {
                names: vec!["git".into(), "no-such-program".into()],
            },
        ),
        response(
            "which_reply",
            54,
            Reply::Which {
                found: BTreeMap::from([
                    ("git".to_owned(), Some("/usr/bin/git".to_owned())),
                    ("no-such-program".to_owned(), None),
                ]),
            },
        ),
        request("sessions", 40, Call::Sessions),
        request(
            "end_session_interrupt",
            41,
            Call::EndSession {
                id: "00112233445566778899aabbccddeeff".into(),
                signal: Some(Signal::Interrupt),
            },
        ),
        request(
            "end_session_default",
            42,
            Call::EndSession {
                id: "00112233445566778899aabbccddeeff".into(),
                signal: None,
            },
        ),
        response(
            "sessions_reply",
            43,
            Reply::Sessions {
                sessions: vec![
                    Listed {
                        id: "00112233445566778899aabbccddeeff".into(),
                        name: Some("build".into()),
                        kind: Kind::Exec,
                        program: "make".into(),
                        started: Timestamp {
                            secs: 1_700_000_000,
                            nanos: 5,
                        },
                        attached: false,
                        exit: None,
                        next: 1024,
                        buffered: 1024,
                        peer: None,
                    },
                    Listed {
                        id: "ffeeddccbbaa99887766554433221100".into(),
                        name: None,
                        kind: Kind::Shell,
                        program: "bash".into(),
                        started: Timestamp::default(),
                        attached: true,
                        exit: Some(exec::Exit {
                            code: Some(0),
                            ..exec::Exit::default()
                        }),
                        next: 7,
                        buffered: 7,
                        peer: Some("sha256:ab".into()),
                    },
                ],
            },
        ),
        response("link_reply", 10, Reply::Link { target: "a".into() }),
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
    let mut detached = exec::Request::argv(["make", "all"]);
    detached.session = Some(SessionRequest {
        name: Some("build".into()),
        scrollback: Some(65536),
        on_full: Some(OnFull::DropOldest),
        linger_ms: Some(60_000),
        detach: true,
    });
    let signalled_reply = |signal, delivered, message: Option<&str>| exec::Signalled {
        signal,
        delivered,
        message: message.map(str::to_owned),
    };
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
        message("open_session", OPEN, "open", "exec.Request", &detached),
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
        message("open_compressed", OPEN, "open", "exec.Request", &{
            let mut request = exec::Request::argv(["cat"]);
            request.stdin = Stdin::Open;
            request.output_compression = Some(Compression::auto());
            request.stdin_compression = Some(Compression::new(Codec::Lz4));
            request
        }),
        output(
            "started",
            exec::Output::Started(exec::Started::new(4242)),
            to_json(&exec::Started::new(4242)),
        ),
        output(
            "started_compressed",
            exec::Output::Started(exec::Started {
                pid: 4242,
                output_compression: Some(Compression::new(Codec::Zstd).level(3)),
                stdin_compression: Some(Compression::new(Codec::Lz4)),
                session: None,
            }),
            to_json(&exec::Started {
                pid: 4242,
                output_compression: Some(Compression::new(Codec::Zstd).level(3)),
                stdin_compression: Some(Compression::new(Codec::Lz4)),
                session: None,
            }),
        ),
        output(
            "started_session",
            exec::Output::Started(exec::Started {
                session: Some("00112233445566778899aabbccddeeff".into()),
                ..exec::Started::new(4242)
            }),
            to_json(&exec::Started {
                session: Some("00112233445566778899aabbccddeeff".into()),
                ..exec::Started::new(4242)
            }),
        ),
        output(
            "signalled_delivered",
            exec::Output::Signalled(signalled_reply(Signal::Interrupt, true, None)),
            to_json(&signalled_reply(Signal::Interrupt, true, None)),
        ),
        output(
            "signalled_refused",
            exec::Output::Signalled(signalled_reply(
                Signal::User1,
                false,
                Some("Windows has no such signal"),
            )),
            to_json(&signalled_reply(
                Signal::User1,
                false,
                Some("Windows has no such signal"),
            )),
        ),
        output(
            "stdout_compressed",
            exec::Output::StdoutCompressed(unhex(ZSTD_PIECE)),
            raw(&unhex(ZSTD_PIECE)),
        ),
        output(
            "stderr_compressed",
            exec::Output::StderrCompressed(unhex(DEFLATE_PIECE)),
            raw(&unhex(DEFLATE_PIECE)),
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
        input(
            "stdin_compressed",
            exec::Input::StdinCompressed(unhex(LZ4_PIECE)),
            raw(&unhex(LZ4_PIECE)),
        ),
        input("stdin_end", exec::Input::StdinEnd, Value::Null),
        input("kill", exec::Input::Kill, Value::Null),
        input(
            "signal_interrupt",
            exec::Input::Signal(Signal::Interrupt),
            to_json(&bohio_proto::signal::SignalRequest {
                signal: Signal::Interrupt,
            }),
        ),
        input(
            "signal_user2",
            exec::Input::Signal(Signal::User2),
            to_json(&bohio_proto::signal::SignalRequest {
                signal: Signal::User2,
            }),
        ),
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
    let mut with_deadline = shell::Request::argv(["/usr/bin/top"]);
    with_deadline.timeout_ms = Some(30_000);
    let mut detached = shell::Request::kind(ShellKind::Bash);
    detached.session = Some(SessionRequest {
        name: None,
        scrollback: Some(8192),
        on_full: None,
        linger_ms: None,
        detach: false,
    });
    let exit = shell::Exit {
        code: Some(0),
        signal: None,
        timed_out: false,
        killed: false,
    };
    let deadline_exit = shell::Exit {
        code: None,
        signal: Some(9),
        timed_out: true,
        killed: true,
    };
    let answer = shell::Signalled {
        signal: Signal::Terminate,
        delivered: true,
        message: None,
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
        message(
            "open_console",
            OPEN,
            "open",
            "shell.Request",
            &shell::Request::console(Some("console")),
        ),
        message(
            "open_console_default",
            OPEN,
            "open",
            "shell.Request",
            &shell::Request::console(None),
        ),
        message("open_argv_sized", OPEN, "open", "shell.Request", &by_argv),
        message(
            "open_argv_with_deadline",
            OPEN,
            "open",
            "shell.Request",
            &with_deadline,
        ),
        message("open_session", OPEN, "open", "shell.Request", &detached),
        output(
            "started",
            shell::Output::Started(shell::Started::new(77)),
            to_json(&shell::Started::new(77)),
        ),
        output(
            "started_session",
            shell::Output::Started(shell::Started {
                pid: Some(77),
                session: Some("ffeeddccbbaa99887766554433221100".into()),
            }),
            to_json(&shell::Started {
                pid: Some(77),
                session: Some("ffeeddccbbaa99887766554433221100".into()),
            }),
        ),
        output(
            "started_console",
            shell::Output::Started(shell::Started {
                pid: None,
                session: Some("console:console".into()),
            }),
            to_json(&shell::Started {
                pid: None,
                session: Some("console:console".into()),
            }),
        ),
        output(
            "signalled",
            shell::Output::Signalled(answer.clone()),
            to_json(&answer),
        ),
        output(
            "exit_at_the_deadline",
            shell::Output::Exit(deadline_exit),
            to_json(&deadline_exit),
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
        input(
            "signal_interrupt",
            shell::Input::Signal(Signal::Interrupt),
            to_json(&bohio_proto::signal::SignalRequest {
                signal: Signal::Interrupt,
            }),
        ),
    ]
}

fn attach_cases() -> Vec<Case> {
    let attached = |name: &str, value: Attached| {
        frame(
            name,
            TO_CLIENT,
            "session.Attached",
            value.encode().unwrap(),
            to_json(&value),
        )
    };
    vec![
        message(
            "open_from_start",
            OPEN,
            "open",
            "session.Attach",
            &Attach {
                session: "00112233445566778899aabbccddeeff".into(),
                from: Some(0),
            },
        ),
        message(
            "open_only_what_comes_next",
            OPEN,
            "open",
            "session.Attach",
            &Attach {
                session: "00112233445566778899aabbccddeeff".into(),
                from: None,
            },
        ),
        attached(
            "attached_exec",
            Attached {
                kind: Kind::Exec,
                from: 0,
                lost: 0,
            },
        ),
        attached(
            "attached_shell_with_loss",
            Attached {
                kind: Kind::Shell,
                from: 4096,
                lost: 4096,
            },
        ),
        frame(
            "error_not_found",
            TO_CLIENT,
            "exec.Output",
            exec::Output::Error(ErrorBody::new(
                ErrorCode::NotFound,
                "there is no such session",
            ))
            .encode()
            .unwrap(),
            to_json(&ErrorBody::new(
                ErrorCode::NotFound,
                "there is no such session",
            )),
        ),
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
    let read_done = ReadDone {
        hashes: Digests::from([
            (
                Algorithm::Sha256,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            ),
            (Algorithm::Xxh3, "78af5f94892f3950".into()),
        ]),
        index: None,
    };
    let compressed_header = ReadHeader {
        size: 9_000_000,
        length: 9_000_000,
        compression: Some(Compression::new(Codec::Zstd).level(19).window_log(23)),
        ..ReadHeader::raw(9_000_000, 9_000_000)
    };
    let item_header = ReadHeader {
        index: Some(2),
        modified: Some(Timestamp {
            secs: 1_700_000_000,
            nanos: 5,
        }),
        mode: Some(0o644),
        ..ReadHeader::raw(5, 5)
    };
    let stream_header = ReadHeader {
        compression: Some(Compression::new(Codec::Zstd).level(3)),
        ..ReadHeader::raw(0, 0)
    };
    let item_done = ReadDone {
        hashes: Digests::from([(Algorithm::Crc32c, "e3069283".into())]),
        index: Some(2),
    };
    let item_error = ItemError::new(3, ErrorBody::new(ErrorCode::NotFound, "no such file"));
    let delta_header = ReadHeader {
        delta: true,
        ..ReadHeader::raw(9_000_000, 9_000_300)
    };
    let prepared_header = ReadHeader {
        size: 9_000_000,
        length: 9_000_000,
        compression: Some(Compression::new(Codec::Zstd).level(3)),
        compressed_length: Some(1_200_000),
        token: Some("0123456789abcdef0123456789abcdef".into()),
        ..ReadHeader::raw(9_000_000, 9_000_000)
    };
    let signature_batch = SignatureBatch {
        first: 0,
        weak: vec![0x0001_0002, 7],
        strong: vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04],
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
                ..ReadRequest::whole("/srv/big.bin")
            },
        ),
        message(
            "open_compressed",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                compression: Some(Compression::new(Codec::Zstd).level(19).window_log(23)),
                ..ReadRequest::whole("/srv/big.log")
            },
        ),
        message(
            "open_prepared",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                compression: Some(Compression::auto()),
                prepare: true,
                ..ReadRequest::whole("/srv/big.log")
            },
        ),
        message(
            "open_resume",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                resume: Some(Resume {
                    token: "0123456789abcdef0123456789abcdef".into(),
                    offset: 65_536,
                }),
                ..ReadRequest::whole("/srv/big.log")
            },
        ),
        message(
            "open_hashed",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                hashes: vec![Algorithm::Sha256, Algorithm::Xxh3],
                ..ReadRequest::whole("/srv/big.bin")
            },
        ),
        output(
            "header",
            ReadOutput::Header(ReadHeader::raw(9, 5)),
            to_json(&ReadHeader::raw(9, 5)),
        ),
        output(
            "header_compressed",
            ReadOutput::Header(compressed_header.clone()),
            to_json(&compressed_header),
        ),
        message(
            "open_delta",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                delta: Some(ReadDelta {
                    block_size: 8192,
                    weak: "rsync".into(),
                    strong: Algorithm::Blake3,
                    strong_len: 16,
                    base_size: 64_000_000,
                }),
                ..ReadRequest::whole("/srv/big.bin")
            },
        ),
        output(
            "header_delta",
            ReadOutput::Header(delta_header.clone()),
            to_json(&delta_header),
        ),
        output(
            "copy",
            ReadOutput::Copy(DeltaCopy {
                block: 4096,
                count: 12,
            }),
            to_json(&DeltaCopy {
                block: 4096,
                count: 12,
            }),
        ),
        frame(
            "signature",
            TO_HOST,
            "fs.ReadInput",
            ReadInput::Signature(signature_batch.clone())
                .encode()
                .unwrap(),
            to_json(&signature_batch),
        ),
        frame(
            "signature_end",
            TO_HOST,
            "fs.ReadInput",
            ReadInput::SignatureEnd.encode().unwrap(),
            Value::Null,
        ),
        output(
            "header_prepared",
            ReadOutput::Header(prepared_header.clone()),
            to_json(&prepared_header),
        ),
        output("data", ReadOutput::Data(b"hello".to_vec()), raw(b"hello")),
        output(
            "compressed",
            ReadOutput::Compressed(unhex(ZSTD_PIECE)),
            raw(&unhex(ZSTD_PIECE)),
        ),
        output(
            "compressed_lz4",
            ReadOutput::Compressed(unhex(LZ4_PIECE)),
            raw(&unhex(LZ4_PIECE)),
        ),
        output(
            "compressed_deflate",
            ReadOutput::Compressed(unhex(DEFLATE_PIECE)),
            raw(&unhex(DEFLATE_PIECE)),
        ),
        message(
            "open_follow",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                follow: Some(ReadFollow {
                    by: Some(FollowBy::Name),
                    interval_ms: Some(500),
                    tail: Some(4096),
                }),
                ..ReadRequest::whole("/var/log/app.log")
            },
        ),
        message(
            "open_follow_defaults",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                follow: Some(ReadFollow::default()),
                ..ReadRequest::whole("/var/log/app.log")
            },
        ),
        output(
            "header_follow",
            ReadOutput::Header(ReadHeader {
                follow: true,
                ..ReadHeader::raw(9_000, 4_096)
            }),
            to_json(&ReadHeader {
                follow: true,
                ..ReadHeader::raw(9_000, 4_096)
            }),
        ),
        output(
            "truncated",
            ReadOutput::Truncated(Truncated {
                size: 120,
                replaced: false,
            }),
            to_json(&Truncated {
                size: 120,
                replaced: false,
            }),
        ),
        output(
            "truncated_replaced",
            ReadOutput::Truncated(Truncated {
                size: 0,
                replaced: true,
            }),
            to_json(&Truncated {
                size: 0,
                replaced: true,
            }),
        ),
        output(
            "progress",
            ReadOutput::Progress(Progress {
                prepared: 1_048_576,
            }),
            to_json(&Progress {
                prepared: 1_048_576,
            }),
        ),
        message(
            "open_bundle",
            OPEN,
            "open",
            "fs.ReadRequest",
            &ReadRequest {
                hashes: vec![Algorithm::Crc32c],
                compression: Some(Compression::new(Codec::Zstd).level(3)),
                ..ReadRequest::bundle()
            },
        ),
        frame(
            "item",
            TO_HOST,
            "fs.ReadInput",
            ReadInput::Item(ReadItem::whole("/srv/a.txt"))
                .encode()
                .unwrap(),
            to_json(&ReadItem::whole("/srv/a.txt")),
        ),
        frame(
            "item_range",
            TO_HOST,
            "fs.ReadInput",
            ReadInput::Item(ReadItem {
                path: "/srv/big.bin".into(),
                offset: 1024,
                length: Some(4096),
            })
            .encode()
            .unwrap(),
            to_json(&ReadItem {
                path: "/srv/big.bin".into(),
                offset: 1024,
                length: Some(4096),
            }),
        ),
        frame(
            "items_end",
            TO_HOST,
            "fs.ReadInput",
            ReadInput::ItemsEnd.encode().unwrap(),
            Value::Null,
        ),
        output(
            "header_bundle",
            ReadOutput::Header(stream_header.clone()),
            to_json(&stream_header),
        ),
        output(
            "header_item",
            ReadOutput::Header(item_header.clone()),
            to_json(&item_header),
        ),
        output(
            "done_item",
            ReadOutput::Done(item_done.clone()),
            to_json(&item_done),
        ),
        output(
            "item_error",
            ReadOutput::ItemError(item_error.clone()),
            to_json(&item_error),
        ),
        output(
            "end_bundle",
            ReadOutput::End(BundleEnd { items: 4 }),
            to_json(&BundleEnd { items: 4 }),
        ),
        output(
            "done",
            ReadOutput::Done(ReadDone::default()),
            to_json(&ReadDone::default()),
        ),
        output(
            "done_hashed",
            ReadOutput::Done(read_done.clone()),
            to_json(&read_done),
        ),
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
    let sha256 = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let done = WriteDone {
        written: 5,
        hashes: Digests::from([
            (Algorithm::Md5, "5d41402abc4b2a76b9719d911017c592".into()),
            (Algorithm::Sha256, sha256.into()),
        ]),
        index: None,
    };
    let item_done = WriteDone {
        index: Some(1),
        ..done.clone()
    };
    let item = WriteItem {
        path: "/srv/out/a.txt".into(),
        mode: WriteMode::Create,
        parents: true,
        file_mode: Some(0o600),
        modified: Some(Timestamp {
            secs: 1_700_000_000,
            nanos: 5,
        }),
        expect: Digests::from([(Algorithm::Sha256, sha256.into())]),
    };
    let item_error = ItemError::new(2, ErrorBody::new(ErrorCode::Exists, "exists"));
    let mut checked = WriteRequest::replace("/srv/out.txt");
    checked.parents = true;
    checked.file_mode = Some(0o600);
    checked.expect = Digests::from([(Algorithm::Sha256, sha256.into())]);
    checked.hashes = Some(vec![Algorithm::Sha256, Algorithm::Md5]);
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
            "open_replace_if_unchanged",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                base: Some(DeltaBase {
                    size: 120,
                    modified: Some(Timestamp {
                        secs: 1_700_000_000,
                        nanos: 5,
                    }),
                    hashes: Digests::from([(Algorithm::Sha256, sha256.into())]),
                }),
                ..WriteRequest::replace("/srv/out.txt")
            },
        ),
        message(
            "open_compressed",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                compression: Some(Compression::new(Codec::Zstd).level(9)),
                ..WriteRequest::replace("/srv/out.log")
            },
        ),
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
        message(
            "open_delta",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                mode: WriteMode::Delta {
                    block_size: 8192,
                    base: DeltaBase {
                        size: 64_000_000,
                        modified: Some(Timestamp {
                            secs: 1_700_000_000,
                            nanos: 5,
                        }),
                        hashes: Digests::new(),
                    },
                },
                expect: Digests::from([(Algorithm::Sha256, sha256.into())]),
                ..WriteRequest::replace("/srv/big.bin")
            },
        ),
        output("ready", WriteOutput::ready(), Value::Null),
        output(
            "ready_token",
            WriteOutput::Ready(WriteReady {
                token: Some("0123456789abcdef0123456789abcdef".into()),
                offset: None,
            }),
            to_json(&WriteReady {
                token: Some("0123456789abcdef0123456789abcdef".into()),
                offset: None,
            }),
        ),
        output(
            "ready_offset",
            WriteOutput::Ready(WriteReady {
                token: None,
                offset: Some(1_048_576),
            }),
            to_json(&WriteReady {
                token: None,
                offset: Some(1_048_576),
            }),
        ),
        output(
            "flushed",
            WriteOutput::Flushed(Flushed { written: 65_536 }),
            to_json(&Flushed { written: 65_536 }),
        ),
        output(
            "item_done",
            WriteOutput::Done(item_done.clone()),
            to_json(&item_done),
        ),
        output(
            "item_error",
            WriteOutput::ItemError(item_error.clone()),
            to_json(&item_error),
        ),
        output(
            "end_bundle",
            WriteOutput::End(BundleEnd { items: 3 }),
            to_json(&BundleEnd { items: 3 }),
        ),
        message(
            "open_resumable",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                resume: true,
                ..WriteRequest::replace("/srv/big.iso")
            },
        ),
        message(
            "open_resume",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                mode: WriteMode::Resume {
                    token: "0123456789abcdef0123456789abcdef".into(),
                },
                ..WriteRequest::replace("/srv/big.iso")
            },
        ),
        message(
            "open_bundle",
            OPEN,
            "open",
            "fs.WriteRequest",
            &WriteRequest {
                hashes: Some(vec![Algorithm::Crc32c]),
                compression: Some(Compression::new(Codec::Zstd).level(3)),
                ..WriteRequest::bundle()
            },
        ),
        input("item", WriteInput::Item(item.clone()), to_json(&item)),
        input("items_end", WriteInput::ItemsEnd, Value::Null),
        input("flush", WriteInput::Flush, Value::Null),
        output("done", WriteOutput::Done(done.clone()), to_json(&done)),
        output(
            "error_exists",
            WriteOutput::Error(ErrorBody::new(ErrorCode::Exists, "exists")),
            to_json(&ErrorBody::new(ErrorCode::Exists, "exists")),
        ),
        input("data", WriteInput::Data(b"hello".to_vec()), raw(b"hello")),
        input(
            "compressed",
            WriteInput::Compressed(unhex(ZSTD_PIECE)),
            raw(&unhex(ZSTD_PIECE)),
        ),
        input(
            "copy",
            WriteInput::Copy(DeltaCopy {
                block: 3,
                count: 40,
            }),
            to_json(&DeltaCopy {
                block: 3,
                count: 40,
            }),
        ),
        input("end", WriteInput::End, Value::Null),
    ]
}

fn walk_cases() -> Vec<Case> {
    let output = |name: &str, value: WalkOutput, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "walk.Output",
            value.encode().unwrap(),
            body,
        )
    };
    let input = |name: &str, value: WalkInput, body: Value| {
        frame(name, TO_HOST, "walk.Input", value.encode().unwrap(), body)
    };
    let root_entry = WalkEntry {
        path: String::new(),
        metadata: Metadata {
            kind: FileKind::Directory,
            size: 4096,
            mode: Some(0o755),
            ..sample_metadata()
        },
        target: None,
        hashes: None,
    };
    let file_entry = WalkEntry {
        path: "docs/a.txt".into(),
        metadata: sample_metadata(),
        target: None,
        hashes: Some(Digests::from([(Algorithm::Crc32c, "e3069283".into())])),
    };
    let link_entry = WalkEntry {
        path: "docs/latest".into(),
        metadata: Metadata {
            kind: FileKind::Symlink,
            size: 5,
            ..sample_metadata()
        },
        target: Some("a.txt".into()),
        hashes: None,
    };
    let entries = WalkEntries {
        root: 0,
        entries: vec![root_entry, file_entry, link_entry],
    };
    let error = WalkItemError {
        root: 1,
        path: Some("docs/private".into()),
        code: ErrorCode::Denied,
        message: "the operating system refused access".into(),
    };
    let header = WalkHeader {
        compression: Some(Compression::new(Codec::Zstd).level(3)),
    };
    let done = WalkDone {
        entries: 3,
        skipped: 1,
    };
    vec![
        message(
            "open_whole",
            OPEN,
            "open",
            "walk.Request",
            &WalkRequest::default(),
        ),
        message(
            "open_filtered",
            OPEN,
            "open",
            "walk.Request",
            &WalkRequest {
                depth: Some(2),
                follow: true,
                include: vec!["**/*.rs".into()],
                exclude: vec!["target".into(), "*.tmp".into()],
                hashes: vec![Algorithm::Xxh3],
                compression: Some(Compression::new(Codec::Zstd).level(3)),
            },
        ),
        message(
            "open_stat_many",
            OPEN,
            "open",
            "walk.Request",
            &WalkRequest {
                depth: Some(0),
                ..WalkRequest::default()
            },
        ),
        input(
            "root",
            WalkInput::Root(WalkRoot {
                path: "/srv/docs".into(),
            }),
            to_json(&WalkRoot {
                path: "/srv/docs".into(),
            }),
        ),
        input("end", WalkInput::End, Value::Null),
        output(
            "header",
            WalkOutput::Header(WalkHeader::default()),
            to_json(&WalkHeader::default()),
        ),
        output(
            "header_compressed",
            WalkOutput::Header(header.clone()),
            to_json(&header),
        ),
        output(
            "entries",
            WalkOutput::Entries(entries.clone()),
            to_json(&entries),
        ),
        output(
            "compressed",
            WalkOutput::Compressed(unhex(ZSTD_PIECE)),
            raw(&unhex(ZSTD_PIECE)),
        ),
        output(
            "item_error",
            WalkOutput::ItemError(error.clone()),
            to_json(&error),
        ),
        output("done", WalkOutput::Done(done), to_json(&done)),
        output(
            "error_busy",
            WalkOutput::Error(ErrorBody::new(ErrorCode::Busy, "too many walks")),
            to_json(&ErrorBody::new(ErrorCode::Busy, "too many walks")),
        ),
    ]
}

fn search_cases() -> Vec<Case> {
    let output = |name: &str, value: SearchOutput, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "search.Output",
            value.encode().unwrap(),
            body,
        )
    };
    let file = SearchFile {
        root: 0,
        path: "src/main.rs".into(),
        size: 2048,
    };
    let plain = SearchMatch {
        line: 12,
        text: "fn main() {".into(),
        before: Vec::new(),
        after: Vec::new(),
        truncated: false,
    };
    let context = SearchMatch {
        line: 40,
        text: "    run();".into(),
        before: vec!["    setup();".into()],
        after: vec!["}".into(), String::new()],
        truncated: true,
    };
    let error = SearchItemError {
        root: 0,
        path: Some("src/private.rs".into()),
        code: ErrorCode::Denied,
        message: "the operating system refused access".into(),
    };
    let done = SearchDone {
        files: 120,
        matches: 2,
        skipped: Skipped {
            binary: 3,
            large: 1,
            unreadable: 1,
        },
        truncated: false,
    };
    let refused = ErrorBody::new(ErrorCode::BadRequest, "the pattern is empty");
    vec![
        message(
            "open_literal",
            OPEN,
            "open",
            "search.Request",
            &SearchRequest {
                walk: SearchScope {
                    roots: vec!["/srv/docs".into()],
                    ..SearchScope::default()
                },
                pattern: "TODO".into(),
                ..SearchRequest::default()
            },
        ),
        message(
            "open_filtered",
            OPEN,
            "open",
            "search.Request",
            &SearchRequest {
                walk: SearchScope {
                    roots: vec!["/srv/docs".into(), "/var/log".into()],
                    depth: Some(3),
                    include: vec!["**/*.rs".into()],
                    exclude: vec!["target".into()],
                },
                hidden: true,
                pattern: r"fn\s+main".into(),
                literal: false,
                case_insensitive: true,
                context: 2,
                max_matches: Some(100),
                max_files: Some(5000),
            },
        ),
        output("file", SearchOutput::File(file.clone()), to_json(&file)),
        output("match", SearchOutput::Match(plain.clone()), to_json(&plain)),
        output(
            "match_context",
            SearchOutput::Match(context.clone()),
            to_json(&context),
        ),
        output(
            "item_error",
            SearchOutput::ItemError(error.clone()),
            to_json(&error),
        ),
        output("done", SearchOutput::Done(done), to_json(&done)),
        output(
            "error_bad_request",
            SearchOutput::Error(refused.clone()),
            to_json(&refused),
        ),
    ]
}

fn hash_cases() -> Vec<Case> {
    let output = |name: &str, value: HashOutput, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "hash.Output",
            value.encode().unwrap(),
            body,
        )
    };
    let input = |name: &str, value: HashInput, body: Value| {
        frame(name, TO_HOST, "hash.Input", value.encode().unwrap(), body)
    };
    let result = HashResult {
        index: 0,
        size: 3,
        modified: Some(Timestamp {
            secs: 1_700_000_000,
            nanos: 5,
        }),
        length: 3,
        unchanged: false,
        hashes: Digests::from([
            (Algorithm::Md5, "900150983cd24fb0d6963f7d28e17f72".into()),
            (Algorithm::Crc32c, "364b3fb7".into()),
        ]),
        block_size: None,
        strong_len: None,
    };
    let blocked = HashResult {
        block_size: Some(2048),
        strong_len: Some(16),
        ..result.clone()
    };
    let unchanged = HashResult {
        index: 1,
        size: 3,
        modified: result.modified,
        length: 0,
        unchanged: true,
        hashes: Digests::new(),
        block_size: None,
        strong_len: None,
    };
    let blocks = HashBlocks {
        index: 2,
        first: 0,
        size: 2048,
        weak: vec![0x0001_0002, 7],
        strong: vec![0xde, 0xad, 0xbe, 0xef],
    };
    let failed = ItemError::new(
        3,
        ErrorBody::new(ErrorCode::NotFound, "the path does not exist"),
    );
    let item = HashItem {
        path: "/srv/big.bin".into(),
        offset: 1024,
        length: Some(4096),
        if_size: Some(9_000),
        if_modified: Some(Timestamp {
            secs: 1_700_000_000,
            nanos: 5,
        }),
    };
    vec![
        message(
            "open_one",
            OPEN,
            "open",
            "hash.Request",
            &HashRequest::new([Algorithm::Sha256]),
        ),
        message(
            "open_several_with_blocks",
            OPEN,
            "open",
            "hash.Request",
            &HashRequest {
                algorithms: vec![Algorithm::Md5, Algorithm::Blake3, Algorithm::Xxh128],
                blocks: Some(BlockOptions {
                    size: Some(2048),
                    weak: Some("rsync".into()),
                    strong_len: Some(16),
                }),
            },
        ),
        input("item", HashInput::Item(item.clone()), to_json(&item)),
        input(
            "item_whole",
            HashInput::Item(HashItem::whole("/etc/hostname")),
            to_json(&HashItem::whole("/etc/hostname")),
        ),
        input("end", HashInput::End, Value::Null),
        output(
            "result",
            HashOutput::Result(result.clone()),
            to_json(&result),
        ),
        output(
            "result_with_blocks",
            HashOutput::Result(blocked.clone()),
            to_json(&blocked),
        ),
        output(
            "result_unchanged",
            HashOutput::Result(unchanged.clone()),
            to_json(&unchanged),
        ),
        output(
            "blocks",
            HashOutput::Blocks(blocks.clone()),
            to_json(&blocks),
        ),
        output(
            "item_error",
            HashOutput::ItemError(failed.clone()),
            to_json(&failed),
        ),
        output(
            "done",
            HashOutput::Done(HashDone { items: 4 }),
            to_json(&HashDone { items: 4 }),
        ),
        output(
            "error_busy",
            HashOutput::Error(ErrorBody::new(ErrorCode::Busy, "too many hash streams")),
            to_json(&ErrorBody::new(ErrorCode::Busy, "too many hash streams")),
        ),
        Case {
            name: "unknown_tag".into(),
            direction: TO_CLIENT,
            kind: "frame",
            ty: "hash.Output",
            bytes: vec![0x7f],
            value: None,
            decode_only: true,
            error: Some("tag"),
        },
    ]
}

fn forward_cases() -> Vec<Case> {
    let answer = |name: &str, value: Answer, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "forward.Answer",
            value.encode().unwrap(),
            body,
        )
    };
    let connected = Connected {
        address: Some("192.0.2.7:5432".into()),
    };
    let refused = ErrorBody::new(ErrorCode::Refused, "the target refused the connection");
    vec![
        message(
            "open_tcp",
            OPEN,
            "open",
            "forward.ConnectRequest",
            &ConnectRequest::new(Target::Tcp {
                host: "db.lan".into(),
                port: 5432,
            }),
        ),
        message(
            "open_tcp_ipv6",
            OPEN,
            "open",
            "forward.ConnectRequest",
            &ConnectRequest::new(Target::Tcp {
                host: "fd00::5".into(),
                port: 22,
            }),
        ),
        message(
            "open_unix",
            OPEN,
            "open",
            "forward.ConnectRequest",
            &ConnectRequest::new(Target::Unix {
                path: "/run/docker.sock".into(),
            }),
        ),
        message(
            "open_pipe",
            OPEN,
            "open",
            "forward.ConnectRequest",
            &ConnectRequest::new(Target::Pipe {
                name: "docker_engine".into(),
            }),
        ),
        Case {
            name: "open_unknown_kind".into(),
            direction: OPEN,
            kind: "open",
            ty: "forward.ConnectRequest",
            bytes: map(vec![(
                "target",
                Cbor::Map(vec![
                    (text("type"), text("serial")),
                    (text("port"), text("COM3")),
                ]),
            )]),
            value: None,
            decode_only: true,
            error: Some("decode"),
        },
        answer(
            "connected",
            Answer::Connected(connected.clone()),
            to_json(&connected),
        ),
        answer(
            "connected_without_address",
            Answer::Connected(Connected::default()),
            to_json(&Connected::default()),
        ),
        answer(
            "error_refused",
            Answer::Error(refused.clone()),
            to_json(&refused),
        ),
        Case {
            name: "unknown_tag".into(),
            direction: TO_CLIENT,
            kind: "frame",
            ty: "forward.Answer",
            bytes: vec![0x7f],
            value: None,
            decode_only: true,
            error: Some("tag"),
        },
    ]
}

fn gui_cases() -> Vec<Case> {
    let output = |name: &str, value: GuiOutput, body: Value| {
        frame(name, TO_CLIENT, "gui.Output", value.encode().unwrap(), body)
    };
    let input = |name: &str, value: GuiInput, body: Value| {
        frame(name, TO_HOST, "gui.Input", value.encode().unwrap(), body)
    };
    let ready = Ready {
        session: "default".into(),
        granted: vec![Want::View, Want::Input],
        state: sample_state(),
    };
    let header = ImageHeader {
        seq: 1,
        display: DisplayId::from("0"),
        format: ImageFormat::Png,
        width: 960,
        height: 540,
        region: Rect::new(0, 0, 1920, 1080),
        scale: 0.5,
        cursor: Some(Point { x: 10, y: 20 }),
        length: 4,
        settled: Some(Settled {
            settled: true,
            waited_ms: 350,
        }),
    };
    let snapshot = Snapshot {
        seq: 1,
        display: Some(DisplayId::from("0")),
        region: Some(Rect::new(0, 0, 640, 480)),
        format: Some(ImageFormat::Jpeg),
        quality: Some(80),
        max_width: Some(1280),
        max_height: Some(800),
        cursor: true,
        settle: Some(SettleOptions {
            quiet_ms: Some(300),
            timeout_ms: None,
        }),
    };
    let events = Input {
        seq: 2,
        events: vec![
            InputEvent::Move {
                display: None,
                x: 100,
                y: 200,
            },
            InputEvent::Button {
                button: Button::Left,
                action: ButtonAction::Click,
                count: Some(2),
                display: Some(DisplayId::from("0")),
                x: Some(5),
                y: Some(6),
            },
            InputEvent::Scroll {
                display: None,
                x: None,
                y: None,
                dx: 0,
                dy: -3,
            },
            InputEvent::Key {
                key: "page_down".into(),
                action: KeyAction::Press,
            },
            InputEvent::Text {
                text: "h\u{e9}llo".into(),
            },
            InputEvent::Wait { ms: 250 },
        ],
    };
    let settle = Settle {
        seq: 3,
        display: None,
        region: None,
        quiet_ms: Some(300),
        timeout_ms: Some(5000),
    };
    let denied = RequestError::new(
        5,
        ErrorBody::new(ErrorCode::Denied, "this stream was not granted input"),
    );
    let pending = ApproveOutput::Pending {
        requests: vec![PendingRequest {
            id: 7,
            peer: "sha256:00".into(),
            session: "default".into(),
            want: vec![Want::View],
            since: 1_700_000_000,
        }],
    };
    let settled = SettledFrame {
        seq: 3,
        settled: false,
        waited_ms: 5000,
    };
    let clipboard = ClipboardText {
        seq: 4,
        text: "copied".into(),
    };
    let unavailable = ErrorBody::new(ErrorCode::Unavailable, "the desktop is not at the screen");
    let watch = Watch {
        seq: 10,
        display: Some(DisplayId::from("0")),
        region: Some(Rect::new(0, 0, 640, 480)),
        fps: Some(10),
        encoding: Some(WatchEncoding::Auto),
        quality: Some(85),
        max_width: Some(1280),
        max_height: Some(800),
        max_rate: Some(500_000),
        cursor: true,
        refine: Some(true),
        key: true,
    };
    let update = UpdateHeader {
        frame: 3,
        display: DisplayId::from("0"),
        width: 1280,
        height: 720,
        scale: 0.6666666666666666,
        key: false,
        rects: vec![
            UpdateRect {
                x: 64,
                y: 128,
                width: 128,
                height: 64,
                encoding: RectEncoding::ZstdRgb,
                length: 3,
            },
            UpdateRect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
                encoding: RectEncoding::Jpeg,
                length: 1,
            },
        ],
        cursor: Some(Point { x: 10, y: 20 }),
        level: 2,
        ack_ms: Some(180),
    };
    let cursor = CursorFrame {
        display: DisplayId::from("0"),
        x: 31,
        y: 42,
        visible: true,
    };
    vec![
        message(
            "open_view_and_input",
            OPEN,
            "open",
            "gui.Request",
            &GuiRequest::new([Want::View, Want::Input]),
        ),
        message(
            "open_default",
            OPEN,
            "open",
            "gui.Request",
            &GuiRequest::default(),
        ),
        output("ready", GuiOutput::Ready(ready.clone()), to_json(&ready)),
        output("image", GuiOutput::Image(header.clone()), to_json(&header)),
        output(
            "data",
            GuiOutput::Data(vec![0x89, 0x50, 0x4e, 0x47]),
            raw(&[0x89, 0x50, 0x4e, 0x47]),
        ),
        output(
            "input_done",
            GuiOutput::InputDone(InputDone { seq: 2, applied: 6 }),
            to_json(&InputDone { seq: 2, applied: 6 }),
        ),
        output("settled", GuiOutput::Settled(settled), to_json(&settled)),
        output(
            "clipboard",
            GuiOutput::Clipboard(clipboard.clone()),
            to_json(&clipboard),
        ),
        output(
            "state",
            GuiOutput::State(sample_state()),
            to_json(&sample_state()),
        ),
        output(
            "update",
            GuiOutput::Update(update.clone()),
            to_json(&update),
        ),
        output(
            "cursor",
            GuiOutput::Cursor(cursor.clone()),
            to_json(&cursor),
        ),
        output(
            "request_error_denied",
            GuiOutput::RequestError(denied.clone()),
            to_json(&denied),
        ),
        output(
            "error_unavailable",
            GuiOutput::Error(unavailable.clone()),
            to_json(&unavailable),
        ),
        input(
            "snapshot",
            GuiInput::Snapshot(snapshot.clone()),
            to_json(&snapshot),
        ),
        input(
            "snapshot_defaults",
            GuiInput::Snapshot(Snapshot::default()),
            to_json(&Snapshot::default()),
        ),
        input("watch", GuiInput::Watch(watch.clone()), to_json(&watch)),
        input(
            "watch_defaults",
            GuiInput::Watch(Watch::default()),
            to_json(&Watch::default()),
        ),
        input(
            "unwatch",
            GuiInput::Unwatch(Unwatch { seq: 11 }),
            to_json(&Unwatch { seq: 11 }),
        ),
        input(
            "ack",
            GuiInput::Ack(Ack { frame: 3 }),
            to_json(&Ack { frame: 3 }),
        ),
        input("input", GuiInput::Input(events.clone()), to_json(&events)),
        input("settle", GuiInput::Settle(settle.clone()), to_json(&settle)),
        input(
            "clipboard_get",
            GuiInput::ClipboardGet(ClipboardGet { seq: 8 }),
            to_json(&ClipboardGet { seq: 8 }),
        ),
        input(
            "clipboard_set",
            GuiInput::ClipboardSet(ClipboardSet {
                seq: 9,
                text: "set".into(),
            }),
            to_json(&ClipboardSet {
                seq: 9,
                text: "set".into(),
            }),
        ),
        message(
            "approve_pending",
            TO_CLIENT,
            "message",
            "gui.ApproveOutput",
            &pending,
        ),
        message(
            "approve_decided",
            TO_CLIENT,
            "message",
            "gui.ApproveOutput",
            &ApproveOutput::Decided { id: 7, found: true },
        ),
        message(
            "approve_decide",
            TO_HOST,
            "message",
            "gui.ApproveInput",
            &ApproveInput::Decide { id: 7, allow: true },
        ),
        Case {
            name: "unknown_tag".into(),
            direction: TO_CLIENT,
            kind: "frame",
            ty: "gui.Output",
            bytes: vec![0x7f],
            value: None,
            decode_only: true,
            error: Some("tag"),
        },
    ]
}

fn listen_cases() -> Vec<Case> {
    let listen_frame = |name: &str, value: ListenFrame, body: Value| {
        frame(
            name,
            TO_CLIENT,
            "forward.ListenFrame",
            value.encode().unwrap(),
            body,
        )
    };
    let listening = Listening {
        address: "127.0.0.1:40123".into(),
        token: "00112233445566778899aabbccddeeff".into(),
    };
    let arrival = Arrival {
        id: 3,
        source: Some("192.0.2.40:51234".into()),
    };
    let no_source = Arrival {
        id: 4,
        source: None,
    };
    let in_use = ErrorBody::new(ErrorCode::InUse, "that address is already in use");
    let open = |name: &str, request: ListenRequest| {
        message(name, OPEN, "open", "forward.ListenRequest", &request)
    };
    vec![
        open(
            "open_tcp_loopback_any_port",
            ListenRequest::new(Bind::Tcp {
                host: "127.0.0.1".into(),
                port: 0,
            }),
        ),
        open(
            "open_tcp_with_backlog",
            ListenRequest {
                bind: Bind::Tcp {
                    host: "::1".into(),
                    port: 8080,
                },
                backlog: Some(4),
            },
        ),
        open(
            "open_unix",
            ListenRequest::new(Bind::Unix {
                path: "/run/user/1000/app.sock".into(),
            }),
        ),
        open(
            "open_pipe",
            ListenRequest::new(Bind::Pipe { name: "app".into() }),
        ),
        listen_frame(
            "listening",
            ListenFrame::Listening(listening.clone()),
            to_json(&listening),
        ),
        listen_frame(
            "arrival",
            ListenFrame::Arrival(arrival.clone()),
            to_json(&arrival),
        ),
        listen_frame(
            "arrival_without_source",
            ListenFrame::Arrival(no_source.clone()),
            to_json(&no_source),
        ),
        listen_frame(
            "error_in_use",
            ListenFrame::Error(in_use.clone()),
            to_json(&in_use),
        ),
        Case {
            name: "unknown_tag".into(),
            direction: TO_CLIENT,
            kind: "frame",
            ty: "forward.ListenFrame",
            bytes: vec![0x7f],
            value: None,
            decode_only: true,
            error: Some("tag"),
        },
    ]
}

fn accept_cases() -> Vec<Case> {
    let request = AcceptRequest {
        token: "00112233445566778899aabbccddeeff".into(),
        id: 3,
    };
    let connected = Connected {
        address: Some("192.0.2.40:51234".into()),
    };
    let not_found = ErrorBody::new(ErrorCode::NotFound, "no such arrival");
    vec![
        message("open", OPEN, "open", "forward.AcceptRequest", &request),
        frame(
            "connected",
            TO_CLIENT,
            "forward.Answer",
            Answer::Connected(connected.clone()).encode().unwrap(),
            to_json(&connected),
        ),
        frame(
            "error_not_found",
            TO_CLIENT,
            "forward.Answer",
            Answer::Error(not_found.clone()).encode().unwrap(),
            to_json(&not_found),
        ),
    ]
}

fn files() -> Vec<(&'static str, &'static str, Vec<Case>)> {
    vec![
        ("control.json", "bohio/1", control()),
        ("exec.json", "bohio-exec/1", exec_cases()),
        ("shell.json", "bohio-shell/1", shell_cases()),
        ("read.json", "bohio-read/1", read_cases()),
        ("write.json", "bohio-write/1", write_cases()),
        ("hash.json", "bohio-hash/1", hash_cases()),
        ("walk.json", "bohio-walk/1", walk_cases()),
        ("search.json", "bohio-search/1", search_cases()),
        ("forward.json", "bohio-connect/1", forward_cases()),
        ("gui.json", "bohio-gui/1", gui_cases()),
        ("listen.json", "bohio-listen/1", listen_cases()),
        ("accept.json", "bohio-accept/1", accept_cases()),
        ("attach.json", "bohio-attach/1", attach_cases()),
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
        "fs.ReadInput" => ReadInput::decode(bytes).map(|_| ()),
        "fs.WriteRequest" => WriteRequest::from_metadata(bytes).map(|_| ()),
        "fs.WriteOutput" => WriteOutput::decode(bytes).map(|_| ()),
        "fs.WriteInput" => WriteInput::decode(bytes).map(|_| ()),
        "hash.Request" => HashRequest::from_metadata(bytes).map(|_| ()),
        "hash.Output" => HashOutput::decode(bytes).map(|_| ()),
        "hash.Input" => HashInput::decode(bytes).map(|_| ()),
        "walk.Request" => WalkRequest::from_metadata(bytes).map(|_| ()),
        "walk.Output" => WalkOutput::decode(bytes).map(|_| ()),
        "walk.Input" => WalkInput::decode(bytes).map(|_| ()),
        "search.Request" => SearchRequest::from_metadata(bytes).map(|_| ()),
        "search.Output" => SearchOutput::decode(bytes).map(|_| ()),
        "session.Attach" => Attach::from_metadata(bytes).map(|_| ()),
        "session.Attached" => Attached::decode(bytes).map(|_| ()),
        "forward.ConnectRequest" => ConnectRequest::from_metadata(bytes).map(|_| ()),
        "forward.Answer" => Answer::decode(bytes).map(|_| ()),
        "forward.ListenRequest" => ListenRequest::from_metadata(bytes).map(|_| ()),
        "forward.ListenFrame" => ListenFrame::decode(bytes).map(|_| ()),
        "forward.AcceptRequest" => AcceptRequest::from_metadata(bytes).map(|_| ()),
        "gui.Request" => GuiRequest::from_metadata(bytes).map(|_| ()),
        "gui.Output" => GuiOutput::decode(bytes).map(|_| ()),
        "gui.Input" => GuiInput::decode(bytes).map(|_| ()),
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
        "hash.Request" => check_message::<HashRequest>(entry, &bytes),
        "exec.Output" => check_frame(entry, &bytes, exec::Output::decode, |v| v.encode().unwrap()),
        "exec.Input" => check_frame(entry, &bytes, exec::Input::decode, exec::Input::encode),
        "shell.Output" => check_frame(entry, &bytes, shell::Output::decode, |v| {
            v.encode().unwrap()
        }),
        "shell.Input" => check_frame(entry, &bytes, shell::Input::decode, |v| v.encode().unwrap()),
        "fs.ReadOutput" => check_frame(entry, &bytes, ReadOutput::decode, |v| v.encode().unwrap()),
        "fs.ReadInput" => check_frame(entry, &bytes, ReadInput::decode, |v| v.encode().unwrap()),
        "fs.WriteOutput" => {
            check_frame(entry, &bytes, WriteOutput::decode, |v| v.encode().unwrap())
        }
        "fs.WriteInput" => check_frame(entry, &bytes, WriteInput::decode, WriteInput::encode),
        "hash.Output" => check_frame(entry, &bytes, HashOutput::decode, |v| v.encode().unwrap()),
        "hash.Input" => check_frame(entry, &bytes, HashInput::decode, |v| v.encode().unwrap()),
        "walk.Request" => check_message::<WalkRequest>(entry, &bytes),
        "walk.Output" => check_frame(entry, &bytes, WalkOutput::decode, |v| v.encode().unwrap()),
        "walk.Input" => check_frame(entry, &bytes, WalkInput::decode, |v| v.encode().unwrap()),
        "search.Request" => check_message::<SearchRequest>(entry, &bytes),
        "search.Output" => {
            check_frame(entry, &bytes, SearchOutput::decode, |v| v.encode().unwrap())
        }
        "session.Attach" => check_message::<Attach>(entry, &bytes),
        "session.Attached" => check_frame(entry, &bytes, Attached::decode, |v| v.encode().unwrap()),
        "forward.ConnectRequest" => check_message::<ConnectRequest>(entry, &bytes),
        "forward.Answer" => check_frame(entry, &bytes, Answer::decode, |v| v.encode().unwrap()),
        "forward.ListenRequest" => check_message::<ListenRequest>(entry, &bytes),
        "forward.ListenFrame" => {
            check_frame(entry, &bytes, ListenFrame::decode, |v| v.encode().unwrap())
        }
        "forward.AcceptRequest" => check_message::<AcceptRequest>(entry, &bytes),
        "gui.Request" => check_message::<GuiRequest>(entry, &bytes),
        "gui.ApproveOutput" => check_message::<ApproveOutput>(entry, &bytes),
        "gui.ApproveInput" => check_message::<ApproveInput>(entry, &bytes),
        "gui.Output" => check_frame(entry, &bytes, GuiOutput::decode, |v| v.encode().unwrap()),
        "gui.Input" => check_frame(entry, &bytes, GuiInput::decode, |v| v.encode().unwrap()),
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
