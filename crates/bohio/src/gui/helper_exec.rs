//! A helper that runs on another machine, reached through a process of an exec source (an ssh source's login), and speaks
//! `bohio-gui-helper/1` over that process's standard input and output.
//!
//! The helper is `bohio gui-helper --stdio` on the machine. The account check of the pipe helper does not apply: what
//! authenticates the helper is the source's own login, and what the node trusts is the account that source logs in as.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use bohio_api::Process;
use bohio_api::guatiao::library::Object;
use bohio_api::model::ExecStart;
use coqui_transport::{Channel as _, Duplex};

use crate::node::Node;

/// How long one read of the process waits before it looks at the stop flag again.
const POLL_MS: u64 = 200;

/// A helper to run: the exec source that reaches the machine, and the command that starts the helper there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecHelper {
    /// The exec source's name.
    pub source: String,
    /// The helper's program and arguments, run directly (never through a shell of the node).
    pub command: Vec<String>,
}

impl ExecHelper {
    /// Reads `SOURCE=COMMAND LINE`.
    ///
    /// # Errors
    ///
    /// Why the text is not one.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (source, line) = text
            .split_once('=')
            .ok_or_else(|| format!("`{text}` is not SOURCE=COMMAND"))?;
        let source = source.trim();
        if source.is_empty() {
            return Err(format!("`{text}` names no source"));
        }
        let command = bohio_client::dial::split_command(line)?;
        Ok(Self {
            source: source.to_owned(),
            command,
        })
    }
}

/// The output of a process, as a reader that ends when the process closes its output.
struct Out(Arc<Object<dyn Process>>);

impl Read for Out {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(1, buffer, POLL_MS) {
                Ok(0) => {}
                Ok(count) if count > 0 => {
                    return Ok(usize::try_from(count).unwrap_or(0));
                }
                Ok(_) => return Ok(0),
                Err(error) => return Err(io::Error::other(error.message().to_string())),
            }
        }
    }
}

/// The input of a process; dropping it closes the process's input.
struct In(Arc<Object<dyn Process>>);

impl Write for In {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0
            .write(data)
            .map(|taken| usize::try_from(taken).unwrap_or(data.len()))
            .map_err(|error| io::Error::other(error.message().to_string()))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for In {
    fn drop(&mut self) {
        let _ = self.0.close_input();
    }
}

impl Node {
    /// Serves each of `helpers` until `stop` is set: starts the helper on its source, serves it as a session of this node
    /// for as long as it runs and starts it again `retry` after it ends or could not start.
    pub fn serve_exec_helpers(&self, helpers: &[ExecHelper], retry: Duration, stop: &AtomicBool) {
        thread::scope(|scope| {
            for helper in helpers {
                scope.spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        if let Err(why) = self.serve_exec_helper(helper, stop) {
                            tracing::warn!(source = %helper.source, "the helper is not served: {why}");
                        }
                        let until = std::time::Instant::now() + retry;
                        while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                });
            }
        });
    }

    fn serve_exec_helper(&self, helper: &ExecHelper, stop: &AtomicBool) -> Result<(), String> {
        let parts = self.gui_parts();
        let source = parts
            .state
            .sources
            .exec(Some(&helper.source))
            .map_err(|error| error.message)?;
        let process = source
            .get()
            .start(ExecStart {
                argv: helper.command.clone(),
                stdin: true,
                ..ExecStart::default()
            })
            .map_err(|error| error.message().to_string())?;
        let process = Arc::new(process);
        // The helper's messages to the log go to its standard error; unread, a full queue would stop its output too.
        let errors = Arc::clone(&process);
        let name = helper.source.clone();
        let done = Arc::new(AtomicBool::new(false));
        let drain = {
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut buffer = [0_u8; 1024];
                while !done.load(Ordering::SeqCst) {
                    match errors.read(2, &mut buffer, POLL_MS) {
                        Ok(count) if count > 0 => {
                            let end = usize::try_from(count).unwrap_or(0);
                            for line in String::from_utf8_lossy(&buffer[..end]).lines() {
                                tracing::debug!(source = %name, "helper: {line}");
                            }
                        }
                        Ok(0) => {}
                        _ => break,
                    }
                }
            })
        };
        let channel = bohio_client::link::stream(
            Duplex::new(Out(Arc::clone(&process)), In(Arc::clone(&process))),
            &parts.settings,
        )
        .map_err(|error| error.to_string());
        let result = channel.map(|channel| {
            // The account the node lists the session under is the source, which is what authenticated the helper.
            let account = format!("exec:{}", helper.source);
            self.serve_helper_channel(&channel, &account, Some(stop));
            channel.close();
        });
        let _ = process.kill();
        done.store(true, Ordering::SeqCst);
        let _ = drain.join();
        result
    }
}
