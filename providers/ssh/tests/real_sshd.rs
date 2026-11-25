//! The `ssh` provider against a real OpenSSH server.
//!
//! On Linux the test starts a private `sshd` (no root, loopback, a throwaway host key). On Windows it starts the same
//! inside a WSL distribution (`BOHIO_TEST_WSL_DISTRO`, default `FedoraLinux-44`) and reaches it through WSL's
//! localhost relay. Where there is no `sshd` to start, a test says why it skipped and passes.

use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bohio_api::model::{ExecStart, ProgramSpec, TerminalExit, TerminalOpen};
use bohio_api::{ExecSource, Process, TerminalSource};

/// Makes every server of this process its own: the tests run side by side.
static STARTED: AtomicU64 = AtomicU64::new(0);
use bohio_provider_ssh::{Ssh, SshSettings};
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};

/// Where distributions put the server and its SFTP subsystem (Fedora and macOS, Debian).
const SSHD_PATHS: &str = "/usr/sbin/sshd /usr/local/sbin/sshd";
const SFTP_PATHS: &str = "/usr/libexec/openssh/sftp-server /usr/lib/openssh/sftp-server";

/// A command that reads a bash script from its standard input where the `sshd` lives.
fn on_host() -> Command {
    if cfg!(windows) {
        let distro =
            std::env::var("BOHIO_TEST_WSL_DISTRO").unwrap_or_else(|_| "FedoraLinux-44".to_owned());
        let mut command = Command::new("wsl.exe");
        command.args(["-d", &distro, "-e", "bash", "-s"]);
        command
    } else {
        let mut command = Command::new("bash");
        command.arg("-s");
        command
    }
}

/// Runs `script` on the host and answers its standard output.
fn output_of(script: &str) -> Result<String, String> {
    let mut child = on_host()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start the host shell: {error}"))?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .map_err(|error| format!("could not send the script: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("the host shell failed: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).replace('\0', "");
    if output.status.success() {
        Ok(text.trim().to_owned())
    } else {
        Err(format!(
            "exit {:?}: {}{}",
            output.status.code(),
            text.trim(),
            String::from_utf8_lossy(&output.stderr).replace('\0', "")
        ))
    }
}

struct Sshd {
    dir: String,
    port: u16,
    user: String,
    pin: String,
    known_hosts_line: String,
    key_dir: tempfile::TempDir,
}

impl Sshd {
    /// Starts a private `sshd`, or says why it cannot.
    fn start(extra_config: &str) -> Result<Self, String> {
        let found = output_of(&format!(
            "for f in {SSHD_PATHS}; do [ -x $f ] && echo $f && break; done; \
             for f in {SFTP_PATHS}; do [ -x $f ] && echo $f && break; done; \
             command -v ssh-keygen"
        ))
        .map_err(|why| format!("no OpenSSH server to start ({why})"))?;
        let [sshd, sftp_server, _] = found.lines().collect::<Vec<_>>()[..] else {
            return Err(format!(
                "no OpenSSH server to start (found only: {found:?})"
            ));
        };
        let (sshd, sftp_server) = (sshd.to_owned(), sftp_server.to_owned());

        let key_dir = tempfile::tempdir().unwrap();
        let client = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        client
            .write_openssh_file(key_dir.path().join("client_key"), LineEnding::LF)
            .unwrap();
        let authorized = client.public_key().to_openssh().unwrap();
        assert!(!authorized.contains('\''));

        let user = output_of("id -un")?;
        let dir = format!(
            "/tmp/bohio-sshd-{}-{}",
            std::process::id(),
            STARTED.fetch_add(1, Ordering::Relaxed)
        );
        let extra_config = extra_config.replace("@DIR@", &dir);
        let mut last = String::new();
        for attempt in 0..8u16 {
            let port = 20_000
                + u16::try_from(
                    (u64::from(std::process::id()) * 7919
                        + u64::from(attempt) * 104_729
                        + STARTED.load(Ordering::Relaxed) * 211)
                        % 30_000,
                )
                .unwrap();
            if TcpListener::bind(("127.0.0.1", port)).is_err() {
                continue;
            }
            let script = format!(
                r#"set -e
rm -r -f {dir}; mkdir -p {dir}/scratch
ssh-keygen -q -t ed25519 -N '' -f {dir}/host_key
printf '%s\n' '{authorized}' > {dir}/authorized_keys
chmod 600 {dir}/authorized_keys {dir}/host_key
printf '#!/bin/sh
cat >> {dir}/xauth_added
' > {dir}/xauth_stub
chmod 755 {dir}/xauth_stub
cat > {dir}/sshd_config <<CONFIG
Port {port}
ListenAddress 127.0.0.1
HostKey {dir}/host_key
AuthorizedKeysFile {dir}/authorized_keys
PidFile {dir}/pid
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
StrictModes no
AllowTcpForwarding yes
Subsystem sftp {sftp_server}
LogLevel ERROR
{extra_config}
CONFIG
setsid nohup {sshd} -D -f {dir}/sshd_config -E {dir}/log > /dev/null 2>&1 < /dev/null &
sleep 1
test -s {dir}/pid
echo started
"#
            );
            output_of(&script)?;
            // The server is ready when the port answers.
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                // A client given the target port as its own connects to itself; that is not sshd.
                if TcpStream::connect(("127.0.0.1", port))
                    .is_ok_and(|s| s.local_addr().ok() != s.peer_addr().ok())
                {
                    let pin = output_of(&format!(
                        "awk '{{print $2}}' {dir}/host_key.pub | base64 -d | sha256sum | cut -d' ' -f1"
                    ))?;
                    let public = output_of(&format!("awk '{{print $1, $2}}' {dir}/host_key.pub"))?;
                    return Ok(Self {
                        dir,
                        port,
                        user,
                        pin: format!("sha256:{pin}"),
                        known_hosts_line: format!("[127.0.0.1]:{port} {public}\n"),
                        key_dir,
                    });
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            last = output_of(&format!(
                "cat {dir}/log 2>&1; pkill -f '{dir}/sshd_config' || true"
            ))
            .unwrap_or_default();
        }
        let _ = output_of(&format!("rm -r -f {dir}"));
        Err(format!("sshd did not come up: {last}"))
    }

    fn identity(&self) -> String {
        self.key_dir
            .path()
            .join("client_key")
            .to_string_lossy()
            .into_owned()
    }

    /// Settings that pin the host key and log in with the key file, with no agent.
    fn settings(&self) -> SshSettings {
        let mut settings = SshSettings::new("127.0.0.1");
        settings.port = u64::from(self.port);
        settings.user = Some(self.user.clone());
        settings.identity = Some(self.identity());
        settings.agent = false;
        settings.host_key = Some(self.pin.clone());
        settings.reconnect_after = "0s".to_owned();
        settings
    }

    fn ssh(&self) -> Ssh {
        Ssh::try_from(self.settings()).unwrap()
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = output_of(&format!(
            "kill $(cat {dir}/pid) 2>/dev/null; sleep 0.2; rm -r -f {dir}",
            dir = self.dir
        ));
    }
}

macro_rules! sshd {
    ($config:expr) => {
        match Sshd::start($config) {
            Ok(server) => server,
            Err(why) => {
                // A run that must prove the real server says so, and a missing one is then a failure.
                assert!(
                    std::env::var_os("BOHIO_TEST_REQUIRE_SSHD").is_none(),
                    "no private sshd: {why}"
                );
                eprintln!("SKIPPED: {why}");
                return;
            }
        }
    };
}

struct Done {
    stdout: String,
    stderr: String,
    exit: TerminalExit,
}

fn start(source: &Ssh, words: &[&str]) -> bohio_api::guatiao::library::Object<dyn Process> {
    source
        .start(ExecStart {
            argv: words.iter().map(|word| (*word).to_owned()).collect(),
            stdin: true,
            ..ExecStart::default()
        })
        .unwrap()
}

fn finish(process: &bohio_api::guatiao::library::Object<dyn Process>) -> Done {
    let deadline = Instant::now() + Duration::from_secs(120);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut open = [true, true];
    let mut buffer = [0_u8; 8192];
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
    Done {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit: process.wait(10_000).unwrap().expect("an exit"),
    }
}

fn run(source: &Ssh, script: &str) -> Done {
    let process = start(source, &["sh", "-c", script]);
    process.close_input().unwrap();
    finish(&process)
}

#[test]
fn processes_run_on_a_real_openssh_server() {
    let server = sshd!("");
    let source = server.ssh();

    // The pin is the SHA-256 of the key blob, as `sha256sum` computes it.
    let done = run(&source, "echo out; echo err >&2; exit 3");
    assert_eq!(
        (done.stdout.as_str(), done.stderr.as_str(), done.exit.code),
        ("out\n", "err\n", Some(3))
    );

    // A program that does not exist is the host shell's 127.
    let done = run(&source, "no_such_program_for_bohio");
    assert_eq!(done.exit.code, Some(127));
    assert!(done.stderr.contains("not found"), "{}", done.stderr);

    // Quoting survives a real login shell.
    let process = source
        .start(ExecStart {
            argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '%s|' \"$@\"".to_owned(),
                "arg0".to_owned(),
                "it's".to_owned(),
                "a b".to_owned(),
                "$HOME".to_owned(),
                String::new(),
            ],
            ..ExecStart::default()
        })
        .unwrap();
    assert_eq!(finish(&process).stdout, "it's|a b|$HOME||");

    // The working directory and the environment.
    let scratch = format!("{}/scratch", server.dir);
    let process = source
        .start(ExecStart {
            argv: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "pwd; echo \"$FROM_BOHIO\"".to_owned(),
            ],
            cwd: Some(scratch.clone()),
            env: vec![bohio_api::model::EnvVar {
                name: "FROM_BOHIO".to_owned(),
                value: Some("set 'quoted' value".to_owned()),
            }],
            ..ExecStart::default()
        })
        .unwrap();
    assert_eq!(
        finish(&process).stdout,
        format!("{scratch}\nset 'quoted' value\n")
    );

    // Standard input.
    let process = start(&source, &["cat"]);
    process.write(b"to the host").unwrap();
    process.close_input().unwrap();
    assert_eq!(finish(&process).stdout, "to the host");
}

#[test]
fn a_signal_and_a_kill_end_a_real_program() {
    let server = sshd!("");
    let source = server.ssh();
    let process = start(&source, &["cat"]);
    std::thread::sleep(Duration::from_millis(500));
    let delivery = process.signal("terminate").unwrap();
    assert!(delivery.delivered);
    let exit = process
        .wait(30_000)
        .unwrap()
        .expect("the host ends a program it signalled");
    assert_eq!(exit.signal, Some(15), "{exit:?}");

    let process = start(&source, &["cat"]);
    std::thread::sleep(Duration::from_millis(500));
    let exit = process.kill().unwrap();
    assert_eq!(exit.signal, Some(9));
}

#[test]
fn a_real_terminal_has_a_size_and_follows_a_resize() {
    let server = sshd!("");
    let source = server.ssh();
    let terminal = source
        .open(TerminalOpen {
            program: Some(ProgramSpec {
                argv: vec![
                    "sh".to_owned(),
                    "-c".to_owned(),
                    "stty size; read x; stty size; read x; echo done".to_owned(),
                ],
                ..ProgramSpec::default()
            }),
            cols: 90,
            rows: 25,
            ..TerminalOpen::default()
        })
        .unwrap();
    let mut heard = String::new();
    let mut buffer = [0_u8; 1024];
    let mut hear = |until: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !heard.contains(until) {
            assert!(Instant::now() < deadline, "heard {heard:?}");
            match terminal.read(&mut buffer, 100).unwrap() {
                -1 => break,
                0 => {}
                count => heard.push_str(&String::from_utf8_lossy(
                    &buffer[..usize::try_from(count).unwrap()],
                )),
            }
        }
    };
    hear("25 90");
    terminal.resize(120, 40).unwrap();
    terminal.write(b"\n").unwrap();
    hear("40 120");
    terminal.write(b"\n").unwrap();
    hear("done");
}

#[test]
fn a_real_login_shell_is_interactive_and_an_interrupt_reaches_the_foreground_program() {
    let server = sshd!("");
    let source = server.ssh();
    let terminal = source.open(TerminalOpen::default()).unwrap();
    terminal.write(b"echo shell-ok $((6*7))\n").unwrap();
    terminal.write(b"sleep 300\n").unwrap();
    let mut heard = String::new();
    let mut buffer = [0_u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(60);
    while !heard.contains("shell-ok 42") {
        assert!(Instant::now() < deadline, "heard {heard:?}");
        match terminal.read(&mut buffer, 100).unwrap() {
            -1 => break,
            0 => {}
            count => heard.push_str(&String::from_utf8_lossy(
                &buffer[..usize::try_from(count).unwrap()],
            )),
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(terminal.signal("interrupt").unwrap().delivered);
    terminal.write(b"echo after-interrupt\nexit 5\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let exit = loop {
        if let Some(exit) = terminal.exit().unwrap() {
            break exit;
        }
        assert!(Instant::now() < deadline, "the shell never ended");
        // Keep reading so the host is not held up on a full window.
        let _ = terminal.read(&mut buffer, 50).unwrap();
    };
    assert_eq!(exit.code, Some(5));
}

#[test]
fn the_default_check_and_known_hosts_work_against_a_real_host_key() {
    let server = sshd!("");
    let source = server.ssh();
    assert_eq!(run(&source, "true").exit.code, Some(0));

    // Strict with nothing known: refused, naming the key to pin.
    let mut settings = server.settings();
    settings.host_key = None;
    let known = tempfile::tempdir().unwrap();
    let file = known.path().join("known_hosts");
    settings.known_hosts = Some(file.to_string_lossy().into_owned());
    let unknown = Ssh::try_from(settings.clone()).unwrap();
    let error = unknown
        .start(ExecStart {
            argv: vec!["true".to_owned()],
            ..ExecStart::default()
        })
        .expect_err("an unknown host is refused");
    assert!(error.message().contains(&server.pin), "{}", error.message());

    // The real key as ssh-keygen wrote it is in known_hosts: accepted.
    std::fs::write(&file, &server.known_hosts_line).unwrap();
    let known_source = Ssh::try_from(settings).unwrap();
    assert_eq!(run(&known_source, "true").exit.code, Some(0));
}

#[test]
fn the_channel_limit_stays_below_the_servers_and_a_higher_one_is_the_servers_refusal() {
    let server = sshd!("MaxSessions 3");
    let mut settings = server.settings();
    settings.max_channels = 8;
    settings.operation_timeout = "5s".to_owned();
    let source = Ssh::try_from(settings).unwrap();
    let held: Vec<_> = (0..3).map(|_| start(&source, &["cat"])).collect();
    let error = source
        .start(ExecStart {
            argv: vec!["true".to_owned()],
            ..ExecStart::default()
        })
        .expect_err("the fourth session is refused");
    let message = error.message().to_string();
    assert!(
        message.contains("MaxSessions") && message.contains("max_channels"),
        "{message}"
    );
    for process in &held {
        let _ = process.kill();
    }
}

#[test]
fn a_bulk_echo_through_a_real_channel_keeps_its_bytes() {
    // From Windows the relay into WSL has stalled a whole session under full-duplex bulk traffic; the bulk legs run
    // where the server is on the same machine.
    if cfg!(windows) {
        eprintln!(
            "SKIPPED: bulk full-duplex traffic through the Windows-to-WSL relay; it runs inside WSL"
        );
        return;
    }
    let server = sshd!("");
    let source = server.ssh();
    let process = start(&source, &["cat"]);
    let total: usize = 4 * 1024 * 1024;
    let writer = std::thread::scope(|scope| {
        let write = scope.spawn(|| {
            let block = vec![0x5a_u8; 64 * 1024];
            let mut sent = 0;
            while sent < total {
                let take = block.len().min(total - sent);
                let wrote = process.write(&block[..take]).unwrap();
                sent += usize::try_from(wrote).unwrap();
            }
            process.close_input().unwrap();
        });
        let mut got = 0_usize;
        let mut buffer = vec![0_u8; 64 * 1024];
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert!(Instant::now() < deadline, "got {got} of {total}");
            match process.read(1, &mut buffer, 100).unwrap() {
                -1 => break,
                0 => {}
                count => {
                    assert!(
                        buffer[..usize::try_from(count).unwrap()]
                            .iter()
                            .all(|b| *b == 0x5a)
                    );
                    got += usize::try_from(count).unwrap();
                }
            }
        }
        write.join().unwrap();
        got
    });
    assert_eq!(writer, total);
}

#[test]
fn the_exec_contract_holds_against_a_real_openssh_server() {
    let server = sshd!("");
    let start = ExecStart {
        argv: vec!["sh".to_owned(), "-c".to_owned(), "echo contract".to_owned()],
        ..ExecStart::default()
    };
    bohio_api::contract::exec(
        &server.ssh(),
        &start,
        &bohio_api::contract::ContractSettings::default(),
    );
}

fn open_scratch(
    server: &Sshd,
    source: &Ssh,
) -> bohio_api::guatiao::library::Object<dyn bohio_api::Directory> {
    bohio_api::FilesSource::open_dir(source, &format!("{}/scratch", server.dir))
        .expect("the scratch directory opens")
}

#[test]
fn the_files_of_a_real_server_pass_the_contract_suite() {
    let server = sshd!("");
    let source = server.ssh();
    let info = bohio_api::FilesSource::describe(&source).unwrap();
    assert!(info.available, "{info:?}");
    for word in ["atomic_replace", "hard_link", "statvfs", "sync"] {
        assert!(info.offers.contains(&word.to_owned()), "{word}: {info:?}");
    }
    assert!(info.lacks.contains(&"confined".to_owned()));
    bohio_api::contract::files(
        &source,
        &format!("{}/scratch", server.dir),
        &bohio_api::contract::ContractSettings::default(),
    );
}

#[test]
fn a_real_server_replaces_links_and_reports_what_it_holds() {
    use bohio_api::model::OpenModel;
    let server = sshd!("");
    let source = server.ssh();
    let scratch = format!("{}/scratch", server.dir);
    let directory = open_scratch(&server, &source);
    let write = |name: &str, bytes: &[u8]| {
        let file = directory
            .open_file(
                name,
                OpenModel {
                    write: true,
                    create: true,
                    truncate: true,
                    mode: Some(0o640),
                    ..OpenModel::default()
                },
            )
            .unwrap();
        file.write_at(0, bytes).unwrap();
        file.sync().unwrap();
    };
    write("one", b"first");
    write("two", b"second");
    // posix-rename replaces in one step; the plain rename refuses.
    assert!(directory.rename("one", "two", false).is_err());
    directory.rename("one", "two", true).unwrap();
    assert!(directory.stat("one", false).is_err());
    assert_eq!(directory.stat("two", false).unwrap().size, 5);
    assert_eq!(directory.stat("two", false).unwrap().mode, Some(0o640));
    // A hard link, a link with an absolute target and one with a relative target.
    directory.hard_link("two", "hard").unwrap();
    assert_eq!(directory.stat("hard", false).unwrap().size, 5);
    directory
        .symlink(&format!("{scratch}/two"), "abs", false)
        .unwrap();
    assert_eq!(
        directory.read_link("abs").unwrap(),
        format!("{scratch}/two")
    );
    assert_eq!(directory.stat("abs", true).unwrap().size, 5);
    assert_eq!(directory.stat("abs", false).unwrap().kind, "symlink");
    directory.symlink("two", "rel", false).unwrap();
    assert_eq!(directory.read_link("rel").unwrap(), "two");
    // The server's own errors, told apart.
    let word = |error: bohio_api::guatiao::library::ProviderError| {
        bohio_api::files::failure_kind(&error).map(str::to_owned)
    };
    directory.create_dir("full", None, false).unwrap();
    write("full/x", b"x");
    assert_eq!(
        word(directory.remove_dir("full").unwrap_err()).as_deref(),
        Some("not_empty")
    );
    assert_eq!(
        word(directory.create_dir("full", None, false).unwrap_err()).as_deref(),
        Some("exists")
    );
    assert_eq!(
        word(directory.remove_file("full").unwrap_err()).as_deref(),
        Some("is_a_directory")
    );
    assert_eq!(
        word(directory.remove_dir("two").unwrap_err()).as_deref(),
        Some("not_a_directory")
    );
    assert_eq!(
        word(directory.stat("missing", true).unwrap_err()).as_deref(),
        Some("not_found")
    );
    // Attributes: mode, times and the space.
    directory.set_mode("two", 0o600).unwrap();
    assert_eq!(directory.stat("two", false).unwrap().mode, Some(0o600));
    directory.set_readonly("two", true).unwrap();
    assert!(directory.stat("two", false).unwrap().readonly);
    directory
        .set_times(
            "two",
            None,
            Some(bohio_api::model::TimeModel {
                secs: 1_000_000_000,
                nanos: 0,
            }),
        )
        .unwrap();
    assert_eq!(
        directory
            .stat("two", false)
            .unwrap()
            .modified
            .map(|time| time.secs),
        Some(1_000_000_000)
    );
    assert!(directory.statvfs(".").unwrap().blocks > 0);
    // A listing carries each entry's attributes.
    let reader = directory.read_dir(".").unwrap();
    let mut entries = Vec::new();
    loop {
        let page = reader.next(1000).unwrap();
        if page.is_empty() {
            break;
        }
        entries.extend(page);
    }
    let two = entries.iter().find(|entry| entry.name == "two").unwrap();
    assert_eq!(two.stat.as_ref().unwrap().size, 5);
    assert!(entries.iter().all(|entry| entry.stat.is_some()));
    directory.remove_dir_all("full").unwrap();
}

#[test]
fn a_large_file_goes_to_a_real_server_and_comes_back_whole() {
    use bohio_api::model::OpenModel;
    let server = sshd!("");
    let source = server.ssh();
    let directory = open_scratch(&server, &source);
    let size: usize = 32 * 1024 * 1024 + 7;
    let pattern = |at: usize| ((at.wrapping_mul(2_654_435_761) >> 13) & 0xff) as u8;
    let file = directory
        .open_file(
            "large.bin",
            OpenModel {
                write: true,
                create: true,
                truncate: true,
                ..OpenModel::default()
            },
        )
        .unwrap();
    let mut block = vec![0_u8; 256 * 1024];
    let mut at = 0_usize;
    while at < size {
        let take = block.len().min(size - at);
        for (index, byte) in block[..take].iter_mut().enumerate() {
            *byte = pattern(at + index);
        }
        assert_eq!(
            file.write_at(at as u64, &block[..take]).unwrap(),
            take as u64
        );
        at += take;
    }
    file.sync().unwrap();
    drop(file);
    assert_eq!(
        directory.stat("large.bin", false).unwrap().size,
        size as u64
    );
    let file = directory
        .open_file(
            "large.bin",
            OpenModel {
                read: true,
                ..OpenModel::default()
            },
        )
        .unwrap();
    let mut at = 0_usize;
    while at < size {
        let got = usize::try_from(file.read_at(at as u64, &mut block).unwrap()).unwrap();
        assert!(got > 0, "the file ended at {at} of {size}");
        for (index, byte) in block[..got].iter().enumerate() {
            assert_eq!(*byte, pattern(at + index), "byte {}", at + index);
        }
        at += got;
    }
    assert_eq!(file.read_at(size as u64, &mut block).unwrap(), 0);
}

// ---------------------------------------------------------------------------------------------------------------
// The host's X display.

/// The X server these tests use on the host: the first socket there (a desktop session's, or a virtual server's).
fn host_x_display() -> Option<String> {
    let found = output_of("ls /tmp/.X11-unix 2>/dev/null | head -n 1").ok()?;
    let number = found.strip_prefix('X')?;
    number
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| format!(":{number}"))
}

/// The window program as the host runs it: built beside the tests on Linux, named by `BOHIO_TEST_X11_WINDOW` when the
/// tests run on Windows against a Linux host.
fn window_program() -> Option<String> {
    if cfg!(windows) {
        return std::env::var("BOHIO_TEST_X11_WINDOW").ok();
    }
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.parent()?.join("examples").join("x11_window");
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// A window on the host's X display, started by the host itself and ended with the test.
struct Window {
    dir: String,
}

impl Window {
    fn start(server: &Sshd, program: &str, display: &str, extra: &str) -> Result<Self, String> {
        let dir = server.dir.clone();
        let text = output_of(&format!(
            "DISPLAY={display} nohup {program} --seconds 120 {extra} > {dir}/window.out 2>&1 < /dev/null &\n\
             echo $! > {dir}/window.pid\n\
             for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do grep -q mapped {dir}/window.out && break; sleep 0.25; done\n\
             cat {dir}/window.out"
        ))?;
        if !text.contains("mapped") {
            return Err(format!("the window did not appear: {text}"));
        }
        Ok(Self { dir })
    }

    fn heard(&self) -> String {
        output_of(&format!("cat {}/window.out", self.dir)).unwrap_or_default()
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = output_of(&format!("kill $(cat {}/window.pid) 2>/dev/null", self.dir));
    }
}

#[test]
fn the_hosts_x_display_is_a_desktop_that_is_seen_and_clicked() {
    use bohio_api::DesktopSource;
    use bohio_api::model::InputModel;

    let server = sshd!("");
    let Some(display) = host_x_display() else {
        eprintln!("SKIPPED: the host has no X server");
        return;
    };
    let Some(program) = window_program() else {
        eprintln!(
            "SKIPPED: no x11_window program for the host (cargo build -p bohio-x11 --example x11_window)"
        );
        return;
    };
    let window = Window::start(
        &server,
        &program,
        &display,
        "--x 100 --y 100 --size 240 160 --rgb 00ff00",
    )
    .unwrap();

    let mut settings = server.settings();
    settings.x11_display = Some(display);
    let source = Ssh::try_from(settings).unwrap();
    let info = DesktopSource::describe(&source).unwrap();
    assert!(info.available, "{:?}", info.reason);
    assert_eq!(info.provider, "ssh");
    assert!(info.offers.iter().any(|word| word == "capture"), "{info:?}");

    let session = DesktopSource::open(&source).unwrap();
    let state = session.state().unwrap();
    let shown = state.displays.first().expect("a display");
    let size = session.frame_size(&shown.id, None).unwrap();
    let mut rgb = vec![0_u8; usize::try_from(size.len()).unwrap()];
    // Somewhere in the picture is the window's green: its middle, wherever the host's screen puts it. The server may
    // take a moment to show a window it has mapped.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (header, green) = loop {
        let header = session.capture(&shown.id, None, &mut rgb).unwrap();
        let green = rgb
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| **p == [0, 255, 0])
            .count();
        if green > 240 * 160 / 2 || Instant::now() > deadline {
            break (header, green);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let width = usize::try_from(header.width).unwrap();
    assert!(
        green > 240 * 160 / 2,
        "only {green} green pixels in {width}x{}",
        header.height
    );

    // A click lands on the window: the program is told where in the window.
    let at = rgb
        .as_chunks::<3>()
        .0
        .iter()
        .position(|pixel| *pixel == [0, 255, 0])
        .unwrap();
    let (x, y) = (
        i64::try_from(at % width).unwrap() + 120,
        i64::try_from(at / width).unwrap() + 80,
    );
    // The pointer is put on the window first, as a person would, so that the server delivers the press to it.
    session
        .input(vec![InputModel {
            kind: "move".to_owned(),
            display: Some(shown.id.clone()),
            x: Some(x),
            y: Some(y),
            ..InputModel::default()
        }])
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let taken = session
        .input(vec![InputModel {
            kind: "button".to_owned(),
            display: Some(shown.id.clone()),
            x: Some(x),
            y: Some(y),
            button: Some("left".to_owned()),
            action: Some("click".to_owned()),
            ..InputModel::default()
        }])
        .unwrap();
    assert_eq!(taken, 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !window.heard().contains("click") {
        assert!(
            Instant::now() < deadline,
            "the window never heard the click: {}",
            window.heard()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_source_without_a_display_or_with_a_missing_one_says_why() {
    use bohio_api::DesktopSource;

    let server = sshd!("");
    let source = server.ssh();
    let info = DesktopSource::describe(&source).unwrap();
    assert!(!info.available);
    assert!(info.reason.unwrap().contains("x11_display"));

    let mut settings = server.settings();
    settings.x11_display = Some(":987".to_owned());
    let source = Ssh::try_from(settings).unwrap();
    let info = DesktopSource::describe(&source).unwrap();
    assert!(!info.available);
    let reason = info.reason.unwrap();
    assert!(reason.contains("987"), "{reason}");
}

/// What a forwarded program needs on the host: sshd allows it, and its `xauth` is a stub that keeps what it is told.
const FORWARDING: &str = "X11Forwarding yes\nXAuthLocation @DIR@/xauth_stub\n";

#[test]
fn a_program_the_source_starts_opens_its_window_on_the_nodes_display() {
    let server = sshd!(FORWARDING);
    if cfg!(windows) {
        eprintln!("SKIPPED: a Windows node has no X display to forward to");
        return;
    }
    let (Some(display), Some(program)) = (host_x_display(), window_program()) else {
        eprintln!("SKIPPED: the node has no X server or no x11_window program");
        return;
    };
    let mut settings = server.settings();
    settings.x11_forward = true;
    settings.x11_trusted = true;
    settings.x11_local_display = Some(display.clone());
    let source = Ssh::try_from(settings).unwrap();

    // The stub keeps the cookie the source gave the host; the program is handed it the way a real `xauth` would.
    let script = format!(
        "X11_COOKIE=$(awk 'END {{print $NF}}' {}/xauth_added) {program} --seconds 60 --x 300 --y 300 --size 200 120 --rgb 0000ff",
        server.dir
    );
    let process = start(&source, &["sh", "-c", &script]);
    let mut heard = String::new();
    let mut chunk = [0_u8; 512];
    let deadline = Instant::now() + Duration::from_secs(20);
    while !heard.contains("mapped") {
        assert!(
            Instant::now() < deadline,
            "the program never mapped its window: {heard}"
        );
        for stream in [1_u8, 2] {
            if let Ok(count @ 1..) = process.read(stream, &mut chunk, 100) {
                heard.push_str(&String::from_utf8_lossy(
                    &chunk[..usize::try_from(count).unwrap()],
                ));
            }
        }
    }

    // The window is on the node's display: the node's own desktop sees its blue.
    use bohio_x11::Desktop as _;
    let desktop = bohio_x11::X11Desktop::new(bohio_x11::X11Settings {
        display: Some(display),
        headless: true,
        ..bohio_x11::X11Settings::default()
    });
    let state = desktop.state().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let blue = loop {
        let frame = desktop.capture(&state.displays[0].id, None).unwrap();
        let blue = frame
            .rgb()
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| **p == [0, 0, 255])
            .count();
        if blue > 200 * 120 / 2 || Instant::now() > deadline {
            break blue;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        blue > 200 * 120 / 2,
        "only {blue} blue pixels on the node's display"
    );
    process.kill().unwrap();
}

#[test]
fn forwarding_without_trust_is_refused_when_the_source_is_built() {
    let mut settings = SshSettings::new("example.invalid");
    settings.x11_forward = true;
    let error = Ssh::try_from(settings).err().expect("refused");
    assert!(
        error.message().contains("x11_trusted"),
        "{}",
        error.message()
    );
}
