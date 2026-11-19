//! Interactive shells, served and driven in-process over memory channels.
//!
//! Every session a test opens is dropped before it ends, which makes the host end the shell's
//! process tree; a test that leaves a process behind fails.

mod support;

use std::time::Duration;

use bohio::config::ShellConfig;
use bohio::{Grant, Permission};
use bohio_client::Error;
use bohio_client::shell::{Event, Shell};
use bohio_proto::ErrorCode;
use bohio_proto::shell::{Request, ShellKind};
use support::{Link, alive, gone_within};
use tempfile::TempDir;

/// Long enough for a shell to start and answer on a slow machine, short enough to fail a hang.
const WAIT: Option<Duration> = Some(Duration::from_secs(40));

fn link() -> Link {
    Link::shells(Grant::allowing([Permission::Shell]), ShellConfig::default())
}

/// A request for the host's ordinary shell: `sh` on Unix, `cmd` on Windows.
fn plain() -> Request {
    #[cfg(unix)]
    return Request::kind(ShellKind::Sh);
    #[cfg(windows)]
    return Request::kind(ShellKind::Cmd);
}

/// A shell whose language has variables, objects and a process id for what it starts.
fn scripting() -> Request {
    #[cfg(unix)]
    return Request::kind(ShellKind::Sh);
    #[cfg(windows)]
    return Request::argv(["powershell", "-NoProfile", "-NoLogo"]);
}

/// The line that prints the value of the variable `name`, which the typed text does not hold.
fn echo(name: &str) -> String {
    if cfg!(windows) {
        format!("echo %{name}%\r")
    } else {
        format!("echo ${name}\r")
    }
}

fn with_variable(mut request: Request, name: &str, value: &str) -> Request {
    request.env.set.insert(name.to_owned(), value.to_owned());
    request
}

fn open(link: &Link, request: &Request) -> Shell {
    Shell::start(link, request, WAIT).expect("the shell starts")
}

fn refused(link: &Link, request: &Request) -> ErrorCode {
    match Shell::start(link, request, WAIT) {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The number that follows `prefix` in `output`, where the typed text does not hold one.
fn number_after(output: &[u8], prefix: &str) -> Option<u32> {
    let output = text(output);
    output.match_indices(prefix).find_map(|(at, _)| {
        let digits: String = output[at + prefix.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

#[test]
fn a_shell_echoes_a_marker() {
    let link = link();
    let shell = open(&link, &with_variable(plain(), "BOH_MARK", "marker-4417-ok"));
    shell.write(echo("BOH_MARK").as_bytes()).unwrap();
    let seen = shell.read_until(b"marker-4417-ok", WAIT).unwrap();
    assert!(text(&seen).contains("marker-4417-ok"));
    let pid = shell.pid();
    assert!(
        alive(pid),
        "the shell {pid} is running while its session is open"
    );
    drop(shell);
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "shell {pid} survived"
    );
    let served = link.finished_shell();
    assert_eq!(served.len(), 1);
    assert!(
        served[0].as_ref().is_ok_and(|exit| exit.killed),
        "{served:?}"
    );
}

#[test]
fn the_exit_frame_follows_every_output_byte_and_carries_the_code() {
    let link = link();
    let shell = open(&link, &with_variable(plain(), "BOH_LAST", "last-words-77"));
    shell
        .write(format!("{}exit 3\r", echo("BOH_LAST")).as_bytes())
        .unwrap();
    let (output, exit) = shell.wait(WAIT).expect("the shell exits");
    assert!(
        text(&output).contains("last-words-77"),
        "output before the exit arrives before the exit frame: {:?}",
        text(&output)
    );
    assert_eq!(exit.code, Some(3), "{exit:?}");
    assert!(!exit.killed);
    assert_eq!(shell.next(WAIT).expect("the stream ends in order"), None);
    let served = link.finished_shell();
    assert!(served[0].as_ref().is_ok_and(|served| *served == exit));
}

#[test]
fn a_large_output_is_delivered_whole_before_the_exit() {
    let link = link();
    let shell = open(&link, &plain());
    #[cfg(unix)]
    let (script, last) = ("seq 1 50000; echo END$((1+1)); exit\r", "49999");
    #[cfg(windows)]
    let (script, last) = (
        "for /l %i in (1,1,3000) do @echo %i\recho END%OS%\rexit\r",
        "2999",
    );
    shell.write(script.as_bytes()).unwrap();
    let (output, exit) = shell.wait(WAIT).expect("the shell exits");
    let output = text(&output);
    assert!(output.contains(last), "the end of the numbers is there");
    assert!(
        output.contains("END2") || output.contains("ENDWindows_NT"),
        "{}",
        &output[output.len().saturating_sub(200)..]
    );
    assert_eq!(exit.code, Some(0));
}

#[test]
fn a_resize_is_seen_by_the_program() {
    let link = link();
    let shell = open(&link, &scripting());
    #[cfg(unix)]
    {
        shell.write(b"stty size\r").unwrap();
        shell.read_until(b"24 80", WAIT).expect("the opening size");
        shell.resize(100, 30).unwrap();
        shell.write(b"stty size\r").unwrap();
        shell.read_until(b"30 100", WAIT).expect("the new size");
    }
    #[cfg(windows)]
    {
        let ask =
            "'W=' + $Host.UI.RawUI.WindowSize.Width + 'x' + $Host.UI.RawUI.WindowSize.Height\r";
        shell.write(ask.as_bytes()).unwrap();
        shell
            .read_until(b"W=80x24", WAIT)
            .expect("the opening size");
        shell.resize(100, 30).unwrap();
        shell.write(ask.as_bytes()).unwrap();
        shell.read_until(b"W=100x30", WAIT).expect("the new size");
    }
    // A size outside the terminal's range is ignored, and the session goes on.
    shell.resize(0, 0).unwrap();
    #[cfg(unix)]
    shell.write(b"echo AFTER$((20+22))\r").unwrap();
    #[cfg(windows)]
    shell.write(b"'AFTER' + (20+22)\r").unwrap();
    shell
        .read_until(b"AFTER42", WAIT)
        .expect("the session goes on after a size it ignored");
}

#[test]
fn a_session_opens_at_the_size_the_request_names() {
    let link = link();
    let mut request = scripting();
    request.cols = 132;
    request.rows = 40;
    let shell = open(&link, &request);
    #[cfg(unix)]
    {
        shell.write(b"stty size\r").unwrap();
        shell
            .read_until(b"40 132", WAIT)
            .expect("the requested size");
    }
    #[cfg(windows)]
    {
        let ask =
            "'W=' + $Host.UI.RawUI.WindowSize.Width + 'x' + $Host.UI.RawUI.WindowSize.Height\r";
        shell.write(ask.as_bytes()).unwrap();
        shell
            .read_until(b"W=132x40", WAIT)
            .expect("the requested size");
    }
}

#[test]
fn two_sessions_run_at_once_with_their_own_environment() {
    let link = link();
    let first = open(&link, &with_variable(plain(), "BOH_WHO", "first-session-1"));
    let second = open(
        &link,
        &with_variable(plain(), "BOH_WHO", "second-session-2"),
    );
    assert_ne!(first.pid(), second.pid());
    assert!(alive(first.pid()) && alive(second.pid()));
    // Typed in the opposite order of opening, so mixed-up streams would show.
    second.write(echo("BOH_WHO").as_bytes()).unwrap();
    first.write(echo("BOH_WHO").as_bytes()).unwrap();
    let from_first = first.read_until(b"first-session-1", WAIT).unwrap();
    let from_second = second.read_until(b"second-session-2", WAIT).unwrap();
    assert!(!text(&from_first).contains("second-session-2"));
    assert!(!text(&from_second).contains("first-session-1"));
    let pids = [first.pid(), second.pid()];
    drop((first, second));
    for pid in pids {
        assert!(
            gone_within(pid, Duration::from_secs(15)),
            "shell {pid} survived"
        );
    }
    assert_eq!(link.finished_shell().len(), 2);
}

#[test]
fn a_dropped_client_leaves_no_shell_and_no_process_it_started() {
    let link = link();
    let shell = open(&link, &scripting());
    #[cfg(unix)]
    shell.write(b"sleep 300 &\recho BG$!\r").unwrap();
    #[cfg(windows)]
    shell
        .write(
            b"$p = Start-Process -PassThru -WindowStyle Hidden ping -ArgumentList '-n','300','127.0.0.1'; 'BG' + $p.Id\r",
        )
        .unwrap();
    let seen = shell.read_until(b"BG", WAIT).and_then(|first| {
        // The typed text holds `BG` too: keep reading until a number follows one.
        let mut all = first;
        while number_after(&all, "BG").is_none() {
            match shell.next(WAIT)? {
                Some(Event::Data(bytes)) => all.extend_from_slice(&bytes),
                _ => return Err(Error::Closed),
            }
        }
        Ok(all)
    });
    let seen = seen.expect("the shell reports the process it started");
    let background = number_after(&seen, "BG").expect("a process id");
    let pid = shell.pid();
    assert!(
        alive(background),
        "the background process {background} runs"
    );

    drop(shell);
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "shell {pid} survived"
    );
    assert!(
        gone_within(background, Duration::from_secs(15)),
        "process {background}, started by the shell, survived"
    );
    let served = link.finished_shell();
    assert!(
        served[0].as_ref().is_ok_and(|exit| exit.killed),
        "{served:?}"
    );
}

#[test]
fn a_kill_request_ends_the_shell_and_the_session() {
    let link = link();
    let shell = open(&link, &plain());
    let pid = shell.pid();
    shell.kill().unwrap();
    let (_, exit) = shell.wait(WAIT).expect("the session ends");
    assert!(exit.killed, "{exit:?}");
    assert!(
        gone_within(pid, Duration::from_secs(15)),
        "shell {pid} survived"
    );
}

#[test]
fn the_working_directory_is_where_the_shell_starts() {
    let tmp: TempDir = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("boh_cwd_marker")).unwrap();
    let link = link();
    let mut request = plain();
    request.cwd = Some(
        tmp.path()
            .join("boh_cwd_marker")
            .to_string_lossy()
            .into_owned(),
    );
    let shell = open(&link, &request);
    shell
        .write(if cfg!(windows) {
            b"cd\r".as_slice()
        } else {
            b"pwd\r".as_slice()
        })
        .unwrap();
    let seen = shell.read_until(b"boh_cwd_marker", WAIT).unwrap();
    assert!(text(&seen).contains("boh_cwd_marker"));
}

#[test]
fn a_peer_without_shell_and_a_disabled_host_open_nothing() {
    let none = Link::shells(
        Grant::allowing([Permission::Exec, Permission::Info]),
        ShellConfig::default(),
    );
    assert_eq!(refused(&none, &plain()), ErrorCode::Denied);
    let nothing = Link::shells(Grant::none(), ShellConfig::default());
    assert_eq!(refused(&nothing, &plain()), ErrorCode::Denied);
    let off = Link::shells(
        Grant::allowing([Permission::Shell]),
        ShellConfig {
            enabled: false,
            ..ShellConfig::default()
        },
    );
    assert_eq!(refused(&off, &plain()), ErrorCode::Unsupported);
    for link in [&none, &nothing, &off] {
        assert!(link.finished_shell().iter().all(Result::is_err));
    }
}

#[test]
fn impossible_requests_are_structured_refusals() {
    let link = link();
    assert_eq!(
        refused(&link, &Request::argv(Vec::<String>::new())),
        ErrorCode::BadRequest
    );
    assert_eq!(refused(&link, &Request::argv([""])), ErrorCode::BadRequest);
    assert_eq!(
        refused(&link, &Request::argv(["sh", "a\0b"])),
        ErrorCode::BadRequest
    );
    assert_eq!(
        refused(&link, &Request::kind(ShellKind::Other)),
        ErrorCode::BadRequest
    );
    assert_eq!(
        refused(&link, &Request::argv(["bohio-no-such-program-anywhere"])),
        ErrorCode::NotFound
    );
    let mut sized = plain();
    sized.cols = 0;
    assert_eq!(refused(&link, &sized), ErrorCode::BadRequest);
    sized.cols = 80;
    sized.rows = 40_000;
    assert_eq!(refused(&link, &sized), ErrorCode::BadRequest);
    let mut missing = plain();
    missing.cwd = Some("/bohio/no/such/directory/anywhere".into());
    assert_eq!(refused(&link, &missing), ErrorCode::NotFound);
    assert!(link.finished_shell().iter().all(Result::is_err));
}

#[test]
fn a_kind_the_host_lacks_is_not_found() {
    // The shells of the other family are absent from either host.
    #[cfg(unix)]
    let absent = ShellKind::Cmd;
    #[cfg(windows)]
    let absent = ShellKind::Zsh;
    assert_eq!(
        refused(&link(), &Request::kind(absent)),
        ErrorCode::NotFound
    );
}

#[test]
fn an_explicit_program_must_be_on_the_grants_program_list() {
    #[cfg(unix)]
    let (listed, other) = ("sh", "bash");
    #[cfg(windows)]
    let (listed, other) = ("cmd", "powershell");
    let grant = Grant::allowing([Permission::Shell]).with_programs([listed]);
    let link = Link::shells(grant, ShellConfig::default());
    assert_eq!(refused(&link, &Request::argv([other])), ErrorCode::Denied);
    let allowed = open(&link, &Request::argv([listed]));
    let pid = allowed.pid();
    drop(allowed);
    assert!(gone_within(pid, Duration::from_secs(15)));
}

#[test]
fn sessions_are_bounded_by_max_sessions_and_a_slot_is_freed_when_one_ends() {
    let link = Link::shells(
        Grant::allowing([Permission::Shell]),
        ShellConfig {
            max_sessions: 1,
            ..ShellConfig::default()
        },
    );
    let held = open(&link, &plain());
    assert_eq!(refused(&link, &plain()), ErrorCode::Busy);
    let pid = held.pid();
    drop(held);
    assert!(gone_within(pid, Duration::from_secs(15)));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match Shell::start(&link, &plain(), WAIT) {
            Ok(again) => {
                drop(again);
                break;
            }
            Err(Error::Remote(body))
                if body.code == ErrorCode::Busy && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(other) => panic!("{other:?}"),
        }
    }
}

#[test]
fn an_unreadable_open_is_a_bad_request_and_the_stream_is_left_to_the_caller() {
    use bohio_client::Open;
    let link = link();
    let channel = link
        .open(bohio_proto::protocol::SHELL, &[0xff, 0xff])
        .unwrap();
    let frame = channel.receive(WAIT).unwrap().unwrap();
    assert_eq!(frame[0], 0x05);
    let body: bohio_proto::ErrorBody = bohio_proto::decode(&frame[1..]).unwrap();
    assert_eq!(body.code, ErrorCode::BadRequest);
    channel.close();
}
