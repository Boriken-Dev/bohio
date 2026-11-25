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
