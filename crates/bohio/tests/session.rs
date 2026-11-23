//! Detached sessions through a real node: the program outlives a link that was cut, and a new link picks its output up.
//!
//! Every test that starts a program ends it: a session that is left running fails the test through `gone_within`.

#![forbid(unsafe_code)]

mod support;

use std::time::{Duration, Instant};

use bohio::config::{LinkConfig, NodeConfig, SessionsConfig, Span};
use bohio::{Grant, Permission};
use bohio_client::control::Control;
use bohio_client::exec::{Event, Exec};
use bohio_client::shell::Shell;
use bohio_client::{Error, Open};
use bohio_proto::ErrorCode;
use bohio_proto::exec::{Exit, Request};
use bohio_proto::session::{Kind, OnFull, SessionRequest};
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use bohio_proto::signal::Signal;
use support::embedded::{Deadline, Proxy, RECHECK, Running, SLACK};
use support::{gone_within, helper};

const WAIT: Option<Duration> = Some(Duration::from_secs(40));

/// A node whose sessions are checked every 100 ms, with `sessions` otherwise.
fn node_with(sessions: SessionsConfig) -> Running {
    Running::with_node(NodeConfig {
        link: LinkConfig {
            recheck: Span(RECHECK),
            ..LinkConfig::default()
        },
        sessions: SessionsConfig {
            check_every: Span(Duration::from_millis(100)),
            ..sessions
        },
        ..NodeConfig::default()
    })
}

fn node() -> Running {
    node_with(SessionsConfig::default())
}

fn grant() -> Grant {
    Grant::allowing([Permission::Exec, Permission::Shell, Permission::Session])
}

fn detached() -> SessionRequest {
    SessionRequest::default()
}

fn counting(lines: u32, pause_ms: u32, session: SessionRequest) -> Request {
    let mut request = Request::argv([
        helper(),
        "count".to_owned(),
        lines.to_string(),
        pause_ms.to_string(),
    ]);
    request.session = Some(session);
    request
}

/// Starts `request` and returns its stream, the host's process id and the session's id.
fn start(link: &dyn Open, request: &Request) -> (Exec, u32, String) {
    let exec = Exec::start(link, request).expect("the stream opens");
    let pid = match exec.next(WAIT).expect("the host answers") {
        Some(Event::Started { pid }) => pid,
        other => panic!("expected started, got {other:?}"),
    };
    let id = exec.session().expect("started names the session");
    assert_eq!(id.len(), 32, "128 bits of hex: {id}");
    (exec, pid, id)
}

/// Everything the stream says to its end: the standard output bytes and how the program ended.
fn read_to_exit(exec: &Exec) -> (Vec<u8>, Exit) {
    let mut out = Vec::new();
    loop {
        match exec.next(WAIT).expect("events arrive") {
            Some(Event::Stdout(bytes)) => out.extend_from_slice(&bytes),
            Some(Event::Exit(exit)) => return (out, exit),
            Some(_) => {}
            None => panic!("the stream ended before the exit"),
        }
    }
}

fn numbers(bytes: &[u8]) -> Vec<u64> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| line.trim().parse().expect("a number"))
        .collect()
}

/// Waits until the session `id` has exited, as the host lists it.
fn exited(control: &Control, id: &str) -> Exit {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let listed = control.sessions(WAIT).expect("a listing");
        if let Some(exit) = listed
            .iter()
            .find(|session| session.id == id)
            .and_then(|session| session.exit)
        {
            return exit;
        }
        assert!(
            Instant::now() < until,
            "the session did not exit: {listed:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_detached_exec_outlives_a_cut_link_and_a_new_link_reads_every_line() {
    let _deadline = Deadline::start();
    let node = node();
    let proxy = Proxy::to(node.address);
    let identity = node.peer(grant());
    let link = node.link_as(&identity, proxy.address);
    let (exec, pid, id) = start(&link, &counting(50, 20, detached()));
    // Some output arrives, then the network goes: no close, no goodbye, nothing for the streams to say.
    let first = exec.next(WAIT).unwrap();
    assert!(matches!(first, Some(Event::Stdout(_))), "{first:?}");
    proxy.sever();
    drop(exec);
    drop(link);

    // The program ran on while nobody was there, and finished.
    let again = node.link_as(&identity, node.address);
    let control = Control::open(&again).unwrap();
    let exit = exited(&control, &id);
    assert_eq!(exit.code, Some(0), "{exit:?}");
    let (exec, attached) = Exec::attach(&again, &id, Some(0)).expect("the attach is answered");
    assert_eq!(
        (attached.kind, attached.from, attached.lost),
        (Kind::Exec, 0, 0)
    );
    let (out, exit) = read_to_exit(&exec);
    assert_eq!(
        numbers(&out),
        (1..=50).collect::<Vec<_>>(),
        "every line, in order"
    );
    assert_eq!(exit.code, Some(0));
    assert!(!exit.killed);
    let listed = control.sessions(WAIT).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].next, out.len() as u64);
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_reattach_from_an_offset_reads_the_rest_and_one_without_an_offset_reads_only_what_comes() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(grant());
    let (exec, pid, id) = start(&link, &counting(30, 30, detached()));
    // Read what is there, remember where it ends, drop the stream (a detach), and attach from that offset.
    let mut seen = Vec::new();
    while seen.len() < 20 {
        if let Some(Event::Stdout(bytes)) = exec.next(WAIT).unwrap() {
            seen.extend_from_slice(&bytes);
        }
    }
    drop(exec);
    let (exec, attached) = Exec::attach(&link, &id, Some(seen.len() as u64)).unwrap();
    assert_eq!(attached.from, seen.len() as u64);
    let (rest, exit) = read_to_exit(&exec);
    let mut all = seen;
    all.extend_from_slice(&rest);
    assert_eq!(numbers(&all), (1..=30).collect::<Vec<_>>());
    assert_eq!(exit.code, Some(0));

    // An attach with no offset starts at the end: the exited session has nothing more but its exit.
    let (exec, attached) = Exec::attach(&link, &id, None).unwrap();
    assert_eq!(attached.lost, 0);
    let (nothing, exit) = read_to_exit(&exec);
    assert!(nothing.is_empty());
    assert_eq!(exit.code, Some(0));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_small_scrollback_that_drops_the_oldest_reports_what_was_lost_and_keeps_the_tail() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(grant());
    let session = SessionRequest {
        scrollback: Some(2000),
        on_full: Some(OnFull::DropOldest),
        ..detached()
    };
    let (exec, pid, id) = start(&link, &counting(20_000, 0, session));
    drop(exec);
    let control = Control::open(&link).unwrap();
    exited(&control, &id);
    let (exec, attached) = Exec::attach(&link, &id, Some(0)).unwrap();
    assert!(attached.lost > 0, "{attached:?}");
    assert_eq!(attached.from, attached.lost);
    let (out, exit) = read_to_exit(&exec);
    assert_eq!(exit.code, Some(0));
    // The tail is whole: after the first (possibly cut) line, every number follows the one before, to the last.
    let text = String::from_utf8_lossy(&out).into_owned();
    let lines: Vec<u64> = text
        .lines()
        .skip(1)
        .map(|line| line.parse().expect("a number"))
        .collect();
    assert_eq!(lines.last(), Some(&20_000));
    assert!(
        lines.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{lines:?}"
    );
    let listed = control.sessions(WAIT).unwrap();
    assert_eq!(attached.from + out.len() as u64, listed[0].next);
    assert!(listed[0].buffered <= 2000 + 16 * 1024, "{listed:?}");
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_paused_program_waits_for_a_reader_and_nothing_is_lost() {
    let _deadline = Deadline::start();
    // Two frames waiting between the program and the buffer: the program's pipe, the two frames and the 1000 bytes kept
    // hold less than the 30000 lines.
    let node = node_with(SessionsConfig {
        queue_frames: 2,
        ..SessionsConfig::default()
    });
    let (link, _) = node.link(grant());
    let session = SessionRequest {
        scrollback: Some(1000),
        on_full: Some(OnFull::Pause),
        detach: true,
        ..detached()
    };
    let (exec, pid, id) = start(&link, &counting(30_000, 0, session));
    drop(exec);
    // Nobody reads: the program fills its pipe and the session's queue and then waits, still running.
    std::thread::sleep(Duration::from_millis(1500));
    let control = Control::open(&link).unwrap();
    let listed = control.sessions(WAIT).unwrap();
    assert!(
        listed[0].exit.is_none(),
        "it waits, it did not finish: {listed:?}"
    );
    assert!(listed[0].buffered <= 1000 + 16 * 1024, "{listed:?}");
    assert!(support::alive(pid));
    let (exec, attached) = Exec::attach(&link, &id, Some(0)).unwrap();
    assert_eq!((attached.from, attached.lost), (0, 0));
    let (out, exit) = read_to_exit(&exec);
    assert_eq!(exit.code, Some(0));
    assert_eq!(numbers(&out), (1..=30_000).collect::<Vec<_>>());
    assert!(gone_within(pid, Duration::from_secs(10)));
}

/// What the shell of a test echoes for a variable, which the typed text does not hold.
fn echo(name: &str) -> String {
    if cfg!(windows) {
        format!("echo %{name}%\r")
    } else {
        format!("echo ${name}\r")
    }
}

#[test]
fn a_detached_shell_keeps_its_terminal_for_a_new_link() {
    let _deadline = Deadline::start();
    let node = node();
    let proxy = Proxy::to(node.address);
    let identity = node.peer(grant());
    let link = node.link_as(&identity, proxy.address);
    let mut request = if cfg!(windows) {
        ShellRequest::kind(ShellKind::Cmd)
    } else {
        ShellRequest::kind(ShellKind::Sh)
    };
    request
        .env
        .set
        .insert("BOH_MARK".into(), "marker-8812-ok".into());
    request.session = Some(detached());
    let shell = Shell::start(&link, &request, WAIT).unwrap();
    let pid = shell.pid();
    let id = shell.session().expect("a session").to_owned();
    shell.write(echo("BOH_MARK").as_bytes()).unwrap();
    shell.read_until(b"marker-8812-ok", WAIT).unwrap();
    proxy.sever();
    drop(shell);
    drop(link);

    let again = node.link_as(&identity, node.address);
    let (shell, attached) = Shell::attach(&again, &id, Some(0)).unwrap();
    assert_eq!((attached.kind, attached.from), (Kind::Shell, 0));
    // The marker typed before the cut is in what the terminal kept, and the shell still answers.
    shell.read_until(b"marker-8812-ok", WAIT).unwrap();
    assert!(support::alive(pid), "the shell outlived its link");
    shell.write(b"exit 7\r").unwrap();
    let (_, exit) = shell.wait(WAIT).unwrap();
    assert_eq!(exit.code, Some(7), "{exit:?}");
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_second_attach_supersedes_the_first() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(grant());
    let (exec, pid, id) = start(&link, &counting(100, 40, detached()));
    drop(exec);
    let (first, _) = Exec::attach(&link, &id, Some(0)).unwrap();
    assert!(matches!(first.next(WAIT).unwrap(), Some(Event::Stdout(_))));
    let (second, attached) = Exec::attach(&link, &id, Some(0)).unwrap();
    assert_eq!(attached.from, 0);
    // The first stream ends without the program's exit; the second reads everything.
    loop {
        match first.next(WAIT) {
            Ok(Some(Event::Exit(exit))) => panic!("the superseded stream got the exit: {exit:?}"),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let (out, exit) = read_to_exit(&second);
    assert_eq!(numbers(&out), (1..=100).collect::<Vec<_>>());
    assert_eq!(exit.code, Some(0));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn another_peer_cannot_see_attach_to_or_end_a_session_and_an_admin_lists_every_peers() {
    let _deadline = Deadline::start();
    let node = node();
    let (owner, owner_id) = node.link(grant());
    let (other, _) = node.link(grant());
    let (admin, _) = node.link(Grant::allowing([Permission::Session, Permission::Admin]));
    let (exec, pid, id) = start(&owner, &counting(1000, 10, detached()));
    let other_control = Control::open(&other).unwrap();
    assert!(other_control.sessions(WAIT).unwrap().is_empty());
    match Exec::attach(&other, &id, Some(0)) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::NotFound),
        other => panic!("{:?}", other.map(|_| ())),
    }
    match other_control.end_session(&id, None, WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
    let admin_control = Control::open(&admin).unwrap();
    let seen = admin_control.sessions(WAIT).unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].peer.as_deref(), Some(owner_id.to_string().as_str()));
    assert_eq!(seen[0].program, helper());
    // The owner sees its own without a `peer`, and the program is still running after the others tried.
    let own = Control::open(&owner).unwrap().sessions(WAIT).unwrap();
    assert!(own[0].peer.is_none() && own[0].exit.is_none());
    assert!(support::alive(pid));
    admin_control.end_session(&id, None, WAIT).unwrap();
    drop(exec);
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn a_session_nobody_attaches_to_is_ended_at_its_linger() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(grant());
    let session = SessionRequest {
        linger_ms: Some(1000),
        ..detached()
    };
    let mut request = Request::argv([helper(), "sleep".to_owned(), "60000".to_owned()]);
    request.session = Some(session);
    let (exec, pid, id) = start(&link, &request);
    drop(exec);
    assert!(
        gone_within(pid, Duration::from_secs(20)),
        "the session outlived its linger"
    );
    let control = Control::open(&link).unwrap();
    assert!(control.sessions(WAIT).unwrap().is_empty());
    match Exec::attach(&link, &id, Some(0)) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::NotFound),
        other => panic!("{:?}", other.map(|_| ())),
    }
}

#[test]
fn an_attached_session_is_not_ended_by_its_linger() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(grant());
    let session = SessionRequest {
        linger_ms: Some(500),
        ..detached()
    };
    let (exec, pid, _) = start(&link, &counting(40, 100, session));
    // The stream stays attached for longer than the linger.
    let (out, exit) = read_to_exit(&exec);
    assert_eq!(numbers(&out), (1..=40).collect::<Vec<_>>());
    assert_eq!(exit.code, Some(0));
    assert!(gone_within(pid, Duration::from_secs(10)));
}

#[test]
fn taking_session_out_of_the_grant_ends_a_running_session_within_the_check() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, peer) = node.link(grant());
    let mut request = Request::argv([helper(), "sleep".to_owned(), "60000".to_owned()]);
    request.session = Some(detached());
    let (exec, pid, _) = start(&link, &request);
    drop(exec);
    assert!(support::alive(pid));
    node.grants
        .set(peer, Grant::allowing([Permission::Exec, Permission::Shell]));
    assert!(
        gone_within(pid, SLACK + Duration::from_secs(2)),
        "the revoked session's process is still running"
    );
}

#[test]
fn removing_the_key_ends_its_sessions_too() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, peer) = node.link(grant());
    let mut request = Request::argv([helper(), "sleep".to_owned(), "60000".to_owned()]);
    request.session = Some(detached());
    let (exec, pid, _) = start(&link, &request);
    drop(exec);
    node.keys.remove(&peer);
    assert!(gone_within(pid, SLACK + Duration::from_secs(2)));
}

#[test]
fn a_request_for_a_session_without_the_permission_is_denied_and_nothing_starts() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(Grant::allowing([Permission::Exec]));
    let mut request = Request::argv([helper(), "sleep".to_owned(), "60000".to_owned()]);
    request.session = Some(detached());
    let exec = Exec::start(&link, &request).unwrap();
    match exec.next(WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Denied),
        other => panic!("{other:?}"),
    }
    // Without `exec` as well, a peer with only `session` is refused before the request is read: the link does not offer
    // `bohio-exec/1` to it.
    let (link, _) = node.link(Grant::allowing([Permission::Session]));
    let exec = Exec::start(&link, &request).unwrap();
    assert!(
        matches!(
            exec.next(WAIT),
            Err(Error::Transport(coqui_transport::Failure::Permission))
        ),
        "refused at the link"
    );
}

#[test]
fn the_sessions_a_peer_and_the_host_may_hold_are_bounded_by_settings() {
    let _deadline = Deadline::start();
    let node = node_with(SessionsConfig {
        max_per_peer: 1,
        max_detached: 2,
        ..SessionsConfig::default()
    });
    let (link, _) = node.link(grant());
    let sleeping = |ms: &str| {
        let mut request = Request::argv([helper(), "sleep".to_owned(), ms.to_owned()]);
        request.session = Some(detached());
        request
    };
    // A scrollback past what the host keeps is refused, not trimmed.
    let mut greedy = sleeping("60000");
    greedy.session = Some(SessionRequest {
        scrollback: Some(u64::MAX),
        ..detached()
    });
    match Exec::start(&link, &greedy).unwrap().next(WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::BadRequest),
        other => panic!("{other:?}"),
    }
    let (first, pid, id) = start(&link, &sleeping("60000"));
    let second = Exec::start(&link, &sleeping("60000")).unwrap();
    match second.next(WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Busy),
        other => panic!("{other:?}"),
    }
    let (another, _) = node.link(grant());
    let (third, other_pid, _) = start(&another, &sleeping("60000"));
    let (fourth, _) = node.link(grant());
    match Exec::start(&fourth, &sleeping("60000")).unwrap().next(WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Busy),
        other => panic!("{other:?}"),
    }
    Control::open(&link)
        .unwrap()
        .end_session(&id, None, WAIT)
        .unwrap();
    assert!(gone_within(pid, Duration::from_secs(10)));
    // The host stops (its clients have gone): the session that was left running goes with it.
    assert!(support::alive(other_pid));
    drop((first, third, fourth, link, another));
    drop(node);
    assert!(
        gone_within(other_pid, Duration::from_secs(10)),
        "the host's stop ends every session"
    );
}

#[test]
fn ending_a_session_with_a_signal_runs_the_programs_handler_and_a_kill_ends_the_tree() {
    use bohio_proto::signal::Signal as S;
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(grant());
    let mut request = Request::argv([helper(), "trap".to_owned()]);
    request.session = Some(detached());
    let (exec, trap_pid, trap_id) = start(&link, &request);
    // Wait for `ready`, then detach.
    let mut said = Vec::new();
    while !String::from_utf8_lossy(&said).contains("ready") {
        if let Some(Event::Stdout(bytes)) = exec.next(WAIT).unwrap() {
            said.extend_from_slice(&bytes);
        }
    }
    drop(exec);
    let control = Control::open(&link).unwrap();
    let interrupt = if cfg!(windows)
        && !bohio::info::collect(&NodeConfig::default(), &std::ffi::OsString::new())
            .exec
            .signals
            .contains(&S::Interrupt)
    {
        None
    } else {
        Some(S::Interrupt)
    };
    control.end_session(&trap_id, interrupt, WAIT).unwrap();
    if interrupt.is_some() {
        let exit = exited(&control, &trap_id);
        assert_eq!(exit.code, Some(3), "the handler ran: {exit:?}");
        let (exec, _) = Exec::attach(&link, &trap_id, Some(0)).unwrap();
        let (out, _) = read_to_exit(&exec);
        assert!(String::from_utf8_lossy(&out).contains("got-int"));
    } else {
        // Without a console to send the break through, the host's own answer is that the program keeps running.
        std::thread::sleep(Duration::from_millis(500));
        assert!(support::alive(trap_pid));
        control
            .end_session(&trap_id, Some(Signal::Kill), WAIT)
            .unwrap();
    }
    assert!(gone_within(trap_pid, Duration::from_secs(10)));

    let mut request = Request::argv([helper(), "tree".to_owned(), "60000".to_owned()]);
    request.session = Some(detached());
    let (exec, pid, id) = start(&link, &request);
    let mut said = Vec::new();
    while !said.contains(&b'\n') {
        if let Some(Event::Stdout(bytes)) = exec.next(WAIT).unwrap() {
            said.extend_from_slice(&bytes);
        }
    }
    let grandchild: u32 = String::from_utf8_lossy(&said).trim().parse().unwrap();
    drop(exec);
    control.end_session(&id, None, WAIT).unwrap();
    let exit = exited(&control, &id);
    assert!(exit.killed, "{exit:?}");
    assert!(gone_within(pid, Duration::from_secs(10)));
    assert!(gone_within(grandchild, Duration::from_secs(10)));
}

#[test]
fn compression_is_not_offered_to_a_session_and_the_host_says_what_it_keeps() {
    let _deadline = Deadline::start();
    let node = node();
    let (link, _) = node.link(Grant::allowing([
        Permission::Exec,
        Permission::Session,
        Permission::Info,
    ]));
    let mut request = Request::argv([helper(), "echo".to_owned(), "x".to_owned()]);
    request.session = Some(detached());
    request.output_compression = Some(bohio_proto::compress::Compression::auto());
    match Exec::start(&link, &request).unwrap().next(WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Unsupported),
        other => panic!("{other:?}"),
    }
    let info = Control::open(&link).unwrap().info(WAIT).unwrap();
    assert!(info.sessions.enabled);
    assert_eq!(info.sessions.scrollback, 1024 * 1024);
    assert!(info.exec.signals.contains(&Signal::Terminate));
}

// The command line.

use std::path::Path;
use support::embedded::{Spawned, bohio, client_config, id_of, remote_args};

/// A node that grants a command-line peer `grant`: the node, the peer's configuration and the node's id.
fn node_and_peer(dir: &Path, node: Running, grant: Grant) -> (Running, String, String) {
    let config = client_config(dir, "peer");
    let peer = id_of(&config);
    node.keys.accept(peer);
    node.grants.set(peer, grant);
    let server = node.id.to_string();
    (node, config.to_string_lossy().into_owned(), server)
}

fn remote(node: &Running, config: &str, server: &str, words: &[&str]) -> Spawned {
    let mut args = remote_args(node, config, server);
    args.extend_from_slice(words);
    Spawned::start(bohio(&args))
}

fn first_line(out: &[u8]) -> String {
    String::from_utf8_lossy(out)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[test]
fn the_command_line_detaches_lists_attaches_after_a_killed_client_and_ends() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let (node, config, server) = node_and_peer(dir.path(), node(), grant());
    let helper = helper();

    // `exec --detach` prints the id and ends; the program goes on.
    let mut start = remote(
        &node,
        &config,
        &server,
        &[
            "exec", "--detach", "--name", "counter", "--", &helper, "count", "200", "25",
        ],
    );
    assert_eq!(start.finish(), Some(0), "{}", start.stderr());
    let id = first_line(&start.stdout());
    assert_eq!(id.len(), 32, "{id}");

    // A client that attaches and is then killed (the process, not a goodbye on its link) loses nothing.
    let attached = remote(&node, &config, &server, &["attach", &id, "--from", "0"]);
    let until = Instant::now() + Duration::from_secs(30);
    while attached.stdout().len() < 20 {
        assert!(Instant::now() < until, "no output: {}", attached.stderr());
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut attached = attached;
    attached.child.kill().expect("the client is killed");
    let _ = attached.child.wait();

    let mut sessions = remote(&node, &config, &server, &["sessions"]);
    assert_eq!(sessions.finish(), Some(0), "{}", sessions.stderr());
    let listing = String::from_utf8_lossy(&sessions.stdout()).into_owned();
    assert!(
        listing.contains(&id) && listing.contains("counter"),
        "{listing}"
    );

    let mut again = remote(&node, &config, &server, &["attach", &id, "--from", "0"]);
    assert_eq!(again.finish(), Some(0), "{}", again.stderr());
    assert_eq!(numbers(&again.stdout()), (1..=200).collect::<Vec<_>>());

    // `end` ends a running one: the exit is `killed`.
    let mut sleeper = remote(
        &node,
        &config,
        &server,
        &["exec", "--detach", "--", &helper, "sleep", "60000"],
    );
    assert_eq!(sleeper.finish(), Some(0), "{}", sleeper.stderr());
    let sleeping = first_line(&sleeper.stdout());
    let mut end = remote(&node, &config, &server, &["end", &sleeping]);
    assert_eq!(end.finish(), Some(0), "{}", end.stderr());
    let mut json = remote(&node, &config, &server, &["sessions", "--format", "json"]);
    assert_eq!(json.finish(), Some(0), "{}", json.stderr());
    let listed = String::from_utf8_lossy(&json.stdout()).into_owned();
    assert!(listed.contains("\"killed\":true"), "{listed}");

    let mut missing = remote(
        &node,
        &config,
        &server,
        &["end", "00000000000000000000000000000000"],
    );
    assert_eq!(missing.finish(), Some(1));
    assert!(
        missing.stderr().contains("no such session"),
        "{}",
        missing.stderr()
    );
}

#[test]
fn a_detached_terminal_program_is_attached_from_the_command_line_and_exec_on_a_terminal_reports_the_deadline()
 {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let (node, config, server) = node_and_peer(dir.path(), node(), grant());
    let helper = helper();

    let mut pty = remote(
        &node,
        &config,
        &server,
        &[
            "exec",
            "--pty",
            "--timeout-ms",
            "700",
            "--",
            &helper,
            "sleep",
            "60000",
        ],
    );
    assert_eq!(pty.finish(), Some(124), "{}", pty.stderr());

    let mut detached = remote(
        &node,
        &config,
        &server,
        &[
            "exec", "--pty", "--detach", "--", &helper, "count", "10", "20",
        ],
    );
    assert_eq!(detached.finish(), Some(0), "{}", detached.stderr());
    let id = first_line(&detached.stdout());
    let mut attached = remote(&node, &config, &server, &["attach", &id, "--from", "0"]);
    assert_eq!(attached.finish(), Some(0), "{}", attached.stderr());
    let out = String::from_utf8_lossy(&attached.stdout()).into_owned();
    // A terminal's lines carry escape sequences before the first and after the last: a line's number is its trailing digits.
    let seen: Vec<u64> = out
        .split(['\r', '\n'])
        .filter_map(|line| {
            line.rsplit(|c: char| !c.is_ascii_digit())
                .next()?
                .parse()
                .ok()
        })
        .collect();
    assert_eq!(seen, (1..=10).collect::<Vec<_>>(), "{out:?}");
}

#[cfg(unix)]
#[test]
fn a_ctrl_c_of_the_command_is_an_interrupt_to_the_program_and_a_second_one_ends_its_tree() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let (node, config, server) = node_and_peer(dir.path(), node(), grant());
    let helper = helper();
    let signal = |pid: u32, name: &str| {
        let status = std::process::Command::new("kill")
            .args([name, &pid.to_string()])
            .status()
            .expect("kill runs");
        assert!(status.success());
    };
    let wait_for = |child: &Spawned, text: &str| {
        let until = Instant::now() + Duration::from_secs(30);
        while !String::from_utf8_lossy(&child.stdout()).contains(text) {
            assert!(Instant::now() < until, "no `{text}`: {}", child.stderr());
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let mut trap = remote(&node, &config, &server, &["exec", "--", &helper, "trap"]);
    wait_for(&trap, "ready");
    signal(trap.child.id(), "-INT");
    assert_eq!(trap.finish(), Some(3), "{}", trap.stderr());
    assert!(String::from_utf8_lossy(&trap.stdout()).contains("got-int"));

    // A program that ignores the first interrupt is ended by the second within `--kill-after`.
    let mut stubborn = remote(
        &node,
        &config,
        &server,
        &[
            "exec",
            "--kill-after",
            "10s",
            "--",
            "sh",
            "-c",
            "trap '' INT; echo ready; sleep 60",
        ],
    );
    wait_for(&stubborn, "ready");
    signal(stubborn.child.id(), "-INT");
    std::thread::sleep(Duration::from_millis(500));
    signal(stubborn.child.id(), "-INT");
    assert_eq!(stubborn.finish(), Some(128 + 9), "{}", stubborn.stderr());
}
