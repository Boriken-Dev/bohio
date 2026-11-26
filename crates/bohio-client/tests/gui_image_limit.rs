//! A host that announces an image of any length cannot grow the client past its limit.

use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::relay::Plain;
use bohio_client::{Error, Open};
use bohio_proto::gui::{
    DisplayId, GuiOutput, GuiRequest, ImageFormat, ImageHeader, Ready, Rect, State, Want,
};
use cabuya::mux::LinkStream;
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

/// A link whose one stream the test serves.
struct Fake {
    host: Mutex<Option<Box<dyn Channel>>>,
}

impl Open for Fake {
    fn open(&self, _protocol: &str, _metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let (host, client) = memory::pipe(64 * 1024);
        let host = channel(host);
        let ready = GuiOutput::Ready(Ready {
            session: "default".to_owned(),
            granted: vec![Want::View],
            state: State::default(),
        });
        host.send(&ready.encode().expect("ready")).expect("sent");
        *self.host.lock().unwrap() = Some(Box::new(host));
        Ok(Box::new(Plain(channel(client))))
    }
}

/// A host that answers the first request with an image header of `length` bytes and then sends `pieces` frames of data.
fn snapshot_of(length: u64, pieces: usize, limit: Option<u64>) -> (Result<(), Error>, Duration) {
    let link = Fake {
        host: Mutex::new(None),
    };
    let gui = Gui::open(&link, &GuiRequest::new([Want::View])).expect("opens");
    if let Some(limit) = limit {
        gui.set_image_limit(limit);
    }
    let host = link.host.lock().unwrap().take().expect("the host end");
    thread::scope(|scope| {
        scope.spawn(|| {
            let Ok(Some(_request)) = host.receive(Some(Duration::from_secs(10))) else {
                return;
            };
            let header = GuiOutput::Image(ImageHeader {
                seq: 1,
                display: DisplayId("0".to_owned()),
                format: ImageFormat::Png,
                width: 10,
                height: 10,
                region: Rect::new(0, 0, 10, 10),
                scale: 1.0,
                cursor: None,
                length,
                settled: None,
            });
            let _ = host.send(&header.encode().expect("header"));
            for _ in 0..pieces {
                if host
                    .send(&GuiOutput::Data(vec![0; 16 * 1024]).encode().expect("data"))
                    .is_err()
                {
                    return;
                }
            }
        });
        let started = std::time::Instant::now();
        let result = gui
            .snapshot(&SnapshotOptions::default(), Some(Duration::from_secs(2)))
            .map(|_| ());
        let waited = started.elapsed();
        // The host may be waiting to send into a stream nobody reads; ending it lets the thread finish.
        host.close();
        (result, waited)
    })
}

#[test]
fn an_image_announced_past_the_limit_is_refused_at_its_header() {
    let (result, waited) = snapshot_of(1 << 40, 8, None);
    let error = result.expect_err("a terabyte is not read");
    assert!(
        matches!(&error, Error::Screen(text) if text.contains("image")),
        "{error:?}"
    );
    assert!(waited < Duration::from_secs(2), "it waited for the data");
}

#[test]
fn raising_the_limit_lets_the_same_announcement_be_read() {
    // The control: the header is the same and the limit is not, so the client starts to read and waits for the rest.
    let (result, _) = snapshot_of(1 << 40, 8, Some(u64::MAX));
    let error = result.expect_err("the data never completes");
    assert!(matches!(error, Error::Timeout), "{error:?}");
}
