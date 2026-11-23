//! Jump hosts: a link run over a stream another host carries (`bohio remote --jump`, `Connection::through`), proven end to
//! end, over loopback with real nodes.

#![forbid(unsafe_code)]

mod support;

use std::sync::Arc;

use bohio::policy::{Grant, Permission};
use bohio_client::Error;
use bohio_client::Open;
use bohio_client::connect::Connection;
use bohio_client::control::Control;
use bohio_client::dial::Endpoint;
use bohio_client::exec::Exec;
use bohio_client::forward::ForwardSettings;
use bohio_client::link::Settings;
use bohio_proto::exec::Request;
use coqui_link::auth::{Identity, Method, Proof, Trust};
use coqui_peer::PeerId;
use coqui_transport::Failure;
use support::embedded::{
    Deadline, Running, Spawned, WAIT, bohio, client_config, id_of, remote_args,
};
use support::helper;

/// What the peer holds at the jump: a stream to the next hop's port and nothing else.
fn forward_only(port: u16) -> Grant {
    Grant::allowing([Permission::ForwardConnect]).with_connect([format!("127.0.0.1:{port}")])
}

/// What the peer holds at the far host: it may run the helper.
fn runs_the_helper() -> Grant {
    Grant::allowing([Permission::Exec]).with_programs([helper()])
}

/// A new identity that `node` accepts holding `grant`.
fn accepted_by(node: &Running, identity: &Identity, grant: Grant) {
    node.keys.accept(identity.peer_id());
    node.grants.set(identity.peer_id(), grant);
}

/// A proof by `identity` that accepts only `host`.
fn proving(identity: &Identity, host: PeerId) -> Proof {
    Proof::builder(identity.clone())
        .responders(Trust::pinned([host]))
        .build()
        .expect("a proof")
}

fn through(
    jump: &Arc<Connection>,
    endpoint: &Endpoint,
    identity: &Identity,
    host: PeerId,
) -> Result<Connection, Error> {
    Connection::through(
        Arc::clone(jump) as Arc<dyn Open>,
        endpoint,
        &proving(identity, host),
        &Settings::default(),
        &ForwardSettings::default(),
    )
}

fn echoed(link: &dyn Open, words: &[&str]) -> String {
    let mut argv = vec![helper(), "echo".to_owned()];
    argv.extend(words.iter().map(|word| (*word).to_owned()));
    let done = Exec::start(link, &Request::argv(argv))
        .expect("the stream opens")
        .wait(Some(WAIT))
        .expect("the program ends");
    String::from_utf8_lossy(&done.stdout).trim().to_owned()
}

#[test]
fn a_client_runs_exec_on_b_through_a_whose_grant_holds_only_forward_connect() {
    let _deadline = Deadline::start();
    let (a, b) = (Running::start(), Running::start());
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    accepted_by(&a, &identity, forward_only(b.address.port()));
    accepted_by(&b, &identity, runs_the_helper());
    let jump = Arc::new(a.link_as(&identity, a.address));
    let far = through(&jump, &Endpoint::Tcp(b.address), &identity, b.id).expect("through A to B");
    assert_eq!(far.host(), b.id, "the host that answered is B, proven");
    assert_eq!(echoed(&far, &["through", "a", "jump"]), "through a jump");

    // A grants this peer no exec: what ran, ran on B, and A would not have run it.
    let refused = Exec::start(&*jump, &Request::argv([helper(), "echo".to_owned()]))
        .expect("the stream opens")
        .wait(Some(WAIT));
    assert!(
        matches!(
            &refused,
            Err(Error::Remote(_) | Error::Transport(Failure::Permission))
        ),
        "A runs nothing for this peer: {refused:?}"
    );
    // Nor does it carry a stream to any other port.
    let other = a.address;
    let error = through(&jump, &Endpoint::Tcp(other), &identity, a.id).expect_err("not granted");
    assert!(
        matches!(error, Error::Transport(Failure::Permission)),
        "{error:?}"
    );
}

#[test]
fn a_jump_without_forward_connect_to_the_next_hop_is_refused() {
    let _deadline = Deadline::start();
    let (a, b) = (Running::start(), Running::start());
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    accepted_by(&a, &identity, Grant::allowing([Permission::Info]));
    accepted_by(&b, &identity, runs_the_helper());
    let jump = Arc::new(a.link_as(&identity, a.address));
    let error = through(&jump, &Endpoint::Tcp(b.address), &identity, b.id).expect_err("refused");
    assert!(
        matches!(error, Error::Transport(Failure::Permission)),
        "{error:?}"
    );
}

#[test]
fn a_hop_pointed_at_the_jumps_own_listener_cannot_stand_in_for_b() {
    let _deadline = Deadline::start();
    let (a, b) = (Running::start(), Running::start());
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    // A carries streams to itself and to B, so a hop at A's own port reaches a real listener that answers the proof.
    a.keys.accept(identity.peer_id());
    a.grants.set(
        identity.peer_id(),
        Grant::allowing([Permission::ForwardConnect]).with_connect([
            format!("127.0.0.1:{}", a.address.port()),
            format!("127.0.0.1:{}", b.address.port()),
        ]),
    );
    accepted_by(&b, &identity, runs_the_helper());
    let jump = Arc::new(a.link_as(&identity, a.address));
    // Pinned to B, whatever answers at A's port is refused: A holds its own key, not B's.
    let standing_in = through(&jump, &Endpoint::Tcp(a.address), &identity, b.id)
        .expect_err("A cannot prove it is B");
    assert!(
        !matches!(standing_in, Error::Remote(_)),
        "the stream was carried and the proof is what failed: {standing_in:?}"
    );
    // The same route proves when it is A that is trusted, so the failure above was the proof's.
    let honest = through(&jump, &Endpoint::Tcp(a.address), &identity, a.id)
        .expect("A proves itself to the peer that trusts A");
    assert_eq!(honest.host(), a.id);
    // And the real B is reached when the hop names B.
    let far = through(&jump, &Endpoint::Tcp(b.address), &identity, b.id).expect("B");
    assert_eq!(far.host(), b.id);
}

#[test]
fn a_chain_of_two_jumps_reaches_a_third_host() {
    let _deadline = Deadline::start();
    let (a, b, c) = (Running::start(), Running::start(), Running::start());
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    accepted_by(&a, &identity, forward_only(b.address.port()));
    accepted_by(&b, &identity, forward_only(c.address.port()));
    accepted_by(&c, &identity, runs_the_helper());
    let first = Arc::new(a.link_as(&identity, a.address));
    let second = Arc::new(
        through(&first, &Endpoint::Tcp(b.address), &identity, b.id).expect("through A to B"),
    );
    let third = through(&second, &Endpoint::Tcp(c.address), &identity, c.id)
        .expect("through B, which A carries, to C");
    assert_eq!(third.host(), c.id);
    assert_eq!(echoed(&third, &["two", "hops"]), "two hops");
    // Closing the last link, then the ones under it, ends cleanly.
    drop(third);
    drop(second);
    drop(first);
}

#[test]
fn an_endpoint_a_jump_does_not_carry_is_named() {
    let _deadline = Deadline::start();
    let a = Running::start();
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    accepted_by(&a, &identity, Grant::allowing([Permission::Info]));
    let jump = Arc::new(a.link_as(&identity, a.address));
    let endpoint = Endpoint::parse("ws://127.0.0.1:9").expect("an endpoint");
    let error = through(&jump, &endpoint, &identity, a.id).expect_err("not carried");
    assert!(
        matches!(&error, Error::Link(text) if text.contains("ws://127.0.0.1:9")),
        "{error:?}"
    );
}

/// A host that is a `bohio serve --stdio` program: its configuration, its id, and the peer it trusts.
fn stdio_host(dir: &std::path::Path, peer: PeerId) -> (String, String, PeerId) {
    use support::procs::Node;
    let host = Node::new(dir, "stdio-host");
    host.write(&Node::peer_table(
        &peer.to_string(),
        None,
        "allow = [\"info\"]",
        "",
    ));
    let id: PeerId = host.id.parse().expect("a peer id");
    (host.config(), host.id.clone(), id)
}

#[test]
fn a_stdio_hop_runs_bohio_serve_on_the_jump_and_proves_it() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let a = Running::start();
    let identity = Identity::generate(Method::DEFAULT).expect("an identity");
    let program = env!("CARGO_BIN_EXE_bohio").to_owned();
    // A runs one program for this peer and carries no stream: the program is the way on.
    accepted_by(
        &a,
        &identity,
        Grant::allowing([Permission::Exec]).with_programs([program.clone()]),
    );
    let (config, _, host_id) = stdio_host(dir.path(), identity.peer_id());
    let jump = Arc::new(a.link_as(&identity, a.address));
    let hop = Endpoint::Command(vec![
        program.clone(),
        "serve".to_owned(),
        "--stdio".to_owned(),
        "--config".to_owned(),
        config,
    ]);
    let far = through(&jump, &hop, &identity, host_id).expect("through A's program");
    assert_eq!(far.host(), host_id);
    let info = Control::open(&far)
        .expect("a control stream")
        .info(Some(WAIT))
        .expect("info");
    assert_eq!(
        info.host_id.as_deref(),
        Some(host_id.to_string().as_str()),
        "the far host answered info"
    );

    // Without the exec grant the hop is refused.
    let refused = Arc::new(Running::start());
    let nobody = Identity::generate(Method::DEFAULT).expect("an identity");
    accepted_by(&refused, &nobody, Grant::allowing([Permission::Info]));
    let jump = Arc::new(refused.link_as(&nobody, refused.address));
    let error = through(&jump, &hop, &nobody, host_id).expect_err("no exec");
    assert!(
        matches!(
            error,
            Error::Remote(_) | Error::Transport(Failure::Permission)
        ),
        "{error:?}"
    );
}

/// Two nodes and a command-line peer that both accept: A grants a stream to B's port only, B grants the helper.
fn two_hosts(dir: &std::path::Path, with_forward: bool) -> (Running, Running, String, PeerId) {
    let (a, b) = (Running::start(), Running::start());
    let config = client_config(dir, "peer");
    let peer = id_of(&config);
    a.keys.accept(peer);
    a.grants.set(
        peer,
        if with_forward {
            forward_only(b.address.port())
        } else {
            Grant::allowing([Permission::Info])
        },
    );
    b.keys.accept(peer);
    b.grants.set(peer, runs_the_helper());
    (a, b, config.to_string_lossy().into_owned(), peer)
}

#[test]
fn remote_jump_runs_exec_on_b_through_a_and_names_the_hop_that_refused() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let (a, b, config, _) = two_hosts(dir.path(), true);
    let (server, jump_server) = (b.id.to_string(), a.id.to_string());
    let helper = helper();
    let mut args = remote_args(&b, &config, &server);
    args.extend([
        "--jump",
        a.endpoint.as_str(),
        "--jump-server",
        jump_server.as_str(),
        "exec",
        "--",
        helper.as_str(),
        "echo",
        "via",
        "the",
        "cli",
    ]);
    let mut run = Spawned::start(bohio(&args));
    assert_eq!(run.finish(), Some(0), "{}", run.stderr());
    assert_eq!(String::from_utf8_lossy(&run.stdout()).trim(), "via the cli");

    // A without a forward grant: the refusal names the jump that would not carry it.
    let (a, b, config, _) = two_hosts(dir.path(), false);
    let (server, jump_server) = (b.id.to_string(), a.id.to_string());
    let mut args = remote_args(&b, &config, &server);
    args.extend([
        "--jump",
        a.endpoint.as_str(),
        "--jump-server",
        jump_server.as_str(),
        "info",
    ]);
    let mut refused = Spawned::start(bohio(&args));
    assert_eq!(refused.finish(), Some(1));
    let said = refused.stderr();
    assert!(
        said.contains("jump 1")
            && said.contains("would not carry")
            && said.contains("forward.connect"),
        "{said}"
    );
}

#[test]
fn remote_jump_to_the_jumps_own_listener_fails_the_proof_and_a_wrong_jump_id_is_named() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let (a, b, config, peer) = two_hosts(dir.path(), true);
    // A also carries a stream to its own port: the hop reaches a real listener, which is not B.
    a.grants.set(
        peer,
        Grant::allowing([Permission::ForwardConnect]).with_connect([
            format!("127.0.0.1:{}", b.address.port()),
            format!("127.0.0.1:{}", a.address.port()),
        ]),
    );
    let (server, jump_server) = (b.id.to_string(), a.id.to_string());
    // B is pinned (--server), the endpoint is A's own listener through the jump: A cannot stand in for B.
    let mut args = remote_args(&a, &config, &server);
    args.extend([
        "--jump",
        a.endpoint.as_str(),
        "--jump-server",
        jump_server.as_str(),
        "info",
    ]);
    let mut standing_in = Spawned::start(bohio(&args));
    assert_eq!(standing_in.finish(), Some(1));
    let said = standing_in.stderr();
    assert!(
        said.contains("jump 1") && said.contains("not the host this side trusts"),
        "{said}"
    );
    // A jump pinned to the wrong id is refused at the first hop, before anything is carried.
    let mut args = remote_args(&b, &config, &server);
    args.extend([
        "--jump",
        a.endpoint.as_str(),
        "--jump-server",
        server.as_str(),
        "info",
    ]);
    let mut wrong = Spawned::start(bohio(&args));
    assert_eq!(wrong.finish(), Some(1));
    assert!(wrong.stderr().contains("jump 1"), "{}", wrong.stderr());
}

#[test]
fn remote_jump_to_a_stdio_endpoint_runs_the_program_on_the_jump() {
    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let a = Running::start();
    let config = client_config(dir.path(), "peer");
    let peer = id_of(&config);
    let program = env!("CARGO_BIN_EXE_bohio").to_owned();
    a.keys.accept(peer);
    a.grants.set(
        peer,
        Grant::allowing([Permission::Exec]).with_programs([program.clone()]),
    );
    let (host_config, host_id, _) = stdio_host(dir.path(), peer);
    let config = config.to_string_lossy().into_owned();
    let command = format!("{program} serve --stdio --config {host_config}");
    let jump_server = a.id.to_string();
    let args = vec![
        "remote",
        "--command",
        command.as_str(),
        "--config",
        config.as_str(),
        "--server",
        host_id.as_str(),
        "--jump",
        a.endpoint.as_str(),
        "--jump-server",
        jump_server.as_str(),
        "info",
        "--format",
        "json",
    ];
    let mut run = Spawned::start(bohio(&args));
    assert_eq!(run.finish(), Some(0), "{}", run.stderr());
    assert!(
        String::from_utf8_lossy(&run.stdout()).contains("host_id"),
        "{}",
        String::from_utf8_lossy(&run.stdout())
    );
}

#[test]
fn forward_dash_d_through_a_jump_reaches_a_target_only_the_far_host_grants() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use support::embedded::{listening, serve_once};

    let _deadline = Deadline::start();
    let dir = tempfile::tempdir().expect("a directory");
    let (a, b) = (Running::start(), Running::start());
    let config = client_config(dir.path(), "peer");
    let peer = id_of(&config);
    let (listener, port) = listening();
    let served = serve_once(listener, |mut connection| {
        let mut request = Vec::new();
        connection.read_to_end(&mut request).expect("a request");
        connection
            .write_all(&[b"far: ", request.as_slice()].concat())
            .expect("a reply");
    });
    // A carries a stream to B and nothing else; B connects to the target.
    a.keys.accept(peer);
    a.grants.set(peer, forward_only(b.address.port()));
    b.keys.accept(peer);
    b.grants.set(peer, forward_only(port));
    let config = config.to_string_lossy().into_owned();
    let (server, jump_server) = (b.id.to_string(), a.id.to_string());
    let mut args = remote_args(&b, &config, &server);
    args.extend([
        "--jump",
        a.endpoint.as_str(),
        "--jump-server",
        jump_server.as_str(),
        "forward",
        "-D",
        "0",
    ]);
    let forward = Spawned::start(bohio(&args));
    let proxy = forward.line("socks5 ");
    let mut client = TcpStream::connect(&proxy).expect("the proxy answers");
    client.set_read_timeout(Some(WAIT)).expect("a timeout");
    client.write_all(&[5, 1, 0]).expect("a greeting");
    let mut method = [0_u8; 2];
    client.read_exact(&mut method).expect("a method");
    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&port.to_be_bytes());
    client.write_all(&request).expect("a request");
    let mut answer = [0_u8; 10];
    client.read_exact(&mut answer).expect("a reply");
    assert_eq!(answer[1], 0, "connected");
    client.write_all(b"hello").expect("written");
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("half-closed");
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).expect("a reply");
    assert_eq!(reply, b"far: hello");
    served.join().expect("the target ended");
}
