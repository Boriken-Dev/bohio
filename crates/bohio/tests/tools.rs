//! `bohio_client::tools` against a real node: every tool a peer's grant allows, and the refusal of each it does not.

#![forbid(unsafe_code)]

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bohio::config::{ExecutionConfig, NodeConfig};
use bohio::policy::{Grant, Permission};
use bohio_client::tools::{
    Availability, Cancel, REGISTRY, Settings, ToolError, Tools, base64_decode, base64_encode,
};
use bohio_client::{Error, Open};
use bohio_proto::protocol;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::embedded::{Deadline, Running};
use support::helper;
use tempfile::TempDir;

fn grant(root: &Path) -> Grant {
    Grant::allowing([
        Permission::Info,
        Permission::Exec,
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Session,
    ])
    .with_programs([helper()])
    .with_fs_roots([root.to_path_buf()])
}

struct World {
    node: Running,
    dir: TempDir,
}

impl World {
    fn new() -> Self {
        Self {
            node: Running::start(),
            dir: tempfile::tempdir().expect("a directory"),
        }
    }

    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn tools(&self, grant: Grant) -> Tools {
        self.tools_with(grant, Settings::default())
    }

    fn tools_with(&self, grant: Grant, settings: Settings) -> Tools {
        let (connection, _) = self.node.link(grant);
        Tools::with_settings(Arc::new(connection), settings).expect("the tools are made")
    }
}

fn call(tools: &Tools, name: &str, arguments: Value) -> Result<Value, ToolError> {
    tools
        .call(name, arguments, &Cancel::new())
        .map(|output| output.value)
}

fn ok(tools: &Tools, name: &str, arguments: Value) -> Value {
    call(tools, name, arguments).unwrap_or_else(|error| panic!("{name} failed: {error}"))
}

fn failed(tools: &Tools, name: &str, arguments: Value) -> ToolError {
    call(tools, name, arguments).expect_err("the call fails")
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn text_of(stream: &Value) -> String {
    assert_eq!(stream["encoding"], "utf8", "{stream}");
    stream["data"].as_str().unwrap().to_owned()
}

#[test]
fn every_tool_has_a_schema_that_parses_and_a_unique_name() {
    let mut names = std::collections::BTreeSet::new();
    for spec in REGISTRY {
        assert!(names.insert(spec.name), "{} twice", spec.name);
        let schema = (spec.schema)();
        assert_eq!(schema["type"], "object", "{}", spec.name);
        assert_eq!(schema["additionalProperties"], false, "{}", spec.name);
        let properties = schema["properties"].as_object().expect("properties");
        for required in schema["required"].as_array().expect("required") {
            assert!(
                properties.contains_key(required.as_str().unwrap()),
                "{} requires a property it lacks",
                spec.name
            );
        }
        let again = serde_json::to_string(&schema).unwrap();
        serde_json::from_str::<Value>(&again).expect("it is JSON");
    }
}

#[test]
fn exec_runs_a_program_and_bounds_what_it_returns() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let echoed = ok(
        &tools,
        "exec",
        json!({"argv": [helper(), "echo", "hello", "agent"]}),
    );
    assert_eq!(echoed["exit"]["code"], 0);
    assert_eq!(text_of(&echoed["stdout"]).trim(), "hello agent");
    assert_eq!(echoed["stdout"]["truncated"], false);

    let both = ok(&tools, "exec", json!({"argv": [helper(), "both"]}));
    assert_eq!(both["exit"]["code"], 3);
    assert_eq!(text_of(&both["stdout"]), "out");
    assert_eq!(text_of(&both["stderr"]), "err");

    let flood = ok(
        &tools,
        "exec",
        json!({"argv": [helper(), "flood", "100000"], "max_output": 1000}),
    );
    let stdout = &flood["stdout"];
    assert_eq!(stdout["truncated"], true, "{stdout}");
    assert_eq!(stdout["total_bytes"], 100_000);
    let shown = stdout["data"].as_str().unwrap().len() + stdout["tail"].as_str().unwrap().len();
    assert!(shown <= 1000, "{shown}");
    assert_eq!(
        stdout["omitted_bytes"].as_u64().unwrap(),
        100_000 - shown as u64
    );

    let piped = ok(
        &tools,
        "exec",
        json!({"argv": [helper(), "cat"], "stdin": "from the agent"}),
    );
    assert_eq!(text_of(&piped["stdout"]), "from the agent");

    let started = Instant::now();
    let slow = ok(
        &tools,
        "exec",
        json!({"argv": [helper(), "sleep", "20000"], "deadline_ms": 300}),
    );
    assert_eq!(slow["exit"]["timed_out"], true, "{slow}");
    assert!(started.elapsed() < Duration::from_secs(15));

    let both_given = failed(&tools, "exec", json!({"argv": [helper()], "script": "x"}));
    assert_eq!(both_given.code, "bad_request");
    assert_eq!(
        failed(&tools, "exec", json!({"nonsense": 1})).code,
        "bad_request"
    );
}

#[cfg(unix)]
#[test]
fn exec_sends_a_signal_after_a_time() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let done = ok(
        &tools,
        "exec",
        json!({"argv": [helper(), "trap"], "signal": {"signal": "interrupt", "after_ms": 700}}),
    );
    assert!(text_of(&done["stdout"]).contains("got-int"), "{done}");
    assert_eq!(done["exit"]["code"], 3);
    assert_eq!(done["signals"][0]["delivered"], true);
}

#[test]
fn exec_on_a_terminal_merges_the_streams() {
    let _deadline = Deadline::start();
    let world = World::new();
    let with_shell = Grant::allowing([Permission::Info, Permission::Exec, Permission::Shell])
        .with_programs([helper()]);
    let tools = world.tools(with_shell);
    let done = ok(
        &tools,
        "exec",
        json!({"argv": [helper(), "echo", "on a terminal"], "pty": true}),
    );
    assert!(text_of(&done["output"]).contains("on a terminal"), "{done}");
    assert_eq!(done["exit"]["code"], 0);
}

#[test]
fn a_text_read_pages_by_lines_and_by_bytes_and_refuses_binary() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let content: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    std::fs::write(world.path("ten.txt"), &content).unwrap();

    let page = ok(
        &tools,
        "read",
        json!({"path": world.path("ten.txt"), "offset_line": 4, "limit_lines": 3}),
    );
    assert_eq!(page["text"], "line 4\nline 5\nline 6\n");
    assert_eq!(page["more"], true);
    assert_eq!(page["next_offset_line"], 7);
    assert_eq!(page["total_lines"], Value::Null);
    assert_eq!(
        page["sha256"],
        Value::Null,
        "a part has no digest of the whole"
    );

    let whole = ok(&tools, "read", json!({"path": world.path("ten.txt")}));
    assert_eq!(whole["text"], content.as_str());
    assert_eq!(whole["total_lines"], 10);
    assert_eq!(whole["more"], false);
    assert_eq!(whole["sha256"], hex(content.as_bytes()));

    let bytes = ok(
        &tools,
        "read",
        json!({"path": world.path("ten.txt"), "max_bytes": 15}),
    );
    assert_eq!(bytes["text"], "line 1\nline 2\n");
    assert_eq!(bytes["truncated_by_bytes"], true);
    assert_eq!(bytes["next_offset_line"], 3);

    std::fs::write(world.path("long.txt"), "x".repeat(100)).unwrap();
    let cut = ok(
        &tools,
        "read",
        json!({"path": world.path("long.txt"), "max_bytes": 10}),
    );
    assert_eq!(cut["text"], "x".repeat(10));
    assert_eq!(
        cut["line_cut"], true,
        "a line longer than the bound says it was cut"
    );
    assert_eq!(cut["next_offset_line"], 2);

    let beyond = ok(
        &tools,
        "read",
        json!({"path": world.path("ten.txt"), "offset_line": 50}),
    );
    assert_eq!(beyond["text"], "");
    assert_eq!(beyond["total_lines"], 10);

    let raw: Vec<u8> = (0..=255).collect();
    std::fs::write(world.path("raw.bin"), &raw).unwrap();
    let refused = failed(&tools, "read", json!({"path": world.path("raw.bin")}));
    assert_eq!(refused.code, "not_text");
    assert_eq!(refused.detail.as_ref().unwrap()["size"], 256);
    let binary = ok(
        &tools,
        "read",
        json!({"path": world.path("raw.bin"), "encoding": "base64", "offset": 250, "length": 100}),
    );
    assert_eq!(
        base64_decode(binary["data"].as_str().unwrap()).unwrap(),
        &raw[250..]
    );
    assert_eq!(binary["more"], false);
}

#[test]
fn write_and_edit_change_files_and_say_what_went_wrong() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let path = world.path("notes.txt");
    let written = ok(
        &tools,
        "write",
        json!({"path": path, "text": "one two two\n"}),
    );
    assert_eq!(written["written"], 12);
    assert_eq!(written["sha256"], hex(b"one two two\n"));
    assert_eq!(
        failed(
            &tools,
            "write",
            json!({"path": path, "text": "x", "create_only": true})
        )
        .code,
        "exists"
    );

    let missing = failed(
        &tools,
        "edit",
        json!({"path": path, "old": "three", "new": "3"}),
    );
    assert_eq!(missing.code, "no_match");
    let many = failed(
        &tools,
        "edit",
        json!({"path": path, "old": "two", "new": "2"}),
    );
    assert_eq!(many.code, "ambiguous");
    assert_eq!(many.detail.as_ref().unwrap()["matches"], 2);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one two two\n",
        "a refused edit changes nothing"
    );

    let one = ok(
        &tools,
        "edit",
        json!({"path": path, "old": "one", "new": "1"}),
    );
    assert_eq!(one["replacements"], 1);
    let all = ok(
        &tools,
        "edit",
        json!({"path": path, "old": "two", "new": "2", "replace_all": true}),
    );
    assert_eq!(all["replacements"], 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2 2\n");
    assert_eq!(
        failed(&tools, "edit", json!({"path": path, "old": "", "new": "x"})).code,
        "bad_request"
    );

    // Binary content is base64 in and out.
    let data = base64_encode(&[0, 159, 146, 150]);
    ok(
        &tools,
        "write",
        json!({"path": world.path("b.bin"), "base64": data}),
    );
    assert_eq!(
        std::fs::read(world.path("b.bin")).unwrap(),
        [0, 159, 146, 150]
    );
    assert_eq!(
        failed(
            &tools,
            "edit",
            json!({"path": world.path("b.bin"), "old": "a", "new": "b"})
        )
        .code,
        "not_text"
    );
}

/// A link that runs a function once, just before the first write stream opens: a second writer between read and write.
struct Between {
    inner: Arc<dyn Open>,
    act: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Open for Between {
    fn open(
        &self,
        protocol_name: &str,
        metadata: &[u8],
    ) -> Result<Box<dyn coqui_link::mux::LinkStream>, Error> {
        if protocol_name == protocol::WRITE
            && let Some(act) = self.act.lock().unwrap().take()
        {
            act();
        }
        self.inner.open(protocol_name, metadata)
    }
}

#[test]
fn an_edit_is_refused_as_changed_when_a_second_writer_changed_the_file_after_it_was_read() {
    let _deadline = Deadline::start();
    let world = World::new();
    let path = world.path("shared.txt");
    std::fs::write(&path, "alpha beta gamma\n").unwrap();
    let (connection, _) = world.node.link(grant(world.dir.path()));
    let other = path.clone();
    let link = Arc::new(Between {
        inner: Arc::new(connection),
        // The same length, so only the digest (and the time) tell.
        act: Mutex::new(Some(Box::new(move || {
            std::fs::write(&other, "ALPHA beta gamma\n").unwrap();
        }))),
    });
    let tools = Tools::new(link).expect("the tools are made");
    let error = failed(
        &tools,
        "edit",
        json!({"path": path, "old": "beta", "new": "BETA"}),
    );
    assert_eq!(error.code, "changed", "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "ALPHA beta gamma\n",
        "the other writer's file is what is there: the edit replaced nothing"
    );
    // Read again, the edit goes through.
    ok(
        &tools,
        "edit",
        json!({"path": path, "old": "beta", "new": "BETA"}),
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "ALPHA BETA gamma\n"
    );
}

#[test]
fn a_peer_with_write_but_not_read_cannot_ask_a_replace_against_a_base() {
    let _deadline = Deadline::start();
    let world = World::new();
    let path = world.path("blind.txt");
    std::fs::write(&path, "secret\n").unwrap();
    let write_only =
        Grant::allowing([Permission::FsWrite]).with_fs_roots([world.dir.path().to_path_buf()]);
    let (connection, _) = world.node.link(write_only);
    let mut request = bohio_proto::fs::WriteRequest::replace(path);
    request.base = Some(bohio_proto::fs::DeltaBase {
        size: 7,
        modified: None,
        hashes: bohio_proto::hash::Digests::new(),
    });
    let refused =
        bohio_client::fs::Writer::start(&connection, &request, Some(Duration::from_secs(10)));
    assert!(
        matches!(
            refused,
            Err(Error::Transport(coqui_transport::Failure::Permission) | Error::Remote(_))
        ),
        "a base tells what the file holds: it needs fs.read"
    );
}

#[test]
fn files_are_listed_walked_globbed_hashed_and_changed() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    for name in ["a.rs", "b.rs", "c.txt", "d.rs", "e.txt"] {
        std::fs::write(world.path(name), name).unwrap();
    }
    std::fs::create_dir(world.path("sub")).unwrap();
    std::fs::write(world.path("sub").as_str().to_owned() + "/f.rs", "f").unwrap();
    let root = world.dir.path().to_string_lossy().into_owned();

    let mut seen = Vec::new();
    let mut after = Value::Null;
    loop {
        let page = ok(
            &tools,
            "list",
            json!({"path": root, "limit": 2, "after": after}),
        );
        for entry in page["entries"].as_array().unwrap() {
            seen.push(entry["name"].as_str().unwrap().to_owned());
        }
        assert!(page["entries"].as_array().unwrap().len() <= 2);
        if page["more"] == false {
            break;
        }
        after = page["next"].clone();
    }
    assert_eq!(seen, ["a.rs", "b.rs", "c.txt", "d.rs", "e.txt", "sub"]);

    let stat = ok(&tools, "stat", json!({"path": world.path("a.rs")}));
    assert_eq!(stat["metadata"]["size"], 4);

    let found = ok(&tools, "glob", json!({"pattern": "*.rs", "path": root}));
    let mut paths: Vec<&str> = found["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert!(paths.iter().any(|p| p.ends_with("f.rs")), "{paths:?}");
    assert_eq!(paths.iter().filter(|p| p.ends_with(".rs")).count(), 4);

    let first = ok(&tools, "walk", json!({"roots": [root], "limit": 3}));
    assert_eq!(first["entries"].as_array().unwrap().len(), 3);
    assert_eq!(first["more"], true);
    let rest = ok(
        &tools,
        "walk",
        json!({"roots": [root], "skip": first["next_skip"], "limit": 100}),
    );
    assert_eq!(rest["more"], false);
    assert_eq!(
        rest["entries"].as_array().unwrap().len() + 3,
        8,
        "the root, six entries and one inside sub"
    );

    let hashed = ok(
        &tools,
        "hash",
        json!({"path": world.path("c.txt"), "algorithms": ["sha256", "md5"]}),
    );
    assert_eq!(hashed["hashes"]["sha256"], hex(b"c.txt"));
    assert_eq!(hashed["hashes"]["md5"].as_str().unwrap().len(), 32);

    ok(&tools, "mkdir", json!({"path": world.path("x/y")}));
    ok(
        &tools,
        "copy",
        json!({"from": world.path("a.rs"), "to": world.path("x/y/a.copy")}),
    );
    ok(
        &tools,
        "move",
        json!({"from": world.path("x/y/a.copy"), "to": world.path("x/moved")}),
    );
    ok(
        &tools,
        "truncate",
        json!({"path": world.path("x/moved"), "length": 2}),
    );
    assert_eq!(
        std::fs::read_to_string(world.path("x/moved")).unwrap(),
        "a."
    );
    assert_eq!(
        failed(
            &tools,
            "move",
            json!({"from": world.path("x/moved"), "to": world.path("a.rs")})
        )
        .code,
        "exists"
    );
    ok(
        &tools,
        "remove",
        json!({"path": world.path("x"), "recursive": true}),
    );
    assert!(!Path::new(&world.path("x")).exists());
    assert_eq!(
        failed(&tools, "stat", json!({"path": world.path("x")})).code,
        "not_found"
    );
    let outside = failed(
        &tools,
        "read",
        json!({"path": std::env::temp_dir().join("elsewhere").to_string_lossy()}),
    );
    assert!(
        matches!(outside.code.as_str(), "denied" | "outside_root"),
        "{outside}"
    );
}

#[test]
fn several_files_travel_on_one_stream_with_a_budget() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let written = ok(
        &tools,
        "write_many",
        json!({"files": [
            {"path": world.path("m1.txt"), "text": "first"},
            {"path": world.path("m2.txt"), "text": "second"},
            {"path": world.path("m1.txt"), "text": "again", "create_only": true},
        ]}),
    );
    let files = written["files"].as_array().unwrap();
    assert_eq!(files[0]["written"], 5);
    assert_eq!(files[1]["written"], 6);
    assert_eq!(
        files[2]["error"]["code"], "exists",
        "one that fails does not stop the others"
    );

    let read = ok(
        &tools,
        "read_many",
        json!({"paths": [world.path("m1.txt"), world.path("m2.txt"), world.path("nope")]}),
    );
    let files = read["files"].as_array().unwrap();
    assert_eq!(files[0]["data"], "first");
    assert_eq!(files[1]["data"], "second");
    assert_eq!(files[2]["error"]["code"], "not_found");

    let tight = ok(
        &tools,
        "read_many",
        json!({"paths": [world.path("m1.txt"), world.path("m2.txt")], "max_bytes": 6}),
    );
    let files = tight["files"].as_array().unwrap();
    assert_eq!(files[0]["data"], "first");
    assert!(
        files[1]["skipped"].is_string(),
        "no room is said, not silently dropped: {tight}"
    );
    assert_eq!(files[1]["size"], 6);
}

#[test]
fn put_and_get_move_files_of_this_machine_only_under_an_allowed_directory() {
    let _deadline = Deadline::start();
    let world = World::new();
    let local = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut settings = Settings::default();
    settings.local_roots = vec![local.path().to_path_buf()];
    let tools = world.tools_with(grant(world.dir.path()), settings);
    let source = local.path().join("up.txt");
    std::fs::write(&source, "carried up").unwrap();
    ok(
        &tools,
        "put",
        json!({"local": source.to_string_lossy(), "remote": world.path("up.txt")}),
    );
    assert_eq!(
        std::fs::read_to_string(world.path("up.txt")).unwrap(),
        "carried up"
    );
    let back = local.path().join("down.txt");
    ok(
        &tools,
        "get",
        json!({"local": back.to_string_lossy(), "remote": world.path("up.txt")}),
    );
    assert_eq!(std::fs::read_to_string(&back).unwrap(), "carried up");

    let elsewhere = outside.path().join("stolen.txt");
    std::fs::write(&elsewhere, "not for the host").unwrap();
    let refused = failed(
        &tools,
        "put",
        json!({"local": elsewhere.to_string_lossy(), "remote": world.path("s.txt")}),
    );
    assert_eq!(refused.code, "outside_root");
    let dotdot = local
        .path()
        .join("..")
        .join(outside.path().file_name().unwrap())
        .join("stolen.txt");
    assert_eq!(
        failed(
            &tools,
            "put",
            json!({"local": dotdot.to_string_lossy(), "remote": world.path("s.txt")})
        )
        .code,
        "outside_root"
    );

    // With no directory given, the tools are unavailable and say why.
    let bare = world.tools(grant(world.dir.path()));
    let error = failed(&bare, "put", json!({"local": "x", "remote": "y"}));
    assert_eq!(error.code, "unavailable", "{error}");
}

#[test]
fn a_tool_the_grant_does_not_hold_is_refused_with_what_it_needs() {
    let _deadline = Deadline::start();
    let world = World::new();
    let read_only = Grant::allowing([Permission::Info, Permission::FsRead])
        .with_fs_roots([world.dir.path().to_path_buf()]);
    let tools = world.tools(read_only);
    std::fs::write(world.path("r.txt"), "readable").unwrap();
    assert_eq!(
        ok(&tools, "read", json!({"path": world.path("r.txt")}))["text"],
        "readable"
    );
    let write = failed(
        &tools,
        "write",
        json!({"path": world.path("w.txt"), "text": "x"}),
    );
    assert_eq!(write.code, "denied", "{write}");
    assert!(write.message.contains("fs.write"), "{write}");
    let exec = failed(&tools, "exec", json!({"argv": [helper()]}));
    assert_eq!(exec.code, "denied");
    assert!(exec.message.contains("exec"), "{exec}");
    assert_eq!(
        failed(&tools, "no_such_tool", json!({})).code,
        "unknown_tool"
    );
    assert!(!Path::new(&world.path("w.txt")).exists());
}

#[test]
fn what_the_host_turned_off_is_listed_as_unavailable_with_the_reason() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().unwrap();
    let node = Running::with_node(NodeConfig {
        execution: ExecutionConfig {
            enabled: false,
            ..ExecutionConfig::default()
        },
        ..NodeConfig::default()
    });
    let (connection, _) = node.link(grant(dir.path()));
    let tools = Tools::new(Arc::new(connection)).expect("the tools are made");
    let listed = tools.available();
    let exec = listed.iter().find(|(spec, _)| spec.name == "exec").unwrap();
    assert!(
        matches!(&exec.1, Availability::Unavailable(reason) if reason.contains("exec")),
        "{:?}",
        exec.1
    );
    let read = listed.iter().find(|(spec, _)| spec.name == "read").unwrap();
    assert_eq!(read.1, Availability::Available);
    let error = failed(&tools, "exec", json!({"argv": [helper()]}));
    assert_eq!(error.code, "unavailable");
}

#[test]
fn a_peer_without_info_still_gets_tools_that_report_what_the_host_answers() {
    let _deadline = Deadline::start();
    let world = World::new();
    let blind =
        Grant::allowing([Permission::FsRead]).with_fs_roots([world.dir.path().to_path_buf()]);
    let tools = world.tools(blind);
    assert!(tools.info().is_none());
    std::fs::write(world.path("p.txt"), "seen").unwrap();
    assert_eq!(
        ok(&tools, "read", json!({"path": world.path("p.txt")}))["text"],
        "seen"
    );
    assert_eq!(failed(&tools, "info", json!({})).code, "denied");
}

fn read_until_done(tools: &Tools, session: &str) -> (String, Value) {
    let mut text = String::new();
    let mut from = 0_u64;
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let read = ok(
            tools,
            "session_read",
            json!({"session": session, "from": from, "wait_ms": 2000, "quiet_ms": 300}),
        );
        for chunk in read["chunks"].as_array().unwrap() {
            text.push_str(chunk["data"].as_str().unwrap());
        }
        from = read["next"].as_u64().unwrap();
        if read["running"] == false {
            return (text, read["exit"].clone());
        }
        assert!(Instant::now() < until, "the session did not end: {text}");
    }
}

#[test]
fn a_session_runs_on_and_is_read_from_an_offset_written_to_signalled_listed_and_ended() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let started = ok(
        &tools,
        "session_start",
        json!({"argv": [helper(), "count", "20", "20"], "name": "counter"}),
    );
    let id = started["session"].as_str().unwrap().to_owned();
    let (text, exit) = read_until_done(&tools, &id);
    let expected: String = (1..=20).map(|n| format!("{n}\n")).collect();
    assert_eq!(text, expected);
    assert_eq!(exit["code"], 0);

    // The output is still kept: a read from 0 gives it again, and from the end gives none.
    let again = ok(&tools, "session_read", json!({"session": id, "from": 0}));
    let total = again["next"].as_u64().unwrap();
    assert_eq!(total, expected.len() as u64);
    let tail = ok(
        &tools,
        "session_read",
        json!({"session": id, "from": total}),
    );
    assert!(tail["chunks"].as_array().unwrap().is_empty());
    let small = ok(
        &tools,
        "session_read",
        json!({"session": id, "from": 0, "max_output": 7}),
    );
    assert_eq!(small["more"], true, "output left over a bound is said");
    assert_eq!(small["next"], 7);

    let listed = ok(&tools, "session_list", json!({}));
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str() && s["name"] == "counter")
    );
    ok(&tools, "session_end", json!({"session": id}));
    let gone = ok(&tools, "session_list", json!({}));
    assert!(
        !gone["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str() && s["exit"].is_null())
    );
    assert_eq!(
        failed(
            &tools,
            "session_read",
            json!({"session": "00000000000000000000000000000000"})
        )
        .code,
        "not_found"
    );
}

#[test]
fn a_session_takes_input_and_a_signal() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let started = ok(
        &tools,
        "session_start",
        json!({"argv": [helper(), "read-line"], "stdin": true}),
    );
    let id = started["session"].as_str().unwrap().to_owned();
    let first = ok(
        &tools,
        "session_read",
        json!({"session": id, "wait_ms": 5000}),
    );
    assert!(
        first["chunks"][0]["data"]
            .as_str()
            .unwrap()
            .contains("ready"),
        "{first}"
    );
    ok(
        &tools,
        "session_write",
        json!({"session": id, "text": "hello\n"}),
    );
    let (text, exit) = read_until_done(&tools, &id);
    assert!(text.contains("got:hello"), "{text}");
    assert_eq!(exit["code"], 0);
    ok(&tools, "session_end", json!({"session": id}));

    #[cfg(unix)]
    {
        let trap = ok(&tools, "session_start", json!({"argv": [helper(), "trap"]}));
        let id = trap["session"].as_str().unwrap().to_owned();
        let ready = ok(
            &tools,
            "session_read",
            json!({"session": id, "wait_ms": 5000}),
        );
        assert!(
            ready["chunks"][0]["data"]
                .as_str()
                .unwrap()
                .contains("ready")
        );
        let signalled = ok(
            &tools,
            "session_signal",
            json!({"session": id, "signal": "interrupt"}),
        );
        assert_eq!(signalled["delivered"], true);
        let (text, exit) = read_until_done(&tools, &id);
        assert!(text.contains("got-int"), "{text}");
        assert_eq!(exit["code"], 3);
    }
}

#[test]
fn a_local_forward_reaches_a_target_through_the_host_and_is_closed() {
    let _deadline = Deadline::start();
    let world = World::new();
    let echo = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = echo.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut connection, _) = echo.accept().unwrap();
        let mut bytes = [0_u8; 4];
        connection.read_exact(&mut bytes).unwrap();
        connection.write_all(&bytes).unwrap();
    });
    let grant = Grant::allowing([Permission::Info, Permission::ForwardConnect])
        .with_connect([format!("127.0.0.1:{port}")]);
    let tools = world.tools(grant);
    let opened = ok(
        &tools,
        "forward_local",
        json!({"target": format!("127.0.0.1:{port}")}),
    );
    let address = opened["listening"].as_str().unwrap().to_owned();
    let mut client = TcpStream::connect(&address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    client.write_all(b"ping").unwrap();
    let mut back = [0_u8; 4];
    client.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"ping");
    server.join().unwrap();
    let listed = ok(&tools, "forward_list", json!({}));
    assert_eq!(listed["forwards"].as_array().unwrap().len(), 1);
    ok(&tools, "forward_close", json!({"id": opened["id"]}));
    assert!(
        ok(&tools, "forward_list", json!({}))["forwards"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        failed(&tools, "forward_close", json!({"id": 99})).code,
        "not_found"
    );

    let wide = failed(
        &tools,
        "forward_local",
        json!({"listen": "0.0.0.0:0", "target": format!("127.0.0.1:{port}")}),
    );
    assert_eq!(wide.code, "denied", "{wide}");
    let remote = failed(&tools, "forward_remote", json!({"target": "127.0.0.1:1"}));
    assert_eq!(
        remote.code, "unavailable",
        "a remote forward needs a local target the embedder allows: {remote}"
    );
    drop(tools);
}

#[test]
fn a_write_that_names_the_digest_it_read_is_refused_when_the_file_moved_on() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let path = world.path("guarded.txt");
    let first = ok(
        &tools,
        "write",
        json!({"path": path, "text": "version one\n"}),
    );
    let digest = first["sha256"].as_str().unwrap().to_owned();
    ok(
        &tools,
        "write",
        json!({"path": path, "text": "version two\n", "if_sha256": digest}),
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version two\n");
    let stale = failed(
        &tools,
        "write",
        json!({"path": path, "text": "version three\n", "if_sha256": digest}),
    );
    assert_eq!(stale.code, "changed", "{stale}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "version two\n");
    let missing = failed(
        &tools,
        "write",
        json!({"path": world.path("none.txt"), "text": "x", "if_sha256": digest}),
    );
    assert_eq!(missing.code, "not_found", "{missing}");
}

#[test]
fn search_finds_lines_pages_by_skip_and_reports_what_it_could_not_read() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let root = world.dir.path().to_string_lossy().into_owned();
    let lines: String = (1..=7)
        .map(|n| {
            if n % 2 == 1 {
                format!("needle {n}\n")
            } else {
                format!("hay {n}\n")
            }
        })
        .collect();
    std::fs::write(world.path("a.txt"), &lines).unwrap();
    std::fs::write(world.path("b.log"), "a Needle in the log\n").unwrap();
    std::fs::write(world.path("blob.bin"), b"needle\0binary").unwrap();

    let all = ok(
        &tools,
        "search",
        json!({"pattern": "needle", "path": root, "include": ["*.txt"], "context": 1}),
    );
    let matches = all["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 4, "{all}");
    assert_eq!(matches[0]["path"], "a.txt");
    assert_eq!(matches[0]["line"], 1);
    assert_eq!(matches[0]["text"], "needle 1");
    assert_eq!(matches[1]["before"], json!(["hay 2"]));
    assert_eq!(matches[1]["after"], json!(["hay 4"]));
    assert_eq!(all["more"], false);

    // A page of two, then the rest: the pages join into the whole answer, in order.
    let mut seen = Vec::new();
    let mut skip = 0_u64;
    loop {
        let page = ok(
            &tools,
            "search",
            json!({"pattern": "needle", "path": root, "include": ["*.txt"], "limit": 2, "skip": skip}),
        );
        let found = page["matches"].as_array().unwrap();
        assert!(found.len() <= 2);
        seen.extend(found.iter().map(|m| m["line"].as_u64().unwrap()));
        match page["next_skip"].as_u64() {
            Some(next) => skip = next,
            None => break,
        }
    }
    assert_eq!(seen, [1, 3, 5, 7]);

    // Case is the caller's choice; a binary file is skipped and counted, not searched.
    let both = ok(
        &tools,
        "search",
        json!({"pattern": "needle", "path": root, "literal": true, "case_insensitive": true}),
    );
    let paths: Vec<&str> = both["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"b.log") && paths.contains(&"a.txt"),
        "{both}"
    );
    assert!(!paths.contains(&"blob.bin"), "{both}");
    assert_eq!(both["done"]["skipped"]["binary"], 1, "{both}");

    // A pattern that does not compile is the host's bad_request; a peer without fs.read is refused.
    assert_eq!(
        failed(&tools, "search", json!({"pattern": "(", "path": root})).code,
        "bad_request"
    );
    let blind = world.tools(Grant::allowing([Permission::Info]));
    assert_eq!(
        failed(&blind, "search", json!({"pattern": "x", "path": root})).code,
        "denied"
    );
}

#[test]
fn stats_and_which_describe_the_host_and_need_info() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let stats = ok(&tools, "stats", json!({}));
    assert!(stats["memory"]["total"].as_u64().unwrap() > 0, "{stats}");
    assert!(!stats["disks"].as_array().unwrap().is_empty(), "{stats}");
    assert!(stats["cpus"].as_u64().unwrap() >= 1);

    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let found = ok(
        &tools,
        "which",
        json!({"names": [shell, "no-such-program-here"]}),
    );
    let path = found["found"][shell].as_str().expect("the shell is found");
    assert!(Path::new(path).is_absolute(), "{path}");
    assert_eq!(found["found"]["no-such-program-here"], Value::Null);
    assert_eq!(
        failed(&tools, "which", json!({"names": []})).code,
        "bad_request"
    );

    let without = world.tools(
        Grant::allowing([Permission::FsRead]).with_fs_roots([world.dir.path().to_path_buf()]),
    );
    assert_eq!(failed(&without, "stats", json!({})).code, "denied");
    assert_eq!(
        failed(&without, "which", json!({"names": [shell]})).code,
        "denied"
    );
}

#[test]
fn follow_returns_what_is_appended_while_it_watches_and_says_when_the_file_was_cut() {
    let _deadline = Deadline::start();
    let world = World::new();
    let tools = world.tools(grant(world.dir.path()));
    let path = world.path("app.log");
    std::fs::write(&path, "before\n").unwrap();

    let writer = {
        let path = path.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(600));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"appended one\n").unwrap();
            file.sync_all().unwrap();
            thread::sleep(Duration::from_millis(600));
            std::fs::write(&path, "cut\n").unwrap();
        })
    };
    let seen = ok(
        &tools,
        "follow",
        json!({"path": path, "tail": 0, "duration_ms": 3000}),
    );
    writer.join().unwrap();
    let output = text_of(&seen["output"]);
    assert!(output.contains("appended one"), "{seen}");
    assert!(
        !output.contains("before"),
        "tail 0 is only what is appended: {seen}"
    );
    assert_eq!(seen["size_at_start"], 7);
    let cut = seen["truncations"].as_array().unwrap();
    assert_eq!(cut.len(), 1, "{seen}");
    assert_eq!(cut[0]["size"], 4);
    assert!(cut[0]["at_output_byte"].as_u64().unwrap() >= 13);

    // The watch lasts its time, and a missing file is the host's answer.
    let started = Instant::now();
    let idle = ok(
        &tools,
        "follow",
        json!({"path": path, "tail": 0, "duration_ms": 400}),
    );
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(idle["output"]["total_bytes"], 0);
    assert_eq!(
        failed(&tools, "follow", json!({"path": world.path("none.log")})).code,
        "not_found"
    );
}
