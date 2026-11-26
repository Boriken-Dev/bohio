//! Provider programs (`[[providers.program]]`): a child that serves a Bohío kind over guatiao's remote protocol on its
//! standard input and output, what the node reports of it, what a program that dies does to the node, and the owner check with
//! its opt-out.
//!
//! The program is `examples/program_echo` (build it first: `cargo build -p bohio --example program_echo`; a whole test run
//! builds it). It is copied into a directory the test makes, which only this account may write, and started from there.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio::access::{ProgramState, ProvidersReport};
use bohio::config::{Config, ConfigError, NodeConfig};
use bohio::node::{MemoryGrants, MemoryKeys, Node, NodeError};
use bohio_api::Terminal;
use bohio_api::guatiao::library::Object;
use bohio_api::model::TerminalOpen;
use cabuya::auth::{Identity, Method};

const WAIT: Duration = Duration::from_secs(30);

/// Where cargo put the example: beside this test binary's `deps` directory, in `examples`.
fn built() -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let deps = exe.parent().expect("the test binary is in a directory");
    let file = format!("program_echo{}", std::env::consts::EXE_SUFFIX);
    for dir in [
        deps.join("..").join("examples"),
        deps.join("examples"),
        deps.to_path_buf(),
    ] {
        let candidate = dir.join(&file);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "{file} is not built: run `cargo build -p bohio --example program_echo` ({})",
        deps.display()
    );
}

/// A directory only this account may write.
fn private_directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private");
    }
    dir
}

/// A copy of the example in a private directory.
fn program() -> (tempfile::TempDir, PathBuf) {
    let dir = private_directory();
    let file = dir
        .path()
        .join(format!("program_echo{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(built(), &file).expect("the copy");
    (dir, file)
}

/// TOML text for a path: a literal string, so a Windows path needs no escaping.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// A node whose only terminal source is `echo`, served by the program `file` started with `arguments`; `table` adds keys
/// to the `[[providers.program]]` entry and `providers` to `[providers]`.
fn config(file: &Path, arguments: &[&str], table: &str, providers: &str) -> NodeConfig {
    let mut command = quoted(file);
    for argument in arguments {
        command.push_str(&format!(", '{argument}'"));
    }
    let text = format!(
        "[providers]\n{providers}\n[[providers.program]]\ncommand = [{command}]\nrestart_delay = \"0s\"\n{table}\n\
         [source.echo]\nprovider = \"program-echo\"\n[access]\nterminal = [\"echo\"]\ndesktop = []\n"
    );
    Config::from_toml(&text, "bohio.cfg")
        .expect("a valid configuration")
        .node()
}

fn build(config: NodeConfig) -> Result<Node, NodeError> {
    Node::new(
        config,
        Identity::generate(Method::DEFAULT).expect("an identity"),
        Arc::new(MemoryKeys::new()),
        Arc::new(MemoryGrants::new()),
    )
}

fn programs(node: &Node) -> ProvidersReport {
    node.sources().libraries()
}

/// A session of the node's `echo` source.
fn open(node: &Node) -> Object<dyn Terminal> {
    node.sources()
        .terminal(Some("echo"))
        .expect("the echo source")
        .get()
        .open(TerminalOpen::default())
        .expect("a session opens")
}

/// Reads until `want` has arrived.
fn read_until(session: &Object<dyn Terminal>, want: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buffer = [0_u8; 256];
    let until = Instant::now() + WAIT;
    while !got.windows(want.len()).any(|window| window == want) {
        assert!(Instant::now() < until, "{want:?} never came: {got:?}");
        let count = session.read(&mut buffer, 100).expect("a read");
        if let Ok(count) = usize::try_from(count) {
            got.extend_from_slice(&buffer[..count]);
        }
    }
    got
}

#[test]
fn a_program_serves_a_console_and_is_reported_with_its_digest() {
    let (_dir, file) = program();
    let node = build(config(&file, &[], "", "")).expect("the node starts");
    let report = programs(&node);
    assert_eq!(report.programs.len(), 1, "{report:?}");
    let program = &report.programs[0];
    assert_eq!(program.state, ProgramState::Running, "{program:?}");
    assert_eq!(program.name, "program-1");
    assert_eq!(program.path, file);
    assert!(
        program.detail.contains("program-echo"),
        "{}",
        program.detail
    );
    assert!(
        program
            .digest
            .as_deref()
            .is_some_and(|d| d.starts_with("sha256:")),
        "{program:?}"
    );
    assert_eq!(program.restarts, 0);
    assert!(report.libraries.is_empty());

    let session = open(&node);
    read_until(&session, b"echo console ready");
    session.write(b"hello\r\n").expect("typing");
    read_until(&session, b"echo: hello");
}

#[test]
fn a_program_is_configured_where_it_runs_so_a_source_setting_has_nowhere_to_go() {
    let (_dir, file) = program();
    let mut config = config(&file, &["--prefix", ">> "], "", "");
    {
        let node = build(config.clone()).expect("the node starts");
        let session = open(&node);
        session.write(b"x\r\n").expect("typing");
        read_until(&session, b">> x");
    }
    config
        .source
        .get_mut("echo")
        .expect("the source")
        .settings
        .insert("prefix".to_owned(), toml::Value::String(">> ".to_owned()));
    let Err(error) = build(config) else {
        panic!("a setting the provider cannot take is refused");
    };
    assert!(error.to_string().contains("source.echo.prefix"), "{error}");
}

#[test]
fn a_program_that_exits_mid_call_fails_that_call_and_the_node_stays_up() {
    let (_dir, file) = program();
    let node = build(config(&file, &[], "", "")).expect("the node starts");
    let session = open(&node);
    read_until(&session, b"echo console ready");
    // The program ends itself in the middle of this call: it never answers.
    let started = Instant::now();
    assert!(session.write(b"crash").is_err(), "the call has no answer");
    assert!(
        started.elapsed() < WAIT,
        "the call failed at once, not at its timeout"
    );
    // The node is up, and says how the program ended.
    let report = programs(&node);
    let program = &report.programs[0];
    assert_eq!(program.state, ProgramState::Failed, "{program:?}");
    assert!(
        program.detail.contains("exited") && program.detail.contains('3'),
        "{}",
        program.detail
    );
    // The next use starts it again, and a session of the new run works; the old session stays failed.
    let again = open(&node);
    read_until(&again, b"echo console ready");
    again.write(b"back\r\n").expect("typing");
    read_until(&again, b"echo: back");
    assert!(session.write(b"x").is_err(), "the old session is gone");
    let report = programs(&node);
    assert_eq!(report.programs[0].state, ProgramState::Running);
    assert_eq!(report.programs[0].restarts, 1);
}

#[test]
fn restart_limit_bounds_the_starts_and_zero_never_starts_again() {
    let (_dir, file) = program();
    let node = build(config(&file, &[], "restart_limit = 1", "")).expect("the node starts");
    let _ = open(&node).write(b"crash");
    // First crash: one restart is allowed.
    let second = open(&node);
    read_until(&second, b"echo console ready");
    let _ = second.write(b"crash");
    // Second crash: the limit is spent, so the entry stays failed and a session cannot open.
    let source = node.sources().terminal(Some("echo")).expect("the source");
    assert!(source.get().open(TerminalOpen::default()).is_err());
    let report = programs(&node);
    assert_eq!(report.programs[0].state, ProgramState::Failed);
    assert_eq!(report.programs[0].restarts, 1);

    let node = build(config(&file, &[], "restart_limit = 0", "")).expect("the node starts");
    let _ = open(&node).write(b"crash");
    let source = node.sources().terminal(Some("echo")).expect("the source");
    assert!(source.get().open(TerminalOpen::default()).is_err());
    assert_eq!(programs(&node).programs[0].restarts, 0);
}

#[test]
fn a_restart_runs_only_the_file_that_was_first_started_unless_restart_if_changed_says_otherwise() {
    for allowed in [false, true] {
        let (_dir, file) = program();
        let table = if allowed {
            "restart_if_changed = true"
        } else {
            ""
        };
        let node = build(config(&file, &[], table, "")).expect("the node starts");
        let first = programs(&node).programs[0]
            .digest
            .clone()
            .expect("a digest");
        let _ = open(&node).write(b"crash");
        // Another file is put where the program was, while it is down.
        let mut bytes = std::fs::read(&file).expect("the program");
        bytes.extend_from_slice(b"\0changed");
        std::fs::write(&file, bytes).expect("replaced");
        let source = node.sources().terminal(Some("echo")).expect("the source");
        let opened = source.get().open(TerminalOpen::default());
        let report = programs(&node);
        let program = &report.programs[0];
        if allowed {
            assert!(opened.is_ok(), "{program:?}");
            assert_ne!(
                program.digest.as_deref(),
                Some(first.as_str()),
                "{program:?}"
            );
            assert_eq!(program.state, ProgramState::Running);
        } else {
            assert!(opened.is_err(), "the changed file ran: {program:?}");
            assert_eq!(program.state, ProgramState::Failed);
            assert!(program.detail.contains("changed"), "{}", program.detail);
            assert_eq!(program.digest.as_deref(), Some(first.as_str()));
        }
    }
}

#[test]
fn a_program_that_ends_at_once_or_never_speaks_is_failed_and_the_node_starts() {
    let (_dir, file) = program();
    // It ends at once: its exit code is in the report. The `[source.echo]` it should serve cannot be built, so the node refuses
    // the configuration naming that key, as it does for a provider no library offers.
    let Err(error) = build(config(&file, &["--exit", "7"], "restart_limit = 0", "")) else {
        panic!("no provider offers `program-echo`");
    };
    assert!(
        error.to_string().contains("source.echo.provider"),
        "{error}"
    );
    // Without a source naming it the node starts and reports the entry.
    let text = format!(
        "[[providers.program]]\ncommand = [{}, '--exit', '7']\nrestart_limit = 0\n",
        quoted(&file)
    );
    let node = build(Config::from_toml(&text, "bohio.cfg").expect("valid").node())
        .expect("the node starts");
    let program = &programs(&node).programs[0];
    assert_eq!(program.state, ProgramState::Failed, "{program:?}");
    assert!(program.detail.contains('7'), "{}", program.detail);

    // It never speaks: the node ends it after `start_timeout` and starts.
    let text = format!(
        "[[providers.program]]\ncommand = [{}, '--silent']\nstart_timeout = \"500ms\"\nrestart_limit = 0\n",
        quoted(&file)
    );
    let started = Instant::now();
    let node = build(Config::from_toml(&text, "bohio.cfg").expect("valid").node())
        .expect("the node starts");
    assert!(started.elapsed() < WAIT, "the start did not wait for ever");
    let program = &programs(&node).programs[0];
    assert_eq!(program.state, ProgramState::Failed, "{program:?}");
}

#[test]
fn a_name_that_is_no_file_is_failed_and_not_looked_for_on_the_path() {
    let text =
        "[[providers.program]]\nname = \"ghost\"\ncommand = [\"no-such-program-anywhere\"]\n";
    let node = build(Config::from_toml(text, "bohio.cfg").expect("valid").node())
        .expect("the node starts");
    let program = &programs(&node).programs[0];
    assert_eq!(program.name, "ghost");
    assert_eq!(program.state, ProgramState::Failed);
    assert!(
        program.detail.contains("names no file"),
        "{}",
        program.detail
    );
}

#[test]
fn the_entries_are_checked_when_the_configuration_is_read() {
    let invalid = |text: &str| -> String {
        match Config::from_toml(text, "bohio.cfg") {
            Ok(config) => build(config.node()).err().map(|e| e.to_string()),
            Err(error) => Some(error.to_string()),
        }
        .unwrap_or_else(|| panic!("{text} should be refused"))
    };
    assert!(
        invalid("[[providers.program]]\ncommand = []\n").contains("providers.program[1].command")
    );
    assert!(invalid("[[providers.program]]\ncommand = [\"\"]\n").contains("command"));
    assert!(
        invalid(
            "[[providers.program]]\nname = \"a\"\ncommand = [\"x\"]\n[[providers.program]]\nname = \"a\"\ncommand = [\"y\"]\n"
        )
        .contains("providers.program[2].name")
    );
    assert!(invalid("[[providers.program]]\ncommand = [\"x\"]\nbogus = 1\n").contains("bogus"));
    assert!(
        invalid("[[providers.program]]\ncommand = [\"x\"]\nmax_calls = 0\n").contains("max_calls")
    );
    assert!(
        invalid("[[providers.program]]\ncommand = [\"x\"]\nstart_timeout = \"0s\"\n")
            .contains("start_timeout")
    );
    assert!(
        invalid("[[providers.program]]\ncommand = [\"x\"]\nrestart_window = \"0s\"\n")
            .contains("restart_window")
    );
    let ok = Config::from_toml(
        "[[providers.program]]\ncommand = [\"x\"]\nrestart_limit = 0\nrestart_window = \"0s\"\n",
        "bohio.cfg",
    );
    assert!(
        ok.is_ok(),
        "a window nobody counts over may be zero: {ok:?}"
    );
    assert!(matches!(
        Config::from_toml(
            "[[providers.program]]\ncommand = [\"x\"]\nrestart_limit = -1\n",
            "bohio.cfg"
        ),
        Err(ConfigError::Parse { .. })
    ));
}

mod owner {
    use super::*;
    use std::sync::Mutex;

    #[cfg(unix)]
    fn open_to_everyone(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))
            .expect("open to everyone");
    }

    /// Everyone (`S-1-1-0`) may modify the directory and what is made in it.
    #[cfg(windows)]
    fn open_to_everyone(dir: &Path) {
        let status = std::process::Command::new("icacls")
            .arg(dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)M"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("icacls runs");
        assert!(status.success(), "icacls granted Everyone");
    }

    #[test]
    fn a_program_from_a_directory_another_account_can_write_is_refused_by_default() {
        let (dir, file) = program();
        open_to_everyone(dir.path());
        // The program is refused, so nothing offers `program-echo` and a source naming it is refused too.
        assert!(build(config(&file, &[], "", "")).is_err());
        let text = format!("[[providers.program]]\ncommand = [{}]\n", quoted(&file));
        let node = build(Config::from_toml(&text, "bohio.cfg").expect("valid").node())
            .expect("the node starts");
        let program = &programs(&node).programs[0];
        assert_eq!(program.state, ProgramState::Failed, "{program:?}");
        assert!(program.detail.starts_with("refused:"), "{}", program.detail);
    }

    #[test]
    fn with_the_check_off_it_runs_and_the_log_says_so() {
        let (dir, file) = program();
        open_to_everyone(dir.path());
        let log = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(Arc::clone(&log)))
            .with_ansi(false)
            .finish();
        let report = tracing::subscriber::with_default(subscriber, || {
            let node = build(config(&file, &[], "", "check_access = false")).expect("starts");
            programs(&node)
        });
        assert_eq!(
            report.programs[0].state,
            ProgramState::Running,
            "{report:?}"
        );
        assert!(!report.check_access);
        let text = String::from_utf8(log.lock().expect("log").clone()).expect("text");
        assert!(
            text.contains("non-default: [providers] check_access is false"),
            "{text}"
        );
    }

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
}

/// What one call costs in this process and over a child's pipes: the same trivial terminal write both ways.
#[test]
fn the_cost_of_a_call_in_process_and_over_a_child_is_measured() {
    let calls = 2000;
    let (_dir, file) = program();
    let node = build(config(&file, &[], "", "")).expect("the node starts");
    let child = open(&node);
    read_until(&child, b"echo console ready");

    let sources = bohio::access::Sources::unconfigured();
    let link = bohio_api::testing::terminal_link("programs-cost");
    sources.add_terminal(
        "near",
        Arc::new(
            bohio_api::testing::FakeTerminal::try_from(bohio_api::testing::FakeTerminalConfig {
                link: "programs-cost".to_owned(),
                console: true,
            })
            .expect("a fake terminal"),
        ),
    );
    drop(link);
    let near = sources
        .terminal(Some("near"))
        .expect("the in-process source")
        .get()
        .open(TerminalOpen::default())
        .expect("a session");

    let median = |session: &Object<dyn Terminal>| {
        let mut times = Vec::with_capacity(calls);
        for _ in 0..calls {
            let started = Instant::now();
            session.write(b"x").expect("a write");
            times.push(started.elapsed());
        }
        times.sort();
        times[calls / 2]
    };
    let (in_process, over_child) = (median(&near), median(&child));
    println!(
        "write call, median of {calls}: in process {in_process:?}, over a child's pipes {over_child:?} on {}",
        std::env::consts::OS
    );
    assert!(over_child < Duration::from_millis(500));
}

/// The serial provider served by a program (`providers/terminal/serial/examples/serial_program.rs`) gives a console over a
/// null-modem cable made of two pseudo-terminals. Build the example first: `cargo build -p bohio-provider-serial --example
/// serial_program`.
#[cfg(unix)]
mod serial {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};

    /// `socat` joining two pseudo-terminals; `None` when it is not installed.
    struct Cable {
        child: Child,
        near: String,
        far: String,
    }

    impl Cable {
        fn make() -> Option<Self> {
            let mut child = Command::new("socat")
                .args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
                .stderr(Stdio::piped())
                .spawn()
                .ok()?;
            let stderr = child.stderr.take()?;
            let mut paths = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(at) = line.find("PTY is ") {
                    paths.push(line[at + 7..].trim().to_owned());
                }
                if paths.len() == 2 {
                    break;
                }
            }
            let far = paths.pop()?;
            let near = paths.pop()?;
            Some(Self { child, near, far })
        }
    }

    impl Drop for Cable {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn the_serial_provider_served_by_a_program_gives_a_console_over_a_cable() {
        let Some(cable) = Cable::make() else {
            println!("SKIPPED: socat is not installed");
            return;
        };
        let exe = std::env::current_exe().expect("a test binary knows its own path");
        let deps = exe.parent().expect("a directory");
        let Some(built) = [deps.join("..").join("examples"), deps.join("examples")]
            .into_iter()
            .map(|dir| dir.join("serial_program"))
            .find(|file| file.is_file())
        else {
            panic!(
                "serial_program is not built: run `cargo build -p bohio-provider-serial --example serial_program`"
            );
        };
        let dir = private_directory();
        let file = dir.path().join("serial_program");
        std::fs::copy(&built, &file).expect("the copy");
        let text = format!(
            "[[providers.program]]\ncommand = [{}, '--path', '{}', '--reopen_after', '200ms']\n\
             [source.console]\nprovider = \"serial-program\"\n[access]\nterminal = [\"console\"]\ndesktop = []\n",
            quoted(&file),
            cable.near
        );
        let node = build(Config::from_toml(&text, "bohio.cfg").expect("valid").node())
            .expect("the node starts");
        let report = programs(&node);
        assert_eq!(
            report.programs[0].state,
            ProgramState::Running,
            "{report:?}"
        );
        assert!(report.programs[0].detail.contains("serial-program"));

        let mut far = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cable.far)
            .expect("the far end opens");
        let session = node
            .sources()
            .terminal(Some("console"))
            .expect("the console source")
            .get()
            .open(TerminalOpen::default())
            .expect("the console opens");
        far.write_all(b"login: ").expect("the far end writes");
        read_until(&session, b"login: ");
        session.write(b"root\r").expect("typing");
        let mut got = Vec::new();
        let mut byte = [0_u8; 1];
        let until = Instant::now() + WAIT;
        while !got.ends_with(b"root\r") && Instant::now() < until {
            if far.read(&mut byte).unwrap_or(0) == 1 {
                got.push(byte[0]);
            }
        }
        assert!(got.ends_with(b"root\r"), "the far end read {got:?}");
    }
}

/// Puts the process back in its working directory when the test ends.
#[cfg(unix)]
struct Restore(PathBuf);

#[cfg(unix)]
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// The file the entry's `command[0]` names relative to the node's working directory is the one checked, digested and
/// run, not another one of that name under the entry's `cwd` or on `PATH`.
#[cfg(unix)]
#[test]
fn a_relative_command_runs_the_file_that_was_checked_and_not_one_under_the_entrys_cwd() {
    use std::os::unix::fs::PermissionsExt;
    let here = private_directory();
    std::fs::create_dir(here.path().join("bin")).expect("bin");
    std::fs::copy(built(), here.path().join("bin").join("program_echo")).expect("the copy");
    let other = private_directory();
    std::fs::create_dir(other.path().join("bin")).expect("the decoy directory");
    let script = other.path().join("bin").join("program_echo");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("the decoy");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("runnable");

    // The one test here that moves the process: every other test names its files by absolute path.
    let _restore = Restore(std::env::current_dir().expect("a working directory"));
    std::env::set_current_dir(here.path()).expect("moved");
    let relative = Path::new("bin").join("program_echo");
    let table = format!("cwd = {}\nstart_timeout = \"5s\"", quoted(other.path()));
    let node = build(config(&relative, &[], &table, "")).expect("the node starts");
    let program = &programs(&node).programs[0];
    assert_eq!(program.state, ProgramState::Running, "{program:?}");
    assert!(
        program.detail.contains("program-echo"),
        "{}",
        program.detail
    );
}
