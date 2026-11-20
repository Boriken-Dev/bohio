//! A stream's receive window: `open_with` grants the difference on the new stream, and a plain open does not.
//!
//! Two muxes over memory pipes; the side that opens reads nothing, so what the other side can send before it
//! stalls is the credit the opener holds out, which is what the grant raises.

use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::OpenOptions;
use bohio_client::connect::Opener;
use bohio_client::link::{Settings, stream};
use coqui_link::link::LINK_PROTOCOL;
use coqui_link::mux::{CloseReason, Mux, Side};
use coqui_transport::{Channel, Failure, memory};

const MIB: u32 = 1024 * 1024;

/// Sets a flag when dropped, so a panic on one side does not leave the other waiting for it.
struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn settings(bulk_window: u32, max_window: u32) -> Settings {
    let mut settings = Settings::default();
    settings.bulk_window = bulk_window;
    settings.max_window = max_window;
    settings
}

/// How many bytes the answering side could send on a stream the opening side widened to `asked` (when it did)
/// before the link made it wait, with the opener's reader taking nothing.
fn credit_held_out(opener: &Settings, asked: Option<u32>) -> u64 {
    let answerer = Settings::default();
    let (one, two) = memory::pipe(4 * 1024 * 1024);
    let dial_end = stream(one, opener).expect("a channel");
    let answer_end = stream(two, &answerer).expect("a channel");
    // No proof ran here, which is what raises a channel past one proof-sized message.
    for (channel, settings) in [(&dial_end, opener), (&answer_end, &answerer)] {
        channel.set_max_message(settings.max_frame.saturating_add(5));
        let _ = channel.set_pending(settings.read_ahead);
    }
    let done = AtomicBool::new(false);
    let mut sent = 0_u64;
    thread::scope(|scope| {
        scope.spawn(|| {
            Mux::run(&dial_end, Side::Dialed, opener.limits(), None, |mux| {
                let (open, opens) = Opener::pair(opener);
                thread::scope(|inner| {
                    inner.spawn(|| {
                        let options = asked.map_or_else(OpenOptions::default, |bytes| {
                            OpenOptions::receive_window(bytes)
                        });
                        let _held = open
                            .open_stream_with("bohio-test/1", &[], &options)
                            .expect("the stream opens");
                        while !done.load(Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(20));
                        }
                        open.close();
                    });
                    opens.run(mux, opener);
                });
            });
            dial_end.close();
        });
        scope.spawn(|| {
            Mux::run(
                &answer_end,
                Side::Answered,
                answerer.limits(),
                None,
                |mux| {
                    let _finished = SetOnDrop(&done);
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while Instant::now() < deadline {
                        let Ok(Some(incoming)) = mux.accept(Some(Duration::from_millis(50))) else {
                            continue;
                        };
                        if incoming.protocol == LINK_PROTOCOL {
                            // This side answers no document: the opener only logs a difference it finds.
                            incoming
                                .stream
                                .close_with(CloseReason::Refused, "not answered here");
                            continue;
                        }
                        // The grant follows the open: give it time to arrive before counting.
                        thread::sleep(Duration::from_millis(400));
                        let message = vec![0_u8; 64 * 1024];
                        let mut quiet_since = Instant::now();
                        while quiet_since.elapsed() < Duration::from_millis(600) {
                            match incoming.stream.try_send(&message) {
                                Ok(()) => {
                                    sent += message.len() as u64;
                                    quiet_since = Instant::now();
                                }
                                Err(Failure::NoProgress) => {
                                    thread::sleep(Duration::from_millis(10));
                                }
                                Err(other) => panic!("the stream failed: {other:?}"),
                            }
                        }
                        break;
                    }
                },
            );
            answer_end.close();
        });
    });
    sent
}

#[test]
fn open_with_a_receive_window_lets_the_peer_send_that_much_before_the_reader_takes_any() {
    let wide = settings(8 * MIB, 8 * MIB);
    let widened = credit_held_out(&wide, Some(8 * MIB));
    assert!(
        widened >= u64::from(7 * MIB),
        "a stream opened for 8 MiB took only {widened} bytes ahead of its reader"
    );
    assert!(
        widened <= u64::from(9 * MIB),
        "a stream opened for 8 MiB took {widened} bytes ahead of its reader"
    );
}

#[test]
fn a_plain_open_keeps_the_links_own_window_and_a_request_past_max_window_is_capped() {
    let plain = credit_held_out(&Settings::default(), None);
    assert!(
        plain <= u64::from(2 * MIB),
        "a stream opened without options took {plain} bytes ahead of its reader"
    );
    // The control: the same request on a link whose `max_window` stays at `window` is capped there.
    let capped = credit_held_out(&settings(MIB, MIB), Some(8 * MIB));
    assert!(
        capped <= u64::from(2 * MIB),
        "a request past max_window took {capped} bytes ahead of its reader"
    );
}

#[test]
fn bulk_window_is_what_a_read_asks_for_only_when_it_exceeds_the_window() {
    use bohio_client::Open;
    let (wide, _) = Opener::pair(&settings(8 * MIB, 8 * MIB));
    assert_eq!(wide.bulk_window(), Some(8 * MIB));
    let (plain, _) = Opener::pair(&Settings::default());
    assert_eq!(plain.bulk_window(), None);
}
