//! A node that dials out: two `bohio serve` children, the controller listening and the host reaching out to
//! it from its `[peer."<id>"]` table's `endpoint`, with the trust set up by `bohio pair`, `join` and
//! `peer approve` or by the controller approving the key that waits.

mod support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use support::procs::{DEADLINE, Node, Proc, run, run_ok};

/// How fast the host retries in these tests.
const DIAL: &str = "retry_min = \"200ms\"\nretry_max = \"1s\"";

/// A quick heartbeat for both sides of a link.
const BEAT: &str = "heartbeat = \"500ms\"\nheartbeat_misses = 3";

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < until, "{what} within the deadline");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A controller listening on a port of its own, and a host whose table for it dials there.
struct Pair {
    _dir: tempfile::TempDir,
    controller: Node,
    host: Node,
    controller_proc: Proc,
    address: SocketAddr,
    endpoint: String,
}

impl Pair {
    /// The controller accepts the host's key when `known`. The host's configuration is not written yet.
    fn new(known: bool) -> Self {
        let dir = tempfile::tempdir().expect("a directory");
        let controller = Node::new(dir.path(), "controller");
        let host = Node::new(dir.path(), "host");
        let extra = if known {
            Node::peer_table(&host.id, None, "", "")
        } else {
            String::new()
        };
        controller.write_with(BEAT, &extra);
        let (controller_proc, listening) = controller.serve(Some("tcp://127.0.0.1:0"), "info");
        let (address, endpoint) = listening.expect("it listens");
        Self {
            _dir: dir,
            controller,
            host,
            controller_proc,
            address,
            endpoint,
        }
    }

    /// The host's configuration dials the controller at `endpoint`, as `id`.
    fn dial_config(&self, id: &str, endpoint: &str) {
        self.host.write_with(
            BEAT,
            &Node::peer_table(id, Some(endpoint), "allow = [\"info\"]", DIAL),
        );
    }
}

#[test]
fn a_host_dials_a_controller_that_does_not_know_it_and_links_once_it_is_approved() {
    let mut pair = Pair::new(false);
    pair.dial_config(&pair.controller.id.clone(), &pair.endpoint.clone());
    let (mut host, _) = pair.host.serve(None, "info");
    let said = host.expect_line("dialing ");
    assert!(
        said.ends_with(&format!("peer {}", pair.controller.id)),
        "{said}"
    );

    // The controller hears the host's proven key and waits for the operator.
    wait_for("the controller lists the host as waiting", || {
        pair.controller
            .peers()
            .contains(&format!("waiting {}", pair.host.id))
    });
    assert_eq!(pair.controller_proc.stderr_count("link up"), 0);

    run_ok(&[
        "peer",
        "approve",
        &pair.host.id,
        "--config",
        &pair.controller.config(),
    ]);
    pair.controller_proc.wait_stderr("link up", 1, DEADLINE);
    host.wait_stderr("link up", 1, DEADLINE);
    let listing = pair.controller.peers();
    assert!(
        listing.contains(&format!("accepted {}", pair.host.id)),
        "{listing}"
    );
    assert!(!listing.contains("waiting"), "{listing}");
    let hosts = pair.host.peers();
    assert!(
        hosts.contains(&format!("accepted {}", pair.controller.id))
            && hosts.contains(&format!("dial: {}", pair.endpoint)),
        "{hosts}"
    );
    pair.controller_proc.kill();
}

#[test]
fn a_host_that_joined_by_pin_dials_and_links_with_nothing_waiting() {
    let pair = Pair::new(false);
    // `bohio pair` arms the running controller: the host joins on the controller's own listener.
    let mut door = Proc::start(
        &[
            "pair",
            "--config",
            &pair.controller.config(),
            "--timeout",
            "60s",
        ],
        "warn",
    );
    let door_endpoint = pair.endpoint.clone();
    let pin = door.expect_line("pin ");
    pair.host.write_with(BEAT, "");
    run_ok(&[
        "join",
        &door_endpoint,
        "--pin",
        &pin,
        "--config",
        &pair.host.config(),
    ]);
    let done = door.finish();
    assert_eq!(done.code, Some(0), "{}", done.stderr);

    // The host trusts the controller now; the endpoint is the one thing left to say.
    run_ok(&[
        "peer",
        "approve",
        &pair.controller.id,
        "--endpoint",
        &pair.endpoint,
        "--config",
        &pair.host.config(),
    ]);
    let (host, _) = pair.host.serve(None, "info");
    pair.controller_proc.wait_stderr("link up", 1, DEADLINE);
    host.wait_stderr("link up", 1, DEADLINE);
    let listing = pair.controller.peers();
    assert!(
        listing.contains(&format!("accepted {}", pair.host.id)) && !listing.contains("waiting"),
        "{listing}"
    );
}

#[test]
fn a_restarted_controller_on_the_same_port_is_dialed_again() {
    let mut pair = Pair::new(true);
    pair.dial_config(&pair.controller.id.clone(), &pair.endpoint.clone());
    let (host, _) = pair.host.serve(None, "info");
    pair.controller_proc.wait_stderr("link up", 1, DEADLINE);
    host.wait_stderr("link up", 1, DEADLINE);

    pair.controller_proc.kill();
    host.wait_stderr("link down", 1, DEADLINE);
    let listen = format!("tcp://{}", pair.address);
    let (controller_proc, _) = pair.controller.serve(Some(&listen), "info");
    host.wait_stderr("link up", 2, DEADLINE);
    controller_proc.wait_stderr("link up", 1, DEADLINE);
}

/// A TCP forwarder in front of `to` that, while `paused`, reads and drops what arrives without closing
/// anything: a path that went silent the way a NAT's expired mapping does.
struct Proxy {
    address: SocketAddr,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Proxy {
    fn start(to: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (paused, stop) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        thread::spawn({
            let (paused, stop) = (Arc::clone(&paused), Arc::clone(&stop));
            move || {
                while !stop.load(Ordering::SeqCst) {
                    let Ok((near, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    };
                    let Ok(far) = TcpStream::connect(to) else {
                        continue;
                    };
                    for (mut from, mut onto) in [
                        (near.try_clone().unwrap(), far.try_clone().unwrap()),
                        (far, near),
                    ] {
                        let (paused, stop) = (Arc::clone(&paused), Arc::clone(&stop));
                        thread::spawn(move || {
                            let _ = from.set_read_timeout(Some(Duration::from_millis(100)));
                            let mut buffer = [0_u8; 4096];
                            while !stop.load(Ordering::SeqCst) {
                                match from.read(&mut buffer) {
                                    Ok(0) => return,
                                    Ok(count) => {
                                        if !paused.load(Ordering::SeqCst)
                                            && onto.write_all(&buffer[..count]).is_err()
                                        {
                                            return;
                                        }
                                    }
                                    Err(error)
                                        if matches!(
                                            error.kind(),
                                            std::io::ErrorKind::WouldBlock
                                                | std::io::ErrorKind::TimedOut
                                        ) => {}
                                    Err(_) => return,
                                }
                            }
                        });
                    }
                }
            }
        });
        Self {
            address,
            paused,
            stop,
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_heartbeat_ends_a_link_that_went_silent_and_the_host_dials_again() {
    let pair = Pair::new(true);
    let proxy = Proxy::start(pair.address);
    pair.dial_config(&pair.controller.id, &format!("tcp://{}", proxy.address));
    let (host, _) = pair.host.serve(None, "info");
    host.wait_stderr("link up", 1, DEADLINE);

    // Nothing arrives and nothing closes: only the heartbeat can tell the host the link is gone.
    proxy.paused.store(true, Ordering::SeqCst);
    let began = Instant::now();
    host.wait_stderr("link down", 1, DEADLINE);
    // 500 ms x 3 misses is 1.5 s; a loaded machine and the poll granularity may add some.
    assert!(
        began.elapsed() < Duration::from_secs(20),
        "the silent link took {:?} to be noticed",
        began.elapsed()
    );
    proxy.paused.store(false, Ordering::SeqCst);
    host.wait_stderr("link up", 2, DEADLINE);
}

#[test]
fn an_endpoint_whose_key_is_not_the_tables_id_is_not_linked_and_learns_nothing() {
    let pair = Pair::new(false);
    // A third key's id names the table, so whoever answers at the controller's address is the wrong key.
    let other = Node::new(pair._dir.path(), "other");
    pair.dial_config(&other.id, &pair.endpoint);
    let (host, _) = pair.host.serve(None, "info");

    host.wait_stderr("dial failed", 1, DEADLINE);
    // Several more attempts, so the absence of a link is not just that none was made yet.
    host.wait_stderr("dialing", 3, DEADLINE);
    let log = host.stderr();
    assert!(
        log.contains("did not prove itself as the key expected"),
        "{log}"
    );
    assert_eq!(host.stderr_count("link up"), 0);
    assert_eq!(pair.controller_proc.stderr_count("link up"), 0);
    // The host checked the controller's key before sending its own: the controller never saw it.
    let listing = pair.controller.peers();
    assert!(!listing.contains("waiting"), "{listing}");
}

/// A host that dialed its controller, both ready: what `grant` gives the controller on the host side, and
/// `back` what the host is given on the controller side.
struct Linked {
    pair: Pair,
    host: Proc,
}

fn linked(grant: &str, back: &str) -> Linked {
    linked_with(BEAT, grant, back)
}

/// As `linked`, with `host_link` as the host's `[link]` settings.
fn linked_with(host_link: &str, grant: &str, back: &str) -> Linked {
    let pair = Pair::new(false);
    pair.controller
        .write_with(BEAT, &Node::peer_table(&pair.host.id, None, back, ""));
    pair.host.write_with(
        host_link,
        &Node::peer_table(&pair.controller.id, Some(&pair.endpoint), grant, DIAL),
    );
    let (host, _) = pair.host.serve(None, "info");
    pair.controller_proc.wait_stderr("link up", 1, DEADLINE);
    host.wait_stderr("link up", 1, DEADLINE);
    Linked { pair, host }
}

/// `bohio remote --via` run by `from` for the link to `to`.
fn via(from: &Node, to: &Node, action: &[&str]) -> support::procs::Finished {
    let config = from.config();
    let mut args = vec!["remote", "--via", &to.id, "--config", &config];
    args.extend_from_slice(action);
    support::procs::run(&args)
}

fn toml_list(path: &std::path::Path) -> String {
    format!("'{}'", path.display())
}

#[test]
fn a_controller_runs_info_read_and_exec_on_a_host_that_dialed_it() {
    let root = tempfile::tempdir().expect("a directory");
    std::fs::write(
        root.path().join("hello.txt"),
        "read over the link the host made\n",
    )
    .unwrap();
    let grant = format!(
        "allow = [\"info\", \"exec\", \"fs.read\"]\nfs_roots = [{}]",
        toml_list(root.path())
    );
    let up = linked(&grant, "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);

    let info = via(controller, host, &["info", "--format", "json"]);
    assert_eq!(info.code, Some(0), "{}", info.stderr);
    assert!(
        info.stdout
            .contains(&format!("\"host_id\":\"{}\"", host.id)),
        "{}",
        info.stdout
    );
    let path = root.path().join("hello.txt").to_string_lossy().into_owned();
    let read = via(controller, host, &["read", &path]);
    assert_eq!(read.code, Some(0), "{}", read.stderr);
    assert_eq!(read.stdout, "read over the link the host made\n");
    let helper = support::helper();
    let echo = via(
        controller,
        host,
        &["exec", "--", &helper, "echo", "from", "the", "host"],
    );
    assert_eq!(echo.code, Some(0), "{}", echo.stderr);
    assert_eq!(echo.stdout.trim(), "from the host");
    let exit = via(controller, host, &["exec", "--", &helper, "exit", "3"]);
    assert_eq!(exit.code, Some(3), "{}", exit.stderr);
}

#[test]
fn both_sides_control_each_other_over_the_one_link_at_once() {
    let up = linked("allow = [\"exec\"]", "allow = [\"exec\"]");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let helper = support::helper();
    thread::scope(|scope| {
        let from_controller = scope.spawn(|| {
            via(
                controller,
                host,
                &["exec", "--", &helper, "echo", "controller", "ran", "it"],
            )
        });
        let from_host = scope.spawn(|| {
            via(
                host,
                controller,
                &["exec", "--", &helper, "echo", "host", "ran", "it"],
            )
        });
        let (a, b) = (from_controller.join().unwrap(), from_host.join().unwrap());
        assert_eq!(
            (a.code, a.stdout.trim()),
            (Some(0), "controller ran it"),
            "{}",
            a.stderr
        );
        assert_eq!(
            (b.code, b.stdout.trim()),
            (Some(0), "host ran it"),
            "{}",
            b.stderr
        );
    });
}

#[test]
fn a_host_that_grants_nothing_refuses_the_controller_exec() {
    let up = linked("", "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let helper = support::helper();
    let refused = via(controller, host, &["exec", "--", &helper, "echo", "no"]);
    assert_ne!(refused.code, Some(0), "an exec the host never granted ran");
    assert!(
        refused.stdout.is_empty() && refused.stderr.contains("refused"),
        "{}",
        refused.stderr
    );
    // No link at all is said so, and is not a refusal.
    let other = Node::new(up.pair.controller.dir.as_path(), "stranger");
    let none = via(controller, &other, &["info"]);
    assert_ne!(none.code, Some(0));
    assert!(none.stderr.contains("no link to"), "{}", none.stderr);
}

#[test]
fn revoking_the_controller_on_the_host_ends_its_running_exec_within_a_recheck() {
    let up = linked("allow = [\"exec\"]", "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let helper = support::helper();
    let config = controller.config();
    let mut long = Proc::start(
        &[
            "remote", "--via", &host.id, "--config", &config, "exec", "--", &helper, "tree",
            "120000",
        ],
        "warn",
    );
    thread::sleep(Duration::from_secs(2));
    let began = Instant::now();
    run_ok(&["peer", "revoke", &controller.id, "--config", &host.config()]);
    let until = Instant::now() + DEADLINE;
    while long.stderr().is_empty() && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    let took = began.elapsed();
    assert!(
        long.stderr().contains("revoked"),
        "the controller was not told: {}
host log: {}
controller log: {}",
        long.stderr(),
        up.host.stderr(),
        up.pair.controller_proc.stderr()
    );
    // `recheck` is one second here.
    assert!(took < Duration::from_secs(5), "took {took:?}");
    long.kill();
}

/// The key check and the stream recheck are two timers, so either may find a revoke first. Here only the key check can: the stream
/// recheck is an hour away. The link goes, and the stream's peer is still told it was revoked, not that the wire broke.
#[test]
fn a_revoke_found_by_the_links_key_check_still_reaches_the_running_exec_as_revoked() {
    let host_link = format!("{BEAT}\nrecheck = \"1h\"\nkey_recheck = \"250ms\"");
    let up = linked_with(&host_link, "allow = [\"exec\"]", "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let helper = support::helper();
    let config = controller.config();
    let mut long = Proc::start(
        &[
            "remote", "--via", &host.id, "--config", &config, "exec", "--", &helper, "tree",
            "120000",
        ],
        "warn",
    );
    thread::sleep(Duration::from_secs(2));
    let began = Instant::now();
    run_ok(&["peer", "revoke", &controller.id, "--config", &host.config()]);
    let until = Instant::now() + DEADLINE;
    while long.stderr().is_empty() && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    let took = began.elapsed();
    assert!(
        long.stderr().contains("revoked") && !long.stderr().contains("wire format"),
        "the controller was not told it was revoked: {}
host log: {}
controller log: {}",
        long.stderr(),
        up.host.stderr(),
        up.pair.controller_proc.stderr()
    );
    assert!(took < Duration::from_secs(5), "took {took:?}");
    long.kill();
}

/// The same revoke found by the key check, on a transfer part way through: a reader that takes a few megabytes a second keeps
/// a large file from finishing before the revoke.
#[test]
fn a_revoke_found_by_the_links_key_check_still_reaches_a_running_read_as_revoked() {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let root = tempfile::tempdir().expect("a directory");
    let big = root.path().join("big.bin");
    std::fs::File::create(&big)
        .and_then(|file| file.set_len(512 * 1024 * 1024))
        .expect("the file is made");
    let grant = format!(
        "allow = [\"fs.read\"]\nfs_roots = [{}]",
        toml_list(root.path())
    );
    let host_link = format!("{BEAT}\nrecheck = \"1h\"\nkey_recheck = \"250ms\"");
    let up = linked_with(&host_link, &grant, "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let mut child = Command::new(env!("CARGO_BIN_EXE_bohio"))
        .args([
            "remote",
            "--via",
            &host.id,
            "--config",
            &controller.config(),
        ])
        .args(["read", &big.to_string_lossy()])
        .env_remove("BOHIO_CONFIG")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let slow = thread::spawn(move || {
        let mut chunk = vec![0_u8; 64 * 1024];
        let mut total = 0_u64;
        while let Ok(count) = stdout.read(&mut chunk) {
            if count == 0 {
                break;
            }
            total += count as u64;
            thread::sleep(Duration::from_millis(10));
        }
        total
    });
    let said = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    thread::sleep(Duration::from_secs(2));
    assert!(
        child.try_wait().expect("it can be asked").is_none(),
        "the transfer ended before the revoke"
    );
    run_ok(&["peer", "revoke", &controller.id, "--config", &host.config()]);
    let until = Instant::now() + DEADLINE;
    while child.try_wait().expect("it can be asked").is_none() {
        if Instant::now() > until {
            let _ = child.kill();
            panic!("the read did not end within the deadline");
        }
        thread::sleep(Duration::from_millis(50));
    }
    let said = said.join().unwrap();
    let received = slow.join().unwrap();
    assert!(received < 512 * 1024 * 1024, "the whole file arrived");
    assert!(
        said.contains("revoked") && !said.contains("wire format"),
        "the reader was not told it was revoked: {said}
host log: {}
controller log: {}",
        up.host.stderr(),
        up.pair.controller_proc.stderr()
    );
}

#[test]
fn revoking_the_host_on_the_controller_ends_the_link_within_two_rechecks() {
    let up = linked("allow = [\"exec\"]", "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let began = Instant::now();
    run_ok(&["peer", "revoke", &host.id, "--config", &controller.config()]);
    up.pair
        .controller_proc
        .wait_stderr("link down", 1, DEADLINE);
    let took = began.elapsed();
    assert!(took < Duration::from_secs(7), "took {took:?}");
    let _ = &up.host;
}

#[test]
fn bohio_links_shows_each_side_and_what_each_offers() {
    let up = linked("allow = [\"info\", \"exec\"]", "");
    let (controller, host) = (&up.pair.controller, &up.pair.host);
    let on_host = run_ok(&["links", "--config", &host.config()]);
    let line = on_host.stdout.trim();
    assert!(
        line.starts_with(&controller.id)
            && line.contains(" dialed ")
            && line.contains(&up.pair.endpoint),
        "{line}"
    );
    // The host offers the controller what it grants; the controller offers the host nothing.
    assert!(
        line.contains("offers-to-peer: bohio-exec/1,bohio/1"),
        "{line}"
    );
    assert!(line.contains("offers-from-peer: none"), "{line}");
    let on_controller = run_ok(&["links", "--config", &controller.config()]);
    let line = on_controller.stdout.trim();
    assert!(
        line.starts_with(&host.id) && line.contains(" answered "),
        "{line}"
    );
    assert!(line.contains("offers-to-peer: none"), "{line}");
    assert!(
        line.contains("offers-from-peer: bohio-exec/1,bohio/1"),
        "{line}"
    );
    let json = run_ok(&[
        "links",
        "--format",
        "json",
        "--config",
        &controller.config(),
    ]);
    assert!(
        json.stdout.contains("\"side\":\"answered\""),
        "{}",
        json.stdout
    );
}

/// Lets every other account of the machine add files to `directory`.
fn open_to_others(directory: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o777))
            .expect("the directory is opened");
    }
    #[cfg(windows)]
    {
        let done = std::process::Command::new("icacls")
            .arg(directory)
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)M"])
            .output()
            .expect("icacls runs");
        assert!(done.status.success(), "icacls could not open the directory");
    }
}

/// `bohio serve` with no layer at all, finding them where the platform keeps them: the system file under `home`
/// on Windows (`PROGRAMDATA`, as `C:\ProgramData`), the user's file under it elsewhere (`XDG_CONFIG_HOME`), and
/// its data under `data`. Answers the node, its endpoint and id, and the layer file it would read once made.
fn serve_finding_layers_in(
    home: &std::path::Path,
    data: &std::path::Path,
    nowhere: &std::path::Path,
) -> (Proc, String, String, std::path::PathBuf) {
    let environment: Vec<(&str, &std::path::Path)> = if cfg!(windows) {
        vec![
            ("PROGRAMDATA", home),
            ("APPDATA", nowhere),
            ("LOCALAPPDATA", data),
        ]
    } else {
        vec![("XDG_CONFIG_HOME", home), ("XDG_DATA_HOME", data)]
    };
    let mut serving = Proc::start_in(
        &["serve", "--listen", "tcp://127.0.0.1:0"],
        "info",
        &environment,
    );
    let endpoint = serving.expect_line("listening ");
    let id = serving.expect_line("host ");
    (serving, endpoint, id, home.join("bohio").join("bohio.cfg"))
}

/// A layer that does not exist when the node starts is read once it is made, as a peer approved later is, but only
/// where no other account could make it: a file another account makes in an open directory (`%ProgramData%` lets
/// every user add one) is never checked, so it would choose who the node accepts and what each may do.
#[test]
fn a_layer_made_while_serving_is_read_only_where_no_other_account_could_make_it() {
    let dir = tempfile::tempdir().expect("a directory");
    let client = Node::new(dir.path(), "client");
    let nowhere = dir.path().join("nowhere");
    std::fs::create_dir(&nowhere).expect("a directory");
    let info = |endpoint: &str, host: &str| {
        client.write(&Node::peer_table(host, None, "", ""));
        run(&["remote", endpoint, "--config", &client.config(), "info"])
    };
    let grant = Node::peer_table(&client.id, None, "allow = [\"info\"]", "");

    let open = dir.path().join("open");
    std::fs::create_dir(&open).expect("a directory");
    open_to_others(&open);
    let (serving, endpoint, host, layer) =
        serve_finding_layers_in(&open, &dir.path().join("open-data"), &nowhere);
    std::fs::create_dir_all(layer.parent().expect("a directory")).expect("a directory");
    std::fs::write(&layer, &grant).expect("the layer is made");
    let refused = info(&endpoint, &host);
    assert_ne!(
        refused.code,
        Some(0),
        "a layer another account could have made granted info: {}",
        refused.stdout
    );
    serving.wait_stderr("is not read until a restart", 1, DEADLINE);
    drop(serving);

    let private = dir.path().join("private");
    std::fs::create_dir(&private).expect("a directory");
    let (_serving, endpoint, host, layer) =
        serve_finding_layers_in(&private, &dir.path().join("private-data"), &nowhere);
    std::fs::create_dir_all(layer.parent().expect("a directory")).expect("a directory");
    std::fs::write(&layer, &grant).expect("the layer is made");
    let served = info(&endpoint, &host);
    assert_eq!(served.code, Some(0), "{}", served.stderr);
}
