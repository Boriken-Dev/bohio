//! Files and trees: reading, writing, editing, listing, hashing, walking, moving and transferring.

use std::path::{Component, Path, PathBuf};

use bohio_proto::fs::{DeltaBase, ReadItem, ReadRequest, WriteMode, WriteRequest};
use bohio_proto::hash::{Algorithm, Digests};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    Cancel, Output, ToolError, Tools, base64_decode, base64_encode, encoded, parse, text_cut,
};
use crate::bundle::{BundleFile, BundleOptions, Fetch, put_many, read_bundle};
use crate::control::Control;
use crate::fs::{Fs, Reader, Writer};
use crate::hash::hash_file;
use crate::sync::{GetOptions, PutOptions, get, put};
use crate::walk::{WalkItem, WalkOptions, walk_with};

impl Tools {
    fn fs(&self) -> Result<Fs, ToolError> {
        Ok(Fs::connect(self.link())?.with_timeout(Some(self.settings.reply_timeout)))
    }
}

fn sha256(digests: &Digests) -> Option<&String> {
    digests.get(&Algorithm::Sha256)
}

fn bytes_of(
    text: Option<&String>,
    base64: Option<&String>,
    what: &str,
    max: u64,
) -> Result<Vec<u8>, ToolError> {
    let bytes = match (text, base64) {
        (Some(_), Some(_)) => {
            return Err(ToolError::bad(format!(
                "give `{what}` as text or as base64, not both"
            )));
        }
        (Some(text), None) => text.as_bytes().to_vec(),
        (None, Some(data)) => base64_decode(data)
            .ok_or_else(|| ToolError::bad(format!("`{what}_base64` is not base64")))?,
        (None, None) => Vec::new(),
    };
    if bytes.len() as u64 > max {
        return Err(ToolError::new(
            "too_large",
            format!(
                "{} bytes is more than the {max} one call may send: use `put` for a file of this machine",
                bytes.len()
            ),
        ));
    }
    Ok(bytes)
}

// ---- info -----------------------------------------------------------------------------------------------------------

pub(super) fn info(
    tools: &Tools,
    _arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let control = Control::open(tools.link())?;
    let info = control.info(Some(tools.settings.reply_timeout))?;
    control.close();
    Output::json(&info)
}

// ---- read -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset_line: Option<u64>,
    limit_lines: Option<u64>,
    max_bytes: Option<u64>,
    encoding: Option<String>,
    offset: Option<u64>,
    length: Option<u64>,
}

/// The lines of a file from one on, as many as the limits let through.
struct Window {
    skip: u64,
    limit: u64,
    max: usize,
    buf: Vec<u8>,
    lines: u64,
    skipped: u64,
    done: bool,
    full: bool,
    partial: bool,
}

impl Window {
    /// Takes `chunk`; returns how many of its bytes were used.
    fn feed(&mut self, chunk: &[u8]) -> usize {
        let mut used = 0;
        while used < chunk.len() && !self.done {
            let rest = &chunk[used..];
            let end = rest.iter().position(|byte| *byte == b'\n');
            if self.skip > 0 {
                if let Some(at) = end {
                    self.skip -= 1;
                    self.skipped += 1;
                    used += at + 1;
                } else {
                    used = chunk.len();
                }
                continue;
            }
            let take = end.map_or(rest.len(), |at| at + 1);
            let room = self.max - self.buf.len();
            if take > room {
                self.full = true;
                self.done = true;
                if self.lines == 0 {
                    let cut = text_cut(&rest[..take], room);
                    self.buf.extend_from_slice(&rest[..cut]);
                    self.partial = true;
                    used += cut;
                } else if let Some(last) = self.buf.iter().rposition(|byte| *byte == b'\n') {
                    // A line that does not fit is left for the next page; what is of it already held is dropped.
                    self.buf.truncate(last + 1);
                }
                break;
            }
            self.buf.extend_from_slice(&rest[..take]);
            used += take;
            if end.is_some() {
                self.lines += 1;
                if self.lines >= self.limit {
                    self.done = true;
                }
            }
        }
        used
    }
}

pub(super) fn read(tools: &Tools, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
    let args: ReadArgs = parse(arguments)?;
    let settings = &tools.settings;
    let max_bytes = settings.output.pick(args.max_bytes);
    let binary = match args.encoding.as_deref() {
        None | Some("text") => false,
        Some("base64") => true,
        Some(_) => return Err(ToolError::bad("`encoding` is `text` or `base64`")),
    };
    let mut request = ReadRequest::whole(args.path.clone());
    request.hashes = vec![Algorithm::Sha256];
    if binary {
        request.offset = args.offset.unwrap_or(0);
        request.length = Some(
            args.length
                .map_or(max_bytes, |length| length.min(max_bytes)),
        );
        let reader = Reader::start(tools.link(), &request, Some(settings.reply_timeout))?;
        let header = reader.header();
        let bytes = reader.read_to_end(tools.transfer_wait())?;
        let end = request.offset + bytes.len() as u64;
        return Output::json(&json!({
            "path": args.path,
            "encoding": "base64",
            "data": base64_encode(&bytes),
            "offset": request.offset,
            "length": bytes.len(),
            "size": header.size,
            "more": end < header.size,
            "next_offset": (end < header.size).then_some(end),
            "sha256": (request.offset == 0 && end == header.size)
                .then(|| reader.digests().and_then(|d| sha256(&d).cloned()))
                .flatten(),
        }));
    }
    if args.offset.is_some() || args.length.is_some() {
        return Err(ToolError::bad(
            "`offset` and `length` are bytes, for `encoding: base64`; a text read takes `offset_line` and `limit_lines`",
        ));
    }
    let first = args.offset_line.unwrap_or(1);
    if first == 0 {
        return Err(ToolError::bad("`offset_line` counts from 1"));
    }
    let reader = Reader::start(tools.link(), &request, Some(settings.reply_timeout))?;
    let size = reader.header().size;
    let mut window = Window {
        skip: first - 1,
        limit: settings.lines.pick(args.limit_lines),
        max: usize::try_from(max_bytes).unwrap_or(usize::MAX),
        buf: Vec::new(),
        lines: 0,
        skipped: 0,
        done: false,
        full: false,
        partial: false,
    };
    let mut position = 0_u64;
    let mut complete = false;
    while !window.done {
        if cancel.is_cancelled() {
            return Err(ToolError::new("cancelled", "the read was cancelled"));
        }
        match reader.next_chunk(tools.transfer_wait())? {
            Some(chunk) => {
                position += window.feed(&chunk) as u64;
            }
            None => {
                complete = true;
                break;
            }
        }
    }
    if complete && !window.buf.is_empty() && window.buf.last() != Some(&b'\n') {
        // The last line has no end of line.
        window.lines += 1;
    }
    let text = match String::from_utf8(window.buf) {
        Ok(text) if !text.contains('\0') => text,
        _ => {
            return Err(ToolError::new(
                "not_text",
                format!(
                    "the file is not UTF-8 text ({size} bytes): read it with `encoding: base64`"
                ),
            )
            .with_detail(json!({"size": size})));
        }
    };
    let more = !complete;
    let whole = first == 1 && complete;
    let next_line = if window.partial {
        Some(first + 1)
    } else {
        more.then_some(first + window.lines)
    };
    let sha = if whole {
        reader.digests().and_then(|d| sha256(&d).cloned())
    } else {
        None
    };
    let _ = position;
    Output::json(&json!({
        "path": args.path,
        "text": text,
        "first_line": first,
        "lines": window.lines.max(u64::from(window.partial)),
        "total_lines": complete.then_some(window.skipped + window.lines),
        "size": size,
        "more": more,
        "next_offset_line": next_line,
        "line_cut": window.partial,
        "truncated_by_bytes": window.full,
        "sha256": sha,
    }))
}

// ---- write and edit -------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    text: Option<String>,
    base64: Option<String>,
    #[serde(default)]
    create_only: bool,
    #[serde(default)]
    parents: bool,
    if_sha256: Option<String>,
}

pub(super) fn write(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: WriteArgs = parse(arguments)?;
    let bytes = bytes_of(
        args.text.as_ref(),
        args.base64.as_ref(),
        "text",
        tools.settings.max_text,
    )?;
    if args.text.is_none() && args.base64.is_none() {
        return Err(ToolError::bad("give `text` or `base64`"));
    }
    let mut request = WriteRequest::replace(args.path.clone());
    if args.create_only {
        request.mode = WriteMode::Create;
    }
    request.parents = args.parents;
    request.hashes = Some(vec![Algorithm::Sha256]);
    if let Some(wanted) = &args.if_sha256 {
        if args.create_only {
            return Err(ToolError::bad(
                "`if_sha256` is for a file that exists: not with `create_only`",
            ));
        }
        let seen = tools.fs()?.stat(&args.path, true)?;
        request.base = Some(DeltaBase {
            size: seen.size,
            modified: seen.modified,
            hashes: Digests::from([(Algorithm::Sha256, wanted.to_ascii_lowercase())]),
        });
    }
    let done = send(tools, &request, &bytes)?;
    Output::json(&json!({
        "path": args.path,
        "written": done.written,
        "sha256": sha256(&done.hashes),
    }))
}

fn send(
    tools: &Tools,
    request: &WriteRequest,
    bytes: &[u8],
) -> Result<bohio_proto::fs::WriteDone, ToolError> {
    let writer = Writer::start(tools.link(), request, Some(tools.settings.reply_timeout))?;
    for chunk in bytes.chunks(Writer::DEFAULT_CHUNK) {
        writer.write(chunk)?;
    }
    Ok(writer.finish(tools.transfer_wait())?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
}

pub(super) fn edit(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: EditArgs = parse(arguments)?;
    if args.old.is_empty() {
        return Err(ToolError::bad("`old` is the text to replace, not empty"));
    }
    if args.old == args.new {
        return Err(ToolError::bad(
            "`old` and `new` are the same: nothing would change",
        ));
    }
    let fs = tools.fs()?;
    let seen = fs.stat(&args.path, true)?;
    if seen.size > tools.settings.max_text {
        return Err(ToolError::new(
            "too_large",
            format!(
                "the file is {} bytes, more than the {} an edit reads",
                seen.size, tools.settings.max_text
            ),
        ));
    }
    let mut request = ReadRequest::whole(args.path.clone());
    request.hashes = vec![Algorithm::Sha256];
    let reader = Reader::start(tools.link(), &request, Some(tools.settings.reply_timeout))?;
    let bytes = reader.read_to_end(tools.transfer_wait())?;
    let digests = reader.digests().unwrap_or_default();
    let text = String::from_utf8(bytes).map_err(|error| {
        ToolError::new(
            "not_text",
            format!(
                "the file is not UTF-8 text ({} bytes)",
                error.as_bytes().len()
            ),
        )
        .with_detail(json!({"size": error.as_bytes().len()}))
    })?;
    let count = text.matches(&args.old).count();
    if count == 0 {
        return Err(ToolError::new(
            "no_match",
            "`old` is not in the file: read it again and copy the text exactly",
        )
        .with_detail(json!({"matches": 0})));
    }
    if count > 1 && !args.replace_all {
        return Err(ToolError::new(
            "ambiguous",
            format!(
                "`old` occurs {count} times: add context to make it unique, or set `replace_all`"
            ),
        )
        .with_detail(json!({"matches": count})));
    }
    let edited = text.replace(&args.old, &args.new);
    let mut write = WriteRequest::replace(args.path.clone());
    write.hashes = Some(vec![Algorithm::Sha256]);
    // The host replaces nothing when the file is no longer the one that was read.
    write.base = Some(DeltaBase {
        size: text.len() as u64,
        modified: seen.modified,
        hashes: digests,
    });
    let done = send(tools, &write, edited.as_bytes())?;
    Output::json(&json!({
        "path": args.path,
        "replacements": count,
        "written": done.written,
        "sha256": sha256(&done.hashes),
    }))
}

// ---- listing --------------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: String,
    after: Option<String>,
    limit: Option<u64>,
}

pub(super) fn list(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: ListArgs = parse(arguments)?;
    let limit = tools.settings.list.pick(args.limit);
    let listing = tools
        .fs()?
        .list(&args.path, args.after.as_deref(), Some(limit))?;
    Output::json(&json!({
        "path": args.path,
        "entries": listing.entries,
        "next": listing.next,
        "more": listing.next.is_some(),
        "skipped": listing.skipped,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatArgs {
    path: String,
    #[serde(default = "yes")]
    follow: bool,
}

const fn yes() -> bool {
    true
}

pub(super) fn stat(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: StatArgs = parse(arguments)?;
    let metadata = tools.fs()?.stat(&args.path, args.follow)?;
    Output::json(&json!({"path": args.path, "metadata": metadata}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HashArgs {
    path: String,
    algorithms: Option<Vec<String>>,
}

pub(super) fn hash(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: HashArgs = parse(arguments)?;
    let algorithms: Vec<Algorithm> = args
        .algorithms
        .unwrap_or_else(|| vec!["sha256".to_owned()])
        .iter()
        .map(|name| Algorithm::from_name(name))
        .collect();
    let digests = hash_file(tools.link(), &args.path, &algorithms)?;
    let named: serde_json::Map<String, Value> = digests
        .iter()
        .map(|(algorithm, hex)| (algorithm.name().to_owned(), json!(hex)))
        .collect();
    Output::json(&json!({"path": args.path, "hashes": named}))
}

// ---- walk and glob --------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WalkArgs {
    roots: Vec<String>,
    depth: Option<u64>,
    #[serde(default)]
    follow: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    hashes: Vec<String>,
    #[serde(default)]
    skip: u64,
    limit: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    #[serde(default = "dot")]
    path: String,
    #[serde(default)]
    skip: u64,
    limit: Option<u64>,
}

fn dot() -> String {
    ".".to_owned()
}

fn walked(
    tools: &Tools,
    args: &WalkArgs,
    cancel: &Cancel,
    only_matches: bool,
) -> Result<Output, ToolError> {
    if args.roots.is_empty() {
        return Err(ToolError::bad("name at least one root"));
    }
    let limit = tools.settings.walk.pick(args.limit);
    let roots: Vec<&str> = args.roots.iter().map(String::as_str).collect();
    let options = WalkOptions {
        depth: args.depth,
        follow: args.follow,
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        hashes: args
            .hashes
            .iter()
            .map(|name| Algorithm::from_name(name))
            .collect(),
        compression: None,
        ..WalkOptions::default()
    };
    let mut iter = walk_with(tools.link(), &roots, &options, tools.transfer_wait())?;
    let (mut entries, mut failures, mut seen) = (Vec::new(), Vec::new(), 0_u64);
    let mut more = false;
    for item in iter.by_ref() {
        if cancel.is_cancelled() {
            return Err(ToolError::new("cancelled", "the walk was cancelled"));
        }
        match item? {
            WalkItem::Entry { root, entry } => {
                // A root is listed first whatever the globs say; a search for matches is not told of it.
                if only_matches && entry.path.is_empty() {
                    continue;
                }
                seen += 1;
                if seen <= args.skip {
                    continue;
                }
                if entries.len() as u64 >= limit {
                    more = true;
                    break;
                }
                entries.push(json!({
                    "root": root,
                    "path": entry.path,
                    "metadata": entry.metadata,
                    "target": entry.target,
                    "hashes": entry.hashes,
                }));
            }
            WalkItem::Failure(error) => failures.push(json!({
                "root": error.root,
                "path": error.path,
                "code": error.code,
                "message": error.message,
            })),
        }
    }
    let next = args.skip + entries.len() as u64;
    Output::json(&json!({
        "entries": entries,
        "failures": failures,
        "more": more,
        "next_skip": more.then_some(next),
    }))
}

pub(super) fn walk(tools: &Tools, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
    let args: WalkArgs = parse(arguments)?;
    walked(tools, &args, cancel, false)
}

pub(super) fn glob(tools: &Tools, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
    let args: GlobArgs = parse(arguments)?;
    let walk = WalkArgs {
        roots: vec![args.path],
        depth: None,
        follow: false,
        include: vec![args.pattern],
        exclude: Vec::new(),
        hashes: Vec::new(),
        skip: args.skip,
        limit: args.limit,
    };
    walked(tools, &walk, cancel, true)
}

// ---- several files --------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadManyArgs {
    paths: Vec<String>,
    max_bytes: Option<u64>,
}

pub(super) fn read_many(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: ReadManyArgs = parse(arguments)?;
    let budget = tools.settings.bundle.pick(args.max_bytes);
    let items: Vec<ReadItem> = args.paths.iter().map(ReadItem::whole).collect();
    let mut results: Vec<Value> = Vec::new();
    let mut held: Vec<u8> = Vec::new();
    let (mut used, mut keep, mut size) = (0_u64, false, 0_u64);
    let options = BundleOptions {
        timeout: tools.transfer_wait(),
        ..BundleOptions::default()
    };
    let paths = &args.paths;
    read_bundle(tools.link(), &items, &options, &mut |fetch| {
        match fetch {
            Fetch::Begin(header) => {
                size = header.length;
                held.clear();
                keep = used + size <= budget;
            }
            Fetch::Data(bytes) => {
                if keep {
                    held.extend_from_slice(bytes);
                }
            }
            Fetch::End { index, hashes } => {
                let path = usize::try_from(index).ok().and_then(|i| paths.get(i));
                let mut value = if keep {
                    used += size;
                    encoded(&held)
                } else {
                    json!({"skipped": "this call has no room for the file: read it with `read`"})
                };
                value["path"] = json!(path);
                value["size"] = json!(size);
                value["sha256"] = json!(sha256(&hashes));
                results.push(value);
            }
            Fetch::Failed { index, error } => {
                let path = usize::try_from(index).ok().and_then(|i| paths.get(i));
                results.push(json!({
                    "path": path,
                    "error": {"code": ToolError::from(error.clone()).code, "message": error.message},
                }));
            }
        }
        Ok(())
    })?;
    Output::json(&json!({"files": results, "bytes": used, "budget": budget}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileArgs {
    path: String,
    text: Option<String>,
    base64: Option<String>,
    #[serde(default)]
    create_only: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteManyArgs {
    files: Vec<FileArgs>,
    #[serde(default)]
    parents: bool,
}

pub(super) fn write_many(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: WriteManyArgs = parse(arguments)?;
    let mut contents: Vec<std::io::Cursor<Vec<u8>>> = Vec::new();
    let mut total = 0_u64;
    for file in &args.files {
        let bytes = bytes_of(
            file.text.as_ref(),
            file.base64.as_ref(),
            "text",
            tools.settings.max_text,
        )?;
        total += bytes.len() as u64;
        if total > tools.settings.max_text {
            return Err(ToolError::new(
                "too_large",
                format!(
                    "the files hold more than the {} one call may send",
                    tools.settings.max_text
                ),
            ));
        }
        contents.push(std::io::Cursor::new(bytes));
    }
    let mut files: Vec<BundleFile<'_>> = Vec::new();
    for (file, source) in args.files.iter().zip(contents.iter_mut()) {
        let mut entry = BundleFile::replace(file.path.clone(), source);
        if file.create_only {
            entry.item.mode = WriteMode::Create;
        }
        entry.item.parents = args.parents;
        files.push(entry);
    }
    let options = BundleOptions {
        timeout: tools.transfer_wait(),
        ..BundleOptions::default()
    };
    let put = put_many(tools.link(), &mut files, &options)?;
    let results: Vec<Value> = args
        .files
        .iter()
        .zip(put.results)
        .map(|(file, result)| match result {
            Ok(done) => {
                json!({"path": file.path, "written": done.written, "sha256": sha256(&done.hashes)})
            }
            Err(error) => json!({
                "path": file.path,
                "error": {"code": ToolError::from(error.clone()).code, "message": error.message},
            }),
        })
        .collect();
    Output::json(&json!({"files": results}))
}

// ---- one-call changes -----------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TwoPaths {
    from: String,
    to: String,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    preserve: bool,
}

pub(super) fn move_to(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: TwoPaths = parse(arguments)?;
    tools.fs()?.move_to(&args.from, &args.to, args.overwrite)?;
    Output::json(&json!({"from": args.from, "to": args.to}))
}

pub(super) fn copy(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: TwoPaths = parse(arguments)?;
    let options = crate::fs::CopyOptions {
        overwrite: args.overwrite,
        recursive: args.recursive,
        preserve: args.preserve,
    };
    tools.fs()?.copy_with(&args.from, &args.to, &options)?;
    Output::json(&json!({"from": args.from, "to": args.to}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TruncateArgs {
    path: String,
    length: u64,
}

pub(super) fn truncate(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: TruncateArgs = parse(arguments)?;
    tools.fs()?.truncate(&args.path, args.length)?;
    Output::json(&json!({"path": args.path, "length": args.length}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
    #[serde(default = "yes")]
    parents: bool,
}

pub(super) fn mkdir(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: MkdirArgs = parse(arguments)?;
    tools.fs()?.mkdir(&args.path, args.parents)?;
    Output::json(&json!({"path": args.path}))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    path: String,
    #[serde(default)]
    recursive: bool,
}

pub(super) fn remove(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: RemoveArgs = parse(arguments)?;
    tools.fs()?.remove(&args.path, args.recursive)?;
    Output::json(&json!({"path": args.path}))
}

// ---- files of this machine ------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferArgs {
    local: String,
    remote: String,
}

/// `path` as a place under one of the allowed roots, looked at with links resolved: a link out of a root is not under it.
fn confined(tools: &Tools, path: &str) -> Result<PathBuf, ToolError> {
    let outside = || {
        ToolError::new(
            "outside_root",
            "that path of this machine is not under a directory this tool may use",
        )
    };
    let path = Path::new(path);
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(outside());
    }
    // The file may not exist yet: resolve its parent.
    let resolved = match std::fs::canonicalize(path) {
        Ok(found) => found,
        Err(_) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let name = path.file_name().ok_or_else(outside)?;
            std::fs::canonicalize(parent)
                .map_err(|_| outside())?
                .join(name)
        }
    };
    for root in &tools.settings.local_roots {
        if let Ok(root) = std::fs::canonicalize(root)
            && resolved.starts_with(&root)
        {
            return Ok(resolved);
        }
    }
    Err(outside())
}

pub(super) fn put_file(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: TransferArgs = parse(arguments)?;
    let local = confined(tools, &args.local)?;
    let options = PutOptions {
        timeout: tools.transfer_wait(),
        ..PutOptions::default()
    };
    let moved = put(tools.link(), &local, &args.remote, &options)?;
    Output::json(&json!({
        "remote": args.remote,
        "size": moved.size,
        "literal_bytes": moved.literal_bytes,
        "copied_bytes": moved.copied_bytes,
        "wire_bytes": moved.wire_bytes,
        "delta": moved.delta,
        "skipped": moved.skipped,
    }))
}

pub(super) fn get_file(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: TransferArgs = parse(arguments)?;
    let local = confined(tools, &args.local)?;
    let options = GetOptions {
        timeout: tools.transfer_wait(),
        ..GetOptions::default()
    };
    let moved = get(tools.link(), &args.remote, &local, &options)?;
    Output::json(&json!({
        "remote": args.remote,
        "size": moved.size,
        "literal_bytes": moved.literal_bytes,
        "copied_bytes": moved.copied_bytes,
        "wire_bytes": moved.wire_bytes,
        "delta": moved.delta,
        "skipped": moved.skipped,
    }))
}
