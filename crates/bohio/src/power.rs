//! `bohio-power/1`: the power of the system a node is for, through a power source.
//!
//! [`serve`] checks the peer's grant, picks the power source, asks it (a status, or an action bounded by
//! `[power] action_timeout`) and answers once. The node's own event says who did what to which source and how it came
//! out; the source's reasons are the peer's answer, never what it was given.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use bohio_api::PowerSource;
use bohio_api::model::PowerState;
use bohio_proto::power::{Action, Answer, Request, State};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_peer::PeerId;
use coqui_transport::Channel;

use crate::access::{Sources, code_of};
use crate::config::PowerConfig;
use crate::limit::Slots;
use crate::link::{NodeEvent, NodeEvents};
use crate::policy::{Grant, Permission};

/// What [`serve`] needs besides the stream.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do.
    pub grant: &'a Grant,
    /// The `[power]` settings.
    pub config: &'a PowerConfig,
    /// The bound on concurrent streams, shared by every peer.
    pub slots: &'a Slots,
    /// The node's sources.
    pub sources: &'a Sources,
    /// The peer.
    pub peer: PeerId,
    /// Where the owner hears what happened.
    pub events: &'a dyn NodeEvents,
}

/// Runs `call` on a thread of its own and waits at most `limit` for it: a source that never answers cannot hold a stream.
/// A call that outlives the limit finishes on its own thread, and its answer is dropped.
pub(crate) fn within<T: Send + 'static>(
    limit: Duration,
    call: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = channel();
    thread::Builder::new()
        .name("bohio-source-call".to_owned())
        .spawn(move || {
            let _ = sender.send(call());
        })
        .ok()?;
    receiver.recv_timeout(limit).ok()
}

fn refuse(code: ErrorCode, message: &str) -> ErrorBody {
    ErrorBody::new(code, message)
}

/// Serves one `bohio-power/1` stream whose open carried `metadata`: one request, one answer.
///
/// Order of checks: decode (`bad_request`), `[power] enabled` (`unsupported`), the grant holds `power` (`denied`), the
/// source (`unsupported` when the node serves none, `not_found`, `denied` when the grant does not reach it), a free slot
/// (`busy`), then the source's own answer, or `timeout` when it took longer than `action_timeout`. A refused request gets
/// one `error` frame and the same error is returned.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer.
pub fn serve(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<State, ErrorBody> {
    let answered = decide(metadata, context);
    let frame = match &answered {
        Ok(state) => Answer::State(state.clone()),
        Err(error) => Answer::Error(error.clone()),
    }
    .encode()
    .unwrap_or_else(|_| vec![0x05]);
    let _ = channel.send(&frame);
    answered
}

fn decide(metadata: &[u8], context: &Context<'_>) -> Result<State, ErrorBody> {
    let request = Request::from_metadata(metadata)
        .map_err(|_| refuse(ErrorCode::BadRequest, "the request cannot be read"))?;
    let named = request.source.as_deref();
    let outcome = (|| {
        if !context.config.enabled {
            return Err(refuse(
                ErrorCode::Unsupported,
                "this host does not serve power",
            ));
        }
        if !context.grant.allows(Permission::Power) {
            return Err(refuse(ErrorCode::Denied, "this peer may not use power"));
        }
        let source = context.sources.power(named)?;
        if !context.sources.permits(context.grant, source.name()) {
            return Err(refuse(
                ErrorCode::Denied,
                "this peer may not use that power source",
            ));
        }
        let _permit = context.slots.acquire().ok_or_else(|| {
            refuse(
                ErrorCode::Busy,
                "the host is serving as many power requests as it allows",
            )
        })?;
        let name = source.name().to_owned();
        let action = request.action;
        let limit = context.config.action_timeout.0;
        let shared: Arc<dyn PowerSource> = source.arc();
        let answered = within(limit, move || match action {
            Action::Status => shared.state(),
            other => shared.act(other.name()),
        })
        .ok_or_else(|| {
            refuse(
                ErrorCode::Timeout,
                "the power source did not answer in time ([power] action_timeout)",
            )
        })?;
        let PowerState { state, done } = answered
            .map_err(|error| ErrorBody::new(code_of(error.status), error.message().to_string()))?;
        Ok((name, State { state, done }))
    })();
    let (source, result) = match outcome {
        Ok((name, state)) => (name, Ok(state)),
        Err(error) => (named.unwrap_or("").to_owned(), Err(error)),
    };
    context.events.event(&NodeEvent::Power {
        peer: context.peer,
        source,
        action: request.action.name().to_owned(),
        outcome: match &result {
            Ok(state) => state.state.clone(),
            Err(error) => format!("{:?}", error.code),
        },
    });
    result
}
