//! A GUI helper that runs on another machine: the node starts `bohio gui-helper --stdio` through an ssh source (an in-process
//! SSH server whose `sh` runs the command), serves its desktop as a session, and a peer sees and clicks it. The helper is the
//! painted desktop (`examples/gui_helper_painted.rs`): no screen is touched.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::Sources;
use bohio::config::{AccessConfig, GuiConfig, NodeConfig, SourceTables};
use bohio::gui::helper_exec::{ExecHelper, HELPER_WORD};
use bohio::gui::helper_upload::{self, Upload};
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::{Grant, Permission};
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::gui::{Button, ButtonAction, GuiRequest, InputEvent, Want};
use cabuya::auth::{Identity, Method};
use coqui_transport::{Channel as _, memory};

#[path = "../../../providers/ssh/tests/support/mod.rs"]
mod support;
use support::{FakeSsh, shell_available};

const WAIT: Duration = Duration::from_secs(60);

fn helper_program() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let name = format!("gui_helper_painted{}", std::env::consts::EXE_SUFFIX);
    let path = exe.parent()?.parent()?.join("examples").join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// The sources of a node whose exec (and, with `files`, files) source is the in-process SSH server.
fn ssh_sources(server: &FakeSsh, files: bool) -> Sources {
    let text = format!(
        "[source.remote]
provider = \"ssh\"
host = \"127.0.0.1\"
port = {}
user = \"{}\"
identity = {:?}
host_key = \"{}\"
agent = false
",
        server.port,
        support::USER,
        server.identity().to_string_lossy(),
        server.pin()
    );
    sources_from(&text, files)
}

/// The sources of a node whose `remote` source is the `[source.remote]` table in `text`.
fn sources_from(text: &str, files: bool) -> Sources {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    let tables = toml::from_str::<Whole>(text).expect("source tables").source;
    let sources = Sources::unconfigured();
    sources
        .reconfigure(
            &tables,
            &AccessConfig {
                exec: Some(vec!["remote".to_owned()]),
                fs: files.then(|| vec!["remote".to_owned()]),
                desktop: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");
    sources
}

/// Runs the scenario with `command` as the helper's command line; `width` is the first display's width when it is known.
fn scenario(command: Vec<String>, width: Option<u32>) {
    let server = FakeSsh::start();
    let helper = ExecHelper {
        source: "remote".to_owned(),
        command,
        upload: None,
    };
    run(ssh_sources(&server, false), helper, width);
}

/// Serves `helper` from a node that reaches the machine through `sources`, and a peer sees and clicks its desktop.
fn run(sources: Sources, helper: ExecHelper, width: Option<u32>) {
    let name = helper.source.clone();

    let make = |sources: Option<Sources>, gui: GuiConfig| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node = Node::new(
            NodeConfig {
                gui,
                ..NodeConfig::default()
            },
            identity,
            keys.clone(),
            grants.clone(),
        )
        .expect("the node is built")
        .with_path(std::env::var_os("PATH").unwrap_or_default());
        if let Some(sources) = sources {
            node = node.with_sources(sources);
        }
        (Arc::new(node), id, keys, grants)
    };
    let gui = GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    };
    let (host, host_id, host_keys, host_grants) = make(Some(sources), gui);
    let (client, client_id, client_keys, _) = make(None, GuiConfig::default());
    host_keys.accept(client_id);
    host_grants.set(
        client_id,
        Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput]),
    );
    client_keys.accept(host_id);
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<JoinHandle<()>> = vec![
        thread::spawn({
            let (node, channel) = (client.clone(), dial_end.clone());
            move || {
                let _ = node.connect_channel(&*channel, &host_id, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (node, channel) = (host.clone(), answer_end.clone());
            move || {
                node.serve_channel(&*channel, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (node, stop) = (host.clone(), stop.clone());
            move || {
                node.serve_exec_helpers(
                    std::slice::from_ref(&helper),
                    Duration::from_millis(200),
                    &stop,
                );
            }
        }),
    ];
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.peer(&host_id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };

    // The helper's session is listed once it has connected.
    let gui = loop {
        match Gui::open(&link, &GuiRequest::new([Want::View, Want::Input])) {
            Ok(gui) => break gui,
            Err(error) => {
                assert!(
                    Instant::now() < until,
                    "the helper never came up: {error:?}"
                );
                thread::sleep(Duration::from_millis(200));
            }
        }
    };
    assert!(
        gui.session().starts_with(&format!("exec:{name}")),
        "the session is the helper's: {}",
        gui.session()
    );
    let image = gui
        .snapshot(&SnapshotOptions::default(), Some(WAIT))
        .expect("a snapshot through the helper");
    match width {
        Some(width) => assert_eq!(image.header.width, width),
        None => assert!(image.header.width > 0),
    }
    let click = InputEvent::Button {
        button: Button::Left,
        action: ButtonAction::Click,
        count: None,
        display: None,
        x: Some(100),
        y: Some(50),
    };
    // Only the painted desktop is clicked: a click on a person's real screen is not a test's to make.
    if width.is_some() {
        assert_eq!(
            gui.input(&[click], Some(WAIT))
                .expect("input through the helper"),
            1
        );
    }
    gui.close();

    stop.store(true, Ordering::SeqCst);
    dial_end.close();
    answer_end.close();
    for handle in threads {
        let _ = handle.join();
    }
}

#[test]
fn a_peer_sees_and_clicks_the_desktop_of_a_helper_the_node_runs_through_an_ssh_source() {
    if !shell_available() {
        eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
        return;
    }
    let Some(program) = helper_program() else {
        eprintln!(
            "SKIPPED: build the helper first (cargo build -p bohio --example gui_helper_painted)"
        );
        return;
    };
    scenario(vec![program], Some(320));
}

/// The real helper, `bohio gui-helper --stdio`, on the machine's own X display.
#[cfg(target_os = "linux")]
#[test]
fn the_real_helper_serves_the_machines_x_display_through_an_ssh_source() {
    if !shell_available() {
        eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
        return;
    }
    let Some(display) = std::fs::read_dir("/tmp/.X11-unix")
        .ok()
        .and_then(|mut found| found.find_map(Result::ok))
        .and_then(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix('X')
                .map(|number| format!(":{number}"))
        })
    else {
        eprintln!("SKIPPED: this machine has no X server");
        return;
    };
    scenario(
        vec![
            "env".to_owned(),
            "-u".to_owned(),
            "WAYLAND_DISPLAY".to_owned(),
            "-u".to_owned(),
            "XDG_RUNTIME_DIR".to_owned(),
            format!("DISPLAY={display}"),
            env!("CARGO_BIN_EXE_bohio").to_owned(),
            "gui-helper".to_owned(),
            "--stdio".to_owned(),
            "--session".to_owned(),
            "remote".to_owned(),
        ],
        None,
    );
}

fn native_sources() -> Sources {
    Sources::from_config(
        &SourceTables::new(),
        &AccessConfig {
            desktop: Some(Vec::new()),
            ..AccessConfig::default()
        },
    )
    .expect("the node's own sources")
}

fn upload_into(dir: &std::path::Path, binary: Option<std::path::PathBuf>) -> Upload {
    Upload {
        files: None,
        dir: dir.to_string_lossy().replace('\\', "/"),
        check_platform: false,
        binary,
        overwrite: false,
        shared_dir: false,
    }
}

fn helper_for(source: &str, upload: Upload) -> ExecHelper {
    ExecHelper {
        source: source.to_owned(),
        command: vec![HELPER_WORD.to_owned()],
        upload: Some(upload),
    }
}

/// The helper goes through the node's own files source into a directory and runs from there.
#[test]
fn a_helper_put_through_a_files_source_is_run_and_serves_its_desktop() {
    let Some(program) = helper_program() else {
        eprintln!(
            "SKIPPED: build the helper first (cargo build -p bohio --example gui_helper_painted)"
        );
        return;
    };
    let dir = tempfile::tempdir().expect("a directory");
    let upload = upload_into(dir.path(), Some(program.into()));
    run(native_sources(), helper_for("native", upload), Some(320));
    let put: Vec<_> = std::fs::read_dir(dir.path())
        .expect("the directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(put.len(), 1, "only the helper is left there: {put:?}");
    assert!(put[0].starts_with("bohio-helper-"), "{put:?}");
}

#[test]
fn an_upload_reuses_its_own_file_and_refuses_another_by_default() {
    let Some(program) = helper_program() else {
        eprintln!("SKIPPED: build the helper first");
        return;
    };
    let sources = native_sources();
    let stop = AtomicBool::new(false);
    let dir = tempfile::tempdir().expect("a directory");
    let upload = upload_into(dir.path(), Some(program.clone().into()));
    let path = helper_upload::upload(&sources, "native", &upload, &stop).expect("the first put");
    let first = std::fs::metadata(&path).expect("the file").modified().ok();
    assert_eq!(
        helper_upload::upload(&sources, "native", &upload, &stop).expect("the second"),
        path
    );
    assert_eq!(
        std::fs::metadata(&path).expect("the file").modified().ok(),
        first,
        "the same file is not written again"
    );

    // Another file of that name is not the helper to replace.
    std::fs::write(&path, b"not the helper").expect("a planted file");
    let refused = helper_upload::upload(&sources, "native", &upload, &stop).unwrap_err();
    assert!(refused.contains("helper_upload_overwrite"), "{refused}");
    assert_eq!(std::fs::read(&path).unwrap(), b"not the helper");
    let weaker = Upload {
        overwrite: true,
        ..upload.clone()
    };
    assert_eq!(
        helper_upload::upload(&sources, "native", &weaker, &stop).expect("replaced"),
        path
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(&program).unwrap()
    );

    // A file that is no executable is not put anywhere.
    let other = tempfile::tempdir().expect("a directory");
    let text = other.path().join("script");
    std::fs::write(&text, b"#!/bin/sh\n").unwrap();
    let refused = helper_upload::upload(
        &sources,
        "native",
        &upload_into(other.path(), Some(text)),
        &stop,
    )
    .unwrap_err();
    assert!(refused.contains("not an executable"), "{refused}");
    assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn a_directory_other_accounts_can_write_is_refused_unless_the_operator_accepts_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let Some(program) = helper_program() else {
        eprintln!("SKIPPED: build the helper first");
        return;
    };
    let sources = native_sources();
    let stop = AtomicBool::new(false);
    let dir = tempfile::tempdir().expect("a directory");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let upload = upload_into(dir.path(), Some(program.into()));
    let refused = helper_upload::upload(&sources, "native", &upload, &stop).unwrap_err();
    assert!(refused.contains("writable by other accounts"), "{refused}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    let path = helper_upload::upload(
        &sources,
        "native",
        &Upload {
            shared_dir: true,
            ..upload
        },
        &stop,
    )
    .expect("accepted");
    let mode = std::fs::metadata(path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "the file is the account alone");
}

/// The machine answers `uname -sm`: a file built for another platform is refused and nothing is put.
#[cfg(unix)]
#[test]
fn a_helper_built_for_another_platform_is_refused_and_not_put() {
    let Some(program) = helper_program() else {
        eprintln!("SKIPPED: build the helper first");
        return;
    };
    let sources = native_sources();
    let stop = AtomicBool::new(false);
    let checked = |dir: &std::path::Path, binary: std::path::PathBuf| Upload {
        check_platform: true,
        ..upload_into(dir, Some(binary))
    };
    let dir = tempfile::tempdir().expect("a directory");
    helper_upload::upload(
        &sources,
        "native",
        &checked(dir.path(), program.into()),
        &stop,
    )
    .expect("this machine platform is accepted");

    let other = tempfile::tempdir().expect("a directory");
    let mut windows = vec![0_u8; 256];
    windows[..2].copy_from_slice(b"MZ");
    windows[0x3C..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
    windows[0x80..0x84].copy_from_slice(b"PE\0\0");
    windows[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
    let file = other.path().join("windows.exe");
    std::fs::write(&file, windows).unwrap();
    let target = tempfile::tempdir().expect("a directory");
    let refused = helper_upload::upload(
        &sources,
        "native",
        &checked(target.path(), file.clone()),
        &stop,
    )
    .unwrap_err();
    assert!(
        refused.contains("pe executable") && refused.contains("uname"),
        "{refused}"
    );
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);

    // The operator who knows better turns the check off.
    let weaker = Upload {
        check_platform: false,
        ..checked(target.path(), file)
    };
    helper_upload::upload(&sources, "native", &weaker, &stop).expect("unchecked");
}

#[cfg(unix)]
/// The helper goes through an SSH server files and runs through its exec: the machine has nothing of Bohio on it.
#[test]
fn a_helper_is_put_through_ssh_files_and_run_through_its_exec() {
    if !shell_available() {
        eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
        return;
    }
    let Some(program) = helper_program() else {
        eprintln!("SKIPPED: build the helper first");
        return;
    };
    let server = FakeSsh::start();
    let target = tempfile::tempdir().expect("a directory");
    let upload = Upload {
        // `uname` is this machine own: the platform check is the real one.
        check_platform: true,
        ..upload_into(target.path(), Some(program.into()))
    };
    let stop = AtomicBool::new(false);
    let put = helper_upload::upload(&ssh_sources(&server, true), "remote", &upload, &stop);
    assert!(put.is_ok(), "{put:?}");
    std::fs::remove_file(put.unwrap()).unwrap();
    run(
        ssh_sources(&server, true),
        helper_for("remote", upload),
        Some(320),
    );
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 1);
}

/// A private `sshd` of this account on loopback, with a throwaway host key and client key.
#[cfg(target_os = "linux")]
struct RealSshd {
    dir: std::path::PathBuf,
    port: u16,
    user: String,
    pin: String,
    keys: tempfile::TempDir,
}

#[cfg(target_os = "linux")]
impl RealSshd {
    fn start() -> Result<Self, String> {
        use std::process::Command;
        let bash = |script: &str| -> Result<String, String> {
            let out = Command::new("bash")
                .args(["-c", script])
                .output()
                .map_err(|error| error.to_string())?;
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
            } else {
                Err(String::from_utf8_lossy(&out.stderr).into_owned())
            }
        };
        let found = bash(
            "for f in /usr/sbin/sshd /usr/local/sbin/sshd; do [ -x $f ] && echo $f && break; done; \
             for f in /usr/libexec/openssh/sftp-server /usr/lib/openssh/sftp-server; do [ -x $f ] && echo $f && break; done; \
             command -v ssh-keygen",
        )
        .map_err(|why| format!("no OpenSSH server to start ({why})"))?;
        let [sshd, sftp_server, _] = found.lines().collect::<Vec<_>>()[..] else {
            return Err(format!("no OpenSSH server to start (found only {found:?})"));
        };
        let keys = tempfile::tempdir().expect("a directory");
        let client = russh_key_pair(keys.path());
        let user = bash("id -un")?;
        let dir = std::env::temp_dir().join(format!("bohio-upload-sshd-{}", std::process::id()));
        let dir_text = dir.to_string_lossy().into_owned();
        let port = {
            let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
            probe.local_addr().map_err(|e| e.to_string())?.port()
        };
        bash(&format!(
            "set -e\nmkdir -p {dir_text}/upload && chmod 700 {dir_text} {dir_text}/upload\n\
             ssh-keygen -q -t ed25519 -N '' -f {dir_text}/host_key\n\
             printf '%s\\n' '{client}' > {dir_text}/authorized_keys\nchmod 600 {dir_text}/authorized_keys\n\
             printf 'Port {port}\\nListenAddress 127.0.0.1\\nHostKey {dir_text}/host_key\\nAuthorizedKeysFile {dir_text}/authorized_keys\\nPidFile {dir_text}/pid\\nUsePAM no\\nPasswordAuthentication no\\nStrictModes no\\nSubsystem sftp {sftp_server}\\nLogLevel ERROR\\n' > {dir_text}/sshd_config\n\
             setsid nohup {sshd} -D -f {dir_text}/sshd_config -E {dir_text}/log > /dev/null 2>&1 < /dev/null &\n\
             sleep 1\ntest -s {dir_text}/pid"
        ))?;
        let pin = bash(&format!(
            "awk '{{print $2}}' {dir_text}/host_key.pub | base64 -d | sha256sum | cut -d' ' -f1"
        ))?;
        Ok(Self {
            dir,
            port,
            user,
            pin: format!("sha256:{pin}"),
            keys,
        })
    }

    fn sources(&self) -> Sources {
        let text = format!(
            "[source.remote]\nprovider = \"ssh\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\nidentity = {:?}\nhost_key = \"{}\"\nagent = false\n",
            self.port,
            self.user,
            self.keys.path().join("client_key").to_string_lossy(),
            self.pin
        );
        sources_from(&text, true)
    }
}

#[cfg(target_os = "linux")]
impl Drop for RealSshd {
    fn drop(&mut self) {
        let dir = self.dir.to_string_lossy().into_owned();
        let _ = std::process::Command::new("bash")
            .args([
                "-c",
                &format!("kill $(cat {dir}/pid) 2>/dev/null; sleep 0.2; rm -r -f {dir}"),
            ])
            .status();
    }
}

/// Writes a client key to `dir/client_key` and answers its public line.
#[cfg(target_os = "linux")]
fn russh_key_pair(dir: &std::path::Path) -> String {
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(dir.join("client_key"))
        .status()
        .expect("ssh-keygen");
    assert!(out.success());
    std::fs::read_to_string(dir.join("client_key.pub"))
        .expect("the public key")
        .trim()
        .to_owned()
}

/// The done-when of the upload: a node puts its own helper on a machine through its SFTP, runs it through its exec and
/// serves the machine's X display, with a real `sshd` and no Bohio installed on the machine.
#[cfg(target_os = "linux")]
#[test]
fn a_node_puts_its_own_helper_on_a_real_sshd_and_serves_the_x_display_through_it() {
    let Some(display) = std::fs::read_dir("/tmp/.X11-unix")
        .ok()
        .and_then(|mut found| found.find_map(Result::ok))
        .and_then(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix('X')
                .map(|number| format!(":{number}"))
        })
    else {
        eprintln!("SKIPPED: this machine has no X server");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
    let server = match RealSshd::start() {
        Ok(server) => server,
        Err(why) => {
            assert!(
                std::env::var_os("BOHIO_TEST_REQUIRE_SSHD").is_none(),
                "{why}"
            );
            eprintln!("SKIPPED: {why}");
            return;
        }
    };
    let upload = Upload {
        files: None,
        dir: format!("{}/upload", server.dir.to_string_lossy()),
        check_platform: true,
        binary: Some(env!("CARGO_BIN_EXE_bohio").into()),
        overwrite: false,
        shared_dir: false,
    };
    let helper = ExecHelper {
        source: "remote".to_owned(),
        command: [
            "env",
            "-u",
            "WAYLAND_DISPLAY",
            &format!("DISPLAY={display}"),
            HELPER_WORD,
            "gui-helper",
            "--stdio",
            "--session",
            "remote",
        ]
        .map(str::to_owned)
        .to_vec(),
        upload: Some(upload),
    };
    let stop = AtomicBool::new(false);
    let put = helper_upload::upload(
        &server.sources(),
        "remote",
        helper.upload.as_ref().unwrap(),
        &stop,
    );
    assert!(put.is_ok(), "{put:?}");
    std::fs::remove_file(put.unwrap()).unwrap();
    run(server.sources(), helper, None);
    let put = std::fs::read_dir(server.dir.join("upload"))
        .expect("the upload directory")
        .count();
    assert_eq!(put, 1, "the helper is on the machine");
}
