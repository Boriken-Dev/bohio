//! What another local account can do to a node: a layer it owns. These need a
//! second account, reached with `sudo -n -u ACCOUNT`, so they run only when `BOHIO_TEST_OTHER_ACCOUNT` names one
//! (`nobody`, say) and `sudo -n` works; otherwise they print SKIPPED and pass.
#![cfg(unix)]

mod support;

use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use support::procs::{DEADLINE, Proc};

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
