//! `bohio mcp` driven as a real MCP client does: a child process speaking JSON-RPC lines on its standard input and output,
//! against a real `bohio serve` child that grants the child's key.

#![forbid(unsafe_code)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::helper;
use support::procs::{Node, Proc, toml_path};
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(60);

/// A host that grants an agent key, and where the agent keeps its identity.
struct World {
    _dir: TempDir,
    host: Node,
    agent: Node,
    root: std::path::PathBuf,
    serving: Option<Proc>,
    endpoint: String,
}

impl World {
    fn new(allow: &str) -> Self {
        Self::with_config(allow, "")
    }

    /// As [`World::new`], with `extra` text in the host's `bohio.cfg`.
    fn with_config(allow: &str, extra: &str) -> Self {
        let dir = tempfile::tempdir().expect("a directory");
        let host = Node::new(dir.path(), "host");
        let agent = Node::new(dir.path(), "agent");
        let root = dir.path().join("files");
        std::fs::create_dir_all(&root).unwrap();
        host.write(&(format!(
            "[peer.\"{}\"]\nallow = [{allow}]\nfs_roots = [{}]\n[peer.\"{}\".exec]\nprograms = [{}]\n",
            agent.id,
            toml_path(&root),
            agent.id,
            toml_path(Path::new(&helper())),
        ) + extra));
        let mut world = Self {
            _dir: dir,
            host,
            agent,
            root,
            serving: None,
            endpoint: String::new(),
        };
        world.serve("tcp://127.0.0.1:0");
        world
    }

    fn serve(&mut self, listen: &str) {
        let (process, listening) = self.host.serve(Some(listen), "warn");
        let (_, endpoint) = listening.expect("the host listens");
        self.endpoint = endpoint;
        self.serving = Some(process);
    }

    fn path(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }

    fn host_flag(&self, name: &str) -> String {
        format!("{name}={} --server {}", self.endpoint, self.host.id)
    }

    fn mcp(&self, hosts: &[String], extra: &[&str]) -> Client {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
        command
            .arg("mcp")
            .args(["--config", &self.agent.config()])
            .env_remove("BOHIO_CONFIG")
            .env("BOHIO_LOG", "warn")
            .env("NO_COLOR", "1");
        for host in hosts {
            command.args(["--host", host]);
        }
        command.args(extra);
        Client::start(command)
    }
}

/// An MCP client over a child's standard input and output.
struct Client {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<String>,
    next: u64,
}

impl Client {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("bohio mcp starts");
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            lines,
            next: 0,
        }
    }

    fn send_raw(&mut self, text: &str) {
        let input = self.input.as_mut().expect("standard input is open");
        writeln!(input, "{text}").expect("the child reads");
        input.flush().unwrap();
    }

    fn send(&mut self, message: &Value) {
        self.send_raw(&message.to_string());
    }

    fn read_line(&mut self) -> Value {
        match self.lines.recv_timeout(WAIT) {
            Ok(line) => serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}")),
            Err(RecvTimeoutError::Timeout) => panic!("the server said nothing for {WAIT:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("the server closed its output"),
        }
    }

    /// Sends a request and returns its response.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(id)
    }

    fn response(&mut self, id: u64) -> Value {
        loop {
            let message = self.read_line();
            assert_eq!(message["jsonrpc"], "2.0", "{message}");
            if message["id"] == id {
                return message;
            }
        }
    }

    fn start_request(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn initialize(&mut self) -> Value {
        let answer = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "the test", "version": "1"},
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        answer
    }

    /// A tool call's `result`.
    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let answer = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(answer.get("error").is_none(), "{tool}: {answer}");
        answer["result"].clone()
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], false, "{tool}: {result}");
        let text = result["content"][0]["text"].as_str().expect("a text block");
        let parsed: Value = serde_json::from_str(text).expect("the text is the result as JSON");
        assert_eq!(
            parsed, result["structuredContent"],
            "the text and the structured result agree"
        );
        parsed
    }

    fn refused(&mut self, tool: &str, arguments: Value) -> String {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], true, "{tool}: {result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    }

    fn finish(mut self) -> Option<i32> {
        drop(self.input.take());
        let until = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < until,
                "bohio mcp did not end when its input closed"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const ALL: &str = "\"info\", \"exec\", \"fs.read\", \"fs.write\", \"session\"";

fn text_of(stream: &Value) -> String {
    stream["data"].as_str().unwrap().to_owned()
}

#[test]
fn initialize_answers_the_clients_revision_or_its_own_and_lists_tools_with_schemas() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    let answer = client.initialize();
    assert_eq!(answer["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(answer["result"]["serverInfo"]["name"], "bohio");
    assert!(answer["result"]["capabilities"]["tools"].is_object());
    assert!(
        answer["result"]["capabilities"].get("resources").is_none(),
        "tools only"
    );

    let other = client.request(
        "initialize",
        json!({"protocolVersion": "1999-01-01", "capabilities": {}}),
    );
    assert_eq!(
        other["result"]["protocolVersion"], "2025-11-25",
        "its own latest, when the client's is unknown"
    );

    let listed = client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for wanted in [
        "exec",
        "read",
        "write",
        "edit",
        "list",
        "stat",
        "glob",
        "walk",
        "hash",
        "session_start",
        "session_read",
        "session_write",
        "session_signal",
        "session_list",
        "session_end",
        "move",
        "copy",
        "truncate",
        "read_many",
        "write_many",
        "put",
        "get",
        "forward_local",
        "forward_remote",
    ] {
        assert!(names.contains(&wanted), "{wanted} is not listed: {names:?}");
    }
    for desktop in [
        "screenshot",
        "click",
        "type",
        "key",
        "clipboard_read",
        "gui_watch",
    ] {
        assert!(
            !names.contains(&desktop),
            "{desktop} is listed without --gui: {names:?}"
        );
    }
    for tool in tools {
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{tool}");
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("host"), "{}", tool["name"]);
        for required in schema["required"].as_array().unwrap() {
            assert!(
                properties.contains_key(required.as_str().unwrap()),
                "{tool}"
            );
        }
        // With one host, a call need not name it.
        assert!(
            !schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|name| name == "host")
        );
    }
    assert_eq!(client.request("ping", json!({}))["result"], json!({}));
    assert_eq!(client.finish(), Some(0));
}

#[test]
fn bad_messages_are_answered_with_the_json_rpc_codes_and_the_server_goes_on() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    client.initialize();
    client.send_raw("{ this is not json");
    let parse = client.read_line();
    assert_eq!(parse["error"]["code"], -32700, "{parse}");
    assert_eq!(parse["id"], Value::Null);
    client.send_raw("[1, 2]");
    assert_eq!(client.read_line()["error"]["code"], -32600);
    client.send_raw("42");
    assert_eq!(client.read_line()["error"]["code"], -32600);
    let missing = client.request("no/such/method", json!({}));
    assert_eq!(missing["error"]["code"], -32601);
    let unknown = client.request(
        "tools/call",
        json!({"name": "no_such_tool", "arguments": {}}),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    assert!(
        unknown["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no_such_tool")
    );
    let nameless = client.request("tools/call", json!({"arguments": {}}));
    assert_eq!(nameless["error"]["code"], -32602);
    let bad_arguments = client.request("tools/call", json!({"name": "exec", "arguments": [1]}));
    assert_eq!(bad_arguments["error"]["code"], -32602);
    // A call whose arguments do not fit its schema is a result the agent can read.
    let text = client.refused("exec", json!({"argv": []}));
    assert!(text.starts_with("bad_request"), "{text}");
    assert_eq!(
        client.request("ping", json!({}))["result"],
        json!({}),
        "it is still serving"
    );
}

#[test]
fn files_are_read_written_and_edited_through_the_tools() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    client.initialize();
    let path = world.path("notes.txt");

    let written = client.ok("write", json!({"path": path, "text": "one two two\n"}));
    assert_eq!(written["written"], 12);
    let read = client.ok("read", json!({"path": path}));
    assert_eq!(read["text"], "one two two\n");
    assert_eq!(read["total_lines"], 1);
    let digest = read["sha256"].as_str().unwrap().to_owned();

    let missing = client.refused("edit", json!({"path": path, "old": "three", "new": "3"}));
    assert!(missing.starts_with("no_match"), "{missing}");
    let many = client.refused("edit", json!({"path": path, "old": "two", "new": "2"}));
    assert!(
        many.starts_with("ambiguous") && many.contains('2'),
        "{many}"
    );
    let unique = client.ok("edit", json!({"path": path, "old": "one", "new": "1"}));
    assert_eq!(unique["replacements"], 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 two two\n");

    // A second writer moved the file on since the read whose digest this write names: nothing is replaced.
    let stale = client.refused(
        "write",
        json!({"path": path, "text": "lost\n", "if_sha256": digest}),
    );
    assert!(stale.starts_with("changed"), "{stale}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 two two\n");

    std::fs::write(world.path("raw.bin"), [0_u8, 159, 146, 150]).unwrap();
    let binary = client.refused("read", json!({"path": world.path("raw.bin")}));
    assert!(binary.starts_with("not_text"), "{binary}");
    let bytes = client.ok(
        "read",
        json!({"path": world.path("raw.bin"), "encoding": "base64"}),
    );
    assert_eq!(bytes["data"], "AJ+Slg==");

    let listed = client.ok("list", json!({"path": world.root.to_string_lossy()}));
    let names: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["notes.txt", "raw.bin"]);
    let found = client.ok(
        "glob",
        json!({"pattern": "*.txt", "path": world.root.to_string_lossy()}),
    );
    assert_eq!(found["entries"].as_array().unwrap().len(), 1, "{found}");
    let stat = client.ok("stat", json!({"path": path}));
    assert_eq!(stat["metadata"]["size"], 10);
}

#[test]
fn exec_runs_with_a_bound_a_deadline_and_a_cancel() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &["--max-output", "2000"]);
    client.initialize();
    let echo = client.ok("exec", json!({"argv": [helper(), "echo", "over", "mcp"]}));
    assert_eq!(text_of(&echo["stdout"]).trim(), "over mcp");
    assert_eq!(echo["exit"]["code"], 0);

    let flood = client.ok("exec", json!({"argv": [helper(), "flood", "50000"]}));
    assert_eq!(
        flood["stdout"]["truncated"], true,
        "the server's --max-output is the default bound"
    );
    assert_eq!(flood["stdout"]["total_bytes"], 50_000);

    let slow = client.ok(
        "exec",
        json!({"argv": [helper(), "sleep", "30000"], "deadline_ms": 400}),
    );
    assert_eq!(slow["exit"]["timed_out"], true);

    // The client cancels a call that is running: the program is ended and the call answers.
    let id = client.start_request(
        "tools/call",
        json!({"name": "exec", "arguments": {"argv": [helper(), "sleep", "30000"], "deadline_ms": 0}}),
    );
    thread::sleep(Duration::from_millis(800));
    client.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": id}}));
    let started = Instant::now();
    let answer = client.response(id);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(
        answer["result"]["structuredContent"]["cancelled"], true,
        "{answer}"
    );
}

#[test]
fn a_session_starts_is_read_and_ended() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    client.initialize();
    let started = client.ok(
        "session_start",
        json!({"argv": [helper(), "count", "5", "50"], "name": "five"}),
    );
    let id = started["session"].as_str().unwrap().to_owned();
    let mut text = String::new();
    let mut from = 0;
    let until = Instant::now() + WAIT;
    loop {
        let read = client.ok(
            "session_read",
            json!({"session": id, "from": from, "wait_ms": 3000}),
        );
        for chunk in read["chunks"].as_array().unwrap() {
            text.push_str(chunk["data"].as_str().unwrap());
        }
        from = read["next"].as_u64().unwrap();
        if read["running"] == false {
            assert_eq!(read["exit"]["code"], 0);
            break;
        }
        assert!(Instant::now() < until, "{text}");
    }
    assert_eq!(text, "1\n2\n3\n4\n5\n");
    let listed = client.ok("session_list", json!({}));
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == id.as_str())
    );
    client.ok("session_end", json!({"session": id}));
}

#[test]
fn a_tool_the_grant_lacks_is_a_result_with_is_error_and_the_reason() {
    let world = World::new("\"info\", \"fs.read\"");
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    client.initialize();
    std::fs::write(world.path("a.txt"), "readable").unwrap();
    assert_eq!(
        client.ok("read", json!({"path": world.path("a.txt")}))["text"],
        "readable"
    );
    let exec = client.refused("exec", json!({"argv": [helper()]}));
    assert!(
        exec.starts_with("denied") && exec.contains("exec"),
        "{exec}"
    );
    let write = client.refused("write", json!({"path": world.path("b.txt"), "text": "x"}));
    assert!(
        write.starts_with("denied") && write.contains("fs.write"),
        "{write}"
    );
    assert!(!Path::new(&world.path("b.txt")).exists());
    // Once the host's capabilities are known, `tools/list` says what it cannot do.
    let local = client.refused("put", json!({"local": "x", "remote": "y"}));
    assert!(local.starts_with("unavailable"), "{local}");
}

#[test]
fn several_hosts_need_a_name_and_a_lost_link_is_made_again() {
    let mut world = World::new(ALL);
    let port = world.endpoint.rsplit(':').next().unwrap().to_owned();
    let hosts = [world.host_flag("one"), world.host_flag("two")];
    let mut client = world.mcp(&hosts, &[]);
    client.initialize();
    let listed = client.request("tools/list", json!({}));
    let schema = &listed["result"]["tools"][0]["inputSchema"];
    assert_eq!(schema["properties"]["host"]["enum"], json!(["one", "two"]));
    assert!(
        schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "host")
    );
    let nameless = client.refused("stat", json!({"path": world.path("")}));
    assert!(nameless.contains("host"), "{nameless}");
    let wrong = client.refused("stat", json!({"host": "three", "path": world.path("")}));
    assert!(wrong.contains("three"), "{wrong}");
    std::fs::write(world.path("kept.txt"), "still there").unwrap();
    assert_eq!(
        client.ok(
            "read",
            json!({"host": "two", "path": world.path("kept.txt")})
        )["text"],
        "still there"
    );

    // The host goes away and comes back on the same address: the next call makes a new link.
    drop(world.serving.take());
    thread::sleep(Duration::from_millis(500));
    world.serve(&format!("tcp://127.0.0.1:{port}"));
    let until = Instant::now() + WAIT;
    loop {
        let result = client.call(
            "read",
            json!({"host": "two", "path": world.path("kept.txt")}),
        );
        if result["isError"] == false {
            assert_eq!(result["structuredContent"]["text"], "still there");
            break;
        }
        assert!(Instant::now() < until, "no link was made again: {result}");
        thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn the_server_ends_and_ends_what_runs_when_its_input_closes() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    client.initialize();
    let _ = client.start_request(
        "tools/call",
        json!({"name": "exec", "arguments": {"argv": [helper(), "sleep", "60000"], "deadline_ms": 0}}),
    );
    thread::sleep(Duration::from_millis(800));
    let started = Instant::now();
    assert_eq!(client.finish(), Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "it waited for the program"
    );
}

#[test]
fn a_host_that_cannot_be_reached_is_an_error_of_the_call_not_of_the_server() {
    let world = World::new(ALL);
    let down = format!("down=tcp://127.0.0.1:1 --server {}", world.host.id);
    let mut client = world.mcp(&[down, world.host_flag("up")], &[]);
    client.initialize();
    let text = client.refused("stat", json!({"host": "down", "path": "x"}));
    assert!(text.starts_with("link"), "{text}");
    std::fs::write(world.path("a.txt"), "x").unwrap();
    client.ok("stat", json!({"host": "up", "path": world.path("a.txt")}));
}

#[test]
fn search_stats_which_and_follow_are_listed_and_answer_over_mcp() {
    let world = World::new(ALL);
    let mut client = world.mcp(&[world.host_flag("lab")], &[]);
    client.initialize();
    let listed = client.request("tools/list", json!({}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for wanted in ["search", "stats", "which", "follow"] {
        assert!(names.contains(&wanted), "{wanted} is not listed: {names:?}");
    }

    std::fs::write(world.path("one.txt"), "alpha\nneedle here\nomega\n").unwrap();
    std::fs::write(world.path("two.txt"), "nothing\n").unwrap();
    let found = client.ok(
        "search",
        json!({"pattern": "needle", "path": world.root.to_string_lossy(), "context": 1}),
    );
    let matches = found["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{found}");
    assert_eq!(matches[0]["path"], "one.txt");
    assert_eq!(matches[0]["line"], 2);
    assert_eq!(matches[0]["before"], json!(["alpha"]));
    assert_eq!(matches[0]["after"], json!(["omega"]));
    assert_eq!(found["more"], false);

    let stats = client.ok("stats", json!({}));
    assert!(stats["memory"]["total"].as_u64().unwrap() > 0, "{stats}");
    assert!(!stats["disks"].as_array().unwrap().is_empty(), "{stats}");

    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let which = client.ok("which", json!({"names": [shell, "no-such-program-here"]}));
    assert!(which["found"][shell].is_string(), "{which}");
    assert_eq!(which["found"]["no-such-program-here"], Value::Null);

    let log = world.path("app.log");
    std::fs::write(&log, "old line\n").unwrap();
    let writer = {
        let log = log.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            file.write_all(b"new line\n").unwrap();
            file.sync_all().unwrap();
        })
    };
    let followed = client.ok(
        "follow",
        json!({"path": log, "tail": 0, "duration_ms": 2500}),
    );
    writer.join().unwrap();
    assert_eq!(text_of(&followed["output"]), "new line\n", "{followed}");
    assert_eq!(followed["truncations"], json!([]));
}

#[test]
fn a_host_named_in_the_configuration_is_served_by_its_bare_name() {
    let world = World::new(ALL);
    // The agent's own configuration names the host; `--host lab` carries nothing else.
    let config = std::path::PathBuf::from(world.agent.config());
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "\n[remote.lab]\nendpoint = \"{}\"\nserver = \"{}\"\n",
        world.endpoint, world.host.id
    ));
    std::fs::write(&config, text).unwrap();
    let mut client = world.mcp(&["lab".to_owned()], &[]);
    client.initialize();
    std::fs::write(world.path("named.txt"), "by name\n").unwrap();
    let read = client.ok("read", json!({"path": world.path("named.txt")}));
    assert_eq!(read["text"], "by name\n");

    // A bare word that names no host stops the server at its start, saying which.
    let output = Command::new(env!("CARGO_BIN_EXE_bohio"))
        .args([
            "mcp",
            "--config",
            &world.agent.config(),
            "--host",
            "nowhere",
        ])
        .env_remove("BOHIO_CONFIG")
        .stdin(Stdio::null())
        .output()
        .expect("bohio mcp runs");
    assert_eq!(output.status.code(), Some(1));
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(
        said.contains("nowhere") && said.contains("[remote]"),
        "{said}"
    );
}

#[test]
fn a_node_that_serves_no_files_or_processes_lists_no_tool_for_them() {
    let world = World::with_config(
        "\"info\", \"session\", \"fs.read\", \"fs.write\", \"exec\"",
        "[access]
fs = \"none\"
exec = \"none\"
",
    );
    let mut client = world.mcp(&[world.host_flag("host")], &[]);
    client.initialize();
    // The first call makes the link, so the host's capabilities are known when the tools are listed.
    let _ = client.ok("session_list", json!({}));
    let listed = client.request("tools/list", json!({}));
    let names: Vec<String> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    for gone in ["exec", "read", "write", "stat", "list", "hash"] {
        assert!(
            !names.iter().any(|name| name == gone),
            "{gone} is listed: {names:?}"
        );
    }
    assert!(names.iter().any(|name| name == "session_list"), "{names:?}");
    let refused = client.refused("read", json!({"path": world.path("x")}));
    assert!(refused.contains("serves no files"), "{refused}");
}
