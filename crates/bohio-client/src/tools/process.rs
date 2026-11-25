//! Programs: `exec`, and the detached sessions.

use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

use bohio_proto::exec::{Env, Request, Stdin};
use bohio_proto::session::{OnFull, SessionRequest};
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use bohio_proto::signal::Signal;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cancel, Capture, Output, ToolError, Tools, base64_decode, encoded, parse, text_cut};
use crate::Error;
use crate::control::Control;
use crate::exec::{Event, Exec};
use crate::session::{Attachment, attach};
use crate::shell::{Event as ShellEvent, Shell};

/// What to run, as every program-starting tool takes it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Launch {
    argv: Option<Vec<String>>,
    script: Option<String>,
    shell: Option<String>,
    source: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    env_remove: Vec<String>,
    #[serde(default)]
    env_clear: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimedSignal {
    signal: String,
    #[serde(default)]
    after_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecArgs {
    #[serde(flatten)]
    program: Launch,
    stdin: Option<String>,
    stdin_base64: Option<String>,
    deadline_ms: Option<u64>,
    #[serde(default)]
    pty: bool,
    cols: Option<u16>,
    rows: Option<u16>,
    max_output: Option<u64>,
    signal: Option<TimedSignal>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    #[serde(flatten)]
    program: Launch,
    #[serde(default)]
    pty: bool,
    #[serde(default)]
    stdin: bool,
    cols: Option<u16>,
    rows: Option<u16>,
    deadline_ms: Option<u64>,
    name: Option<String>,
    scrollback: Option<u64>,
    on_full: Option<String>,
    linger_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    session: String,
    from: Option<u64>,
    max_output: Option<u64>,
    wait_ms: Option<u64>,
    quiet_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    session: String,
    text: Option<String>,
    base64: Option<String>,
    #[serde(default)]
    close_stdin: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalArgs {
    session: String,
    signal: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndArgs {
    session: String,
    signal: Option<String>,
}

fn signal_named(name: &str) -> Result<Signal, ToolError> {
    Signal::from_name(name).ok_or_else(|| {
        let known: Vec<&str> = Signal::ALL.iter().map(|signal| signal.name()).collect();
        ToolError::bad(format!(
            "`{name}` is not a signal this tool sends: {}",
            known.join(", ")
        ))
    })
}

fn exit_json(exit: &impl serde::Serialize) -> Value {
    serde_json::to_value(exit).unwrap_or(Value::Null)
}

/// The argument vector that runs `script` in `kind`'s shell on a terminal.
fn script_argv(kind: ShellKind, script: &str) -> Vec<String> {
    let program = kind.program().to_owned();
    let flag = match kind {
        ShellKind::Cmd => "/c",
        ShellKind::Powershell | ShellKind::Pwsh => "-Command",
        _ => "-c",
    };
    vec![program, flag.to_owned(), script.to_owned()]
}

impl Tools {
    fn shell_kind(&self, name: Option<&str>) -> Result<ShellKind, ToolError> {
        if let Some(name) = name {
            return serde_json::from_value(Value::String(name.to_owned())).map_err(|_| {
                ToolError::bad(format!(
                    "`{name}` is not a shell kind: sh, bash, zsh, cmd, powershell, pwsh"
                ))
            });
        }
        self.info
            .as_ref()
            .and_then(|info| info.shells.first())
            .map(|shell| shell.kind)
            .ok_or_else(|| ToolError::bad("name the shell (`shell`): the host's own are not known"))
    }

    fn env_of(program: &Launch) -> Env {
        Env {
            clear: program.env_clear,
            set: program.env.clone(),
            remove: program.env_remove.clone(),
        }
    }

    fn exec_request(&self, program: &Launch) -> Result<Request, ToolError> {
        let mut request = match (&program.argv, &program.script) {
            (Some(argv), None) if !argv.is_empty() => Request::argv(argv.clone()),
            (None, Some(script)) => {
                Request::shell(self.shell_kind(program.shell.as_deref())?, script.clone())
            }
            _ => {
                return Err(ToolError::bad(
                    "give exactly one of `argv` (a program and its arguments, run directly) or `script` (text for a shell)",
                ));
            }
        };
        request.cwd.clone_from(&program.cwd);
        request.source.clone_from(&program.source);
        request.env = Self::env_of(program);
        Ok(request)
    }

    fn terminal_request(&self, program: &Launch) -> Result<ShellRequest, ToolError> {
        let argv = match (&program.argv, &program.script) {
            (Some(argv), None) if !argv.is_empty() => argv.clone(),
            (None, Some(script)) => script_argv(self.shell_kind(program.shell.as_deref())?, script),
            _ => {
                return Err(ToolError::bad("give exactly one of `argv` or `script`"));
            }
        };
        if program.source.is_some() {
            return Err(ToolError::bad(
                "`source` names an exec source: a terminal (`pty`) starts on the host's default terminal source",
            ));
        }
        let mut request = ShellRequest::argv(argv);
        request.cwd.clone_from(&program.cwd);
        request.env = Self::env_of(program);
        Ok(request)
    }
}

fn stdin_bytes(
    text: Option<&String>,
    base64: Option<&String>,
) -> Result<Option<Vec<u8>>, ToolError> {
    match (text, base64) {
        (Some(_), Some(_)) => Err(ToolError::bad("give `stdin` or `stdin_base64`, not both")),
        (Some(text), None) => Ok(Some(text.as_bytes().to_vec())),
        (None, Some(data)) => base64_decode(data)
            .map(Some)
            .ok_or_else(|| ToolError::bad("`stdin_base64` is not base64")),
        (None, None) => Ok(None),
    }
}

pub(super) fn exec(tools: &Tools, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
    let args: ExecArgs = parse(arguments)?;
    let settings = &tools.settings;
    let limit = usize::try_from(settings.output.pick(args.max_output)).unwrap_or(usize::MAX);
    let deadline_ms = args.deadline_ms.unwrap_or(settings.exec_deadline_ms);
    let input = stdin_bytes(args.stdin.as_ref(), args.stdin_base64.as_ref())?;
    let after = args
        .signal
        .as_ref()
        .map(|timed| {
            Ok::<_, ToolError>((
                signal_named(&timed.signal)?,
                Duration::from_millis(timed.after_ms),
            ))
        })
        .transpose()?;
    let hard = (deadline_ms > 0).then(|| Duration::from_millis(deadline_ms) + settings.exec_grace);
    let started = Instant::now();
    let mut signals = Vec::new();
    let mut cancelled = false;
    let mut pending = after;
    let poll = settings.poll;

    if args.pty {
        let mut request = tools.terminal_request(&args.program)?;
        request.cols = args.cols.unwrap_or(request.cols);
        request.rows = args.rows.unwrap_or(request.rows);
        request.timeout_ms = (deadline_ms > 0).then_some(deadline_ms);
        let shell = Shell::start(tools.link(), &request, Some(settings.reply_timeout))?;
        let mut output = Capture::new(limit);
        let exit = thread::scope(|scope| {
            if let Some(bytes) = input {
                let shell = &shell;
                scope.spawn(move || {
                    for chunk in bytes.chunks(16 * 1024) {
                        if shell.write(chunk).is_err() {
                            return;
                        }
                    }
                });
            }
            let result = (|| loop {
                if cancel.is_cancelled() && !cancelled {
                    cancelled = true;
                    let _ = shell.kill();
                }
                if let Some((signal, at)) = pending
                    && started.elapsed() >= at
                {
                    pending = None;
                    let _ = shell.signal(signal);
                }
                if hard.is_some_and(|hard| started.elapsed() > hard) {
                    let _ = shell.kill();
                    return Err(ToolError::new(
                        "timeout",
                        "the host did not report the end of the program after its deadline",
                    ));
                }
                match shell.next(Some(poll)) {
                    Ok(Some(ShellEvent::Data(bytes))) => output.push(&bytes),
                    Ok(Some(ShellEvent::Signalled(answer))) => signals.push(exit_json(&answer)),
                    Ok(Some(ShellEvent::Exit(exit))) => return Ok(exit),
                    Ok(None) => return Err(ToolError::from(Error::Closed)),
                    Err(Error::Timeout) => {}
                    Err(error) => return Err(error.into()),
                }
            })();
            shell.close();
            result
        })?;
        return Output::json(&json!({
            "exit": exit_json(&exit),
            "pid": shell.pid(),
            "output": output.finish(),
            "signals": signals,
            "cancelled": cancelled,
            "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }));
    }

    let mut request = tools.exec_request(&args.program)?;
    request.timeout_ms = (deadline_ms > 0).then_some(deadline_ms);
    if input.is_some() {
        request.stdin = Stdin::Open;
    }
    let exec = Exec::start(tools.link(), &request)?;
    let (mut out, mut err) = (Capture::new(limit), Capture::new(limit));
    let mut pid = None;
    let exit = thread::scope(|scope| {
        if let Some(bytes) = input {
            let exec = &exec;
            scope.spawn(move || {
                for chunk in bytes.chunks(16 * 1024) {
                    if exec.write_stdin(chunk).is_err() {
                        return;
                    }
                }
                let _ = exec.finish_stdin();
            });
        }
        let result = (|| loop {
            if cancel.is_cancelled() && !cancelled {
                cancelled = true;
                let _ = exec.kill();
            }
            if let Some((signal, at)) = pending
                && started.elapsed() >= at
            {
                pending = None;
                let _ = exec.signal(signal);
            }
            if hard.is_some_and(|hard| started.elapsed() > hard) {
                let _ = exec.kill();
                return Err(ToolError::new(
                    "timeout",
                    "the host did not report the end of the program after its deadline",
                ));
            }
            match exec.next(Some(poll)) {
                Ok(Some(Event::Started { pid: found })) => pid = Some(found),
                Ok(Some(Event::Stdout(bytes))) => out.push(&bytes),
                Ok(Some(Event::Stderr(bytes))) => err.push(&bytes),
                Ok(Some(Event::Signalled(answer))) => signals.push(exit_json(&answer)),
                Ok(Some(Event::Exit(exit))) => return Ok(exit),
                Ok(None) => return Err(ToolError::from(Error::Closed)),
                Err(Error::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        })();
        exec.close();
        result
    })?;
    Output::json(&json!({
        "exit": exit_json(&exit),
        "pid": pid,
        "stdout": out.finish(),
        "stderr": err.finish(),
        "signals": signals,
        "cancelled": cancelled,
        "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}

pub(super) fn session_start(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: StartArgs = parse(arguments)?;
    let on_full = args
        .on_full
        .as_deref()
        .map(|name| {
            serde_json::from_value::<OnFull>(Value::String(name.to_owned()))
                .map_err(|_| ToolError::bad("`on_full` is `drop_oldest` or `pause`"))
        })
        .transpose()?;
    let session = SessionRequest {
        name: args.name.clone(),
        scrollback: args.scrollback,
        on_full,
        linger_ms: args.linger_ms,
        detach: true,
    };
    let wait = Some(tools.settings.reply_timeout);
    let timeout_ms = args.deadline_ms.filter(|ms| *ms > 0);
    if args.pty {
        let mut request = tools.terminal_request(&args.program)?;
        request.cols = args.cols.unwrap_or(request.cols);
        request.rows = args.rows.unwrap_or(request.rows);
        request.timeout_ms = timeout_ms;
        request.session = Some(session);
        let shell = Shell::start(tools.link(), &request, wait)?;
        let id = shell.session().map(str::to_owned);
        let pid = shell.pid();
        drop(shell);
        let id = id.ok_or_else(|| ToolError::new("unsupported", "the host made no session"))?;
        return Output::json(&json!({"session": id, "kind": "shell", "pid": pid}));
    }
    let mut request = tools.exec_request(&args.program)?;
    request.timeout_ms = timeout_ms;
    request.session = Some(session);
    if args.stdin {
        request.stdin = Stdin::Open;
    }
    let exec = Exec::start(tools.link(), &request)?;
    let until = Instant::now() + tools.settings.reply_timeout;
    let pid = loop {
        match exec.next(Some(tools.settings.poll)) {
            Ok(Some(Event::Started { pid: found })) => break found,
            Ok(Some(_)) | Err(Error::Timeout) => {}
            Ok(None) => return Err(Error::Closed.into()),
            Err(error) => return Err(error.into()),
        }
        if Instant::now() > until {
            return Err(Error::Timeout.into());
        }
    };
    let id = exec
        .session()
        .ok_or_else(|| ToolError::new("unsupported", "the host made no session"))?;
    drop(exec);
    Output::json(&json!({"session": id, "kind": "exec", "pid": pid}))
}

/// What a session's stream gave next.
enum Piece {
    Bytes(&'static str, Vec<u8>),
    Exit(Value),
    Other,
}

fn next_piece(attachment: &Attachment, timeout: Duration) -> Result<Option<Piece>, Error> {
    Ok(match attachment {
        Attachment::Exec(exec) => exec.next(Some(timeout))?.map(|event| match event {
            Event::Stdout(bytes) => Piece::Bytes("stdout", bytes),
            Event::Stderr(bytes) => Piece::Bytes("stderr", bytes),
            Event::Exit(exit) => Piece::Exit(exit_json(&exit)),
            _ => Piece::Other,
        }),
        Attachment::Shell(shell) => shell.next(Some(timeout))?.map(|event| match event {
            ShellEvent::Data(bytes) => Piece::Bytes("output", bytes),
            ShellEvent::Exit(exit) => Piece::Exit(exit_json(&exit)),
            _ => Piece::Other,
        }),
    })
}

pub(super) fn session_read(
    tools: &Tools,
    arguments: Value,
    cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: ReadArgs = parse(arguments)?;
    let settings = &tools.settings;
    let cap = settings.output.pick(args.max_output);
    let quiet = Duration::from_millis(args.quiet_ms.unwrap_or(settings.session_quiet_ms));
    let wait = Duration::from_millis(args.wait_ms.unwrap_or(0));
    let (attachment, attached) = attach(tools.link(), &args.session, Some(args.from.unwrap_or(0)))?;
    let mut chunks: Vec<(&'static str, Vec<u8>)> = Vec::new();
    let (mut taken, mut more, mut exit) = (0_u64, false, None);
    let (begun, mut last) = (Instant::now(), Instant::now());
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match next_piece(&attachment, settings.poll) {
            Ok(Some(Piece::Bytes(stream, bytes))) => {
                if bytes.is_empty() {
                    continue;
                }
                let room = usize::try_from(cap - taken).unwrap_or(usize::MAX);
                let keep = if bytes.len() > room {
                    more = true;
                    text_cut(&bytes, room)
                } else {
                    bytes.len()
                };
                if keep > 0 {
                    match chunks.last_mut() {
                        Some((last_stream, held)) if *last_stream == stream => {
                            held.extend_from_slice(&bytes[..keep]);
                        }
                        _ => chunks.push((stream, bytes[..keep].to_vec())),
                    }
                    taken += keep as u64;
                }
                last = Instant::now();
                if more || taken >= cap {
                    more = true;
                    break;
                }
            }
            Ok(Some(Piece::Exit(found))) => {
                exit = Some(found);
                break;
            }
            Ok(Some(Piece::Other)) => {}
            Ok(None) => break,
            Err(Error::Timeout) => {
                let idle = if taken > 0 {
                    last.elapsed()
                } else {
                    begun.elapsed()
                };
                let limit = if taken > 0 { quiet } else { quiet.max(wait) };
                if idle >= limit {
                    break;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    let listed: Vec<Value> = chunks
        .iter()
        .map(|(stream, bytes)| {
            let mut value = encoded(bytes);
            value["stream"] = json!(stream);
            value
        })
        .collect();
    Output::json(&json!({
        "session": args.session,
        "kind": match attachment { Attachment::Exec(_) => "exec", _ => "shell" },
        "from": attached.from,
        "lost": attached.lost,
        "next": attached.from + taken,
        "chunks": listed,
        "more": more,
        "running": exit.is_none(),
        "exit": exit,
    }))
}

pub(super) fn session_write(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: WriteArgs = parse(arguments)?;
    let bytes = stdin_bytes(args.text.as_ref(), args.base64.as_ref())?.unwrap_or_default();
    if bytes.is_empty() && !args.close_stdin {
        return Err(ToolError::bad("give `text` or `base64`, or `close_stdin`"));
    }
    let (attachment, _) = attach(tools.link(), &args.session, None)?;
    match &attachment {
        Attachment::Exec(exec) => {
            exec.write_stdin(&bytes)?;
            if args.close_stdin {
                exec.finish_stdin()?;
            }
        }
        Attachment::Shell(shell) => {
            if args.close_stdin {
                return Err(ToolError::bad(
                    "a terminal session has no standard input to close: type the end of input (ctrl+d is the byte 4)",
                ));
            }
            shell.write(&bytes)?;
        }
    }
    // What was queued is delivered as the stream closes.
    drop(attachment);
    Output::json(&json!({"session": args.session, "written": bytes.len()}))
}

pub(super) fn session_signal(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: SignalArgs = parse(arguments)?;
    let signal = signal_named(&args.signal)?;
    let (attachment, _) = attach(tools.link(), &args.session, None)?;
    match &attachment {
        Attachment::Exec(exec) => exec.signal(signal)?,
        Attachment::Shell(shell) => shell.signal(signal)?,
    }
    let until = Instant::now() + tools.settings.reply_timeout;
    loop {
        let answer = match &attachment {
            Attachment::Exec(exec) => match exec.next(Some(tools.settings.poll)) {
                Ok(Some(Event::Signalled(answer))) => Some(answer),
                Ok(Some(_)) => None,
                Ok(None) => return Err(Error::Closed.into()),
                Err(Error::Timeout) => None,
                Err(error) => return Err(error.into()),
            },
            Attachment::Shell(shell) => match shell.next(Some(tools.settings.poll)) {
                Ok(Some(ShellEvent::Signalled(answer))) => Some(answer),
                Ok(Some(_)) => None,
                Ok(None) => return Err(Error::Closed.into()),
                Err(Error::Timeout) => None,
                Err(error) => return Err(error.into()),
            },
        };
        if let Some(answer) = answer {
            return Output::json(&json!({
                "session": args.session,
                "signal": args.signal,
                "delivered": answer.delivered,
                "message": answer.message,
            }));
        }
        if Instant::now() > until {
            return Err(Error::Timeout.into());
        }
    }
}

pub(super) fn session_list(
    tools: &Tools,
    _arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let control = Control::open(tools.link())?;
    let listed = control.sessions(Some(tools.settings.reply_timeout))?;
    control.close();
    Output::json(&json!({"sessions": listed}))
}

pub(super) fn session_end(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: EndArgs = parse(arguments)?;
    let signal = args.signal.as_deref().map(signal_named).transpose()?;
    let control = Control::open(tools.link())?;
    control.end_session(&args.session, signal, Some(tools.settings.reply_timeout))?;
    control.close();
    Output::json(&json!({"session": args.session, "ended": true}))
}
