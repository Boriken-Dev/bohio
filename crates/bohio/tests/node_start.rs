//! What a node does once, when it begins to serve, and what ends it.

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bohio::config::{FilesystemConfig, LinkConfig, ListenConfig, NodeConfig, Span};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use cabuya::auth::{Identity, Method};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn node(config: NodeConfig) -> Node {
    Node::new(
        config,
        Identity::generate(Method::DEFAULT).expect("an identity"),
        Arc::new(MemoryKeys::new()),
        Arc::new(MemoryGrants::new()),
    )
    .expect("the node is built")
}

#[test]
fn the_settings_a_node_logs_are_logged_once_however_often_it_is_rebuilt() {
    let log = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer({
            let log = log.clone();
            move || log.clone()
        })
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let config = NodeConfig {
            filesystem: FilesystemConfig {
                allow_id_bits: true,
                ..FilesystemConfig::default()
            },
            ..NodeConfig::default()
        };
        let node = node(config.clone())
            .with_path(std::ffi::OsString::new())
            .with_sources(bohio::access::Sources::default());
        drop(node);
    });
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    let lines = text.matches("[filesystem] allow_id_bits is true").count();
    assert_eq!(lines, 1, "{text}");
}

#[test]
fn a_node_told_to_stop_does_not_wait_for_a_connection_that_has_not_proven_itself() {
    let config = NodeConfig {
        link: LinkConfig {
            proof_timeout: Span(Duration::from_secs(120)),
            ..LinkConfig::default()
        },
        ..NodeConfig::default()
    };
    let node = Arc::new(node(config.clone()));
    let entry = ListenConfig {
        endpoint: "tcp://127.0.0.1:0".to_owned(),
        ..ListenConfig::default()
    };
    let (listening, bound) = Listening::bind(&entry, &config.link.settings()).unwrap();
    let address = bound.to_string().strip_prefix("tcp://").unwrap().to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    let serving = thread::spawn({
        let (node, stop) = (Arc::clone(&node), Arc::clone(&stop));
        move || node.serve_listening(&listening, &stop)
    });
    // Connects and says nothing: the node holds it while it waits for a proof.
    let silent = TcpStream::connect(&address).unwrap();
    thread::sleep(Duration::from_millis(500));
    let began = Instant::now();
    stop.store(true, Ordering::SeqCst);
    serving.join().unwrap().unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "stopping waited {:?} for a connection",
        began.elapsed()
    );
    drop(silent);
}
