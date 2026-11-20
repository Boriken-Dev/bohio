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

use support::procs::{DEADLINE, Node, Proc, run_ok};

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
    // The controller's pairing door, as a second process of the same node.
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
    let door_endpoint = door.expect_line("pairing on ");
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
