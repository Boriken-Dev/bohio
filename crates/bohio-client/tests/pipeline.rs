//! Pipelined control calls against a host that answers in order and sometimes late.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bohio_client::control::Control;
use bohio_client::relay::Plain;
use bohio_client::{Error, Open};
use bohio_proto::control::{Call, Reply, Request, Response};
use bohio_proto::{decode, encode};
use coqui_link::mux::LinkStream;
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{Channel, StreamChannel, StreamChannelBuilder, memory};

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

/// A link whose one stream is served by a closure on a thread.
struct Fake {
    host: std::sync::Mutex<Option<Box<dyn Channel>>>,
}

impl Open for Fake {
    fn open(&self, _protocol: &str, _metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let (host, client) = memory::pipe(64 * 1024);
        *self.host.lock().unwrap() = Some(Box::new(channel(host)));
        Ok(Box::new(Plain(channel(client))))
    }
}

/// Answers every request in order; the one whose position is `late` is held for `delay` first. A `which` call is answered
/// with its own names, so a reply that reaches the wrong call is seen.
fn serve(host: &dyn Channel, late: usize, delay: Duration, answered: &AtomicUsize) {
    let mut position = 0;
    while let Ok(Some(frame)) = host.receive(Some(Duration::from_secs(20))) {
        let request: Request = decode(&frame).unwrap();
        if position == late {
            thread::sleep(delay);
        }
        position += 1;
        let reply = match request.call {
            Call::Which { names } => Reply::Which {
                found: names
                    .into_iter()
                    .map(|name| (name.clone(), Some(name)))
                    .collect::<BTreeMap<_, _>>(),
            },
            _ => Reply::Done,
        };
        if host
            .send(&encode(&Response::ok(request.id, reply)).unwrap())
            .is_err()
        {
            return;
        }
        answered.fetch_add(1, Ordering::SeqCst);
    }
}

fn which(name: &str) -> Call {
    Call::Which {
        names: vec![name.to_owned()],
    }
}

fn echoed(answer: &Result<Reply, Error>, name: &str) -> bool {
    matches!(answer, Ok(Reply::Which { found }) if found.get(name) == Some(&Some(name.to_owned())))
}

#[test]
fn every_answer_reaches_its_own_call() {
    let link = Fake {
        host: std::sync::Mutex::new(None),
    };
    let control = Control::open(&link).unwrap();
    let host = link.host.lock().unwrap().take().unwrap();
    let answered = AtomicUsize::new(0);
    thread::scope(|scope| {
        scope.spawn(|| serve(&*host, usize::MAX, Duration::ZERO, &answered));
        let names: Vec<String> = (0..1000).map(|index| format!("n{index}")).collect();
        let answers = control.call_many(
            names.iter().map(|name| which(name)).collect(),
            Some(Duration::from_secs(10)),
        );
        assert_eq!(answers.len(), 1000);
        for (name, answer) in names.iter().zip(&answers) {
            assert!(echoed(answer, name), "{name}: {answer:?}");
        }
        // Submitting first and collecting out of order keeps each answer for its call.
        let ids: Vec<u64> = (0..5)
            .map(|index| control.submit(which(&format!("m{index}")), None).unwrap())
            .collect();
        for index in [3_usize, 0, 4, 1, 2] {
            let answer = control.collect(ids[index], Some(Duration::from_secs(10)));
            assert!(echoed(&answer, &format!("m{index}")), "{answer:?}");
        }
        // An id collected once has nothing left to collect.
        assert!(
            control
                .collect(ids[0], Some(Duration::from_millis(50)))
                .is_err()
        );
        control.close();
    });
}

#[test]
fn a_call_that_times_out_mid_batch_leaves_the_stream_and_the_other_answers_intact() {
    let link = Fake {
        host: std::sync::Mutex::new(None),
    };
    let control = Control::open(&link).unwrap();
    let host = link.host.lock().unwrap().take().unwrap();
    let answered = AtomicUsize::new(0);
    thread::scope(|scope| {
        // The sixth request is held for 700 ms; the timeout is 150 ms.
        scope.spawn(|| serve(&*host, 5, Duration::from_millis(700), &answered));
        let names: Vec<String> = (0..10).map(|index| format!("n{index}")).collect();
        let started = Instant::now();
        let answers = control.call_many(
            names.iter().map(|name| which(name)).collect(),
            Some(Duration::from_millis(150)),
        );
        for index in 0..5 {
            assert!(
                echoed(&answers[index], &names[index]),
                "{index}: {:?}",
                answers[index]
            );
        }
        assert!(
            matches!(answers[5], Err(Error::Timeout)),
            "{:?}",
            answers[5]
        );
        // A call the host answered behind the late one is either its own answer or a timeout, never another call's.
        for index in 6..10 {
            assert!(
                echoed(&answers[index], &names[index])
                    || matches!(answers[index], Err(Error::Timeout)),
                "{index}: {:?}",
                answers[index]
            );
        }
        assert!(started.elapsed() < Duration::from_secs(5));
        // Once the host catches up the stream answers the next call with its own answer, not a late one.
        let answer = control.call(which("after"), Some(Duration::from_secs(10)));
        assert!(echoed(&answer, "after"), "{answer:?}");
        control.close();
    });
}
