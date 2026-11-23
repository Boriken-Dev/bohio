//! The tools: one entry each.

use serde_json::{Value, json};

use super::{Effect, Need, ToolSpec, files, net, object, process, screen};

fn text(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn count(description: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": description})
}

fn flag(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

fn list_of(item: &Value, description: &str) -> Value {
    json!({"type": "array", "items": item, "description": description})
}

fn paths_as_text() -> Value {
    list_of(&json!({"type": "string"}), "paths on the host")
}

/// The arguments every tool that starts a program takes.
fn launch() -> Value {
    json!({
        "argv": list_of(
            &json!({"type": "string"}),
            "the program and its arguments, run directly with no shell (give this or `script`)"
        ),
        "script": text("text for a shell to run (give this or `argv`)"),
        "shell": text("the shell for `script`: sh, bash, zsh, cmd, powershell or pwsh; default the host's first"),
        "cwd": text("the working directory on the host"),
        "env": json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "variables to set"}),
        "env_remove": list_of(&json!({"type": "string"}), "variables to remove"),
        "env_clear": flag("start from an empty environment"),
    })
}

fn with(mut base: Value, more: Value) -> Value {
    if let (Some(base), Some(more)) = (base.as_object_mut(), more.as_object()) {
        for (key, value) in more {
            base.insert(key.clone(), value.clone());
        }
    }
    base
}

fn exec_schema() -> Value {
    object(
        with(
            launch(),
            json!({
                "stdin": text("text sent to the program's standard input, which is then closed"),
                "stdin_base64": text("bytes for standard input, base64"),
                "deadline_ms": count("the host ends the program's whole tree after this long; 0 is no deadline; default is the tools' setting (120000)"),
                "pty": flag("run on a terminal (needs the `shell` permission): one merged `output`, typed input, escape sequences"),
                "cols": count("terminal width with `pty`"),
                "rows": count("terminal height with `pty`"),
                "max_output": count("bytes of each output stream to return: the first half and the last half are kept, `omitted_bytes` says how many lay between"),
                "signal": json!({
                    "type": "object",
                    "properties": {
                        "signal": {"type": "string", "enum": ["interrupt", "terminate", "hangup", "quit", "user1", "user2", "kill"]},
                        "after_ms": {"type": "integer", "minimum": 0}
                    },
                    "required": ["signal"],
                    "additionalProperties": false,
                    "description": "send a signal to the program this long after it starts (the deadline still ends it)"
                }),
            }),
        ),
        &[],
    )
}

fn read_schema() -> Value {
    object(
        json!({
            "path": text("the file on the host"),
            "offset_line": count("the first line to return, counting from 1 (default 1)"),
            "limit_lines": count("how many lines at most (the tools' default 2000)"),
            "max_bytes": count("how many bytes at most (the tools' default 65536)"),
            "encoding": json!({"type": "string", "enum": ["text", "base64"], "description": "`text` (default) is UTF-8 by lines; `base64` is any file by bytes, with `offset` and `length`"}),
            "offset": count("with base64: the first byte"),
            "length": count("with base64: how many bytes at most"),
        }),
        &["path"],
    )
}

fn write_schema() -> Value {
    object(
        json!({
            "path": text("the file on the host"),
            "text": text("the new content as text"),
            "base64": text("the new content as bytes, base64 (give this or `text`)"),
            "create_only": flag("fail when the file exists"),
            "if_sha256": text("replace the file only if its content still has this sha256 (the `sha256` of a read): else the write is refused as `changed`; needs fs.read as well"),
            "parents": flag("create missing directories above the file"),
        }),
        &["path"],
    )
}

fn edit_schema() -> Value {
    object(
        json!({
            "path": text("the text file on the host"),
            "old": text("the exact text to replace; it must occur once unless `replace_all`"),
            "new": text("what replaces it"),
            "replace_all": flag("replace every occurrence"),
        }),
        &["path", "old", "new"],
    )
}

fn list_schema() -> Value {
    object(
        json!({
            "path": text("a directory on the host"),
            "after": text("the `next` of the previous page"),
            "limit": count("entries in the page"),
        }),
        &["path"],
    )
}

fn stat_schema() -> Value {
    object(
        json!({
            "path": text("a path on the host"),
            "follow": flag("describe what a link points at (default true)"),
        }),
        &["path"],
    )
}

fn hash_schema() -> Value {
    object(
        json!({
            "path": text("a file on the host"),
            "algorithms": list_of(&json!({"type": "string"}), "md5, sha1, sha256, sha384, sha512, blake3, xxh3, xxh128, xxh64, crc32c; default sha256"),
        }),
        &["path"],
    )
}

fn walk_schema() -> Value {
    object(
        json!({
            "roots": paths_as_text(),
            "depth": count("how far below a root; 0 is the roots alone; default the whole tree"),
            "follow": flag("enter links to directories inside the root"),
            "include": list_of(&json!({"type": "string"}), "globs: only matching entries are listed"),
            "exclude": list_of(&json!({"type": "string"}), "globs: matching entries and their subtrees are left out"),
            "hashes": list_of(&json!({"type": "string"}), "digests of each file's content, computed on the host"),
            "skip": count("entries to skip: the `next_skip` of the previous page"),
            "limit": count("entries in the page"),
        }),
        &["roots"],
    )
}

fn glob_schema() -> Value {
    object(
        json!({
            "pattern": text("a glob on the path below the root or on the name, such as **/*.rs"),
            "path": text("the directory to look in (default .)"),
            "skip": count("entries to skip: the `next_skip` of the previous page"),
            "limit": count("entries in the page"),
        }),
        &["pattern"],
    )
}

fn read_many_schema() -> Value {
    object(
        json!({
            "paths": paths_as_text(),
            "max_bytes": count("bytes of content for all the files together; a file with no room is listed with its size"),
        }),
        &["paths"],
    )
}

fn write_many_schema() -> Value {
    object(
        json!({
            "files": json!({"type": "array", "items": write_schema_file(), "description": "the files, each put in place on its own"}),
            "parents": flag("create missing directories above each file"),
        }),
        &["files"],
    )
}

fn write_schema_file() -> Value {
    object(
        json!({
            "path": text("the file on the host"),
            "text": text("the content as text"),
            "base64": text("the content, base64"),
            "create_only": flag("fail when the file exists"),
        }),
        &["path"],
    )
}

fn move_schema() -> Value {
    object(
        json!({"from": text("the path to move"), "to": text("where it goes"), "overwrite": flag("replace an existing target")}),
        &["from", "to"],
    )
}

fn copy_schema() -> Value {
    object(
        json!({
            "from": text("the path to copy"), "to": text("the copy"),
            "overwrite": flag("replace an existing target"),
            "recursive": flag("copy a directory and everything in it"),
            "preserve": flag("keep permission bits and modification time"),
        }),
        &["from", "to"],
    )
}

fn truncate_schema() -> Value {
    object(
        json!({"path": text("a regular file"), "length": count("its new length: shorter cuts it, longer pads with zeros")}),
        &["path", "length"],
    )
}

fn mkdir_schema() -> Value {
    object(
        json!({"path": text("the directory"), "parents": flag("create the ones above (default true)")}),
        &["path"],
    )
}

fn remove_schema() -> Value {
    object(
        json!({"path": text("the file, link or directory"), "recursive": flag("remove a directory and everything in it")}),
        &["path"],
    )
}

fn transfer_schema() -> Value {
    object(
        json!({
            "local": text("a path of the machine this tool runs on, under a directory it may use"),
            "remote": text("the path on the host"),
        }),
        &["local", "remote"],
    )
}

fn session_start_schema() -> Value {
    object(
        with(
            launch(),
            json!({
                "pty": flag("run on a terminal (needs the `shell` permission)"),
                "stdin": flag("keep standard input open so `session_write` can send to it (not for a terminal, which always takes input)"),
                "cols": count("terminal width with `pty`"),
                "rows": count("terminal height with `pty`"),
                "deadline_ms": count("the host ends the program after this long"),
                "name": text("a name to find it by in `session_list`"),
                "scrollback": count("bytes of output the host keeps"),
                "on_full": json!({"type": "string", "enum": ["drop_oldest", "pause"], "description": "what the host does when the kept output is full"}),
                "linger_ms": count("how long an unattached session is kept"),
            }),
        ),
        &[],
    )
}

fn session_read_schema() -> Value {
    object(
        json!({
            "session": text("the session's id"),
            "from": count("the offset to read from; 0 (default) is everything still kept; use the `next` of the last read to go on"),
            "max_output": count("bytes to return at most; `more` says when output is left"),
            "wait_ms": count("when nothing has arrived, wait this long for the first output"),
            "quiet_ms": count("end the read this long after the last output (the tools' default 300)"),
        }),
        &["session"],
    )
}

fn session_write_schema() -> Value {
    object(
        json!({
            "session": text("the session's id"),
            "text": text("what to send to the program's standard input, or to type into a terminal (Enter is \\r)"),
            "base64": text("bytes to send, base64"),
            "close_stdin": flag("then close standard input"),
        }),
        &["session"],
    )
}

fn session_signal_schema() -> Value {
    object(
        json!({
            "session": text("the session's id"),
            "signal": json!({"type": "string", "enum": ["interrupt", "terminate", "hangup", "quit", "user1", "user2", "kill"]}),
        }),
        &["session", "signal"],
    )
}

fn session_end_schema() -> Value {
    object(
        json!({
            "session": text("the session's id"),
            "signal": json!({"type": "string", "enum": ["interrupt", "terminate", "hangup", "quit", "user1", "user2", "kill"], "description": "default kill"}),
        }),
        &["session"],
    )
}

fn nothing() -> Value {
    object(json!({}), &[])
}

fn forward_local_schema() -> Value {
    object(
        json!({
            "listen": text("an address on this machine, default 127.0.0.1:0 (a free port); only loopback unless the embedder allows more"),
            "target": text("what the host connects each connection to: HOST:PORT, unix:PATH or pipe:NAME"),
        }),
        &["target"],
    )
}

fn forward_remote_schema() -> Value {
    object(
        json!({
            "bind": text("what the host listens on, default 127.0.0.1:0 (it chooses the port)"),
            "target": text("the target on this machine each connection is bridged to; the embedder lists the ones allowed"),
        }),
        &["target"],
    )
}

fn forward_close_schema() -> Value {
    object(json!({"id": count("a forward's id")}), &["id"])
}

fn snapshot_schema() -> Value {
    object(
        json!({
            "display": text("a display's id; the primary one by default"),
            "region": json!({"type": "object", "properties": {"x": {"type": "integer"}, "y": {"type": "integer"}, "width": {"type": "integer"}, "height": {"type": "integer"}}, "required": ["x", "y", "width", "height"], "additionalProperties": false}),
            "format": json!({"type": "string", "enum": ["png", "jpeg", "webp"]}),
            "quality": count("JPEG quality, 1 to 100"),
            "max_width": count("scale down to fit this width"),
            "max_height": count("scale down to fit this height"),
            "cursor": flag("report where the pointer is"),
            "settle": json!({"description": "wait for the screen to stop changing first: true, or { quiet_ms, timeout_ms }"}),
        }),
        &[],
    )
}

fn input_schema() -> Value {
    object(
        json!({
            "events": list_of(&json!({"type": "object"}), "events as the protocol names them: {type: move|button|scroll|key|text|wait, ...}"),
            "keys": text("a key combination in xdotool spelling, such as ctrl+s or Return"),
            "text": text("text typed as it is"),
        }),
        &[],
    )
}

fn watch_schema() -> Value {
    object(
        json!({
            "duration_ms": count("how long to watch (default 2000, at most the tools' desktop wait)"),
            "display": text("a display's id"),
            "region": json!({"type": "object"}),
            "fps": count("updates a second at most"),
            "max_width": count("scale the frame down to fit this width"),
            "max_height": count("scale the frame down to fit this height"),
            "cursor": flag("report the pointer"),
        }),
        &[],
    )
}

/// Every tool. A new group of calls is new entries here and a function; nothing else lists tools.
pub static REGISTRY: &[ToolSpec] = &[
    ToolSpec {
        name: "info",
        description: "The host's capabilities: operating system, shells, what it serves and its limits. Needs the `info` permission.",
        schema: nothing,
        needs: &[Need::Info],
        effect: Effect::ReadOnly,
        run: files::info,
    },
    ToolSpec {
        name: "exec",
        description: "Run a program on the host and wait for it. Output is bounded: when it is longer than `max_output` the first and last halves come back with `omitted_bytes`. A program that outlives its deadline is ended with its whole process tree. Use a session (`session_start`) for anything long.",
        schema: exec_schema,
        needs: &[Need::Exec],
        effect: Effect::Runs,
        run: process::exec,
    },
    ToolSpec {
        name: "read",
        description: "Read a text file by lines, or any file by bytes as base64. A file that is not UTF-8 is refused with its size (`not_text`): read it with encoding base64. Long files are paged: `next_offset_line` says where to go on. `sha256` is present when the whole file was read.",
        schema: read_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::read,
    },
    ToolSpec {
        name: "write",
        description: "Write a whole file, replacing it atomically (a reader sees the old file or the new one). To change part of a file use `edit`.",
        schema: write_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::write,
    },
    ToolSpec {
        name: "edit",
        description: "Replace text in a file: `old` must occur once (or set `replace_all`). Zero matches and several matches are errors that say the count. The write is refused as `changed` when the file changed after it was read.",
        schema: edit_schema,
        needs: &[Need::FsRead, Need::FsWrite],
        effect: Effect::Changes,
        run: files::edit,
    },
    ToolSpec {
        name: "list",
        description: "List a directory, one page at a time (`next` goes in `after`).",
        schema: list_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::list,
    },
    ToolSpec {
        name: "stat",
        description: "Describe a path: kind, size, times, permissions.",
        schema: stat_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::stat,
    },
    ToolSpec {
        name: "hash",
        description: "Digests of a file, computed on the host.",
        schema: hash_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::hash,
    },
    ToolSpec {
        name: "walk",
        description: "List whole trees on one stream, with include and exclude globs, paged by `skip` and `limit`.",
        schema: walk_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::walk,
    },
    ToolSpec {
        name: "glob",
        description: "Find paths by a glob below a directory, paged by `skip` and `limit`.",
        schema: glob_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::glob,
    },
    ToolSpec {
        name: "read_many",
        description: "Read several files on one stream; a budget bounds the content, and a file with no room is listed with its size.",
        schema: read_many_schema,
        needs: &[Need::FsRead],
        effect: Effect::ReadOnly,
        run: files::read_many,
    },
    ToolSpec {
        name: "write_many",
        description: "Write several files on one stream, each put in place on its own; one that fails does not stop the others.",
        schema: write_many_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::write_many,
    },
    ToolSpec {
        name: "move",
        description: "Move or rename a file or directory on the host.",
        schema: move_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::move_to,
    },
    ToolSpec {
        name: "copy",
        description: "Copy a file or directory on the host, made there and never through this machine.",
        schema: copy_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::copy,
    },
    ToolSpec {
        name: "truncate",
        description: "Set a file's length.",
        schema: truncate_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::truncate,
    },
    ToolSpec {
        name: "mkdir",
        description: "Make a directory.",
        schema: mkdir_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::mkdir,
    },
    ToolSpec {
        name: "remove",
        description: "Remove a file, link or directory.",
        schema: remove_schema,
        needs: &[Need::FsWrite],
        effect: Effect::Changes,
        run: files::remove,
    },
    ToolSpec {
        name: "put",
        description: "Send a file of this machine (under an allowed directory) to the host; only what changed travels when the host has an older copy.",
        schema: transfer_schema,
        needs: &[Need::FsWrite, Need::LocalRoots],
        effect: Effect::Changes,
        run: files::put_file,
    },
    ToolSpec {
        name: "get",
        description: "Fetch a file of the host to this machine (under an allowed directory); only what changed travels when a copy is there.",
        schema: transfer_schema,
        needs: &[Need::FsRead, Need::LocalRoots],
        effect: Effect::Changes,
        run: files::get_file,
    },
    ToolSpec {
        name: "session_start",
        description: "Start a program that keeps running when this link ends (a detached session) and return its id. Read its output with `session_read`, send input with `session_write`, interrupt it with `session_signal`, stop it with `session_end`.",
        schema: session_start_schema,
        needs: &[Need::Session, Need::Exec],
        effect: Effect::Runs,
        run: process::session_start,
    },
    ToolSpec {
        name: "session_read",
        description: "Read a session's output from an offset: stdout and stderr in order, bounded; go on from `next`. `running` is false once it exited, with its `exit`.",
        schema: session_read_schema,
        needs: &[Need::Session],
        effect: Effect::ReadOnly,
        run: process::session_read,
    },
    ToolSpec {
        name: "session_write",
        description: "Send input to a session's program.",
        schema: session_write_schema,
        needs: &[Need::Session],
        effect: Effect::Runs,
        run: process::session_write,
    },
    ToolSpec {
        name: "session_signal",
        description: "Send a signal (interrupt, terminate, ...) to a session's program, which keeps its session unless it exits.",
        schema: session_signal_schema,
        needs: &[Need::Session],
        effect: Effect::Runs,
        run: process::session_signal,
    },
    ToolSpec {
        name: "session_list",
        description: "The detached sessions this peer holds.",
        schema: nothing,
        needs: &[Need::Session],
        effect: Effect::ReadOnly,
        run: process::session_list,
    },
    ToolSpec {
        name: "session_end",
        description: "End a session's program (killed unless a signal is named) and free it.",
        schema: session_end_schema,
        needs: &[Need::Session],
        effect: Effect::Runs,
        run: process::session_end,
    },
    ToolSpec {
        name: "forward_local",
        description: "Listen on this machine and connect every connection to a target through the host (ssh -L). Loopback only by default.",
        schema: forward_local_schema,
        needs: &[Need::ForwardConnect],
        effect: Effect::Changes,
        run: net::forward_local,
    },
    ToolSpec {
        name: "forward_remote",
        description: "Have the host listen and bridge every connection to an allowed target of this machine (ssh -R).",
        schema: forward_remote_schema,
        needs: &[Need::ForwardListen, Need::LocalTargets],
        effect: Effect::Changes,
        run: net::forward_remote,
    },
    ToolSpec {
        name: "forward_list",
        description: "The forwards this agent holds open.",
        schema: nothing,
        needs: &[],
        effect: Effect::ReadOnly,
        run: net::forward_list,
    },
    ToolSpec {
        name: "forward_close",
        description: "Close a forward.",
        schema: forward_close_schema,
        needs: &[],
        effect: Effect::Changes,
        run: net::forward_close,
    },
    ToolSpec {
        name: "gui_snapshot",
        description: "A picture of the host's desktop. Everything on the screen is readable, passwords shown in clear included.",
        schema: snapshot_schema,
        needs: &[Need::GuiView],
        effect: Effect::ReadOnly,
        run: screen::snapshot,
    },
    ToolSpec {
        name: "gui_input",
        description: "Type, click, scroll or press keys on the host's desktop, with the authority of the person who owns it.",
        schema: input_schema,
        needs: &[Need::GuiInput],
        effect: Effect::Runs,
        run: screen::input,
    },
    ToolSpec {
        name: "gui_watch",
        description: "Watch the desktop for a short time and report how much changed (updates, rectangles, bytes).",
        schema: watch_schema,
        needs: &[Need::GuiView],
        effect: Effect::ReadOnly,
        run: screen::watch,
    },
];
