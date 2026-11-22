//! A started process holds nothing of this process but its standard streams.
//!
//! The standard library's `TcpStream::try_clone` makes a socket that a child started with handle
//! inheritance on keeps, and a connection a child holds is neither closed when its owner drops it
//! (the end never arrives) nor closed cleanly when the child is killed (the peer reads a reset).
//! A stream split into a reader and a writer is cloned this way.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use bohio_proto::exec::Env;

use super::{Spec, spawn};

/// A program that runs for a while and reads nothing, started the way `exec` starts one.
fn sleeper() -> Spec {
    Spec {
        program: OsString::from("cmd"),
        args: Vec::new(),
        raw_args: Some(OsString::from("/D /C ping -n 60 127.0.0.1 >NUL")),
        cwd: None,
        env: Env::default(),
        stdin: false,
        on_host_death: crate::config::HostDeath::default(),
    }
}

/// A connected pair: the client end, and the server end with a clone of it, as a split stream has.
fn connected() -> (TcpStream, TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a listener");
    let client = TcpStream::connect(listener.local_addr().expect("an address")).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    let clone = server.try_clone().expect("a clone");
    (client, server, clone)
}

#[test]
fn a_child_does_not_keep_a_connection_open_by_holding_a_clone_of_its_socket() {
    let (mut client, server, clone) = connected();
    let child = spawn(sleeper()).expect("the child starts");
    drop(server);
    drop(clone);
    client.set_nonblocking(true).expect("non-blocking");
    let until = Instant::now() + Duration::from_secs(10);
    let mut byte = [0_u8; 1];
    let ended = loop {
        match client.read(&mut byte) {
            Ok(0) => break true,
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("the end of the connection was a failure: {error}"),
        }
        if Instant::now() >= until {
            break false;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let _ = child.control.kill();
    assert!(
        ended,
        "the owner dropped the connection and the peer never read its end: the child holds it"
    );
}

#[test]
fn killing_children_does_not_reset_the_connections_open_beside_them() {
    // Each transfer ends with the server dropping its socket and a clone of it, as a host does with
    // a stream it served, while another thread starts children and ends them.
    let stop = std::sync::atomic::AtomicBool::new(false);
    let failures = thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok(child) = spawn(sleeper()) {
                    let _ = child.control.kill();
                }
            }
        });
        let workers: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| (0..15).filter_map(|_| transfer().err()).collect::<Vec<_>>()))
            .collect();
        let failures: Vec<String> = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a worker"))
            .collect();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        failures
    });
    assert!(failures.is_empty(), "transfers failed: {failures:?}");
}

/// 1 MiB each way over loopback; the server reads it all, answers, and drops its sockets.
fn transfer() -> Result<(), String> {
    let (mut client, mut server, clone) = connected();
    let data = vec![7_u8; 1 << 20];
    let echo = data.clone();
    let serving = thread::spawn(move || {
        let mut buffer = vec![0_u8; 1 << 16];
        let mut seen = 0;
        while seen < echo.len() {
            match server.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => seen += read,
            }
        }
        let _ = server.write_all(&echo);
        drop(clone);
    });
    let mut writer = client.try_clone().map_err(|error| error.to_string())?;
    let sending = thread::spawn(move || writer.write_all(&data));
    let mut all = Vec::new();
    let read = client.read_to_end(&mut all);
    let _ = sending.join();
    let _ = serving.join();
    match read {
        Ok(_) if all.len() == 1 << 20 => Ok(()),
        Ok(_) => Err(format!("short: {} bytes", all.len())),
        Err(error) => Err(error.to_string()),
    }
}
