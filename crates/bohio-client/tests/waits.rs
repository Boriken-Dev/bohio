//! A host that accepts a stream and then says nothing does not hold a call forever.

use std::thread;
use std::time::{Duration, Instant};

use bohio_client::exec::Exec;
use bohio_client::hash::{hash_blocks_within, hash_items};
use bohio_client::relay::Plain;
use bohio_client::shell::Shell;
use bohio_client::{Error, Open};
use bohio_proto::hash::{Algorithm, BlockOptions, HashItem, HashRequest};
use cabuya::mux::LinkStream;
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{Channel, StreamChannel, StreamChannelBuilder, memory};

const SHORT: Option<Duration> = Some(Duration::from_millis(200));

fn channel(stream: memory::Stream) -> StreamChannel {
    StreamChannelBuilder::new()
        .codec(LengthPrefix::u32_be())
        .stream(stream)
        .pending(4)
        .queued_bytes(1024 * 1024)
        .send_limit(Duration::from_secs(8))
        .linger(Duration::from_millis(50))
        .build()
        .expect("the channel starts")
}

/// A link whose every stream is held open by a host that never sends.
struct Silent;

impl Open for Silent {
    fn open(&self, _protocol: &str, _metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let (host, client) = memory::pipe(64 * 1024);
        thread::spawn(move || {
            let host = channel(host);
            let until = Instant::now() + Duration::from_secs(10);
            while Instant::now() < until
                && matches!(
                    host.receive(Some(Duration::from_secs(1))),
                    Ok(_) | Err(coqui_transport::Failure::Timeout)
                )
            {}
        });
        Ok(Box::new(Plain(channel(client))))
    }
}

fn timed_out<T: std::fmt::Debug>(outcome: Result<T, Error>, started: Instant) {
    assert!(matches!(outcome, Err(Error::Timeout)), "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_hash_waits_for_the_bound_the_caller_gives_and_not_longer() {
    let started = Instant::now();
    let request = HashRequest::new([Algorithm::Sha256]);
    timed_out(
        hash_items(&Silent, &request, &[HashItem::whole("f")], SHORT),
        started,
    );
    let started = Instant::now();
    timed_out(
        hash_blocks_within(
            &Silent,
            HashItem::whole("f"),
            &[Algorithm::Blake3],
            &BlockOptions::default(),
            0,
            SHORT,
        ),
        started,
    );
}

#[test]
fn an_attach_waits_for_the_bound_the_caller_gives_and_not_longer() {
    let started = Instant::now();
    timed_out(Exec::attach_within(&Silent, "s", None, SHORT), started);
    let started = Instant::now();
    timed_out(Shell::attach_within(&Silent, "s", None, SHORT), started);
    let started = Instant::now();
    timed_out(
        bohio_client::session::attach_within(&Silent, "s", None, SHORT),
        started,
    );
}

#[test]
fn the_default_wait_is_finite() {
    assert_eq!(bohio_client::DEFAULT_TIMEOUT, Duration::from_secs(300));
}

#[test]
fn collecting_an_id_nothing_is_waiting_for_is_a_usage_error_and_not_a_corrupt_message() {
    let control = bohio_client::control::Control::open(&Silent).unwrap();
    let outcome = control.collect(7, SHORT);
    assert!(
        matches!(outcome, Err(Error::NothingOutstanding)),
        "{outcome:?}"
    );
}

/// A link whose every stream starts with an `attached` frame for a session of `kind`.
struct Attaches(bohio_proto::session::Kind);

impl Open for Attaches {
    fn open(&self, _protocol: &str, _metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let (host, client) = memory::pipe(64 * 1024);
        let kind = self.0;
        thread::spawn(move || {
            let host = channel(host);
            let frame = bohio_proto::session::Attached {
                kind,
                from: 0,
                lost: 0,
            };
            let _ = host.send(&frame.encode().unwrap());
            let _ = host.receive(Some(Duration::from_secs(2)));
        });
        Ok(Box::new(Plain(channel(client))))
    }
}

#[test]
fn attaching_to_a_session_of_the_other_kind_says_which_kind_it_is() {
    use bohio_proto::session::Kind;
    let outcome = Exec::attach(&Attaches(Kind::Shell), "s", None);
    assert!(
        matches!(outcome, Err(Error::WrongKind { found: Kind::Shell })),
        "{outcome:?}"
    );
    let outcome = Shell::attach(&Attaches(Kind::Exec), "s", None);
    assert!(
        matches!(outcome, Err(Error::WrongKind { found: Kind::Exec })),
        "{outcome:?}"
    );
}
