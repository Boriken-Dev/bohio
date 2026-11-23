//! Forwards: a listener here that reaches a target through the host, and a listener there that reaches a target here.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use bohio_proto::forward::{Bind, Target};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cancel, Output, ToolError, Tools, lock, parse};
use crate::forward::{Ended, RemoteForward, listen_tcp};

/// The forwards a [`Tools`] holds open.
#[derive(Default)]
pub(crate) struct Held {
    next: u64,
    items: BTreeMap<u64, Forward>,
}

enum Forward {
    Local {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<Ended>>,
        listening: String,
        target: String,
        refused: Arc<AtomicU64>,
    },
    Remote {
        forward: RemoteForward,
        target: String,
        refused: Arc<AtomicU64>,
    },
}

impl Held {
    /// Ends every forward: listeners stop and their threads are joined.
    pub(crate) fn close_all(&mut self) {
        for (_, forward) in std::mem::take(&mut self.items) {
            forward.close();
        }
    }
}

impl Forward {
    fn close(self) {
        match self {
            Self::Local { stop, thread, .. } => {
                stop.store(true, Ordering::SeqCst);
                // The listener stops accepting; a connection it already carries runs to its end, so this does not wait for it.
                drop(thread);
            }
            Self::Remote { forward, .. } => forward.close(),
        }
    }
}

impl Drop for Tools {
    fn drop(&mut self) {
        lock(&self.forwards).close_all();
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalArgs {
    listen: Option<String>,
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteArgs {
    bind: Option<String>,
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseArgs {
    id: u64,
}

impl Tools {
    fn add(&self, forward: Forward) -> Result<u64, ToolError> {
        let mut held = lock(&self.forwards);
        // A listener that ended on its own no longer counts.
        let ended: Vec<u64> = held
            .items
            .iter()
            .filter(|(_, forward)| ended_text(forward).is_some())
            .map(|(id, _)| *id)
            .collect();
        for id in ended {
            if let Some(gone) = held.items.remove(&id) {
                gone.close();
            }
        }
        if held.items.len() >= self.settings.max_forwards {
            forward.close();
            return Err(ToolError::new(
                "busy",
                format!(
                    "{} forwards are open already: close one",
                    self.settings.max_forwards
                ),
            ));
        }
        held.next += 1;
        let id = held.next;
        held.items.insert(id, forward);
        Ok(id)
    }
}

fn ended_text(forward: &Forward) -> Option<String> {
    match forward {
        Forward::Local { thread, .. } => thread
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
            .then(|| "the listener ended".to_owned()),
        Forward::Remote { forward, .. } => forward.ended().map(|ended| format!("{ended:?}")),
    }
}

pub(super) fn forward_local(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: LocalArgs = parse(arguments)?;
    let target: Target = args
        .target
        .parse()
        .map_err(|error: bohio_proto::forward::BadTarget| ToolError::bad(error.to_string()))?;
    let wanted: SocketAddr = args
        .listen
        .as_deref()
        .unwrap_or("127.0.0.1:0")
        .parse()
        .map_err(|_| ToolError::bad("`listen` is an address and a port such as 127.0.0.1:0"))?;
    if !wanted.ip().is_loopback() && !tools.settings.forward_allow_remote_bind {
        return Err(ToolError::new(
            "denied",
            "a listener beyond loopback gives whoever reaches it this peer's grant: only loopback is allowed here",
        ));
    }
    let listener = TcpListener::bind(wanted).map_err(|error| {
        ToolError::new(
            "io",
            format!("cannot listen on {wanted}: {:?}", error.kind()),
        )
    })?;
    let bound = listener
        .local_addr()
        .map_err(|_| ToolError::new("io", "the listener has no address"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let refused = Arc::new(AtomicU64::new(0));
    let thread = std::thread::spawn({
        let (link, stop, refused) = (
            Arc::clone(&tools.link),
            Arc::clone(&stop),
            Arc::clone(&refused),
        );
        let (target, settings) = (target.clone(), tools.settings.forward);
        move || {
            let report = move |_: &crate::Error| {
                refused.fetch_add(1, Ordering::SeqCst);
            };
            listen_tcp(&*link, &listener, &target, &settings, &stop, &report)
                .unwrap_or(Ended::Closed)
        }
    });
    let id = tools.add(Forward::Local {
        stop,
        thread: Some(thread),
        listening: bound.to_string(),
        target: target.to_string(),
        refused,
    })?;
    Output::json(&json!({"id": id, "listening": bound.to_string(), "target": target.to_string()}))
}

pub(super) fn forward_remote(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: RemoteArgs = parse(arguments)?;
    let local: Target = args
        .target
        .parse()
        .map_err(|error: bohio_proto::forward::BadTarget| ToolError::bad(error.to_string()))?;
    let allowed = tools
        .settings
        .forward_local_targets
        .iter()
        .any(|rule| rule.parse::<Target>().is_ok_and(|rule| rule == local));
    if !allowed {
        return Err(ToolError::new(
            "denied",
            "that target of this machine is not one a remote forward may reach",
        ));
    }
    let bind: Bind = args
        .bind
        .as_deref()
        .unwrap_or("127.0.0.1:0")
        .parse()
        .map_err(|error: bohio_proto::forward::BadTarget| ToolError::bad(error.to_string()))?;
    let refused = Arc::new(AtomicU64::new(0));
    let report: Arc<dyn Fn(&crate::Error) + Send + Sync> = {
        let refused = Arc::clone(&refused);
        Arc::new(move |_| {
            refused.fetch_add(1, Ordering::SeqCst);
        })
    };
    let forward = RemoteForward::open(
        Arc::clone(&tools.link),
        &bind,
        &local,
        &tools.settings.forward,
        report,
    )?;
    let address = forward.address().to_owned();
    let id = tools.add(Forward::Remote {
        forward,
        target: local.to_string(),
        refused,
    })?;
    Output::json(&json!({"id": id, "address": address, "target": local.to_string()}))
}

pub(super) fn forward_list(
    tools: &Tools,
    _arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let held = lock(&tools.forwards);
    let listed: Vec<Value> = held
        .items
        .iter()
        .map(|(id, forward)| match forward {
            Forward::Local {
                listening,
                target,
                refused,
                ..
            } => json!({
                "id": id, "direction": "local", "listening": listening, "target": target,
                "refused_connections": refused.load(Ordering::SeqCst), "ended": ended_text(forward),
            }),
            Forward::Remote {
                forward: remote,
                target,
                refused,
            } => json!({
                "id": id, "direction": "remote", "address": remote.address(), "target": target,
                "refused_connections": refused.load(Ordering::SeqCst), "ended": ended_text(forward),
            }),
        })
        .collect();
    Output::json(&json!({"forwards": listed}))
}

pub(super) fn forward_close(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: CloseArgs = parse(arguments)?;
    let forward = lock(&tools.forwards).items.remove(&args.id);
    match forward {
        Some(forward) => {
            forward.close();
            Output::json(&json!({"id": args.id, "closed": true}))
        }
        None => Err(ToolError::new("not_found", "no forward has that id")),
    }
}
