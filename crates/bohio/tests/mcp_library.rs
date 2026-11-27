//! `bohio::mcp::serve` in this process, over in-memory streams: no command, no pipe to a child.

#![forbid(unsafe_code)]

mod support;

use std::io::{BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::mcp::{Host, Options, serve};
use bohio::policy::{Grant, Permission};
use bohio_client::Open;
use bohio_client::tools::Settings;
use serde_json::{Value, json};
use support::embedded::Running;
use support::helper;

/// What the server writes, one message to a line.
struct Lines {
    tx: Sender<String>,
    pending: Vec<u8>,
}

impl Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let text = String::from_utf8_lossy(&line[..end]).into_owned();
            let _ = self.tx.send(text);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A client of the server: what it says goes through a pipe, what the server answers comes back a line at a time.
struct Client {
    input: Option<std::io::PipeWriter>,
    answers: Receiver<String>,
    server: Option<JoinHandle<Result<(), String>>>,
    dialed: Arc<AtomicUsize>,
}

const WAIT: Duration = Duration::from_secs(30);

impl Client {
    fn start(hosts: &[&str], options: Options) -> Self {
        let node = Arc::new(Running::start());
        let dialed = Arc::new(AtomicUsize::new(0));
        let hosts: Vec<Host> = hosts
            .iter()
            .map(|name| {
                let node = Arc::clone(&node);
                let dialed = Arc::clone(&dialed);
                Host {
                    name: (*name).to_owned(),
                    dial: Box::new(move || {
                        dialed.fetch_add(1, Ordering::SeqCst);
                        let grant = Grant::allowing([Permission::Info, Permission::Exec])
                            .with_programs([helper()]);
                        let (connection, _) = node.link(grant);
                        Ok(Arc::new(connection) as Arc<dyn Open>)
                    }),
                    settings: Settings::default(),
                }
            })
            .collect();
        Self::over(hosts, options, dialed, node)
    }

    fn over(
        hosts: Vec<Host>,
        options: Options,
        dialed: Arc<AtomicUsize>,
        node: Arc<Running>,
    ) -> Self {
        let (reader, writer) = std::io::pipe().expect("a pipe");
        let (tx, answers) = mpsc::channel();
        let server = thread::spawn(move || {
            let _node = node;
            serve(
                hosts,
                &options,
                BufReader::new(reader),
                Lines {
                    tx,
                    pending: Vec::new(),
                },
            )
        });
        Self {
            input: Some(writer),
            answers,
            server: Some(server),
            dialed,
        }
    }

    fn send(&mut self, message: &Value) {
        self.send_text(&message.to_string());
    }

    fn send_text(&mut self, text: &str) {
        let input = self.input.as_mut().expect("the input is open");
        writeln!(input, "{text}").expect("the server reads");
        input.flush().expect("flushed");
    }

    /// The next message the server wrote.
    fn next(&self) -> Value {
        let line = self
            .answers
            .recv_timeout(WAIT)
            .expect("the server answers in time");
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("`{line}` is not JSON: {error}"))
    }

    fn ask(&mut self, id: i64, method: &str, params: &Value) -> Value {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let answer = self.next();
        assert_eq!(answer["id"], id, "{answer}");
        answer
    }

    /// Ends the input and waits for the server to finish: what it wrote after the last message is returned.
    fn finish(mut self) -> (Result<(), String>, Vec<Value>) {
        drop(self.input.take());
        let result = self.server.take().expect("a server").join().expect("ended");
        let mut rest = Vec::new();
        while let Ok(line) = self.answers.try_recv() {
            rest.push(serde_json::from_str(&line).expect("JSON"));
        }
        (result, rest)
    }
}

fn exec_sleep(millis: u64) -> Value {
    json!({"name": "exec", "arguments": {"argv": [helper(), "sleep", millis.to_string()]}})
}

#[test]
fn initialize_answers_the_clients_revision_when_it_is_known_and_the_newest_otherwise() {
    let mut client = Client::start(&["alpha"], Options::default());
    let known = client.ask(1, "initialize", &json!({"protocolVersion": "2024-11-05"}));
    assert_eq!(known["result"]["protocolVersion"], "2024-11-05", "{known}");
    assert_eq!(known["result"]["serverInfo"]["name"], "bohio");
    assert_eq!(
        known["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    let unknown = client.ask(2, "initialize", &json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(
        unknown["result"]["protocolVersion"], "2025-11-25",
        "{unknown}"
    );
    assert!(
        !unknown["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("screenshot")
    );
    let ping = client.ask(3, "ping", &Value::Null);
    assert_eq!(ping["result"], json!({}));
    assert_eq!(
        client.dialed.load(Ordering::SeqCst),
        0,
        "no link is made until a call needs one"
    );
    let (result, rest) = client.finish();
    assert_eq!(result, Ok(()));
    assert!(rest.is_empty(), "{rest:?}");
}

#[test]
fn the_tools_listed_name_the_hosts_and_the_desktop_tools_only_when_asked_for() {
    let mut client = Client::start(&["alpha", "beta"], Options::default());
    let listed = client.ask(1, "tools/list", &Value::Null);
    let tools = listed["result"]["tools"].as_array().expect("tools");
    let named = |name: &str| tools.iter().find(|tool| tool["name"] == name);
    let exec = named("exec").expect("exec is listed");
    assert_eq!(
        exec["inputSchema"]["properties"]["host"]["enum"],
        json!(["alpha", "beta"])
    );
    assert!(
        exec["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("host")),
        "a server of two hosts needs the host named: {exec}"
    );
    assert!(named("screenshot").is_none(), "the desktop is not offered");
    assert_eq!(
        named("read").expect("read")["annotations"]["readOnlyHint"],
        true
    );
    drop(client.finish());

    let mut with_gui = Client::start(&["alpha"], Options::default().with_gui(true));
    let listed = with_gui.ask(1, "tools/list", &Value::Null);
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert!(tools.iter().any(|tool| tool["name"] == "screenshot"));
    let called = with_gui.ask(
        2,
        "tools/call",
        &json!({"name": "screenshot", "arguments": {}}),
    );
    assert_ne!(
        called["error"]["code"], -32602,
        "with the desktop on, a screenshot is a tool: {called}"
    );
    drop(with_gui.finish());
}

#[test]
fn a_call_makes_the_link_on_first_use_and_answers_the_tools_result() {
    let mut client = Client::start(&["alpha"], Options::default());
    let answer = client.ask(
        7,
        "tools/call",
        &json!({"name": "info", "arguments": {"host": "alpha"}}),
    );
    assert_eq!(answer["result"]["isError"], false, "{answer}");
    assert_eq!(answer["result"]["content"][0]["type"], "text");
    assert!(
        answer["result"]["structuredContent"].is_object(),
        "{answer}"
    );
    assert_eq!(client.dialed.load(Ordering::SeqCst), 1);
    // A second call, with the host left out (there is one), uses the same link.
    let again = client.ask(8, "tools/call", &json!({"name": "info"}));
    assert_eq!(again["result"]["isError"], false, "{again}");
    assert_eq!(client.dialed.load(Ordering::SeqCst), 1);
    let refused = client.ask(
        9,
        "tools/call",
        &json!({"name": "info", "arguments": {"host": "gamma"}}),
    );
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert_eq!(
        refused["result"]["structuredContent"]["code"],
        "bad_request"
    );
    drop(client.finish());
}

#[test]
fn a_call_that_is_cancelled_ends_and_says_so() {
    let mut client = Client::start(&["alpha"], Options::default());
    let started = Instant::now();
    client.send(&json!({
        "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": exec_sleep(20_000)
    }));
    // The link is made, and the program runs, before the request can be cancelled meaningfully.
    let deadline = Instant::now() + WAIT;
    while client.dialed.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the call never started");
        thread::sleep(Duration::from_millis(20));
    }
    client.send(&json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5}
    }));
    let answer = client.next();
    assert_eq!(answer["id"], 5, "{answer}");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the program was ended, not waited for"
    );
    let body = &answer["result"]["structuredContent"];
    assert_eq!(body["cancelled"], true, "{answer}");
    assert_eq!(body["exit"]["killed"], true, "{answer}");
    drop(client.finish());
}

#[test]
fn a_call_beyond_the_cap_is_answered_busy() {
    let mut client = Client::start(&["alpha"], Options::default().with_max_calls(1));
    client.send(&json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": exec_sleep(20_000)
    }));
    let deadline = Instant::now() + WAIT;
    while client.dialed.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "the call never started");
        thread::sleep(Duration::from_millis(20));
    }
    let busy = client.ask(2, "tools/call", &json!({"name": "info"}));
    assert_eq!(busy["result"]["isError"], true, "{busy}");
    assert_eq!(busy["result"]["structuredContent"]["code"], "busy");
    // Ending the input cancels the call that runs, and the server waits for it.
    let (result, rest) = client.finish();
    assert_eq!(result, Ok(()));
    assert_eq!(rest.len(), 1, "the cancelled call is answered: {rest:?}");
    assert_eq!(rest[0]["id"], 1);
}

#[test]
fn what_is_not_a_request_is_answered_with_the_protocols_error() {
    let mut client = Client::start(&["alpha"], Options::default());
    let method = client.ask(1, "resources/list", &Value::Null);
    assert_eq!(method["error"]["code"], -32601, "{method}");
    assert!(
        method["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resources/list")
    );
    let tool = client.ask(2, "tools/call", &json!({"name": "no_such_tool"}));
    assert_eq!(tool["error"]["code"], -32602, "{tool}");
    let nameless = client.ask(3, "tools/call", &json!({"arguments": {}}));
    assert_eq!(nameless["error"]["code"], -32602, "{nameless}");
    let arguments = client.ask(4, "tools/call", &json!({"name": "info", "arguments": 3}));
    assert_eq!(arguments["error"]["code"], -32602, "{arguments}");

    client.send_text("this is not JSON");
    assert_eq!(client.next()["error"]["code"], -32700);
    client.send_text("[]");
    assert_eq!(client.next()["error"]["code"], -32600);
    client.send_text("3");
    assert_eq!(client.next()["error"]["code"], -32600);
    client.send_text("");
    client.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let after = client.ask(5, "ping", &Value::Null);
    assert_eq!(after["result"], json!({}), "the server went on");
    drop(client.finish());
}

#[test]
fn a_server_needs_at_least_one_host_and_each_name_once() {
    let none = serve(
        Vec::new(),
        &Options::default(),
        std::io::empty(),
        Vec::new(),
    );
    assert!(none.is_err());
    let node = Arc::new(Running::start());
    let dialed = Arc::new(AtomicUsize::new(0));
    let host = |name: &str| Host {
        name: name.to_owned(),
        dial: Box::new(|| Err("no link in this test".to_owned())),
        settings: Settings::default(),
    };
    drop((node, dialed));
    let twice = serve(
        vec![host("a"), host("a")],
        &Options::default(),
        std::io::empty(),
        Vec::new(),
    );
    assert_eq!(twice, Err("two hosts are named `a`".to_owned()));
    // A host whose link cannot be made answers the call with why.
    let (reader, mut writer) = std::io::pipe().unwrap();
    writeln!(
        writer,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "info"}})
    )
    .unwrap();
    drop(writer);
    let written = Arc::new(Mutex::new(Vec::new()));
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serve(
        vec![host("a")],
        &Options::default(),
        BufReader::new(reader),
        Shared(Arc::clone(&written)),
    )
    .unwrap();
    let text = String::from_utf8(written.lock().unwrap().clone()).unwrap();
    let answer: Value = serde_json::from_str(text.lines().next().expect("an answer")).unwrap();
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert!(
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no link in this test"),
        "{answer}"
    );
}
