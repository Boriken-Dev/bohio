//! The tools of the system a node is for: the state of its power and of its virtual media. They only read: an action that
//! changes power or media is for a person at `bohio remote`, never an agent.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cancel, Output, ToolError, Tools, parse};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceArgs {
    #[serde(default)]
    source: Option<String>,
}

pub(super) fn power_status(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: SourceArgs = parse(arguments)?;
    let state = crate::power::status(
        tools.link(),
        args.source.as_deref(),
        Some(tools.settings.reply_timeout),
    )?;
    Output::json(&json!({"state": state.state, "done": state.done}))
}

pub(super) fn media_status(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: SourceArgs = parse(arguments)?;
    let slots = crate::media::status(
        tools.link(),
        args.source.as_deref(),
        Some(tools.settings.reply_timeout),
    )?;
    Output::json(&json!({"slots": slots}))
}
