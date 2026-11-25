//! A GUI helper over its standard input and output that serves the tests' painted desktop: what `bohio gui-helper --stdio`
//! does, with no screen. The tests start it through a login (an ssh source) to prove a node serves a helper that runs on
//! another machine.

#![forbid(unsafe_code)]

#[path = "support/painted.rs"]
mod painted;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bohio::access::{Capability, DesktopOf, Sources};
use bohio::config::GuiConfig;
use bohio::gui::helper::{HelperSettings, serve_node};
use bohio_client::dial::Endpoint;
use bohio_client::link::{Settings, stdio};
use coqui_transport::Channel as _;
use painted::Painted;

fn main() {
    let sources = Arc::new(
        Sources::unconfigured()
            .with_desktop("app", Arc::new(DesktopOf::new(Arc::new(Painted::new()))))
            .with_access(Capability::Desktop, ["app"]),
    );
    let settings = HelperSettings {
        endpoint: Endpoint::parse("pipe://stdio").expect("an endpoint"),
        node_account: String::new(),
        session: "remote".to_owned(),
        user: Some("tester".to_owned()),
        gui: GuiConfig::default(),
        link: Settings::default(),
    };
    let channel = stdio(&settings.link).expect("a channel over standard input and output");
    serve_node(&channel, &settings, sources, &AtomicBool::new(false));
    channel.close();
}
