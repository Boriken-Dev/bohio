// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Reverse forwards: a port on the host that reaches an address on this machine.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::ProviderError;

use crate::link::{Link, lock, refuse, within};

/// The forwards the host listens for: the host's port to the address connections are carried to. They outlive a
/// connection, which asks the host for each again when it is remade.
#[derive(Default)]
pub(crate) struct Reverse {
    targets: Mutex<BTreeMap<u32, (String, String)>>,
}

impl Reverse {
    fn add(&self, host: String, port: u32, target: String) {
        lock(&self.targets).insert(port, (host, target));
    }

    fn remove(&self, port: u32) {
        lock(&self.targets).remove(&port);
    }

    pub(crate) fn target(&self, port: u32) -> Option<String> {
        lock(&self.targets)
            .get(&port)
            .map(|(_, target)| target.clone())
    }

    /// The host addresses and ports to listen on again after a reconnect.
    pub(crate) fn listening(&self) -> Vec<(String, u32)> {
        lock(&self.targets)
            .iter()
            .map(|(port, (host, _))| (host.clone(), *port))
            .collect()
    }
}

/// A port the host listens on whose connections are carried to an address on this machine (OpenSSH's `-R`). Closing
/// it, or dropping it, makes the host stop listening.
///
/// It outlives the connection: a remade connection asks the host to listen again on the same port; when the host will
/// not give the port back the forward stays closed and nothing is reported.
pub struct ReverseForward {
    link: Arc<Link>,
    remote_host: String,
    remote_port: u16,
    open: AtomicBool,
}

impl core::fmt::Debug for ReverseForward {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReverseForward")
            .field("remote_host", &self.remote_host)
            .field("remote_port", &self.remote_port)
            .field("open", &self.open.load(Ordering::Acquire))
            .finish()
    }
}

/// Asks the host to listen on `remote_host:remote_port` (port `0`: the host chooses) and carries each connection to
/// `target` (`HOST:PORT`).
pub(crate) fn open(
    link: &Arc<Link>,
    remote_host: &str,
    remote_port: u16,
    target: &str,
) -> Result<ReverseForward, ProviderError> {
    if link.settings.openssh.is_some() {
        return Err(bohio_api::unsupported(
            "a reverse forward needs the one connection this provider keeps: client = \"openssh\" starts a process for each session",
        ));
    }
    let wait = link.settings.operation_timeout;
    let port = within(link, wait, async {
        let conn = link.conn().await?;
        conn.handle
            .tcpip_forward(remote_host.to_owned(), u32::from(remote_port))
            .await
            .map_err(|error| {
                refuse(
                    Status::GUATIAO_ERR_INTERNAL,
                    format!("the host did not accept the forward: {error}"),
                )
            })
    })??;
    // A request for port 0 is answered with the port the host chose; a fixed one is not.
    let port = if remote_port == 0 {
        port
    } else {
        u32::from(remote_port)
    };
    let chosen = u16::try_from(port).map_err(|_| {
        refuse(
            Status::GUATIAO_ERR_INTERNAL,
            format!("the host chose the invalid port {port}"),
        )
    })?;
    link.reverse
        .add(remote_host.to_owned(), port, target.to_owned());
    Ok(ReverseForward {
        link: Arc::clone(link),
        remote_host: remote_host.to_owned(),
        remote_port: chosen,
        open: AtomicBool::new(true),
    })
}

impl ReverseForward {
    /// The address the host listens on.
    #[must_use]
    pub fn remote_host(&self) -> &str {
        &self.remote_host
    }

    /// The port the host listens on: the one asked for, or the one the host chose for port `0`.
    #[must_use]
    pub const fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Makes the host stop listening, and waits for it to, so the port can be asked for again. Closing twice does
    /// nothing. Call it from a thread that is not the runtime's.
    pub fn close(&self) {
        if !self.open.swap(false, Ordering::AcqRel) {
            return;
        }
        let port = u32::from(self.remote_port);
        self.link.reverse.remove(port);
        let link = Arc::clone(&self.link);
        let host = self.remote_host.clone();
        let _ = within(
            &self.link,
            self.link.settings.operation_timeout,
            async move {
                if let Some(conn) = link.current().await {
                    let _ = conn.handle.cancel_tcpip_forward(host, port).await;
                }
            },
        );
    }
}

impl Drop for ReverseForward {
    fn drop(&mut self) {
        // The cancel is sent from the runtime, so a drop on any thread never blocks.
        if self.open.swap(false, Ordering::AcqRel) {
            let port = u32::from(self.remote_port);
            self.link.reverse.remove(port);
            let link = Arc::clone(&self.link);
            let host = self.remote_host.clone();
            self.link.rt.handle().spawn(async move {
                if let Some(conn) = link.current().await {
                    let _ = conn.handle.cancel_tcpip_forward(host, port).await;
                }
            });
        }
    }
}
