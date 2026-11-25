//! What a stream still holds when its link ends: the rest of what it received and why it ended.

use std::thread;
use std::time::{Duration, Instant};

use bohio_client::connect::Opener;
use bohio_client::link::{self, Settings};
use cabuya::mux::{CloseReason, Mux, Side, StreamEnd};
use coqui_transport::{Channel, Failure, memory};

const WAIT: Option<Duration> = Some(Duration::from_secs(10));

/// A reader that has not taken what a stream delivered is still given all of it, and the stream's own end, after the link has
/// ended: a revoke is a revoke, not an orderly end part way through a transfer.
#[test]
fn a_stream_ended_by_a_revoke_delivers_what_it_held_and_says_revoked_after_its_link_ended() {
    let mut settings = Settings::default();
    // A relay holds one message at a time, so the reader below is behind when the link ends.
    settings.queue_bytes = 1;
    let (one, two) = memory::pipe(1024 * 1024);
    let near = link::stream(one, &settings).expect("a channel");
    let far = link::stream(two, &settings).expect("a channel");
    let (opener, opens) = Opener::pair(&settings);
    thread::scope(|scope| {
        scope.spawn(|| {
            Mux::run(&near, Side::Dialed, settings.limits(), None, |mux| {
                opens.run(mux, &settings);
            });
        });
        scope.spawn(|| {
            Mux::run(&far, Side::Answered, settings.limits(), None, |mux| {
                // The link's own document stream comes first, then the two this test opened.
                let mut kept = Vec::new();
                for _ in 0..3 {
                    kept.push(mux.accept(WAIT).unwrap().unwrap());
                }
                let mut take = |protocol: &str| {
                    let at = kept.iter().position(|open| open.protocol == protocol);
                    kept.remove(at.expect("it was opened")).stream
                };
                let (bulk, marker) = (take("bulk"), take("marker"));
                for number in 0..3_u8 {
                    bulk.send(&[number; 16]).unwrap();
                }
                bulk.close_with(CloseReason::Revoked, "this peer may no longer use that");
                // The link keeps frames in order, so the marker arrives after the revoke, and the answer says it was read.
                marker.send(b"revoked").unwrap();
                assert_eq!(marker.receive(WAIT).unwrap().unwrap(), b"read");
            });
            // The link ends with the first stream's reader still behind.
            far.close();
        });
        let bulk = opener.open_stream("bulk", &[]).expect("the stream opens");
        let marker = opener.open_stream("marker", &[]).expect("the stream opens");
        assert_eq!(marker.receive(WAIT).unwrap().unwrap(), b"revoked");
        marker.send(b"read").unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while opener.is_up() {
            assert!(Instant::now() < until, "the link ended");
            thread::sleep(Duration::from_millis(10));
        }
        for number in 0..3_u8 {
            assert_eq!(bulk.receive(WAIT), Ok(Some(vec![number; 16])));
        }
        assert_eq!(bulk.receive(WAIT), Err(Failure::Permission));
        assert!(matches!(bulk.end(), Some(StreamEnd::Revoked(_))));
    });
}
