//! What another local account can do to a node: a layer it owns, and a control endpoint it serves. These need a
//! second account, reached with `sudo -n -u ACCOUNT`, so they run only when `BOHIO_TEST_OTHER_ACCOUNT` names one
//! (`nobody`, say) and `sudo -n` works; otherwise they print SKIPPED and pass.
#![forbid(unsafe_code)]
#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use support::procs::{DEADLINE, Node, Proc, run};

/// The other account, when the tests may use it.
fn other_account() -> Option<String> {
    let account = std::env::var("BOHIO_TEST_OTHER_ACCOUNT")
        .ok()
        .filter(|name| !name.is_empty());
    // sudo is asked only when an account is named, so a run without one starts nothing.
    let sudo = || {
        Command::new("sudo")
            .args(["-n", "true"])
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    match account {
        Some(account) if sudo() => Some(account),
        _ => {
            println!("SKIPPED: set BOHIO_TEST_OTHER_ACCOUNT to an account `sudo -n -u` may run as");
            None
        }
    }
}

fn sudo(args: &[&str]) {
    let done = Command::new("sudo")
        .arg("-n")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .expect("sudo runs");
    assert!(done.success(), "sudo {args:?} failed");
}

fn mode(path: &Path, bits: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).expect("the mode is set");
}

/// Whoever owns a layer wrote what it says, so a layer another account owns is refused even when it says
/// `check_access = false`: the account that made it could have written that line too.
#[test]
fn a_layer_another_account_owns_cannot_turn_its_own_check_off() {
    let Some(account) = other_account() else {
        return;
    };
    let dir = tempfile::tempdir().expect("a directory");
    let config = dir.path().join("planted.cfg");
    std::fs::write(
        &config,
        format!(
            "[service]\ndata_dir = '{}'\ncheck_access = false\n",
            dir.path().join("data").display()
        ),
    )
    .expect("the layer is written");
    let path = config.to_string_lossy().into_owned();
    sudo(&["chown", &account, &path]);
    let serving = Proc::start(
        &["serve", "--config", &path, "--listen", "tcp://127.0.0.1:0"],
        "warn",
    );
    let until = Instant::now() + DEADLINE;
    let said = loop {
        assert!(
            !serving.stdout().contains("listening "),
            "a layer another account owns was served"
        );
        if serving.stderr().contains("bohio: ") {
            break serving.stderr();
        }
        assert!(Instant::now() < until, "serve neither served nor gave up");
        thread::sleep(Duration::from_millis(50));
    };
    assert!(said.contains("owned by another account"), "{said}");
}

/// The control endpoint is not sealed, so a client refuses one another account serves before it sends anything:
/// that account would read every request (an exec's arguments, a file written) and could answer `bohio pair` with
/// a key of its own as the one that paired.
#[test]
fn a_control_endpoint_another_account_serves_is_refused_before_anything_is_sent() {
    let Some(account) = other_account() else {
        return;
    };
    if !Command::new("socat")
        .arg("-V")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        println!("SKIPPED: socat is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a directory");
    mode(dir.path(), 0o755);
    let squat = dir.path().join("squat");
    std::fs::create_dir(&squat).expect("a directory");
    mode(&squat, 0o755);
    let squat_text = squat.to_string_lossy().into_owned();
    sudo(&["chown", &account, &squat_text]);
    let socket = squat.join("control.sock");
    let heard = squat.join("heard");
    let mut squatter = Command::new("sudo")
        .args(["-n", "-u", &account, "timeout", "60", "socat", "-u"])
        .arg(format!("UNIX-LISTEN:{},mode=777,fork", socket.display()))
        .arg(format!("OPEN:{},creat,append", heard.display()))
        .stdin(Stdio::null())
        .spawn()
        .expect("socat starts");
    let until = Instant::now() + DEADLINE;
    while !socket.exists() {
        assert!(Instant::now() < until, "the squatter did not bind");
        thread::sleep(Duration::from_millis(50));
    }

    let client = Node::new(dir.path(), "client");
    client.write(&format!(
        "[control]\nendpoint = \"pipe://{}\"\n",
        socket.display()
    ));
    let done = run(&["links", "--config", &client.config(), "--wait", "200ms"]);
    let _ = Command::new("kill")
        .args(["-TERM", &squatter.id().to_string()])
        .status();
    let _ = squatter.wait();
    let me = Command::new("id").arg("-u").output().expect("id runs");
    let me = String::from_utf8_lossy(&me.stdout).trim().to_owned();
    sudo(&["chown", "-R", &me, &squat_text]);
    let bytes = std::fs::metadata(&heard).map_or(0, |meta| meta.len());
    assert_eq!(
        bytes, 0,
        "the client sent {bytes} bytes to another account's endpoint"
    );
    assert_ne!(done.code, Some(0), "{}", done.stdout);
    assert!(done.stderr.contains("another account"), "{}", done.stderr);
}

fn socat_installed() -> bool {
    Command::new("socat")
        .arg("-V")
        .stdout(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn my_uid() -> String {
    let me = Command::new("id").arg("-u").output().expect("id runs");
    String::from_utf8_lossy(&me.stdout).trim().to_owned()
}

/// A helper endpoint another account serves is refused by the helper before it sends anything: the helper was told to trust
/// this account's node, and the account that answers is another.
#[test]
fn a_helper_refuses_a_socket_another_account_serves_and_sends_nothing() {
    let Some(account) = other_account() else {
        return;
    };
    if !socat_installed() {
        println!("SKIPPED: socat is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a directory");
    mode(dir.path(), 0o755);
    let squat = dir.path().join("squat");
    std::fs::create_dir(&squat).expect("a directory");
    mode(&squat, 0o755);
    let squat_text = squat.to_string_lossy().into_owned();
    sudo(&["chown", &account, &squat_text]);
    let socket = squat.join("gui.sock");
    let heard = squat.join("heard");
    let mut squatter = Command::new("sudo")
        .args(["-n", "-u", &account, "timeout", "60", "socat", "-u"])
        .arg(format!("UNIX-LISTEN:{},mode=777,fork", socket.display()))
        .arg(format!("OPEN:{},creat,append", heard.display()))
        .stdin(Stdio::null())
        .spawn()
        .expect("socat starts");
    let until = Instant::now() + DEADLINE;
    while !socket.exists() {
        assert!(Instant::now() < until, "the squatter did not bind");
        thread::sleep(Duration::from_millis(50));
    }
    let config = dir.path().join("helper.cfg");
    std::fs::write(&config, "# nothing\n").expect("a config");
    let mine = my_uid();
    let done = run(&[
        "gui-helper",
        "--config",
        &config.to_string_lossy(),
        "--endpoint",
        &format!("pipe://{}", socket.display()),
        "--node-account",
        &mine,
    ]);
    let _ = Command::new("kill")
        .args(["-TERM", &squatter.id().to_string()])
        .status();
    let _ = squatter.wait();
    sudo(&["chown", "-R", &mine, &squat_text]);
    let bytes = std::fs::metadata(&heard).map_or(0, |meta| meta.len());
    assert_eq!(
        bytes, 0,
        "the helper sent {bytes} bytes to another account's socket"
    );
    assert_eq!(done.code, Some(1), "{}", done.stderr);
    assert!(done.stderr.contains(&mine), "{}", done.stderr);
}

/// How long a connection of `account` to `socket` stays open, up to `within`: `true` when the node closed it.
fn closed_within(account: &str, socket: &Path, within: Duration) -> bool {
    let mut client = Command::new("sudo")
        .args(["-n", "-u", account, "timeout", "10", "socat", "-"])
        .arg(format!("UNIX-CONNECT:{}", socket.display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("socat starts");
    let until = Instant::now() + within;
    let closed = loop {
        if client.try_wait().expect("the client is asked").is_some() {
            break true;
        }
        if Instant::now() >= until {
            break false;
        }
        thread::sleep(Duration::from_millis(50));
    };
    // The client of another account ends by its own `timeout`; this side never signals a process it cannot own.
    drop(client.stdin.take());
    let _ = client.wait();
    closed
}

/// The node serves a helper only for the accounts it lists: a real second account is closed unread when only this account is
/// listed, and held when its own uid is.
#[test]
fn a_helper_connection_from_another_account_is_closed_unread_unless_it_is_listed() {
    let Some(account) = other_account() else {
        return;
    };
    if !socat_installed() {
        println!("SKIPPED: socat is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("a directory");
    mode(dir.path(), 0o755);
    let socket = dir.path().join("gui.sock");
    let node = Node::new(dir.path(), "node");
    let mine = my_uid();
    for (listed, served) in [(mine, false), (account.clone(), true)] {
        node.write(&format!(
            "[gui]\nenabled = true\nhelper_accounts = [\"{listed}\"]\nhelper_endpoint = \"pipe://{}\"\n",
            socket.display()
        ));
        let (mut serving, _) = node.serve(Some("tcp://127.0.0.1:0"), "warn");
        let until = Instant::now() + DEADLINE;
        while !socket.exists() {
            assert!(
                Instant::now() < until,
                "the node did not bind its helper endpoint"
            );
            thread::sleep(Duration::from_millis(50));
        }
        let closed = closed_within(&account, &socket, Duration::from_millis(2500));
        serving.kill();
        assert_eq!(
            closed,
            !served,
            "listed {listed}: the other account's connection was {}",
            if closed { "closed unread" } else { "held" }
        );
        let _ = std::fs::remove_file(&socket);
    }
}
