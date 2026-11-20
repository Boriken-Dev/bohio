//! The raw TCP control: the same number of bytes over a plain socket, with no proof, no seal and no
//! mux, so a result says what the path can carry and what Bohío adds to it.
//!
//! A client sends one byte (`D` the server sends, `U` the server reads) and the size as 8 big-endian
//! bytes. For `D` the server sends that many bytes and the client reads them; for `U` the client sends
//! them and the server answers one byte once it has read them all. A sample includes the connect.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use crate::corpus::Xorshift;

/// Serves raw transfers on `listen` until the process ends. Prints `raw listening <address>` once bound.
///
/// # Errors
///
/// The error of binding the address.
pub fn serve(listen: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    println!("raw listening {}", listener.local_addr()?);
    std::io::stdout().flush()?;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        thread::spawn(move || {
            let _ = handle(stream);
        });
    }
    Ok(())
}

fn block() -> Vec<u8> {
    let mut bytes = vec![0_u8; 1 << 20];
    Xorshift::fixed().fill(&mut bytes);
    bytes
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut head = [0_u8; 9];
    stream.read_exact(&mut head)?;
    let size = u64::from_be_bytes(head[1..].try_into().unwrap_or([0; 8]));
    let block = block();
    match head[0] {
        b'D' => {
            let mut left = size;
            while left > 0 {
                let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(block.len());
                stream.write_all(&block[..take])?;
                left -= take as u64;
            }
            stream.flush()?;
            // Wait for the client to be done, so no byte is lost to a reset.
            let mut end = [0_u8; 1];
            let _ = stream.read(&mut end);
        }
        b'U' => {
            let mut left = size;
            let mut sink = vec![0_u8; 1 << 16];
            while left > 0 {
                let take = usize::try_from(left.min(sink.len() as u64)).unwrap_or(sink.len());
                let count = stream.read(&mut sink[..take])?;
                if count == 0 {
                    return Ok(());
                }
                left -= count as u64;
            }
            stream.write_all(&[1])?;
        }
        _ => {}
    }
    Ok(())
}

/// Connects to `address` within `timeout` and returns the stream, with `TCP_NODELAY` as Bohío sets it.
///
/// # Errors
///
/// The error of connecting.
pub fn connect(address: SocketAddr, timeout: Duration) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

/// Downloads `size` bytes.
///
/// # Errors
///
/// The error of connecting or reading, or `UnexpectedEof` when the server sent less.
pub fn download(address: SocketAddr, size: u64, timeout: Duration) -> std::io::Result<()> {
    let mut stream = connect(address, timeout)?;
    let mut head = vec![b'D'];
    head.extend_from_slice(&size.to_be_bytes());
    stream.write_all(&head)?;
    let mut left = size;
    let mut sink = vec![0_u8; 1 << 16];
    while left > 0 {
        let count = stream.read(&mut sink)?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        left = left.saturating_sub(count as u64);
    }
    let _ = stream.write_all(&[1]);
    Ok(())
}

/// Uploads `size` bytes and waits for the server to say it has them all.
///
/// # Errors
///
/// The error of connecting, writing or waiting for the answer.
pub fn upload(address: SocketAddr, size: u64, timeout: Duration) -> std::io::Result<()> {
    let mut stream = connect(address, timeout)?;
    let mut head = vec![b'U'];
    head.extend_from_slice(&size.to_be_bytes());
    stream.write_all(&head)?;
    let block = block();
    let mut left = size;
    while left > 0 {
        let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(block.len());
        stream.write_all(&block[..take])?;
        left -= take as u64;
    }
    stream.flush()?;
    let mut answer = [0_u8; 1];
    stream.read_exact(&mut answer)
}
