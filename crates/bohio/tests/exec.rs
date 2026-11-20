//! One-shot process execution, served and driven in-process over memory channels.

mod support;

use std::time::{Duration, Instant};

use bohio::config::{ExecutionConfig, Span};
use bohio::{Grant, Permission};
use bohio_client::Error;
use bohio_client::exec::{Completed, Event, Exec};
use bohio_proto::ErrorCode;
use bohio_proto::exec::{Env, Request, Stdin};
use bohio_proto::shell::ShellKind;
use support::{Link, WAIT, alive, gone_within, helper};

fn runner() -> Link {
    Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    )
}

fn run(link: &Link, request: &Request) -> Completed {
    Exec::start(link, request)
        .expect("the stream opens")
        .wait(WAIT)
        .expect("the process ends")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn refused(link: &Link, request: &Request) -> ErrorCode {
    match Exec::start(link, request)
        .expect("the stream opens")
        .wait(WAIT)
    {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn an_argv_echo_returns_its_stdout_and_exit_zero() {
    let link = runner();
    let done = run(
        &link,
        &Request::argv([helper().as_str(), "echo", "hello", "world"]),
    );
    assert_eq!(text(&done.stdout), "hello world");
    assert!(done.stderr.is_empty());
    assert_eq!(done.exit.code, Some(0));
    assert!(!done.exit.killed && !done.exit.timed_out);
    assert!(done.pid.is_some());
    assert!(link.finished().iter().all(Result::is_ok));
}

#[test]
fn a_nonzero_exit_and_stderr_arrive_separately_from_stdout() {
    let link = runner();
    let done = run(&link, &Request::argv([helper().as_str(), "both"]));
    assert_eq!(text(&done.stdout), "out");
    assert_eq!(text(&done.stderr), "err");
    assert_eq!(done.exit.code, Some(3));
}

#[test]
fn the_exit_frame_is_the_last_and_the_stream_then_ends() {
    let link = runner();
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "echo", "x"])).unwrap();
    let mut last = None;
    while let Some(event) = exec.next(WAIT).expect("events arrive") {
        last = Some(event);
        if matches!(last, Some(Event::Exit(_))) {
            break;
        }
    }
    assert!(matches!(last, Some(Event::Exit(_))));
    assert_eq!(exec.next(WAIT).expect("the stream ends in order"), None);
}

#[test]
fn a_timeout_kills_a_sleeping_child_tree_and_leaves_no_survivor() {
    let link = runner();
    let mut request = Request::argv([helper().as_str(), "tree", "60000"]);
    request.timeout_ms = Some(700);
    let exec = Exec::start(&link, &request).unwrap();
    let mut grandchild = None;
    let mut exit = None;
    while exit.is_none() {
        match exec.next(WAIT).expect("events arrive") {
            Some(Event::Stdout(bytes)) => grandchild = text(&bytes).parse::<u32>().ok(),
            Some(Event::Exit(done)) => exit = Some(done),
            Some(_) => {}
            None => panic!("the stream ended before the exit"),
        }
    }
    let exit = exit.unwrap();
    assert!(exit.timed_out && exit.killed, "{exit:?}");
    let grandchild = grandchild.expect("the helper printed its child's id");
    assert!(
        gone_within(grandchild, Duration::from_secs(10)),
        "the grandchild {grandchild} survived the timeout"
    );
    assert!(link.finished().iter().all(Result::is_ok));
}

#[test]
fn the_grandchild_is_alive_before_the_deadline() {
    let link = runner();
    let mut request = Request::argv([helper().as_str(), "tree", "60000"]);
    request.timeout_ms = Some(60_000);
    let exec = Exec::start(&link, &request).unwrap();
    let mut grandchild = None;
    while grandchild.is_none() {
        if let Some(Event::Stdout(bytes)) = exec.next(WAIT).unwrap() {
            grandchild = text(&bytes).parse::<u32>().ok();
        }
    }
    let grandchild = grandchild.unwrap();
    assert!(alive(grandchild), "the check can see a running process");
    exec.kill().unwrap();
    let done = exec.wait(WAIT).unwrap();
    assert!(done.exit.killed && !done.exit.timed_out);
    assert!(gone_within(grandchild, Duration::from_secs(10)));
}

#[test]
fn closing_the_stream_ends_the_process_tree() {
    let link = runner();
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "tree", "60000"])).unwrap();
    let mut grandchild = None;
    while grandchild.is_none() {
        if let Some(Event::Stdout(bytes)) = exec.next(WAIT).unwrap() {
            grandchild = text(&bytes).parse::<u32>().ok();
        }
    }
    drop(exec);
    let grandchild = grandchild.unwrap();
    assert!(
        gone_within(grandchild, Duration::from_secs(10)),
        "the grandchild survived the close"
    );
    let served = link.finished();
    assert_eq!(served.len(), 1);
    assert!(served[0].as_ref().is_ok_and(|exit| exit.killed));
}

#[test]
fn stdin_round_trips_through_a_process() {
    let link = runner();
    let mut request = Request::argv([helper().as_str(), "cat"]);
    request.stdin = Stdin::Open;
    let exec = Exec::start(&link, &request).unwrap();
    exec.write_stdin(b"first ").unwrap();
    exec.write_stdin(&[0, 1, 2, 255]).unwrap();
    exec.finish_stdin().unwrap();
    let done = exec.wait(WAIT).unwrap();
    assert_eq!(
        done.stdout,
        [b"first ".as_slice(), &[0, 1, 2, 255]].concat()
    );
    assert_eq!(done.exit.code, Some(0));
}

#[test]
fn a_closed_stdin_gives_the_process_an_empty_input() {
    let link = runner();
    let done = run(&link, &Request::argv([helper().as_str(), "cat"]));
    assert!(done.stdout.is_empty());
    assert_eq!(done.exit.code, Some(0));
}

#[test]
fn an_unknown_program_is_a_structured_not_found() {
    let link = runner();
    assert_eq!(
        refused(&link, &Request::argv(["bohio-no-such-program-anywhere"])),
        ErrorCode::NotFound
    );
    let served = link.finished();
    assert_eq!(served[0].as_ref().unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn a_missing_working_directory_is_not_found() {
    let link = runner();
    let mut request = Request::argv([helper().as_str(), "cwd"]);
    request.cwd = Some("/bohio/no/such/directory/anywhere".into());
    assert_eq!(refused(&link, &request), ErrorCode::NotFound);
}

#[test]
fn a_peer_without_exec_is_refused_and_nothing_starts() {
    let link = Link::new(
        Grant::allowing([Permission::Info]),
        ExecutionConfig::default(),
    );
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "echo", "x"])).unwrap();
    match exec.next(WAIT) {
        Err(Error::Remote(body)) => assert_eq!(body.code, ErrorCode::Denied),
        other => panic!("expected a refusal first, got {other:?}"),
    }
    assert_eq!(link.slots.in_use(), 0);
    let nothing = Link::new(Grant::none(), ExecutionConfig::default());
    assert_eq!(
        refused(&nothing, &Request::argv(["anything"])),
        ErrorCode::Denied
    );
}

#[test]
fn the_program_list_refuses_an_unlisted_program_and_allows_a_listed_one() {
    let helper = helper();
    let link = Link::new(
        Grant::allowing([Permission::Exec]).with_programs([helper.clone()]),
        ExecutionConfig::default(),
    );
    assert_eq!(done_code(&link, &[&helper, "exit", "0"]), Some(0));
    assert_eq!(
        refused(&link, &Request::argv(["not-listed"])),
        ErrorCode::Denied
    );
    // A listed program named another way is a different request.
    let upper = helper.to_uppercase();
    if upper != helper {
        assert_eq!(
            refused(&link, &Request::argv([upper.as_str(), "exit", "0"])),
            ErrorCode::Denied
        );
    }
}

fn done_code(link: &Link, argv: &[&str]) -> Option<i32> {
    run(link, &Request::argv(argv.iter().copied())).exit.code
}

#[test]
fn a_program_list_also_refuses_shell_scripts_since_a_shell_runs_anything() {
    let link = Link::new(
        Grant::allowing([Permission::Exec]).with_programs([helper()]),
        ExecutionConfig::default(),
    );
    assert_eq!(
        refused(&link, &Request::shell(host_shell(), "echo hi")),
        ErrorCode::Denied
    );
}

fn host_shell() -> ShellKind {
    if cfg!(windows) {
        ShellKind::Cmd
    } else {
        ShellKind::Sh
    }
}

#[test]
fn an_explicit_shell_runs_its_script() {
    let link = runner();
    let done = run(&link, &Request::shell(host_shell(), "echo shell-says-hi"));
    assert_eq!(text(&done.stdout), "shell-says-hi");
    assert_eq!(done.exit.code, Some(0));
}

#[test]
fn argv_is_never_interpreted_by_a_shell() {
    let link = runner();
    let done = run(
        &link,
        &Request::argv([helper().as_str(), "echo", "a && b", "$HOME", "%PATH%"]),
    );
    assert_eq!(text(&done.stdout), "a && b $HOME %PATH%");
}

#[test]
fn an_empty_argument_vector_and_garbage_metadata_are_bad_requests() {
    let link = runner();
    assert_eq!(
        refused(&link, &Request::argv(Vec::<String>::new())),
        ErrorCode::BadRequest
    );
    use bohio_client::Open;
    let channel = link
        .open(bohio_proto::protocol::EXEC, &[0xff, 0xfe])
        .unwrap();
    let frame = channel.receive(WAIT).unwrap().expect("an error frame");
    match bohio_proto::exec::Output::decode(&frame).unwrap() {
        bohio_proto::exec::Output::Error(body) => assert_eq!(body.code, ErrorCode::BadRequest),
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn execution_disabled_in_the_configuration_is_unsupported() {
    let config = ExecutionConfig {
        enabled: false,
        ..ExecutionConfig::default()
    };
    let link = Link::new(Grant::allowing([Permission::Exec]), config);
    assert_eq!(
        refused(&link, &Request::argv(["x"])),
        ErrorCode::Unsupported
    );
}

#[test]
fn at_the_process_limit_another_request_is_busy_until_one_ends() {
    let config = ExecutionConfig {
        max_processes: 1,
        ..ExecutionConfig::default()
    };
    let link = Link::new(Grant::allowing([Permission::Exec]), config);
    let first = Exec::start(&link, &Request::argv([helper().as_str(), "sleep", "60000"])).unwrap();
    // Started means the slot is taken.
    assert!(matches!(
        first.next(WAIT).unwrap(),
        Some(Event::Started { .. })
    ));
    assert_eq!(
        refused(&link, &Request::argv([helper().as_str(), "echo", "x"])),
        ErrorCode::Busy
    );
    first.kill().unwrap();
    first.wait(WAIT).unwrap();
    drop(first);
    // The slot frees once the first stream's engine returns.
    let deadline = Instant::now() + Duration::from_secs(10);
    while link.slots.in_use() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let done = run(&link, &Request::argv([helper().as_str(), "echo", "again"]));
    assert_eq!(text(&done.stdout), "again");
}

#[test]
fn the_environment_and_working_directory_apply() {
    let link = runner();
    let directory = std::env::temp_dir().canonicalize().unwrap();
    let mut request = Request::argv([helper().as_str(), "env", "BOHIO_PROBE"]);
    request.env = Env {
        set: [("BOHIO_PROBE".to_owned(), "value 1".to_owned())].into(),
        ..Env::default()
    };
    assert_eq!(text(&run(&link, &request).stdout), "value 1");

    request.env = Env {
        set: [("BOHIO_PROBE".to_owned(), "x".to_owned())].into(),
        remove: vec!["BOHIO_PROBE".to_owned()],
        ..Env::default()
    };
    assert_eq!(text(&run(&link, &request).stdout), "<unset>");

    let mut cwd = Request::argv([helper().as_str(), "cwd"]);
    cwd.cwd = Some(directory.to_string_lossy().into_owned());
    let printed = text(&run(&link, &cwd).stdout);
    let printed = std::path::Path::new(printed.trim_start_matches(r"\\?\"))
        .canonicalize()
        .unwrap();
    assert_eq!(printed, directory);
}

#[test]
fn a_cleared_environment_starts_with_only_what_is_set() {
    let link = runner();
    // `PATH` is in the test's environment; a cleared one must not pass it on.
    let mut request = Request::argv([helper().as_str(), "env", "PATH"]);
    request.env = Env {
        clear: true,
        ..Env::default()
    };
    let done = run(&link, &request);
    assert_eq!(text(&done.stdout), "<unset>");
}

#[test]
fn output_larger_than_the_channel_streams_through_without_loss() {
    let mut link = runner();
    link.capacity = 4096;
    let total = 3 * 1024 * 1024;
    let done = run(
        &link,
        &Request::argv([helper().as_str(), "flood", &total.to_string()]),
    );
    assert_eq!(done.stdout.len(), total);
    assert!(done.stdout.iter().all(|byte| *byte == b'x'));
    assert_eq!(done.exit.code, Some(0));
}

#[test]
fn a_descendant_holding_the_output_open_is_ended_after_the_drain_timeout() {
    let config = ExecutionConfig {
        drain_timeout: Span(Duration::from_millis(300)),
        ..ExecutionConfig::default()
    };
    let link = Link::new(Grant::allowing([Permission::Exec]), config);
    let started = Instant::now();
    let done = run(
        &link,
        &Request::argv([helper().as_str(), "orphan", "60000"]),
    );
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the exit was not held for the orphan"
    );
    assert_eq!(done.exit.code, Some(0));
    let orphan: u32 = text(&done.stdout)
        .parse()
        .expect("the helper printed the orphan's id");
    assert!(
        gone_within(orphan, Duration::from_secs(10)),
        "the orphan {orphan} survived"
    );
}

#[test]
fn a_killed_process_reports_the_signal_on_unix_and_no_code() {
    let link = runner();
    let exec = Exec::start(&link, &Request::argv([helper().as_str(), "sleep", "60000"])).unwrap();
    assert!(matches!(
        exec.next(WAIT).unwrap(),
        Some(Event::Started { .. })
    ));
    exec.kill().unwrap();
    let done = exec.wait(WAIT).unwrap();
    assert!(done.exit.killed);
    if cfg!(unix) {
        assert_eq!(done.exit.signal, Some(9));
        assert_eq!(done.exit.code, None);
    }
}

/// A copy of the helper named `git` in a new directory: what a program list's `git` can resolve to.
fn git_in(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).expect("a directory");
    let target = dir.join(format!("git{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(helper(), &target).expect("a copy of the helper");
    target
}

/// A link whose host finds `git` in `good`, for a peer allowed `git` alone.
fn git_only(good: &std::path::Path) -> Link {
    let mut link = Link::new(
        Grant::allowing([Permission::Exec]).with_programs(["git"]),
        ExecutionConfig::default(),
    );
    link.path = std::sync::Arc::new(good.as_os_str().to_owned());
    link
}

fn whoami(link: &Link, env: Env, cwd: Option<&std::path::Path>) -> Result<String, ErrorCode> {
    let mut request = Request::argv(["git", "whoami"]);
    request.env = env;
    request.cwd = cwd.map(|dir| dir.to_string_lossy().into_owned());
    match Exec::start(link, &request)
        .expect("the stream opens")
        .wait(WAIT)
    {
        Ok(done) => Ok(text(&done.stdout)),
        Err(Error::Remote(body)) => Err(body.code),
        Err(other) => panic!("{other:?}"),
    }
}

/// A variable can make a listed program run code the peer chose (`LD_PRELOAD` loads a library into it,
/// `BASH_ENV` runs a file in every shell it starts), so under a program list a request may change only
/// the variables the grant names.
#[test]
fn under_a_program_list_a_peer_changes_only_the_variables_its_grant_names() {
    let scratch = tempfile::tempdir().expect("a directory");
    let good = scratch.path().join("good");
    git_in(&good);
    let link = git_only(&good);
    let setting = |name: &str| {
        let mut env = Env::default();
        env.set.insert(name.to_owned(), "x".to_owned());
        env
    };
    for name in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "DYLD_INSERT_LIBRARIES",
        "BASH_ENV",
        "GIT_SSH_COMMAND",
        "LANG",
    ] {
        assert_eq!(
            whoami(&link, setting(name), None),
            Err(ErrorCode::Denied),
            "{name} was accepted under a program list"
        );
    }
    let mut removing = Env::default();
    removing.remove.push("HOME".to_owned());
    assert_eq!(whoami(&link, removing, None), Err(ErrorCode::Denied));
    assert_eq!(link.slots.in_use(), 0);

    // What the grant names may be changed, and nothing else; PATH is refused unless the grant names it.
    let mut named = Link::new(
        Grant::allowing([Permission::Exec])
            .with_programs(["git"])
            .with_env(["LANG", "PATH"]),
        ExecutionConfig::default(),
    );
    named.path = std::sync::Arc::new(good.as_os_str().to_owned());
    assert!(whoami(&named, setting("LANG"), None).is_ok());
    let mut both = setting("LANG");
    both.remove.push("LANG".to_owned());
    assert!(whoami(&named, both, None).is_ok());
    assert_eq!(
        whoami(&named, setting("LD_PRELOAD"), None),
        Err(ErrorCode::Denied)
    );
    // Naming PATH is the operator's choice; the program still starts from the host's own path.
    assert!(whoami(&named, setting_path(scratch.path()), None).is_ok());
    let unnamed = {
        let mut other = Link::new(
            Grant::allowing([Permission::Exec])
                .with_programs(["git"])
                .with_env(["LANG"]),
            ExecutionConfig::default(),
        );
        other.path = std::sync::Arc::new(good.as_os_str().to_owned());
        other
    };
    assert_eq!(
        whoami(&unnamed, setting_path(scratch.path()), None),
        Err(ErrorCode::Denied)
    );
    if cfg!(windows) {
        assert!(whoami(&named, setting("lang"), None).is_ok());
    }
}

fn setting_path(dir: &std::path::Path) -> Env {
    let mut env = Env::default();
    env.set
        .insert("PATH".to_owned(), dir.to_string_lossy().into_owned());
    env
}

#[test]
fn a_peer_allowed_git_cannot_run_another_program_by_pointing_path_at_its_own_directory() {
    let scratch = tempfile::tempdir().expect("a directory");
    let (good, evil) = (scratch.path().join("good"), scratch.path().join("evil"));
    let real = git_in(&good);
    let planted = git_in(&evil);
    let link = git_only(&good);

    // The host's own `git` is the one that starts, found on the host's path.
    let started = whoami(&link, Env::default(), None).expect("git runs");
    assert_eq!(
        std::path::Path::new(&started).canonicalize().ok(),
        real.canonicalize().ok()
    );

    // Setting, removing or clearing what finds programs is refused before anything starts.
    assert_eq!(
        whoami(&link, setting_path(&evil), None),
        Err(ErrorCode::Denied)
    );
    if cfg!(windows) {
        let mut env = Env::default();
        env.set
            .insert("Path".to_owned(), evil.to_string_lossy().into_owned());
        assert_eq!(whoami(&link, env, None), Err(ErrorCode::Denied));
    }
    let mut removed = Env::default();
    removed.remove.push("PATH".to_owned());
    assert_eq!(whoami(&link, removed, None), Err(ErrorCode::Denied));
    let cleared = Env {
        clear: true,
        ..Env::default()
    };
    assert_eq!(whoami(&link, cleared, None), Err(ErrorCode::Denied));
    assert_eq!(link.slots.in_use(), 0);

    // A working directory the peer controls is no more a place to find `git` than a PATH is.
    let from_evil = whoami(&link, Env::default(), Some(&evil)).expect("git runs");
    assert_eq!(
        std::path::Path::new(&from_evil).canonicalize().ok(),
        real.canonicalize().ok()
    );
    assert_ne!(
        std::path::Path::new(&from_evil).canonicalize().ok(),
        planted.canonicalize().ok()
    );
}

#[test]
fn without_a_program_list_a_peer_may_point_path_anywhere_since_it_may_run_anything() {
    // The control for the refusal above: the attack works when no list limits it, so the refusal
    // above is what stops it and not an accident of the test's setup.
    let scratch = tempfile::tempdir().expect("a directory");
    let (good, evil) = (scratch.path().join("good"), scratch.path().join("evil"));
    git_in(&good);
    let planted = git_in(&evil);
    let mut link = Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    );
    link.path = std::sync::Arc::new(good.as_os_str().to_owned());
    let started = whoami(&link, setting_path(&evil), None).expect("git runs");
    assert_eq!(
        std::path::Path::new(&started).canonicalize().ok(),
        planted.canonicalize().ok()
    );
}

#[test]
fn a_listed_name_that_the_host_cannot_find_is_not_found_and_a_relative_directory_is_never_searched()
{
    let scratch = tempfile::tempdir().expect("a directory");
    let good = scratch.path().join("good");
    git_in(&good);
    let link = Link::new(
        Grant::allowing([Permission::Exec]).with_programs(["missing", "./git"]),
        ExecutionConfig::default(),
    );
    let mut link = link;
    link.path = std::sync::Arc::new(good.as_os_str().to_owned());
    assert_eq!(
        refused(&link, &Request::argv(["missing"])),
        ErrorCode::NotFound
    );
    assert_eq!(
        refused(&link, &Request::argv(["./git", "whoami"])),
        ErrorCode::NotFound
    );
}
