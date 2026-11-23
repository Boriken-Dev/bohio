//! `bohio mcp`: the agent tools over the Model Context Protocol, on standard input and output.
//!
//! The protocol is newline-delimited JSON-RPC 2.0 (the stdio transport), `tools` only. It is written here rather than taken
//! from an SDK: the SDKs run on an async runtime and this program runs one thread to a stream. The tools are
//! `bohio_client::tools`; this file only carries them: it makes one link to each named host on first use (and again, once,
//! after a loss), turns a call into a [`bohio_client::tools::Tools::call`] on a thread of its own, and writes the answer.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use bohio_client::dial::split_command;
use bohio_client::tools::{Availability, Cancel, REGISTRY, Settings, ToolError, Tools};
use bohio_client::{Error, Open};
use clap::Parser;
use serde_json::{Map, Value, json};

use super::remote::{Dialer, dialer};
use super::{McpArgs, RemoteArgs};
use crate::config::Environment;

/// The protocol revisions this server speaks, newest first. Revision `2025-11-25` is the latest at the time of writing
/// (modelcontextprotocol.io/specification); the server answers `initialize` with the client's revision when it is one of
/// these, else with the first.
const REVISIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// One host's arguments, as `bohio remote` takes them.
#[derive(Debug, Parser)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct HostLine {
    #[command(flatten)]
    args: RemoteArgs,
}

/// A link made when first needed and made again, once, when a stream cannot be opened on a lost one.
struct Lazy {
    dial: Dialer,
    current: Mutex<Option<(u64, Arc<dyn Open>)>>,
    generation: AtomicU64,
}

impl Lazy {
    fn link(&self) -> Result<(u64, Arc<dyn Open>), Error> {
        let mut current = self.current.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((generation, link)) = current.as_ref()
            && link.is_up()
        {
            return Ok((*generation, Arc::clone(link)));
        }
        let link = (self.dial)().map_err(Error::Link)?;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        *current = Some((generation, Arc::clone(&link)));
        Ok((generation, link))
    }
}

impl Open for Lazy {
    fn open(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<Box<dyn coqui_link::mux::LinkStream>, Error> {
        let (_, link) = self.link()?;
        match link.open(protocol, metadata) {
            Err(_) if !link.is_up() => {
                // The link was lost before this stream opened: nothing started, so opening on a new one repeats nothing.
                let (_, link) = self.link()?;
                link.open(protocol, metadata)
            }
            other => other,
        }
    }

    fn bulk_window(&self) -> Option<u32> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|(_, link)| link.bulk_window())
    }
}

/// A host the agent may name.
struct Host {
    name: String,
    lazy: Arc<Lazy>,
    settings: Settings,
    tools: Mutex<Option<(u64, Arc<Tools>)>>,
}

impl Host {
    /// The tools of the link as it is now; made again when the link was.
    fn tools(&self) -> Result<Arc<Tools>, ToolError> {
        let (generation, _) = self.lazy.link()?;
        let mut held = self.tools.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((made, tools)) = held.as_ref()
            && *made == generation
        {
            return Ok(Arc::clone(tools));
        }
        let tools = Arc::new(Tools::with_settings(
            Arc::clone(&self.lazy) as Arc<dyn Open>,
            self.settings.clone(),
        )?);
        *held = Some((generation, Arc::clone(&tools)));
        Ok(tools)
    }

    /// The tools when a call has made them, without making a link.
    fn made(&self) -> Option<Arc<Tools>> {
        self.tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(_, tools)| Arc::clone(tools))
    }
}

struct Server {
    hosts: Vec<Host>,
    out: Mutex<Box<dyn Write + Send>>,
    running: Mutex<HashMap<String, Cancel>>,
    active: AtomicUsize,
    max_calls: usize,
}

impl Server {
    fn send(&self, message: &Value) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        let line = message.to_string();
        // A client that is gone is found by the next read of standard input.
        let _ = out
            .write_all(line.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn fail(&self, id: &Value, code: i64, message: &str) {
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
        );
    }

    fn host(&self, name: Option<&str>) -> Result<&Host, ToolError> {
        match (name, self.hosts.as_slice()) {
            (Some(name), hosts) => hosts.iter().find(|host| host.name == name).ok_or_else(|| {
                let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
                ToolError::bad(format!(
                    "`{name}` is not a host this server serves: {}",
                    names.join(", ")
                ))
            }),
            (None, [only]) => Ok(only),
            (None, _) => Err(ToolError::bad(
                "name the host: `host` is required when the server serves more than one",
            )),
        }
    }

    /// Whether every host this server has heard from says its node serves no such thing (`[access] fs` or `exec` is
    /// `none`): such a tool is not listed at all, rather than listed as unavailable.
    fn served_nowhere(&self, spec: &bohio_client::tools::ToolSpec) -> bool {
        let heard: Vec<_> = self.hosts.iter().filter_map(|host| host.made()).collect();
        !heard.is_empty()
            && heard.iter().all(|tools| {
                matches!(
                    tools.availability(spec),
                    Availability::Unavailable(reason)
                        if reason.starts_with("this node serves no") || reason.starts_with("this node runs no")
                )
            })
    }

    fn list(&self) -> Value {
        let names: Vec<&str> = self.hosts.iter().map(|host| host.name.as_str()).collect();
        let tools: Vec<Value> = REGISTRY
            .iter()
            .filter(|spec| !self.served_nowhere(spec))
            .map(|spec| {
                let mut schema = (spec.schema)();
                if let Some(properties) =
                    schema.get_mut("properties").and_then(Value::as_object_mut)
                {
                    properties.insert(
                        "host".to_owned(),
                        json!({"type": "string", "enum": names, "description": "which host"}),
                    );
                }
                if names.len() > 1
                    && let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut)
                {
                    required.push(json!("host"));
                }
                // What a host has said it lacks is told where it is known, without making a link to find out.
                let unavailable: Vec<String> = self
                    .hosts
                    .iter()
                    .filter_map(|host| {
                        let tools = host.made()?;
                        match tools.availability(spec) {
                            Availability::Unavailable(reason) => {
                                Some(format!("{}: {reason}", host.name))
                            }
                            _ => None,
                        }
                    })
                    .collect();
                let mut description = spec.description.to_owned();
                if !unavailable.is_empty() {
                    description =
                        format!("UNAVAILABLE ({}). {description}", unavailable.join("; "));
                }
                json!({
                    "name": spec.name,
                    "description": description,
                    "inputSchema": schema,
                    "annotations": {
                        "readOnlyHint": spec.effect == bohio_client::tools::Effect::ReadOnly,
                        "openWorldHint": true,
                    },
                })
            })
            .collect();
        json!({"tools": tools})
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = asked
            .filter(|asked| REVISIONS.contains(asked))
            .unwrap_or(REVISIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "bohio", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Tools that act on remote hosts, bounded by what each host grants this server's key. Output is bounded and says what was cut off.",
        })
    }

    /// `tools/call`, on a thread of its own: the call may run for minutes and the client may ping, cancel or ask for more.
    fn call(self: &Arc<Self>, id: Value, params: Value) {
        let Some(name) = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            self.fail(&id, INVALID_PARAMS, "a tool call names the tool");
            return;
        };
        if !REGISTRY.iter().any(|spec| spec.name == name) {
            self.fail(&id, INVALID_PARAMS, &format!("Unknown tool: {name}"));
            return;
        }
        let mut arguments = match params.get("arguments") {
            Some(Value::Object(map)) => map.clone(),
            None | Some(Value::Null) => Map::new(),
            Some(_) => {
                self.fail(&id, INVALID_PARAMS, "`arguments` is an object");
                return;
            }
        };
        let host = arguments
            .remove("host")
            .and_then(|value| value.as_str().map(str::to_owned));
        if self.active.fetch_add(1, Ordering::SeqCst) >= self.max_calls {
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.reply(
                &id,
                failure(&ToolError::new(
                    "busy",
                    format!(
                        "{} calls are running: wait for one to finish",
                        self.max_calls
                    ),
                )),
            );
            return;
        }
        let cancel = Cancel::new();
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.to_string(), cancel.clone());
        let server = Arc::clone(self);
        thread::spawn(move || {
            let result = server
                .host(host.as_deref())
                .and_then(|host| host.tools())
                .and_then(|tools| tools.call(&name, Value::Object(arguments), &cancel));
            server
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id.to_string());
            server.active.fetch_sub(1, Ordering::SeqCst);
            let answer = match result {
                Ok(output) => success(&output),
                Err(error) => failure(&error),
            };
            server.reply(&id, answer);
        });
    }

    fn cancel_all(&self) {
        for cancel in self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            cancel.cancel();
        }
    }

    fn handle(self: &Arc<Self>, message: &Value) {
        let Some(object) = message.as_object() else {
            self.fail(&Value::Null, INVALID_REQUEST, "a message is a JSON object");
            return;
        };
        let id = object.get("id").cloned();
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            // A response to a request this server never made.
            if id.is_none() || !object.contains_key("result") && !object.contains_key("error") {
                self.fail(
                    &id.unwrap_or(Value::Null),
                    INVALID_REQUEST,
                    "a request names a method",
                );
            }
            return;
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            if method == "notifications/cancelled"
                && let Some(request) = params.get("requestId")
                && let Some(cancel) = self
                    .running
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&request.to_string())
            {
                cancel.cancel();
            }
            return;
        };
        match method {
            "initialize" => self.reply(&id, self.initialize(&params)),
            "ping" => self.reply(&id, json!({})),
            "tools/list" => self.reply(&id, self.list()),
            "tools/call" => self.call(id, params),
            other => self.fail(&id, METHOD_NOT_FOUND, &format!("no method `{other}`")),
        }
    }
}

/// A finished tool as a `tools/call` result.
fn success(output: &bohio_client::tools::Output) -> Value {
    let mut content = vec![json!({"type": "text", "text": output.value.to_string()})];
    for image in &output.images {
        content.push(json!({
            "type": "image",
            "data": bohio_client::tools::base64_encode(&image.data),
            "mimeType": image.mime,
        }));
    }
    json!({"content": content, "structuredContent": output.value, "isError": false})
}

/// A refused or failed tool as a `tools/call` result: a result, not a protocol error, so the agent reads why.
fn failure(error: &ToolError) -> Value {
    json!({
        "content": [{"type": "text", "text": error.to_string()}],
        "structuredContent": {"code": error.code, "message": error.message, "detail": error.detail},
        "isError": true,
    })
}

/// A host's `[NAME=]ARGS` as its name and the words of `bohio remote` it stands for.
fn host_words(spec: &str) -> Result<(String, Vec<String>), String> {
    let plain = |text: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    };
    if let Some((name, rest)) = spec.split_once('=')
        && plain(name)
        && !name.starts_with('-')
    {
        return Ok((name.to_owned(), split_command(rest)?));
    }
    if plain(spec) && !spec.starts_with('-') {
        // A bare name: `bohio remote NAME` resolves it.
        return Ok((spec.to_owned(), vec![spec.to_owned()]));
    }
    Ok(("host".to_owned(), split_command(spec)?))
}

/// Everything `bohio mcp` serves.
pub(super) fn run(args: &McpArgs, environment: &Environment) -> Result<ExitCode, String> {
    if args.host.is_empty() {
        return Err(
            "name at least one host with --host [NAME=]ENDPOINT [FLAGS of `bohio remote`]"
                .to_owned(),
        );
    }
    let mut hosts = Vec::new();
    for spec in &args.host {
        let (name, mut words) = host_words(spec)?;
        if hosts.iter().any(|host: &Host| host.name == name) {
            return Err(format!("two hosts are named `{name}`"));
        }
        // The action is the one a line needs to parse; the tools replace it.
        words.push("info".to_owned());
        let mut line = HostLine::try_parse_from(&words).map_err(|error| {
            format!(
                "--host `{name}`: {}",
                error.to_string().lines().next().unwrap_or("")
            )
        })?;
        if line.args.config.config.is_empty() {
            line.args.config.config.clone_from(&args.config.config);
        }
        let (dial, mut settings) = dialer(&line.args, environment)?;
        if let Some(bytes) = args.max_output {
            settings.output.default = bytes;
            settings.output.ceiling = settings.output.ceiling.max(bytes);
        }
        settings.local_roots.clone_from(&args.local_root);
        settings
            .forward_local_targets
            .clone_from(&args.forward_local_target);
        settings.forward_allow_remote_bind = args.forward_allow_remote_bind;
        hosts.push(Host {
            name,
            lazy: Arc::new(Lazy {
                dial,
                current: Mutex::new(None),
                generation: AtomicU64::new(0),
            }),
            settings,
            tools: Mutex::new(None),
        });
    }
    let server = Arc::new(Server {
        hosts,
        out: Mutex::new(Box::new(std::io::stdout())),
        running: Mutex::new(HashMap::new()),
        active: AtomicUsize::new(0),
        max_calls: args.max_calls.max(1),
    });
    serve(&server, std::io::stdin().lock());
    Ok(ExitCode::SUCCESS)
}

/// Reads messages until the client closes standard input, then ends what is running and waits for it.
fn serve(server: &Arc<Server>, input: impl BufRead) {
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(_)) => {
                server.fail(
                    &Value::Null,
                    INVALID_REQUEST,
                    "a batch is not supported: send one message a line",
                );
            }
            Ok(message) => server.handle(&message),
            Err(_) => server.fail(&Value::Null, PARSE_ERROR, "the message is not JSON"),
        }
    }
    server.cancel_all();
    while server.active.load(Ordering::SeqCst) > 0 {
        thread::sleep(std::time::Duration::from_millis(20));
    }
}
