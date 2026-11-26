//! Forwards that survive a lost link: `[peer."<id>".forward] local` and `remote` kept by a node, and `bohio remote ... forward`,
//! `connect` and `listen` making the link again. The far node is a `bohio serve` child that is killed and started again on
//! the same port; nothing is run after it is back.

mod support;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use bohio::config::{ForwardConfig, LinkConfig};
use bohio::policy::{Grant, Permission};
use bohio_client::forward::{ForwardSettings, Target, connect};
use support::embedded::{Deadline, Running, Spawned, bohio, client_config, id_of, listening};
use support::procs::{DEADLINE, Node, Proc, run};

/// A quick heartbeat for both sides of a link.
const BEAT: &str = "heartbeat = \"500ms\"\nheartbeat_misses = 3";

/// How fast a node dials again in these tests.
const DIAL: &str = "retry_min = \"200ms\"\nretry_max = \"1s\"";

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let until = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < until, "{what} within the deadline");
        thread::sleep(Duration::from_millis(25));
    }
}

/// A TCP echo server on a free loopback port; the port.
fn echo() -> u16 {
    let (listener, port) = listening();
    thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut connection = connection;
                let mut buffer = [0_u8; 4096];
                loop {
                    match connection.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            if connection.write_all(&buffer[..count]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = connection.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

/// Sends `payload` to `address`, half-closes and reads what comes back until the end.
fn exchange(address: &str, payload: &[u8], within: Duration) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(within))?;
    stream.write_all(payload)?;
    stream.shutdown(Shutdown::Write)?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    Ok(reply)
}

const LONG: Duration = Duration::from_secs(40);

/// The address before ` -> ` in a `local ADDR -> TARGET peer ID` line (without its first word).
fn address(rest: &str) -> String {
    rest.split(" -> ").next().unwrap_or(rest).to_owned()
}

/// The `ADDRESS -> TARGET ...` lines a node printed for `word` (`local` or `remote`): where each forward listened, each time.
fn ups(process: &Proc, word: &str) -> Vec<String> {
    process
        .stdout_lines(word)
        .into_iter()
        .filter(|line| line.contains(" -> "))
        .collect()
}

/// A far node (a `bohio serve` child on a fixed port, killed and started again by the test), a near node that dials it, and a
/// client for `bohio remote`.
struct Site {
    _dir: tempfile::TempDir,
    far: Node,
    near: Node,
    client: Node,
    port: Option<u16>,
    echo_l: u16,
    echo_r: u16,
    far_proc: Option<Proc>,
}

impl Site {
    fn new() -> Self {
        let dir = crate::support::private_dir::private_tempdir();
        let far = Node::new(dir.path(), "far");
        let near = Node::new(dir.path(), "near");
        let client = Node::new(dir.path(), "client");
        let grant = |id: &str| {
            format!(
                "[peer.\"{id}\"]\nallow = [\"info\", \"forward.connect\", \"forward.listen\"]\n\
                 [peer.\"{id}\".forward]\nconnect = [\"127.0.0.1:*\"]\nlisten = [\"127.0.0.1:*\"]\n"
            )
        };
        far.write_with(BEAT, &(grant(&near.id) + &grant(&client.id)));
        Self {
            _dir: dir,
            far,
            near,
            client,
            port: None,
            echo_l: echo(),
            echo_r: echo(),
            far_proc: None,
        }
    }

    fn endpoint(&self) -> String {
        format!("tcp://127.0.0.1:{}", self.port.unwrap_or(1))
    }

    fn start_far(&mut self) {
        // The first start lets the system choose the port and later ones take it again, so no other program is handed it
        // between a test choosing it and the node binding it.
        let endpoint = self
            .port
            .map_or_else(|| "tcp://127.0.0.1:0".to_owned(), |_| self.endpoint());
        let (process, listening) = self.far.serve(Some(&endpoint), "info");
        if let Some((address, _)) = listening {
            self.port = Some(address.port());
        }
        self.far_proc = Some(process);
    }

    fn kill_far(&mut self) {
        if let Some(mut process) = self.far_proc.take() {
            process.kill();
        }
    }

    /// The near node's file: `section` first (such as a `[forward]` table), then its table for the far node.
    fn write_near(&self, section: &str, local: &[String], remote: &[String]) {
        let list = |entries: &[String]| {
            entries
                .iter()
                .map(|entry| format!("\"{entry}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        self.near.write_with(
            BEAT,
            &format!(
                "{section}\n[peer.\"{}\"]\nallow = [\"info\"]\nendpoint = \"{}\"\n[peer.\"{}\".dial]\n{DIAL}\n\
                 [peer.\"{}\".forward]\nlocal = [{}]\nremote = [{}]\n",
                self.far.id,
                self.endpoint(),
                self.far.id,
                self.far.id,
                list(local),
                list(remote)
            ),
        );
    }

    fn local_entry(&self) -> String {
        format!("127.0.0.1:0=127.0.0.1:{}", self.echo_l)
    }

    fn remote_entry(&self) -> String {
        format!("127.0.0.1:0=127.0.0.1:{}", self.echo_r)
    }

    /// The near node running, with one `-L` and one `-R` declared.
    fn start_near(&self, section: &str) -> Proc {
        self.write_near(section, &[self.local_entry()], &[self.remote_entry()]);
        self.near.serve(None, "info").0
    }

    /// `bohio remote` to the far node as the client, with `rest` after `remote ENDPOINT`.
    fn remote(&self, rest: &[&str]) -> Proc {
        let endpoint = self.endpoint();
        let config = self.client.config();
        let mut args = vec![
            "remote",
            endpoint.as_str(),
            "--config",
            config.as_str(),
            "--server",
            self.far.id.as_str(),
        ];
        args.extend_from_slice(rest);
        Proc::start(&args, "warn")
    }
}

#[test]
fn declared_forwards_work_again_after_the_far_node_is_killed_and_restarted() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    let mut near = site.start_near("");
    near.wait_stderr("link up", 1, DEADLINE);
    let local_at = address(&near.expect_line("local "));
    let remote_at = address(&near.expect_line("remote "));
    assert_eq!(
        exchange(&local_at, b"before, -L", LONG).expect("-L"),
        b"before, -L"
    );
    assert_eq!(
        exchange(&remote_at, b"before, -R", LONG).expect("-R"),
        b"before, -R"
    );

    site.kill_far();
    near.wait_stderr("link down", 1, DEADLINE);
    let restarted = Instant::now();
    site.start_far();
    near.wait_stderr("link up", 2, DEADLINE);
    let link_back = restarted.elapsed();
    wait_for("the remote forward is asked for again", || {
        ups(&near, "remote ").len() >= 2
    });
    let remote_back = restarted.elapsed();
    let remote_lines = ups(&near, "remote ");
    assert_eq!(
        address(&remote_lines[1]),
        remote_at,
        "the host is asked for the port it chose the first time"
    );
    assert_eq!(
        ups(&near, "local ").len(),
        1,
        "the local listener stayed bound: {}",
        near.stdout()
    );

    // Nothing was run since the far node came back.
    let served = Instant::now();
    assert_eq!(
        exchange(&local_at, b"after, -L", LONG).expect("-L after"),
        b"after, -L"
    );
    let local_served = served.elapsed();
    assert_eq!(
        exchange(&remote_at, b"after, -R", LONG).expect("-R after"),
        b"after, -R"
    );
    eprintln!(
        "outage timings: the far node is started again, then the link is back after {link_back:?}, \
         the remote forward is asked for again after {remote_back:?}, and a connection through the local forward is served in {local_served:?}"
    );
}

#[test]
fn a_connection_made_during_the_outage_is_served_when_the_link_is_back_within_wait_for_link() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    let mut near = site.start_near("[forward]\nwait_for_link = \"30s\"\n");
    near.wait_stderr("link up", 1, DEADLINE);
    let local_at = address(&near.expect_line("local "));
    site.kill_far();
    near.wait_stderr("link down", 1, DEADLINE);

    let began = Instant::now();
    let waiting = thread::spawn({
        let local_at = local_at.clone();
        move || exchange(&local_at, b"sent during the outage", LONG)
    });
    thread::sleep(Duration::from_millis(1500));
    assert!(!waiting.is_finished(), "the connection waits for the link");
    site.start_far();
    let reply = waiting.join().expect("the client ended").expect("a reply");
    assert_eq!(reply, b"sent during the outage");
    eprintln!(
        "outage timings: a connection made while the link was down was served {:?} after it was made",
        began.elapsed()
    );
}

#[test]
fn a_connection_made_during_the_outage_is_refused_past_wait_for_link() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    let mut near = site.start_near("[forward]\nwait_for_link = \"1s\"\n");
    near.wait_stderr("link up", 1, DEADLINE);
    let local_at = address(&near.expect_line("local "));
    site.kill_far();
    near.wait_stderr("link down", 1, DEADLINE);

    let began = Instant::now();
    let refused = exchange(&local_at, b"too late", LONG);
    let waited = began.elapsed();
    assert!(
        refused.map_or(true, |reply| reply.is_empty()),
        "nothing was carried"
    );
    assert!(
        waited >= Duration::from_millis(900) && waited < Duration::from_secs(15),
        "closed after about wait_for_link, not at once and not never: {waited:?}"
    );
    eprintln!("outage timings: a connection past wait_for_link = 1s was closed after {waited:?}");

    // The listener is still there: the next connection is served when the link is.
    site.start_far();
    near.wait_stderr("link up", 2, DEADLINE);
    assert_eq!(
        exchange(&local_at, b"in time", LONG).expect("a reply"),
        b"in time"
    );
}

#[test]
fn live_edits_start_and_stop_declared_entries() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    site.write_near("", &[site.local_entry()], &[]);
    let mut near = site.near.serve(None, "info").0;
    near.wait_stderr("link up", 1, DEADLINE);
    let first = address(&near.expect_line("local "));
    assert_eq!(
        exchange(&first, b"one", LONG).expect("the first entry"),
        b"one"
    );

    // An entry added to the file is started within the re-check.
    let second_entry = format!("127.0.0.1:0=127.0.0.1:{}", site.echo_r);
    site.write_near("", &[site.local_entry(), second_entry.clone()], &[]);
    wait_for("the second entry listens", || {
        ups(&near, "local ").len() >= 2
    });
    let second = address(&ups(&near, "local ")[1]);
    assert_eq!(
        exchange(&second, b"two", LONG).expect("the second entry"),
        b"two"
    );

    // An entry taken out stops and its port is closed.
    site.write_near("", &[second_entry], &[]);
    wait_for("the first entry's port is closed", || {
        TcpStream::connect(&first).is_err()
    });
    assert_eq!(
        exchange(&second, b"still", LONG).expect("the second entry stays"),
        b"still"
    );
}

#[test]
fn a_bad_declared_entry_is_named_by_config_check_and_approve_keeps_the_entries() {
    let site = Site::new();
    site.write_near("", &["nonsense".to_owned()], &[]);
    let checked = run(&["config", "check", "--config", &site.near.config()]);
    assert_ne!(checked.code, Some(0), "{}", checked.stdout);
    assert!(
        checked.stderr.contains("forward.local") && checked.stderr.contains("nonsense"),
        "{}",
        checked.stderr
    );

    site.write_near("", &[site.local_entry()], &[site.remote_entry()]);
    let approved = run(&[
        "peer",
        "approve",
        &site.far.id,
        "--allow",
        "info",
        "--connect",
        "db.lan:5432",
        "--config",
        &site.near.config(),
    ]);
    assert_eq!(approved.code, Some(0), "{}", approved.stderr);
    let text = std::fs::read_to_string(&site.near.config).expect("the file");
    assert!(
        text.contains("local = [") && text.contains("remote = [") && text.contains("db.lan:5432"),
        "{text}"
    );
    assert!(text.contains("endpoint"), "{text}");
    let checked = run(&["config", "check", "--config", &site.near.config()]);
    assert_eq!(
        checked.code,
        Some(0),
        "{}{}",
        checked.stdout,
        checked.stderr
    );
}

#[test]
fn remote_forward_ends_with_the_link_under_once() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    let spec = site.local_entry();
    let mut forward = site.remote(&["forward", "--once", "-L", &spec]);
    let local_at = address(&forward.expect_line("local "));
    assert_eq!(exchange(&local_at, b"up", LONG).expect("served"), b"up");
    site.kill_far();
    wait_for("--once ends with the link", || forward.exited().is_some());
    assert_eq!(forward.exited(), Some(1));
    assert!(
        forward.stderr().contains("the link to the host ended"),
        "{}",
        forward.stderr()
    );
}

#[test]
fn remote_forward_makes_the_link_again_by_default_and_asks_for_the_same_remote_port() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    let (local, remote) = (site.local_entry(), site.remote_entry());
    let mut forward = site.remote(&[
        "forward",
        "--retry-min",
        "100ms",
        "--retry-max",
        "500ms",
        "-L",
        &local,
        "-R",
        &remote,
    ]);
    let local_at = address(&forward.expect_line("local "));
    let remote_at = address(&forward.expect_line("remote "));
    assert_eq!(exchange(&remote_at, b"-R", LONG).expect("-R"), b"-R");

    site.kill_far();
    forward.wait_stderr("the link to the host was lost", 1, DEADLINE);
    assert_eq!(forward.exited(), None, "the command keeps running");
    site.start_far();
    forward.wait_stderr("the link to the host is back", 1, DEADLINE);
    wait_for("the remote forward is printed again", || {
        forward.stdout_count("remote ") >= 2
    });
    assert_eq!(address(&forward.stdout_lines("remote ")[1]), remote_at);
    assert_eq!(forward.stdout_count("local "), 1);
    assert_eq!(
        exchange(&local_at, b"again -L", LONG).expect("-L"),
        b"again -L"
    );
    assert_eq!(
        exchange(&remote_at, b"again -R", LONG).expect("-R"),
        b"again -R"
    );
}

#[test]
fn remote_listen_listens_again_for_the_same_port_after_the_link_was_lost() {
    let _deadline = Deadline::start();
    let mut site = Site::new();
    site.start_far();
    let listen = site.remote(&[
        "listen",
        "--retry-min",
        "100ms",
        "--retry-max",
        "500ms",
        "127.0.0.1:0",
    ]);
    listen.wait_stderr("listening ", 1, DEADLINE);
    let said = listen.stderr();
    let first = said
        .lines()
        .find_map(|line| line.strip_prefix("listening "))
        .expect("a listening line")
        .to_owned();
    site.kill_far();
    listen.wait_stderr("the link to the host ended", 1, DEADLINE);
    site.start_far();
    let again = || -> Vec<String> {
        listen
            .stderr()
            .lines()
            .filter_map(|line| line.strip_prefix("listening "))
            .map(str::to_owned)
            .collect()
    };
    wait_for("it listens again", || again().len() >= 2);
    assert_eq!(again()[1], first, "{:?}", again());
    let _ = exchange(&first, b"ping", LONG);
    let done = listen.finish();
    assert_eq!(done.code, Some(0), "{}", done.stderr);
    assert_eq!(done.stdout, "ping");
}

/// A burst of `count` connections through one `bohio remote ... forward -L`, to a host that holds `max_pending` opens
/// pending and `per_peer` connections for the client, and a client that retries for `open_retry`: how many failed.
fn burst(max_pending: usize, per_peer: usize, open_retry: &str, count: usize) -> Vec<String> {
    let dir = crate::support::private_dir::private_tempdir();
    let target = echo();
    let node = Running::with_link(
        ForwardConfig {
            max_connections_per_peer: per_peer,
            ..ForwardConfig::default()
        },
        LinkConfig {
            max_pending_opens: max_pending,
            ..LinkConfig::default()
        },
    );
    let config = client_config(dir.path(), "peer");
    let mut text = std::fs::read_to_string(&config).expect("the file");
    text.push_str(&format!("[forward]\nopen_retry = \"{open_retry}\"\n"));
    std::fs::write(&config, text).expect("written");
    let peer = id_of(&config);
    node.keys.accept(peer);
    node.grants.set(
        peer,
        Grant::allowing([Permission::ForwardConnect]).with_connect([format!("127.0.0.1:{target}")]),
    );
    let config = config.to_string_lossy().into_owned();
    let server = node.id.to_string();
    let spec = format!("127.0.0.1:0=127.0.0.1:{target}");
    let mut args = vec![
        "remote",
        node.endpoint.as_str(),
        "--config",
        config.as_str(),
        "--server",
        server.as_str(),
    ];
    args.extend(["forward", "-L", spec.as_str()]);
    let forward = Spawned::start(bohio(&args));
    let local_at = address(&forward.line("local "));
    let barrier = Arc::new(Barrier::new(count));
    let clients: Vec<_> = (0..count)
        .map(|index| {
            let (barrier, local_at) = (Arc::clone(&barrier), local_at.clone());
            thread::spawn(move || {
                let payload: Vec<u8> = (0..16 * 1024)
                    .map(|at| u8::try_from((at + index) % 251).unwrap_or(0))
                    .collect();
                barrier.wait();
                match exchange(&local_at, &payload, Duration::from_secs(90)) {
                    Ok(reply) if reply == payload => None,
                    Ok(reply) => Some(format!(
                        "#{index}: {} bytes came back of {}",
                        reply.len(),
                        payload.len()
                    )),
                    Err(error) => Some(format!("#{index}: {error}")),
                }
            })
        })
        .collect();
    let failed: Vec<String> = clients
        .into_iter()
        .filter_map(|client| client.join().expect("a client ended"))
        .collect();
    let said = forward.stderr();
    drop(forward);
    if !failed.is_empty() {
        eprintln!("the forward said: {said}");
    }
    failed
}

#[test]
fn a_connect_past_the_hosts_bound_waits_for_room_instead_of_failing() {
    let _deadline = Deadline::start();
    let node = Running::with(ForwardConfig {
        max_connections_per_peer: 1,
        ..ForwardConfig::default()
    });
    let port = echo();
    let target = Target::Tcp {
        host: "127.0.0.1".to_owned(),
        port,
    };
    let (link, _) = node.link(
        Grant::allowing([Permission::ForwardConnect]).with_connect([format!("127.0.0.1:{port}")]),
    );
    let settings = ForwardSettings::default();
    let first = connect(&link, &target, &settings).expect("the first");
    let freed = thread::spawn(move || {
        thread::sleep(Duration::from_millis(700));
        drop(first);
    });
    let began = Instant::now();
    let second = connect(&link, &target, &settings).expect("the second waits for room");
    let waited = began.elapsed();
    assert!(
        waited >= Duration::from_millis(600),
        "it was made when the first was dropped, not before: {waited:?}"
    );
    drop(second);
    freed.join().expect("the first was dropped");
    eprintln!("burst timings: a connect past max_connections_per_peer waited {waited:?} for room");
}

#[test]
fn sixty_four_connections_at_once_through_one_local_forward_all_succeed() {
    let _deadline = Deadline::start();
    let began = Instant::now();
    assert_eq!(
        burst(8, 16, "10s", 64),
        Vec::<String>::new(),
        "the default limits"
    );
    eprintln!(
        "burst timings: 64 connections, default limits: {:?}",
        began.elapsed()
    );
}

#[test]
fn a_burst_over_a_host_that_holds_one_open_pending_needs_the_retry() {
    let _deadline = Deadline::start();
    // Control: with no retry, a host that holds one open at a time refuses some of a burst (its per-peer bound is out of the
    // way here, so what is refused is the open itself).
    let failed = (0..5)
        .map(|_| burst(1, 128, "0s", 64).len())
        .find(|failed| *failed > 0);
    assert!(
        failed.is_some(),
        "the host never refused an open: the retry is not what made the burst succeed"
    );
    let began = Instant::now();
    assert_eq!(burst(1, 128, "30s", 64), Vec::<String>::new(), "retrying");
    eprintln!(
        "burst timings: 64 connections, one pending open on the host: {:?} (control without retry: {failed:?} failed)",
        began.elapsed()
    );
}
