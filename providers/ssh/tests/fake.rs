//! The `ssh` provider against an in-process SSH server: login, the host-key policy, processes, terminals, the channel
//! limit and reconnecting.

use std::io::Write as _;
use std::time::{Duration, Instant};

use bohio_api::guatiao::library::Object;
use bohio_api::model::{EnvVar, ExecStart, ProgramSpec, TerminalExit, TerminalOpen};
use bohio_api::{ExecSource, Process, TerminalSource};
use bohio_provider_ssh::{Ssh, SshSettings};

mod support;
use support::{FakeSsh, Options, PASSWORD, USER, shell_available};

/// Skips a test, with the reason on the record, when the fake server has no shell to run.
macro_rules! need_shell {
    () => {
        if !shell_available() {
            eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
            return;
        }
    };
}

fn ssh(settings: SshSettings) -> Ssh {
    Ssh::try_from(settings).expect("the settings are accepted")
}

fn argv(words: &[&str]) -> ExecStart {
    ExecStart {
        argv: words.iter().map(|word| (*word).to_owned()).collect(),
        stdin: true,
        ..ExecStart::default()
    }
}

struct Done {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: TerminalExit,
}

/// Reads both outputs of `process` to their ends and waits for its exit.
fn finish(process: &Object<dyn Process>) -> Done {
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut open = [true, true];
    let mut buffer = [0_u8; 4096];
    while open.iter().any(|open| *open) {
        assert!(Instant::now() < deadline, "the program never ended");
        for (index, stream) in [1_u8, 2].into_iter().enumerate() {
            if !open[index] {
                continue;
            }
            match process.read(stream, &mut buffer, 50).unwrap() {
                -1 => open[index] = false,
                0 => {}
                count => {
                    let kept = usize::try_from(count).unwrap();
                    let into = if stream == 1 {
                        &mut stdout
                    } else {
                        &mut stderr
                    };
                    into.extend_from_slice(&buffer[..kept]);
                }
            }
        }
    }
    let exit = process
        .wait(10_000)
        .unwrap()
        .expect("the program has an exit");
    Done {
        stdout,
        stderr,
        exit,
    }
}

fn run(source: &Ssh, words: &[&str]) -> Done {
    let process = source.start(argv(words)).expect("the program starts");
    process.close_input().unwrap();
    finish(&process)
}

/// A directory as the shell the fake server runs names it (Git for Windows' `sh` takes `/c/...`).
fn shell_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    match text.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => {
            format!("/{}/{rest}", drive.to_ascii_lowercase())
        }
        _ => text,
    }
}

fn first_error(source: &Ssh) -> String {
    source
        .start(argv(&["true"]))
        .expect_err("the connection should be refused")
        .message()
        .to_string()
}

#[test]
fn a_program_has_its_output_apart_and_its_exit_status() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let done = run(&source, &["sh", "-c", "echo out; echo err >&2; exit 3"]);
    assert_eq!(done.stdout, b"out\n");
    assert_eq!(done.stderr, b"err\n");
    assert_eq!(done.exit.code, Some(3));
    assert_eq!(done.exit.signal, None);
    assert_eq!(
        server.seen.lock().unwrap().connections,
        1,
        "one connection serves it"
    );
}

#[test]
fn standard_input_reaches_the_program_and_ends_it() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let process = source.start(argv(&["cat"])).unwrap();
    assert_eq!(process.write(b"hello ").unwrap(), 6);
    assert_eq!(process.write(b"there").unwrap(), 5);
    process.close_input().unwrap();
    let done = finish(&process);
    assert_eq!(done.stdout, b"hello there");
    assert_eq!(done.exit.code, Some(0));
}

#[test]
fn the_directory_and_the_environment_are_the_requests() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let dir = tempfile::tempdir().unwrap();
    let mut request = argv(&["sh", "-c", "pwd; echo \"$A|$B\""]);
    request.cwd = Some(shell_path(dir.path()));
    request.env = vec![
        EnvVar {
            name: "A".to_owned(),
            value: Some("it's a value".to_owned()),
        },
        EnvVar {
            name: "B".to_owned(),
            value: None,
        },
    ];
    let process = source.start(request).unwrap();
    process.close_input().unwrap();
    let done = finish(&process);
    let text = String::from_utf8(done.stdout).unwrap();
    let mut lines = text.lines();
    let reported = lines.next().unwrap();
    assert!(
        reported.ends_with(dir.path().file_name().unwrap().to_str().unwrap()),
        "{reported}"
    );
    assert_eq!(lines.next(), Some("it's a value|"));
}

#[test]
fn a_login_shell_that_reads_quotes_as_fish_does_starts_no_program_unless_the_check_is_off() {
    need_shell!();
    let server = FakeSsh::start_with(Options {
        fish: true,
        ..Options::default()
    });
    let source = ssh(server.settings());
    let error = source
        .start(argv(&["echo", "a'; id; #"]))
        .expect_err("a login shell that is not POSIX serves no program");
    assert!(error.message().contains("POSIX"), "{}", error.message());
    // A command with words in a terminal is refused the same way.
    let terminal = TerminalOpen {
        program: Some(bohio_api::model::ProgramSpec {
            argv: vec!["sh".to_owned()],
            ..bohio_api::model::ProgramSpec::default()
        }),
        ..TerminalOpen::default()
    };
    assert!(TerminalSource::open(&source, terminal).is_err());

    // The control: a POSIX login shell starts one, and the operator can skip the check.
    let posix = FakeSsh::start();
    let source = ssh(posix.settings());
    let process = source.start(argv(&["echo", "a'; id; #"])).unwrap();
    assert!(
        String::from_utf8(finish(&process).stdout)
            .unwrap()
            .starts_with("a'; id; #")
    );
    let mut unchecked = server.settings();
    unchecked.login_shell_check = false;
    let source = ssh(unchecked);
    assert!(source.start(argv(&["echo", "hi"])).is_ok());
}

#[test]
fn a_shell_script_runs_on_the_host() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let mut request = ExecStart {
        shell: Some("sh".to_owned()),
        script: Some("echo $((6*7))".to_owned()),
        ..ExecStart::default()
    };
    request.stdin = false;
    let process = source.start(request).unwrap();
    let done = finish(&process);
    assert_eq!(done.stdout, b"42\n");

    let mut cmd = ExecStart {
        shell: Some("cmd".to_owned()),
        script: Some("echo hi".to_owned()),
        ..ExecStart::default()
    };
    cmd.stdin = false;
    let error = source.start(cmd).expect_err("cmd is refused");
    assert!(error.message().contains("cmd"), "{}", error.message());
}

#[test]
fn killing_a_program_ends_it_and_answers_the_same_again() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let process = source.start(argv(&["sleep", "300"])).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let first = process.kill().unwrap();
    assert_eq!(first.signal, Some(9));
    assert_eq!(process.kill().unwrap(), first);
    assert_eq!(process.wait(1_000).unwrap(), Some(first));
    assert!(started.elapsed() < Duration::from_secs(30));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !server
        .seen
        .lock()
        .unwrap()
        .signals
        .iter()
        .any(|name| name.contains("KILL"))
    {
        assert!(Instant::now() < deadline, "the server never saw the signal");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_signal_is_sent_by_name() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let process = source.start(argv(&["sleep", "300"])).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let delivery = process.signal("terminate").unwrap();
    assert!(delivery.delivered && !delivery.ended);
    assert!(process.signal("bogus").is_err());
    let deadline = Instant::now() + Duration::from_secs(30);
    while !server
        .seen
        .lock()
        .unwrap()
        .signals
        .iter()
        .any(|s| s.contains("TERM"))
    {
        assert!(Instant::now() < deadline, "the server never saw the signal");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = process.kill();
}

#[test]
fn a_terminal_has_a_size_that_changes_and_a_login_shell() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let terminal = source
        .open(TerminalOpen {
            program: Some(ProgramSpec {
                argv: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "read x; echo got $x".to_owned(),
                ],
                ..ProgramSpec::default()
            }),
            cols: 100,
            rows: 30,
            ..TerminalOpen::default()
        })
        .unwrap();
    assert_eq!(server.seen.lock().unwrap().ptys, vec![(100, 30)]);
    terminal.resize(120, 40).unwrap();
    terminal.write(b"abc\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut heard = Vec::new();
    let mut buffer = [0_u8; 256];
    while !String::from_utf8_lossy(&heard).contains("got abc") {
        assert!(
            Instant::now() < deadline,
            "heard {:?}",
            String::from_utf8_lossy(&heard)
        );
        match terminal.read(&mut buffer, 100).unwrap() {
            -1 => break,
            0 => {}
            count => heard.extend_from_slice(&buffer[..usize::try_from(count).unwrap()]),
        }
    }
    assert_eq!(server.seen.lock().unwrap().window_changes, vec![(120, 40)]);
    let exit = (|| {
        let until = Instant::now() + Duration::from_secs(30);
        while Instant::now() < until {
            if let Some(exit) = terminal.exit().unwrap() {
                return Some(exit);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    })();
    assert_eq!(exit.expect("it exits").code, Some(0));

    // No program: the account's login shell.
    let shell = source.open(TerminalOpen::default()).unwrap();
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .commands
            .contains(&"<shell>".to_owned())
    );
    let _ = shell.kill();
}

#[test]
fn a_terminal_interrupt_is_the_control_byte() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let terminal = source.open(TerminalOpen::default()).unwrap();
    let delivery = terminal.signal("interrupt").unwrap();
    assert!(delivery.delivered);
    let _ = terminal.kill();
}

#[test]
fn a_pinned_host_key_is_accepted_and_a_different_one_is_refused() {
    need_shell!();
    let server = FakeSsh::start();
    assert_eq!(run(&ssh(server.settings()), &["true"]).exit.code, Some(0));

    let wrong = format!("sha256:{}", "00".repeat(32));
    let mut settings = server.unpinned_settings();
    settings.host_key = Some(wrong.clone());
    let message = first_error(&ssh(settings));
    assert!(message.contains("not the pinned"), "{message}");
    assert!(
        message.contains(&server.pin()) && message.contains(&wrong),
        "{message}"
    );
    assert_eq!(
        server.seen.lock().unwrap().commands.len(),
        // The login shell probe and `true`, both before the wrong key was presented.
        2,
        "nothing ran on the host that presented the wrong key"
    );
}

#[test]
fn the_default_check_refuses_an_unknown_host_and_names_the_key_to_pin() {
    let server = FakeSsh::start();
    let message = first_error(&ssh(server.unpinned_settings()));
    assert!(message.contains(&server.pin()), "{message}");
    assert!(message.contains("host_key"), "{message}");
    assert!(message.contains("tofu"), "{message}");
    assert!(server.seen.lock().unwrap().commands.is_empty());
}

#[test]
fn known_hosts_holds_the_key_and_a_changed_key_is_refused() {
    need_shell!();
    let server = FakeSsh::start();
    let file = server.dir.path().join("known_hosts");
    std::fs::write(&file, server.known_hosts_line()).unwrap();
    let mut settings = server.unpinned_settings();
    settings.known_hosts = Some(file.to_string_lossy().into_owned());
    assert_eq!(run(&ssh(settings), &["true"]).exit.code, Some(0));

    // The same address with another key.
    let other = FakeSsh::start();
    let mut line = other.known_hosts_line();
    line = line.replace(
        &format!("[127.0.0.1]:{}", other.port),
        &format!("[127.0.0.1]:{}", server.port),
    );
    std::fs::write(&file, line).unwrap();
    let mut settings = server.unpinned_settings();
    settings.known_hosts = Some(file.to_string_lossy().into_owned());
    let message = first_error(&ssh(settings));
    assert!(message.contains("line 1"), "{message}");
}

#[test]
fn trust_on_first_use_accepts_the_first_key_and_holds_it() {
    need_shell!();
    let first = FakeSsh::start();
    let port = first.port;
    let keep = tempfile::tempdir().unwrap();
    let key = keep.path().join("client_key");
    std::fs::copy(first.identity(), &key).unwrap();
    let mut settings = first.unpinned_settings();
    settings.host_key_check = "tofu".to_owned();
    settings.identity = Some(key.to_string_lossy().into_owned());
    let source = ssh(settings);
    assert_eq!(run(&source, &["true"]).exit.code, Some(0));
    drop(first);

    // A different server takes the address: its key is not the one first trusted.
    let Some(second) = FakeSsh::start_on(port, &key) else {
        eprintln!("SKIPPED: the port could not be reused for a second server");
        return;
    };
    let message = first_error(&source);
    assert!(message.contains("trusted at first contact"), "{message}");
    assert!(second.seen.lock().unwrap().commands.is_empty());
}

#[test]
fn with_the_check_off_any_key_is_accepted() {
    need_shell!();
    let server = FakeSsh::start();
    let mut settings = server.unpinned_settings();
    settings.host_key_check = "off".to_owned();
    assert_eq!(run(&ssh(settings), &["true"]).exit.code, Some(0));
}

#[test]
fn a_password_logs_in_when_no_key_does() {
    need_shell!();
    let server = FakeSsh::start_with(Options {
        password: true,
        keys: false,
        ..Options::default()
    });
    let mut settings = server.settings();
    settings.identity = None;
    settings.password = Some(PASSWORD.to_owned());
    // The home directory of the test account holds no key the server knows.
    assert_eq!(run(&ssh(settings), &["true"]).exit.code, Some(0));

    let mut wrong = server.settings();
    wrong.password = Some("not it".to_owned());
    let message = first_error(&ssh(wrong));
    assert!(message.contains("password"), "{message}");
    assert!(!message.contains("not it"), "{message}");
}

#[test]
fn login_failures_say_what_failed() {
    let server = FakeSsh::start();
    // A key the server does not know.
    let other = tempfile::tempdir().unwrap();
    let key =
        russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
    let path = other.path().join("stranger");
    key.write_openssh_file(&path, russh::keys::ssh_key::LineEnding::LF)
        .unwrap();
    let mut settings = server.settings();
    settings.identity = Some(path.to_string_lossy().into_owned());
    assert!(first_error(&ssh(settings)).contains("refused the key"));

    // A key file that does not exist.
    let mut settings = server.settings();
    settings.identity = Some(
        server
            .dir
            .path()
            .join("no_such_key")
            .to_string_lossy()
            .into_owned(),
    );
    assert!(first_error(&ssh(settings)).contains("no_such_key"));
}

#[test]
fn a_host_that_does_not_answer_is_unavailable_with_the_reason_and_the_source_still_builds() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let mut settings = SshSettings::new("127.0.0.1");
    settings.port = u64::from(port);
    settings.user = Some(USER.to_owned());
    settings.agent = false;
    let source = ssh(settings);
    let info = TerminalSource::describe(&source).unwrap();
    assert!(!info.available);
    let reason = info.reason.expect("a reason");
    assert!(reason.starts_with(&format!("127.0.0.1:{port}")), "{reason}");
    let info = ExecSource::describe(&source).unwrap();
    assert!(!info.available);
    assert!(info.offers.contains(&"start".to_owned()));
}

#[test]
fn a_reachable_host_is_available_and_says_what_it_offers() {
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let info = ExecSource::describe(&source).unwrap();
    assert!(info.available, "{info:?}");
    assert_eq!(info.provider, "ssh");
    assert!(info.lacks.contains(&"kill_tree".to_owned()));
    let info = TerminalSource::describe(&source).unwrap();
    assert!(info.offers.contains(&"resize".to_owned()));
}

#[test]
fn a_failed_connect_is_not_tried_again_until_reconnect_after() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let mut settings = SshSettings::new("127.0.0.1");
    settings.port = u64::from(port);
    settings.user = Some(USER.to_owned());
    settings.agent = false;
    settings.reconnect_after = "1h".to_owned();
    let source = ssh(settings);
    let first = first_error(&source);
    // The server appears; the source does not look until the wait is over.
    assert_eq!(first_error(&source), first);
}

#[test]
fn a_dropped_connection_is_remade_for_the_next_use() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    assert_eq!(run(&source, &["true"]).exit.code, Some(0));
    assert_eq!(server.seen.lock().unwrap().connections, 1);
    server.disconnect_all();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(run(&source, &["true"]).exit.code, Some(0));
    assert_eq!(server.seen.lock().unwrap().connections, 2);
}

#[test]
fn with_reconnect_off_a_lost_connection_stays_lost() {
    need_shell!();
    let server = FakeSsh::start();
    let mut settings = server.settings();
    settings.reconnect = false;
    let source = ssh(settings);
    assert_eq!(run(&source, &["true"]).exit.code, Some(0));
    server.disconnect_all();
    std::thread::sleep(Duration::from_millis(300));
    let message = first_error(&source);
    assert!(message.contains("reconnect"), "{message}");
    assert_eq!(server.seen.lock().unwrap().connections, 1);
}

#[test]
fn channels_past_max_channels_wait_and_then_are_busy_naming_the_setting() {
    need_shell!();
    let server = FakeSsh::start();
    let mut settings = server.settings();
    settings.max_channels = 2;
    settings.operation_timeout = "1s".to_owned();
    let source = ssh(settings);
    let a = source.start(argv(&["sleep", "300"])).unwrap();
    let b = source.start(argv(&["sleep", "300"])).unwrap();
    let started = Instant::now();
    let error = source
        .start(argv(&["true"]))
        .expect_err("a third is refused");
    assert!(
        error.message().contains("max_channels"),
        "{}",
        error.message()
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    // A channel let go frees its slot.
    let _ = a.kill();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match source.start(argv(&["true"])) {
            Ok(process) => {
                process.close_input().unwrap();
                break;
            }
            Err(error) => {
                assert!(Instant::now() < deadline, "{}", error.message());
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let _ = b.kill();
    assert!(server.seen.lock().unwrap().most_open <= 3);
}

#[test]
fn a_server_that_refuses_a_channel_is_busy_naming_max_channels() {
    need_shell!();
    let server = FakeSsh::start_with(Options {
        max_sessions: 1,
        ..Options::default()
    });
    // The login shell probe takes a channel of its own for a moment; this test counts the program's.
    let mut settings = server.settings();
    settings.login_shell_check = false;
    let source = ssh(settings);
    let first = source.start(argv(&["sleep", "300"])).unwrap();
    let error = source
        .start(argv(&["true"]))
        .expect_err("the server refuses the second");
    let message = error.message().to_string();
    assert!(
        message.contains("MaxSessions") && message.contains("max_channels"),
        "{message}"
    );
    let _ = first.kill();
}

#[test]
fn settings_are_validated_when_the_source_is_built() {
    let base = SshSettings::new("h");
    let mut bad = Vec::new();
    for edit in [
        |s: &mut SshSettings| s.host = String::new(),
        |s: &mut SshSettings| s.port = 0,
        |s: &mut SshSettings| s.port = 70_000,
        |s: &mut SshSettings| s.host_key = Some("sha256:short".to_owned()),
        |s: &mut SshSettings| s.host_key_check = "sometimes".to_owned(),
        |s: &mut SshSettings| s.connect_timeout = "0s".to_owned(),
        |s: &mut SshSettings| s.operation_timeout = "soon".to_owned(),
        |s: &mut SshSettings| s.max_channels = 0,
        |s: &mut SshSettings| s.keepalive_max = 0,
        |s: &mut SshSettings| s.worker_threads = 0,
    ] {
        let mut settings = base.clone();
        settings.user = Some("u".to_owned());
        edit(&mut settings);
        if Ssh::try_from(settings.clone()).is_ok() {
            bad.push(settings);
        }
    }
    assert!(bad.is_empty(), "accepted: {bad:?}");
    // Weaker choices are values, not errors.
    let mut weak = base;
    weak.user = Some("u".to_owned());
    weak.host_key_check = "off".to_owned();
    weak.keepalive = "0s".to_owned();
    weak.max_channels = 50;
    assert!(Ssh::try_from(weak).is_ok());
}

#[test]
fn nothing_the_source_says_holds_a_password() {
    need_shell!();
    let server = FakeSsh::start_with(Options {
        password: true,
        keys: false,
        ..Options::default()
    });
    let mut settings = server.settings();
    settings.identity = None;
    settings.password = Some("a-secret-value".to_owned());
    let source = ssh(settings);
    let reason = format!(
        "{:?} {}",
        TerminalSource::describe(&source),
        first_error(&source)
    );
    assert!(!reason.contains("a-secret-value"), "{reason}");
    let _ = std::io::stderr().flush();
}

#[test]
fn the_exec_contract_holds_against_the_fake_server() {
    need_shell!();
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let start = ExecStart {
        argv: vec!["sh".to_owned(), "-c".to_owned(), "echo contract".to_owned()],
        ..ExecStart::default()
    };
    bohio_api::contract::exec(
        &source,
        &start,
        &bohio_api::contract::ContractSettings::default(),
    );
    // And for a source nobody answers for.
    let mut settings = SshSettings::new("127.0.0.1");
    settings.port = 1;
    settings.user = Some(USER.to_owned());
    settings.agent = false;
    settings.connect_timeout = "2s".to_owned();
    bohio_api::contract::exec(
        &ssh(settings),
        &start,
        &bohio_api::contract::ContractSettings::default(),
    );
}

fn scratch_root() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = shell_path(dir.path());
    (dir, root)
}

fn open_root(source: &Ssh, root: &str) -> Object<dyn bohio_api::Directory> {
    bohio_api::FilesSource::open_dir(source, root).expect("the root opens")
}

fn file_options(write: bool) -> bohio_api::model::OpenModel {
    bohio_api::model::OpenModel {
        read: !write,
        write,
        create: write,
        truncate: write,
        ..Default::default()
    }
}

#[test]
fn the_files_of_the_source_pass_the_contract_suite() {
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let (_dir, root) = scratch_root();
    bohio_api::contract::files(
        &source,
        &root,
        &bohio_api::contract::ContractSettings::default(),
    );
    assert_eq!(
        server.seen.lock().unwrap().connections,
        1,
        "one connection serves it"
    );
}

#[test]
fn small_packets_and_a_short_window_pass_the_contract_suite_too() {
    let server = FakeSsh::start();
    let mut settings = server.settings();
    settings.sftp_packet = 1000;
    settings.sftp_in_flight = 3;
    let source = ssh(settings);
    let (_dir, root) = scratch_root();
    bohio_api::contract::files(
        &source,
        &root,
        &bohio_api::contract::ContractSettings::default(),
    );
}

#[test]
fn a_server_without_the_extensions_says_what_it_lacks_and_refuses_those_calls() {
    let server = FakeSsh::start_with(Options {
        sftp: Some(support::Advertise {
            posix_rename: false,
            hardlink: false,
            statvfs: false,
            fsync: false,
        }),
        ..Options::default()
    });
    let source = ssh(server.settings());
    let info = bohio_api::FilesSource::describe(&source).unwrap();
    assert!(info.available, "{info:?}");
    for word in ["atomic_replace", "hard_link", "statvfs", "sync", "confined"] {
        assert!(info.lacks.contains(&word.to_owned()), "{word}: {info:?}");
    }
    for word in ["list_stats", "posix_paths", "symlink", "set_mode"] {
        assert!(info.offers.contains(&word.to_owned()), "{word}: {info:?}");
    }
    let (_dir, root) = scratch_root();
    bohio_api::contract::files(
        &source,
        &root,
        &bohio_api::contract::ContractSettings::default(),
    );
}

#[test]
fn a_server_with_no_sftp_subsystem_is_unavailable_for_files_and_not_for_programs() {
    need_shell!();
    let server = FakeSsh::start_with(Options {
        sftp: None,
        ..Options::default()
    });
    let source = ssh(server.settings());
    let info = bohio_api::FilesSource::describe(&source).unwrap();
    assert!(!info.available);
    assert!(info.reason.unwrap().contains("sftp"));
    assert_eq!(run(&source, &["true"]).exit.code, Some(0));
}

#[test]
fn a_large_file_is_written_and_read_back_with_every_byte() {
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let (dir, root) = scratch_root();
    let directory = open_root(&source, &root);
    let size = 6 * 1024 * 1024 + 123;
    let content: Vec<u8> = (0..size)
        .map(|index: usize| (index.wrapping_mul(31) >> 3) as u8)
        .collect();
    let file = directory.open_file("big.bin", file_options(true)).unwrap();
    for (index, block) in content.chunks(100_000).enumerate() {
        assert_eq!(
            file.write_at((index * 100_000) as u64, block).unwrap(),
            block.len() as u64
        );
    }
    file.sync().unwrap();
    drop(file);
    assert_eq!(std::fs::read(dir.path().join("big.bin")).unwrap(), content);
    let file = directory.open_file("big.bin", file_options(false)).unwrap();
    let mut back = Vec::new();
    let mut at = 0_u64;
    let mut buffer = vec![0_u8; 70_000];
    loop {
        let got = file.read_at(at, &mut buffer).unwrap();
        if got == 0 {
            break;
        }
        back.extend_from_slice(&buffer[..usize::try_from(got).unwrap()]);
        at += got;
    }
    assert_eq!(back.len(), content.len());
    assert!(back == content, "the bytes differ");
    // A read in the middle, and one past the end.
    let mut small = [0_u8; 10];
    assert_eq!(file.read_at(1_000_003, &mut small).unwrap(), 10);
    assert_eq!(&small, &content[1_000_003..1_000_013]);
    assert_eq!(
        file.read_at(content.len() as u64 + 5, &mut small).unwrap(),
        0
    );
}

#[test]
fn a_listing_is_read_in_pages_with_the_attributes_of_each_entry() {
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let (dir, root) = scratch_root();
    for index in 0..10 {
        std::fs::write(dir.path().join(format!("f{index}")), vec![1_u8; index]).unwrap();
    }
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let directory = open_root(&source, &root);
    let reader = directory.read_dir(".").unwrap();
    let mut seen = Vec::new();
    loop {
        let page = reader.next(4).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 4);
        seen.extend(page);
    }
    seen.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(seen.len(), 11);
    let sub = seen.iter().find(|entry| entry.name == "sub").unwrap();
    assert_eq!(sub.stat.as_ref().unwrap().kind, "dir");
    let f7 = seen.iter().find(|entry| entry.name == "f7").unwrap();
    assert_eq!(f7.stat.as_ref().unwrap().size, 7);
}

#[test]
fn a_dropped_connection_is_remade_for_the_next_file_call() {
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let (_dir, root) = scratch_root();
    let directory = open_root(&source, &root);
    directory.stat(".", true).unwrap();
    server.disconnect_all();
    std::thread::sleep(Duration::from_millis(300));
    // The call that finds the session dead fails; the next one opens another.
    let mut worked = false;
    for _ in 0..3 {
        if directory.stat(".", true).is_ok() {
            worked = true;
            break;
        }
    }
    assert!(worked, "the source never recovered");
    assert!(server.seen.lock().unwrap().connections >= 2);
}

#[test]
fn names_that_leave_a_directory_are_refused_before_they_reach_the_host() {
    let server = FakeSsh::start();
    let source = ssh(server.settings());
    let (_dir, root) = scratch_root();
    let directory = open_root(&source, &root);
    for name in ["../x", "a/../../x", "/etc/passwd"] {
        let error = directory.stat(name, true).expect_err("refused");
        assert_eq!(
            bohio_api::files::failure_kind(&error),
            Some("outside_root"),
            "{name}"
        );
    }
    assert!(bohio_api::FilesSource::open_dir(&source, "relative").is_err());
}

#[test]
fn the_direct_runner_takes_a_command_line_of_the_providers_shape_apart() {
    let line = support::parse_line(
        "cd '/work dir' && exec env -i -u 'GONE' 'A=it'\\''s' 'B=' 'prog' 'one two' '--x'",
    )
    .expect("a line");
    assert_eq!(line.cwd.as_deref(), Some("/work dir"));
    assert!(line.clear_env);
    assert_eq!(
        line.env,
        [
            ("GONE".to_owned(), None),
            ("A".to_owned(), Some("it's".to_owned())),
            ("B".to_owned(), Some(String::new())),
        ]
    );
    assert_eq!(line.argv, ["prog", "one two", "--x"]);
    let plain = support::parse_line("exec 'prog'").expect("a plain line");
    assert!(plain.cwd.is_none() && !plain.clear_env && plain.env.is_empty());
    assert!(support::parse_line("echo hi; rm x").is_none());
    assert!(support::parse_line("exec 'unterminated").is_none());
}

/// A stand-in for the system's `ssh` that records each login and succeeds.
#[cfg(unix)]
fn counting_ssh(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let log = dir.join("logins");
    let program = dir.join("fake-ssh");
    std::fs::write(
        &program,
        format!("#!/bin/sh\necho login >> '{}'\nexit 0\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    (program.to_string_lossy().into_owned(), log)
}

#[cfg(unix)]
#[test]
fn each_info_of_an_openssh_source_does_not_log_in_again() {
    let dir = tempfile::tempdir().unwrap();
    let (program, log) = counting_ssh(dir.path());
    let mut settings = SshSettings::new("host.invalid");
    settings.user = Some(USER.to_owned());
    settings.client = "openssh".to_owned();
    settings.ssh_program = program;
    settings.probe_every = "1h".to_owned();
    let source = ssh(settings);
    std::thread::scope(|scope| {
        for _ in 0..10 {
            scope.spawn(|| {
                assert!(ExecSource::describe(&source).unwrap().available);
                assert!(TerminalSource::describe(&source).unwrap().available);
            });
        }
    });
    let logins = std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(logins, 1, "twenty describes made {logins} logins");
}

#[cfg(unix)]
#[test]
fn with_probe_every_zero_each_info_of_an_openssh_source_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    let (program, log) = counting_ssh(dir.path());
    let mut settings = SshSettings::new("host.invalid");
    settings.user = Some(USER.to_owned());
    settings.client = "openssh".to_owned();
    settings.ssh_program = program;
    settings.probe_every = "0s".to_owned();
    let source = ssh(settings);
    for _ in 0..3 {
        assert!(ExecSource::describe(&source).unwrap().available);
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 3);
}

/// A `known_hosts` line for `server`'s address holding a key of another type than the server's (an RSA key).
fn line_with_another_key_type(server: &FakeSsh) -> String {
    let other = russh::keys::PrivateKey::random(
        &mut rand::rng(),
        russh::keys::Algorithm::Rsa { hash: None },
    )
    .expect("a key");
    format!(
        "[127.0.0.1]:{} {}\n",
        server.port,
        other.public_key().to_openssh().unwrap()
    )
}

#[test]
fn trust_on_first_use_refuses_a_host_known_under_another_key_type() {
    need_shell!();
    let server = FakeSsh::start();
    let file = server.dir.path().join("known_hosts");
    let mut tofu = server.unpinned_settings();
    tofu.host_key_check = "tofu".to_owned();
    tofu.known_hosts = Some(file.to_string_lossy().into_owned());
    // Control: no entry for the host, and the first key is trusted.
    assert_eq!(run(&ssh(tofu.clone()), &["true"]).exit.code, Some(0));

    let other = FakeSsh::start();
    let mut tofu = other.unpinned_settings();
    tofu.host_key_check = "tofu".to_owned();
    tofu.known_hosts = Some(file.to_string_lossy().into_owned());
    std::fs::write(&file, line_with_another_key_type(&other)).unwrap();
    let message = first_error(&ssh(tofu));
    assert!(message.contains("known_hosts"), "{message}");
    assert!(
        other.seen.lock().unwrap().commands.is_empty(),
        "a host listed under another key was trusted"
    );
}

#[test]
fn trust_on_first_use_refuses_a_known_hosts_it_cannot_read() {
    let server = FakeSsh::start();
    let file = server.dir.path().join("known_hosts");
    std::fs::write(
        &file,
        format!("[127.0.0.1]:{} ssh-ed25519 !!not-base64!!\n", server.port),
    )
    .unwrap();
    let mut tofu = server.unpinned_settings();
    tofu.host_key_check = "tofu".to_owned();
    tofu.known_hosts = Some(file.to_string_lossy().into_owned());
    let message = first_error(&ssh(tofu));
    assert!(message.contains("known_hosts"), "{message}");
    assert!(server.seen.lock().unwrap().commands.is_empty());
}

#[cfg(unix)]
#[test]
fn an_openssh_exit_of_255_says_it_may_be_sshs_own_failure_and_what_ssh_said() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("fake-ssh");
    std::fs::write(
        &program,
        "#!/bin/sh\necho 'ssh: connect to host x port 22: Connection refused' >&2\nexit 255\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut settings = SshSettings::new("host.invalid");
    settings.user = Some(USER.to_owned());
    settings.client = "openssh".to_owned();
    settings.ssh_program = program.to_string_lossy().into_owned();
    let done = run(&ssh(settings), &["true"]);
    assert_eq!(done.exit.code, Some(255));
    let said = String::from_utf8_lossy(&done.stderr).into_owned();
    assert!(said.contains("Connection refused"), "{said}");
    assert!(
        said.contains("exited 255") && said.contains("ssh's own"),
        "the status is not marked as possibly ssh's own: {said}"
    );
    // The last thing ssh said is repeated, so it is what the reader sees last.
    assert!(said.trim_end().ends_with("Connection refused"), "{said}");
}
