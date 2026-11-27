//! What a client holds of a host that lies: its decoders and block signatures stop at the bounds the caller sets.

#![cfg(feature = "zstd")]

use std::thread;
use std::time::Duration;

use bohio_client::fs::{ReadOptions, Reader};
use bohio_client::hash::hash_blocks_within;
use bohio_client::relay::Plain;
use bohio_client::walk::{WalkOptions, walk_with};
use bohio_client::{Error, Open};
use bohio_proto::compress::{Codec, Compression};
use bohio_proto::fs::{ReadDone, ReadHeader, ReadOutput, ReadRequest};
use bohio_proto::hash::{Algorithm, BlockOptions, HashBlocks, HashItem, HashOutput};
use bohio_proto::walk::{WalkHeader, WalkOutput};
use bohio_transfer::compress::{Caps, Encoder, Piece};
use cabuya::mux::LinkStream;
use coqui_transport::frame::LengthPrefix;
use coqui_transport::{Channel, StreamChannel, StreamChannelBuilder, memory};

const WAIT: Option<Duration> = Some(Duration::from_secs(20));

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

/// A link whose every stream is served by one closure on a thread: the host.
struct Host<F>(F);

impl<F: Fn(StreamChannel) + Clone + Send + Sync + 'static> Open for Host<F> {
    fn open(&self, _protocol: &str, _metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        let (host, client) = memory::pipe(64 * 1024);
        let serve = self.0.clone();
        thread::spawn(move || serve(channel(host)));
        Ok(Box::new(Plain(channel(client))))
    }
}

/// One zstd piece that decodes to `mib` mebibytes of zeros: a few dozen bytes.
fn zeros(mib: usize) -> Vec<u8> {
    let mut encoder = Encoder::new(&Compression::new(Codec::Zstd), &Caps::default()).unwrap();
    match encoder.frame(&vec![0_u8; mib << 20]).unwrap() {
        Piece::Compressed(piece) => piece,
        Piece::Raw => panic!("zeros compress"),
    }
}

#[test]
fn a_read_whose_output_far_outgrows_what_was_sent_is_refused_and_the_caller_can_allow_it() {
    let link = Host(|host: StreamChannel| {
        let mut header = ReadHeader::raw(u64::MAX / 2, u64::MAX / 2);
        header.compression = Some(Compression::new(Codec::Zstd));
        let _ = host.send(&ReadOutput::Header(header).encode().unwrap());
        let _ = host.send(&ReadOutput::Compressed(zeros(16)).encode().unwrap());
        let done = ReadDone {
            hashes: Default::default(),
            index: None,
        };
        let _ = host.send(&ReadOutput::Done(done).encode().unwrap());
        let _ = host.receive(Some(Duration::from_secs(5)));
    });
    let request = ReadRequest::whole("f");
    let reader = Reader::start_with(&link, &request, WAIT, ReadOptions::default()).unwrap();
    let outcome = reader.copy_to(&mut std::io::sink(), WAIT);
    assert!(
        matches!(outcome, Err(Error::Compression(_))),
        "the default bound must refuse 16 MiB from a few dozen bytes: {outcome:?}"
    );

    let options = ReadOptions {
        max_expansion: Some(0),
        ..ReadOptions::default()
    };
    let reader = Reader::start_with(&link, &request, WAIT, options).unwrap();
    let mut sink = Vec::new();
    // The check is off: whatever else the stream lacks, the decoder does not refuse the growth.
    let outcome = reader.copy_to(&mut sink, WAIT);
    assert!(
        !matches!(outcome, Err(Error::Compression(_))),
        "{outcome:?}"
    );
    assert_eq!(sink.len(), 16 << 20);
}

#[test]
fn a_walk_whose_entries_far_outgrow_what_was_sent_is_refused() {
    let link = Host(|host: StreamChannel| {
        let header = WalkHeader {
            compression: Some(Compression::new(Codec::Zstd)),
        };
        let _ = host.send(&WalkOutput::Header(header).encode().unwrap());
        let _ = host.send(&WalkOutput::Compressed(zeros(16)).encode().unwrap());
        let _ = host.receive(Some(Duration::from_secs(5)));
    });
    let mut items = walk_with(&link, &["."], &WalkOptions::default(), WAIT).unwrap();
    let first = items.next().expect("an item or an error");
    assert!(matches!(first, Err(Error::Compression(_))), "{first:?}");
}

#[test]
fn a_block_signature_past_the_callers_bound_is_refused() {
    let link = Host(|host: StreamChannel| {
        let blocks = HashBlocks {
            index: 0,
            first: 0,
            size: 4096,
            weak: vec![0; 3000],
            strong: vec![0; 3000 * 8],
        };
        let _ = host.send(&HashOutput::Blocks(blocks).encode().unwrap());
        let _ = host.receive(Some(Duration::from_secs(5)));
    });
    let outcome = hash_blocks_within(
        &link,
        HashItem::whole("f"),
        &[Algorithm::Blake3],
        &BlockOptions::default(),
        1000,
        WAIT,
    );
    assert!(matches!(outcome, Err(Error::Local(_))), "{outcome:?}");
}
