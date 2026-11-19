//! Control calls on a `bohio/1` stream: request and response with ids, so calls overlap.

use serde::{Deserialize, Serialize};

use crate::error::{ErrorBody, ErrorCode};
use crate::info::Info;

/// A call from the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Chosen by the client, unique on the stream; the response repeats it.
    pub id: u64,
    /// What is asked.
    pub call: Call,
}

/// What a request asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Call {
    /// Announces the client and the highest protocol version it speaks.
    Hello {
        /// The client's protocol version.
        #[serde(default)]
        version: u32,
        /// A name for logs; never a credential.
        #[serde(default)]
        client: String,
    },
    /// Asks for the host's [`Info`].
    Info,
}

/// The answer to one request: exactly one of `reply` and `error` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// The request id this answers.
    pub id: u64,
    /// The result of a call that succeeded.
    #[serde(default)]
    pub reply: Option<Reply>,
    /// Why a call failed.
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

impl Response {
    /// A success.
    pub const fn ok(id: u64, reply: Reply) -> Self {
        Self {
            id,
            reply: Some(reply),
            error: None,
        }
    }

    /// A failure.
    pub const fn failed(id: u64, error: ErrorBody) -> Self {
        Self {
            id,
            reply: None,
            error: Some(error),
        }
    }

    /// The reply, or the error the other side sent.
    ///
    /// # Errors
    ///
    /// The sent [`ErrorBody`]; a response with neither field is an `internal` error.
    pub fn into_result(self) -> Result<Reply, ErrorBody> {
        match (self.reply, self.error) {
            (Some(reply), None) => Ok(reply),
            (_, Some(error)) => Err(error),
            (None, None) => Err(ErrorBody::new(
                ErrorCode::Internal,
                "the response holds neither a reply nor an error",
            )),
        }
    }
}

/// What a successful call returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Reply {
    /// The host's side of a hello.
    Hello {
        /// The protocol version the host will speak: the lower of the two.
        version: u32,
    },
    /// The host's capabilities.
    Info(Box<Info>),
}
