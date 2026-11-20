//! The one place the scenarios touch the client library. A change to one of these calls is made
//! here, so every scenario keeps compiling at every commit it is measured on.

use std::time::Duration;

use bohio_client::Open;
use bohio_client::connect::Connection;
use bohio_client::control::Control;
use bohio_client::dial::{Endpoint, ServerTrust};
use bohio_client::exec::Exec;
use bohio_client::fs::{Fs, Reader, Writer};
use bohio_client::hash::hash_file;
#[cfg(feature = "hash")]
use bohio_client::hash::hash_many;
use bohio_client::link::Settings;
use bohio_proto::exec::Request;
use bohio_proto::fs::{ReadRequest, WriteRequest};
use bohio_proto::hash::Algorithm;
#[cfg(feature = "hash")]
use bohio_proto::hash::HashItem;
use coqui_link::auth::Proof;

/// How long one call may take before a sample fails.
pub const LIMIT: Duration = Duration::from_secs(600);

/// Opens the path and proves over it in one call.
pub fn connect(
    endpoint: &Endpoint,
    proof: &Proof,
    settings: &Settings,
) -> Result<Connection, String> {
    Connection::connect_with(
        endpoint,
        settings.proof_timeout,
        proof,
        settings,
        &ServerTrust::default(),
    )
    .map_err(|error| error.to_string())
}

/// Opens the path only (the transport up), returning the unproven channel.
pub fn dial(
    endpoint: &Endpoint,
    settings: &Settings,
) -> Result<Box<dyn coqui_transport::Channel>, String> {
    endpoint
        .dial_with(settings.proof_timeout, settings, &ServerTrust::default())
        .map_err(|error| error.to_string())
}

/// Proves over a dialed channel and starts the link.
pub fn prove(
    channel: Box<dyn coqui_transport::Channel>,
    proof: &Proof,
    settings: &Settings,
) -> Result<Connection, String> {
    Connection::over(channel, proof, settings).map_err(|error| error.to_string())
}

/// A control stream and one `info` call on it.
pub fn info(link: &dyn Open) -> Result<(), String> {
    let control = Control::open(link).map_err(|error| error.to_string())?;
    control
        .info(Some(LIMIT))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// A control stream opened and closed.
pub fn open_close(link: &dyn Open) -> Result<(), String> {
    let control = Control::open(link).map_err(|error| error.to_string())?;
    control.close();
    Ok(())
}

/// A filesystem handle (one control stream).
pub fn fs(link: &dyn Open) -> Result<Fs, String> {
    Fs::connect(link)
        .map(|fs| fs.with_timeout(Some(LIMIT)))
        .map_err(|error| error.to_string())
}

/// One `stat`.
pub fn stat(fs: &Fs, path: &str) -> Result<(), String> {
    fs.stat(path, true)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The whole of `path` read and thrown away; the bytes read.
pub fn read(link: &dyn Open, path: &str) -> Result<u64, String> {
    let reader = Reader::start(link, &ReadRequest::whole(path), Some(LIMIT))
        .map_err(|error| error.to_string())?;
    reader
        .copy_to(&mut std::io::sink(), Some(LIMIT))
        .map_err(|error| error.to_string())
}

/// A read that reports each chunk's size and the header's arrival to `on`.
pub fn read_observed(
    link: &dyn Open,
    path: &str,
    mut on_header: impl FnMut(),
    mut on_chunk: impl FnMut(usize),
) -> Result<u64, String> {
    let reader = Reader::start(link, &ReadRequest::whole(path), Some(LIMIT))
        .map_err(|error| error.to_string())?;
    on_header();
    let mut total = 0_u64;
    while let Some(chunk) = reader
        .next_chunk(Some(LIMIT))
        .map_err(|error| error.to_string())?
    {
        total += chunk.len() as u64;
        on_chunk(chunk.len());
    }
    Ok(total)
}

/// `size` bytes of `block` written to `path` as a replace, creating the directories above it.
pub fn write(link: &dyn Open, path: &str, size: u64, block: &[u8]) -> Result<(), String> {
    let mut request = WriteRequest::replace(path);
    request.parents = true;
    let writer = Writer::start(link, &request, Some(LIMIT)).map_err(|error| error.to_string())?;
    let mut left = size;
    while left > 0 {
        let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(block.len());
        writer
            .write(&block[..take])
            .map_err(|error| error.to_string())?;
        left -= take as u64;
    }
    writer
        .finish(Some(LIMIT))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Removes `path` on the host.
pub fn remove(fs: &Fs, path: &str) {
    let _ = fs.remove(path, false);
}

/// Runs `argv` on the host and returns how many bytes of standard output it wrote.
pub fn exec(link: &dyn Open, argv: &[String]) -> Result<u64, String> {
    let request = Request::argv(argv.iter().cloned());
    let run = Exec::start(link, &request).map_err(|error| error.to_string())?;
    let done = run.wait(Some(LIMIT)).map_err(|error| error.to_string())?;
    Ok(done.stdout.len() as u64)
}

/// The host's SHA-256 of `path`, over a `bohio-hash/1` stream.
pub fn checksum(link: &dyn Open, path: &str) -> Result<(), String> {
    hash_file(link, path, &[Algorithm::Sha256])
        .map(drop)
        .map_err(|error| error.to_string())
}

/// The host's digest of `path` by `algorithm`.
#[cfg(feature = "hash")]
pub fn hash(link: &dyn Open, path: &str, algorithm: &Algorithm) -> Result<(), String> {
    hash_file(link, path, std::slice::from_ref(algorithm))
        .map(drop)
        .map_err(|error| error.to_string())
}

/// The host's SHA-256 of each of `paths`, on one stream.
#[cfg(feature = "hash")]
pub fn hash_batch(link: &dyn Open, paths: &[String]) -> Result<(), String> {
    let items: Vec<_> = paths.iter().map(HashItem::whole).collect();
    let answers =
        hash_many(link, &items, &[Algorithm::Sha256]).map_err(|error| error.to_string())?;
    match answers.iter().find_map(|answer| answer.as_ref().err()) {
        Some(body) => Err(body.to_string()),
        None => Ok(()),
    }
}
