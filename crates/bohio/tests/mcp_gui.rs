//! `bohio mcp --gui` driven as an MCP client does, against the `gui_host` example serving the painted test desktop: no real
//! screen is touched. The example prints each input event it takes (`event click:leftx1@0:160,100`), which is what the
//! tests compare against.

#![forbid(unsafe_code)]

mod support;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::tools::base64_decode;
use serde_json::{Value, json};
use support::procs::Node;
use tempfile::TempDir;

const WAIT: Duration = Duration::from_secs(60);

/// The `gui_host` example built beside the test executable.
fn gui_host_path() -> PathBuf {
    let exe = std::env::current_exe().expect("the test's own path");
    let name = format!("gui_host{}", std::env::consts::EXE_SUFFIX);
    exe.ancestors()
        .skip(1)
        .map(|directory| directory.join("examples").join(&name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "gui_host is not built: run `cargo build -p bohio --example gui_host` before a narrow cargo test (looked above {})",
                exe.display()
            )
        })
}

/// A host serving the painted desktop, and what it printed.
struct Host {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    id: String,
    endpoint: String,
}

impl Host {
    fn start(agent: &str, allow: &str) -> Self {
        let mut child = Command::new(gui_host_path())
            .args(["--scene", "painted", "--peer", agent, "--allow", allow])
            .args(["--listen", "tcp://127.0.0.1:0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("gui_host starts");
        let output = child.stdout.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&lines);
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                shared.lock().unwrap().push(line);
            }
        });
        let mut host = Self {
            child,
            lines,
            id: String::new(),
            endpoint: String::new(),
        };
        host.id = host.wait_line("id ");
        host.endpoint = host.wait_line("listening ");
        host
    }

    fn wait_line(&self, prefix: &str) -> String {
        let until = Instant::now() + WAIT;
        loop {
            if let Some(rest) = self
                .lines
                .lock()
                .unwrap()
                .iter()
                .find_map(|line| line.strip_prefix(prefix))
            {
                return rest.trim().to_owned();
            }
            assert!(Instant::now() < until, "gui_host never printed `{prefix}`");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The input events the desktop took so far, in order.
    fn events(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter_map(|line| line.strip_prefix("event "))
            .map(str::to_owned)
            .collect()
    }

    /// Waits until the desktop has taken `count` events, then returns them.
    fn events_after(&self, count: usize) -> Vec<String> {
        let until = Instant::now() + WAIT;
        loop {
            let events = self.events();
            if events.len() >= count {
                return events;
            }
            assert!(
                Instant::now() < until,
                "the desktop took {events:?}, not {count} events"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let input = self.input.as_mut().expect("standard input is open");
        writeln!(input, "{message}").expect("the child reads");
        input.flush().unwrap();
        loop {
            let line = match self.lines.recv_timeout(WAIT) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => panic!("the server said nothing for {WAIT:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("the server closed its output"),
            };
            let answer: Value =
                serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
            if answer["id"] == id {
                return answer;
            }
        }
    }

    fn initialize(&mut self) -> Value {
        let answer = self.request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "the test", "version": "1"}}),
        );
        let input = self.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        answer
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let answer = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        assert!(answer.get("error").is_none(), "{tool}: {answer}");
        answer["result"].clone()
    }

    /// A call that must succeed: its content blocks, and the result as JSON from the text block.
    fn ok(&mut self, tool: &str, arguments: Value) -> (Vec<Value>, Value) {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], false, "{tool}: {result}");
        let content = result["content"].as_array().unwrap().clone();
        let text = content
            .iter()
            .find(|block| block["type"] == "text")
            .and_then(|block| block["text"].as_str())
            .expect("a text block");
        let parsed: Value = serde_json::from_str(text).expect("the text is JSON");
        assert_eq!(parsed, result["structuredContent"], "{tool}");
        (content, parsed)
    }

    /// A call that must fail as a result: its code and message.
    fn refused(&mut self, tool: &str, arguments: Value) -> (String, String) {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], true, "{tool}: {result}");
        (
            result["structuredContent"]["code"]
                .as_str()
                .unwrap()
                .to_owned(),
            result["structuredContent"]["message"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        drop(self.input.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A host, an agent identity and a client running `bohio mcp` with `extra` flags.
struct World {
    host: Host,
    client: Client,
    _dir: TempDir,
}

fn world(allow: &str, extra: &[&str]) -> World {
    let dir = tempfile::tempdir().expect("a directory");
    let agent = Node::new(dir.path(), "agent");
    let host = Host::start(&agent.id, allow);
    let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
    command
        .arg("mcp")
        .args(["--config", &agent.config()])
        .env_remove("BOHIO_CONFIG")
        .env("BOHIO_LOG", "warn")
        .env("NO_COLOR", "1")
        .args([
            "--host",
            &format!("painted={} --server {}", host.endpoint, host.id),
        ])
        .args(extra);
    let mut client = Client::start(command);
    client.initialize();
    World {
        host,
        client,
        _dir: dir,
    }
}

/// The width and height a PNG says it has.
fn png_size(block: &Value) -> (u32, u32) {
    assert_eq!(block["type"], "image");
    assert_eq!(block["mimeType"], "image/png");
    let bytes = base64_decode(block["data"].as_str().unwrap()).expect("base64");
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let read = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
    (read(16), read(20))
}

const GUI_TOOLS: [&str; 13] = [
    "screenshot",
    "click",
    "move_pointer",
    "drag",
    "scroll",
    "type",
    "key",
    "wait",
    "displays",
    "cursor_position",
    "clipboard_read",
    "clipboard_write",
    "gui_watch",
];

#[test]
fn the_desktop_tools_are_listed_only_with_gui_and_every_schema_parses() {
    let mut off = world("view,input", &[]);
    let tools = off.client.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in GUI_TOOLS {
        assert!(!names.contains(&name), "{name} is listed without --gui");
    }
    let refused = off
        .client
        .request("tools/call", json!({"name": "screenshot", "arguments": {}}));
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(off.host.events().is_empty());

    let mut on = world("view,input", &["--gui"]);
    let listed = on.client.request("tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    for name in GUI_TOOLS {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed with --gui"));
        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{name}");
        assert_eq!(schema["additionalProperties"], false, "{name}");
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("host"), "{name}");
        for required in schema["required"].as_array().unwrap() {
            assert!(
                properties.contains_key(required.as_str().unwrap()),
                "{name}: {required}"
            );
        }
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        // A schema round-trips as JSON text: a client reads it as such.
        let again: Value = serde_json::from_str(&schema.to_string()).unwrap();
        assert_eq!(&again, schema);
    }
    let initialized = on.client.initialize();
    let instructions = initialized["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("untrusted input"), "{instructions}");
    // Reading a tool's effect: only the ones that read are marked so.
    let hint = |name: &str| {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| tool["annotations"]["readOnlyHint"].clone())
            .unwrap()
    };
    assert_eq!(hint("screenshot"), true);
    assert_eq!(hint("click"), false);
    assert_eq!(hint("type"), false);
}

#[test]
fn a_screenshot_is_an_image_of_the_fit_asked_with_its_scale() {
    let mut world = world("view,input", &["--gui"]);
    let (content, value) = world.client.ok(
        "screenshot",
        json!({"max_width": 160, "max_height": 100, "format": "png"}),
    );
    assert_eq!(content[0]["type"], "image", "the image comes first");
    assert_eq!(png_size(&content[0]), (160, 100));
    assert_eq!(value["width"], 160);
    assert_eq!(value["height"], 100);
    assert_eq!(value["scale"], 0.5);
    assert_eq!(value["display"], "0");
    assert_eq!(value["settled"], true);
    assert!(value["cursor"].is_object(), "{value}");

    // Without a size, the default fit applies and a smaller display is not scaled up.
    let (content, value) = world.client.ok("screenshot", json!({"display": "1"}));
    assert_eq!(png_size(&content[0]), (160, 100));
    assert_eq!(value["scale"], 1.0);
}

#[test]
fn a_click_at_screenshot_coordinates_lands_at_the_scaled_point() {
    let mut world = world("view,input", &["--gui"]);
    world
        .client
        .ok("screenshot", json!({"max_width": 160, "max_height": 100}));
    let (_, done) = world.client.ok("click", json!({"x": 80, "y": 50}));
    assert_eq!(done["applied"], 1);
    assert_eq!(world.host.events_after(1), ["click:leftx1@0:160,100"]);

    world.client.ok(
        "click",
        json!({"x": 40, "y": 25, "button": "right", "count": 2}),
    );
    assert_eq!(world.host.events_after(2)[1], "click:rightx2@0:80,50");
    world
        .client
        .ok("click", json!({"x": 7, "y": 9, "space": "display"}));
    assert_eq!(world.host.events_after(3)[2], "click:leftx1@0:7,9");
}

#[test]
fn keys_text_scroll_drag_move_and_wait_reach_the_desktop() {
    let mut world = world("view,input", &["--gui"]);
    world.client.ok("key", json!({"keys": "ctrl+s"}));
    world.client.ok("type", json!({"text": "hello"}));
    assert_eq!(
        world.host.events_after(4),
        [
            "key:control:press",
            "key:s:click",
            "key:control:release",
            "text:hello"
        ]
    );
    world.client.ok(
        "scroll",
        json!({"x": 5, "y": 6, "dy": -2, "space": "display"}),
    );
    world.client.ok(
        "move_pointer",
        json!({"x": 20, "y": 30, "space": "display"}),
    );
    world.client.ok(
        "drag",
        json!({"from": {"x": 1, "y": 2}, "to": {"x": 3, "y": 4}, "space": "display"}),
    );
    let events = world.host.events_after(9);
    assert_eq!(
        events[4..],
        [
            "scroll:0,-2",
            "move:0@20,30",
            "press:leftx1@0:1,2",
            "move:0@3,4",
            "release:leftx1@0:3,4"
        ]
    );
    let (_, waited) = world
        .client
        .ok("wait", json!({"quiet_ms": 100, "timeout_ms": 5000}));
    assert_eq!(waited["settled"], true, "{waited}");
    let (_, displays) = world.client.ok("displays", json!({}));
    assert_eq!(displays["displays"].as_array().unwrap().len(), 2);
    let (_, at) = world.client.ok("cursor_position", json!({}));
    assert_eq!((at["x"].as_i64(), at["y"].as_i64()), (Some(3), Some(4)));
    let (code, _) = world
        .client
        .refused("key", json!({"keys": "ctrl+nonsense"}));
    assert_eq!(code, "bad_request");
}

#[test]
fn a_point_outside_the_display_is_an_error_naming_the_bounds() {
    let mut world = world("view,input", &["--gui"]);
    let (code, message) = world.client.refused("click", json!({"x": 1, "y": 1}));
    assert_eq!(code, "no_screenshot", "{message}");
    world
        .client
        .ok("screenshot", json!({"max_width": 160, "max_height": 100}));
    let (code, message) = world.client.refused("click", json!({"x": 400, "y": 10}));
    assert_eq!(code, "out_of_bounds", "{message}");
    assert!(message.contains("160 by 100"), "{message}");
    assert!(message.contains("320 by 200"), "{message}");
    let (code, message) = world.client.refused(
        "move_pointer",
        json!({"x": 10, "y": 500, "space": "display"}),
    );
    assert_eq!(code, "out_of_bounds", "{message}");
    assert!(message.contains("y 0 to 199"), "{message}");
    assert!(world.host.events().is_empty());
}

#[test]
fn the_clipboard_without_its_grant_is_an_error_and_with_it_works() {
    let mut denied = world("view,input", &["--gui"]);
    let (code, message) = denied.client.refused("clipboard_read", json!({}));
    assert_eq!(code, "denied", "{message}");
    assert!(message.contains("gui.clipboard"), "{message}");
    let (code, _) = denied
        .client
        .refused("clipboard_write", json!({"text": "secret"}));
    assert_eq!(code, "denied");
    assert!(
        !denied
            .host
            .lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("clipboard ")),
        "nothing was written"
    );
    // The other tools still work on that link.
    denied.client.ok("screenshot", json!({}));

    let mut granted = world("view,input,clipboard", &["--gui"]);
    granted
        .client
        .ok("clipboard_write", json!({"text": "copied text"}));
    assert_eq!(granted.host.wait_line("clipboard "), "copied text");
    let (_, read) = granted.client.ok("clipboard_read", json!({}));
    assert_eq!(read["text"], "copied text");
}

#[test]
fn a_view_only_key_looks_and_cannot_drive() {
    let mut world = world("view", &["--gui"]);
    world.client.ok("screenshot", json!({}));
    for (tool, arguments) in [
        ("type", json!({"text": "x"})),
        ("key", json!({"keys": "a"})),
        ("click", json!({"x": 1, "y": 1, "space": "display"})),
    ] {
        let (code, message) = world.client.refused(tool, arguments);
        assert_eq!(code, "denied", "{tool}: {message}");
        assert!(message.contains("gui.input"), "{tool}: {message}");
    }
    assert!(world.host.events().is_empty());
}

#[test]
fn the_after_action_screenshot_and_the_auto_format_follow_the_flags() {
    let mut world = world(
        "view,input",
        &[
            "--gui",
            "--gui-after-action",
            "screenshot",
            "--gui-max-image-bytes",
            "64",
        ],
    );
    let result = world.client.call("type", json!({"text": "a"}));
    assert_eq!(result["isError"], false, "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "image", "{result}");
    assert_eq!(
        content[0]["mimeType"], "image/jpeg",
        "a PNG over the limit is JPEG"
    );
    assert_eq!(result["structuredContent"]["applied"], 1);
    assert_eq!(result["structuredContent"]["screenshot"]["width"], 320);
    // The picture that followed the input is the one the next click maps from.
    world.client.ok("click", json!({"x": 4, "y": 5}));
    assert_eq!(world.host.events_after(2)[1], "click:leftx1@0:4,5");
}
