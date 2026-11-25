//! A stream's receive window: `open_with` grants the difference on the new stream, and a plain open does not.
//!
//! Two muxes over memory pipes; the side that opens reads nothing, so what the other side can send before it
//! stalls is the credit the opener holds out, which is what the grant raises.

#![forbid(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::OpenOptions;
use bohio_client::connect::Opener;
use bohio_client::link::{Settings, stream};
use cabuya::link::LINK_PROTOCOL;
use cabuya::mux::{CloseReason, Mux, Side};
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

/// What `Opens::run` logged on a link whose peer announced `window` in its `link/1` document, with the opener's own
/// settings the defaults.
fn logged_for_peer_window(window: u32) -> String {
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let captured = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer({
            let captured = captured.clone();
            move || captured.clone()
        })
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let opener = Settings::default();
    let (one, two) = memory::pipe(4 * 1024 * 1024);
    let dial_end = stream(one, &opener).expect("a channel");
    let answer_end = stream(two, &opener).expect("a channel");
    for channel in [&dial_end, &answer_end] {
        channel.set_max_message(opener.max_frame.saturating_add(5));
        let _ = channel.set_pending(opener.read_ahead);
    }
    let done = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            tracing::subscriber::with_default(subscriber, || {
                Mux::run(&dial_end, Side::Dialed, opener.limits(), None, |mux| {
                    let (open, opens) = Opener::pair(&opener);
                    thread::scope(|inner| {
                        inner.spawn(|| {
                            while !done.load(Ordering::SeqCst) {
                                thread::sleep(Duration::from_millis(20));
                            }
                            open.close();
                        });
                        opens.run(mux, &opener);
                    });
                });
            });
            dial_end.close();
        });
        scope.spawn(|| {
            Mux::run(
                &answer_end,
                Side::Answered,
                opener.limits(),
                None,
                |mux| {
                    let _finished = SetOnDrop(&done);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < deadline {
                        let Ok(Some(incoming)) = mux.accept(Some(Duration::from_millis(50)))
                        else {
                            continue;
                        };
                        let document = format!(
                            "{{\"version\":1,\"instance\":\"x\",\"offers\":[],\"window\":{window},\"max_frame\":{}}}",
                            opener.max_frame
                        );
                        incoming.stream.send(document.as_bytes()).unwrap();
                        let _ = incoming.stream.finish();
                        // The opener reads it between its polls.
                        thread::sleep(Duration::from_millis(500));
                        break;
                    }
                },
            );
            answer_end.close();
        });
    });
    let bytes = captured.0.lock().unwrap().clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[test]
fn a_peer_window_that_differs_from_ours_is_logged_once_and_an_equal_one_says_nothing() {
    let larger = logged_for_peer_window(4 * MIB);
    assert_eq!(
        larger.matches("window is larger than ours").count(),
        1,
        "{larger}"
    );
    let smaller = logged_for_peer_window(65536);
    assert_eq!(
        smaller.matches("window is smaller than ours").count(),
        1,
        "{smaller}"
    );
    let equal = logged_for_peer_window(MIB);
    assert!(equal.is_empty(), "an equal window was reported: {equal}");
}
