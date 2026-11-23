//! Looking at a host: searching file contents, its statistics, where its programs are, and following a growing file.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cancel, Capture, Output, ToolError, Tools, parse};
use crate::control::Control;
use crate::follow::{FollowEvent, FollowOptions, follow};
use crate::search::{SearchItem, SearchOptions, search_with};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    pattern: String,
    #[serde(default = "dot")]
    path: String,
    depth: Option<u64>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    case_insensitive: bool,
    context: Option<u64>,
    #[serde(default)]
    skip: u64,
    limit: Option<u64>,
}

fn dot() -> String {
    ".".to_owned()
}

pub(super) fn search(
    tools: &Tools,
    arguments: Value,
    cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: SearchArgs = parse(arguments)?;
    let limit = tools.settings.list.pick(args.limit);
    let options = SearchOptions {
        depth: args.depth,
        include: args.include,
        exclude: args.exclude,
        hidden: args.hidden,
        literal: args.literal,
        case_insensitive: args.case_insensitive,
        context: args.context.unwrap_or(0),
        // One more than the page: it says whether there is another.
        max_matches: Some(args.skip.saturating_add(limit).saturating_add(1)),
        max_files: None,
    };
    let mut iter = search_with(
        tools.link(),
        &[args.path.as_str()],
        &args.pattern,
        &options,
        tools.transfer_wait(),
    )?;
    let (mut matches, mut failures, mut seen, mut more) = (Vec::new(), Vec::new(), 0_u64, false);
    for item in iter.by_ref() {
        if cancel.is_cancelled() {
            return Err(ToolError::new("cancelled", "the search was cancelled"));
        }
        match item? {
            SearchItem::Match(found) => {
                seen += 1;
                if seen <= args.skip {
                    continue;
                }
                if matches.len() as u64 >= limit {
                    more = true;
                    break;
                }
                matches.push(json!({
                    "path": found.path,
                    "line": found.hit.line,
                    "text": found.hit.text,
                    "before": found.hit.before,
                    "after": found.hit.after,
                    "line_cut": found.hit.truncated,
                    "size": found.size,
                }));
            }
            SearchItem::Failure(error) => failures.push(json!({
                "path": error.path,
                "code": error.code,
                "message": error.message,
            })),
        }
    }
    let next = args.skip + matches.len() as u64;
    Output::json(&json!({
        "matches": matches,
        "failures": failures,
        "more": more,
        "next_skip": more.then_some(next),
        "done": iter.done(),
    }))
}

pub(super) fn stats(
    tools: &Tools,
    _arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let control = Control::open(tools.link())?;
    let stats = control.stats(Some(tools.settings.reply_timeout))?;
    control.close();
    Output::json(&stats)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WhichArgs {
    names: Vec<String>,
}

pub(super) fn which(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: WhichArgs = parse(arguments)?;
    let names: Vec<&str> = args.names.iter().map(String::as_str).collect();
    let control = Control::open(tools.link())?;
    let found = control.which(&names, Some(tools.settings.reply_timeout))?;
    control.close();
    Output::json(&json!({"found": found}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowArgs {
    path: String,
    duration_ms: Option<u64>,
    tail: Option<u64>,
    offset: Option<u64>,
    #[serde(default)]
    by_name: bool,
    max_output: Option<u64>,
}

pub(super) fn follow_file(
    tools: &Tools,
    arguments: Value,
    cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: FollowArgs = parse(arguments)?;
    let limit = usize::try_from(tools.settings.output.pick(args.max_output)).unwrap_or(usize::MAX);
    let length = Duration::from_millis(tools.settings.follow.pick(args.duration_ms));
    let options = FollowOptions {
        tail: args.tail,
        offset: args.offset.unwrap_or(0),
        by_name: args.by_name,
        interval: None,
    };
    let mut followed = follow(
        tools.link(),
        &args.path,
        &options,
        Some(tools.settings.reply_timeout),
    )?;
    let (size, begun) = (followed.size(), Instant::now());
    let mut output = Capture::new(limit);
    let mut truncations = Vec::new();
    while begun.elapsed() < length && !cancel.is_cancelled() {
        match followed.next_within(Some(tools.settings.poll)) {
            Ok(Some(FollowEvent::Data(bytes))) => output.push(&bytes),
            Ok(Some(FollowEvent::Truncated(cut))) => truncations.push(json!({
                "size": cut.size,
                "replaced": cut.replaced,
                "at_output_byte": output.pushed(),
            })),
            Ok(None) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Output::json(&json!({
        "path": args.path,
        "size_at_start": size,
        "output": output.finish(),
        "truncations": truncations,
        "watched_ms": u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}
