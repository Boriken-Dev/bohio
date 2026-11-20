//! What another local account can do to a node: a layer it owns, and a control endpoint it serves. These need a
//! second account, reached with `sudo -n -u ACCOUNT`, so they run only when `BOHIO_TEST_OTHER_ACCOUNT` names one
//! (`nobody`, say) and `sudo -n` works; otherwise they print SKIPPED and pass.
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
