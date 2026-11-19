//! The control stream (`bohio/1`): hello, info and the small filesystem calls, one request and
//! one response per call.

use std::sync::Arc;

use bohio_proto::control::{Call, Reply, Request, Response};
use bohio_proto::info::Info;
use bohio_proto::{ErrorBody, ErrorCode, PROTOCOL_VERSION, decode, encode};
use coqui_transport::{Channel, Failure};

use crate::fs;
use crate::policy::Permission;
use crate::stream::{message_limit, send_waiting};

/// What [`serve`] needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The filesystem engines' context: the peer's grant, the `[filesystem]` settings and the
    /// transfer bound.
    pub filesystem: fs::Context<'a>,
    /// What `info` answers.
    pub info: &'a Info,
}

/// Serves one `bohio/1` stream until the peer closes it or it fails, answering each request in
/// the order it arrives; returns how many it answered.
///
/// A request that cannot be read is answered with id `0` and `bad_request`. `hello` needs no
/// permission, `info` needs `info`, and each filesystem call needs what [`fs::call`] checks. A
/// reply too large for the stream is replaced by an `internal` error. The stream is not closed.
pub fn serve(channel: &Arc<dyn Channel>, context: &Context<'_>) -> u64 {
    let poll = context.filesystem.config.poll_interval.0;
    let idle = context.filesystem.config.idle_timeout.0;
    let limit = message_limit(channel.as_ref());
    let mut answered = 0_u64;
    loop {
        let frame = match channel.receive(Some(poll)) {
            Ok(Some(frame)) => frame,
            Err(Failure::Timeout) if !channel.is_closed() => continue,
            _ => return answered,
        };
        let response = match decode::<Request>(&frame) {
            Ok(request) => {
                let id = request.id;
                match answer(context, &request.call, limit) {
                    Ok(reply) => Response::ok(id, reply),
                    Err(error) => Response::failed(id, error),
                }
            }
            Err(_) => Response::failed(
                0,
                ErrorBody::new(ErrorCode::BadRequest, "the request cannot be read"),
            ),
        };
        let id = response.id;
        let mut bytes = encode(&response).unwrap_or_default();
        if bytes.is_empty() || bytes.len() > limit {
            let error = ErrorBody::new(ErrorCode::Internal, "the reply does not fit the stream");
            bytes = encode(&Response::failed(id, error)).unwrap_or_default();
        }
        if !send_waiting(channel.as_ref(), &bytes, poll, idle) {
            return answered;
        }
        answered += 1;
    }
}

fn answer(context: &Context<'_>, call: &Call, limit: usize) -> Result<Reply, ErrorBody> {
    match call {
        Call::Hello { version, .. } => {
            if *version == 0 {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    "a protocol version is at least 1",
                ));
            }
            Ok(Reply::Hello {
                version: (*version).min(PROTOCOL_VERSION),
            })
        }
        Call::Info => {
            if !context.filesystem.grant.allows(Permission::Info) {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "this peer does not hold info",
                ));
            }
            Ok(Reply::Info(Box::new(context.info.clone())))
        }
        other => fs::call(&context.filesystem, other, limit),
    }
}
