//! `bohio gui-helper`: lends this user's desktop to the node of a service.
//!
//! A node that runs as a service has no desktop. The helper is a process the desktop user starts at logon (a Task Scheduler task,
//! a systemd user unit, a LaunchAgent) that connects out to the node's helper endpoint and serves the node's `bohio-gui/1`
//! streams over this user's own desktop. It holds no key, reads no peer table and listens on nothing.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use bohio_client::dial::Endpoint;
use clap::Args;
use coqui_transport::Channel as _;

use super::ConfigOption;
use super::host::node_settings;
use crate::access::Sources;
use crate::config::Environment;
use crate::gui::helper::{self, HelperSettings};

/// `bohio gui-helper`.
#[derive(Debug, Args)]
pub struct GuiHelperArgs {
    /// The configuration: this user's `[gui]` narrows what the node allows; `[gui] node_account` of the lowest layer says whose
    /// node this helper serves.
    #[command(flatten)]
    pub config: ConfigOption,
    /// The node's helper endpoint (`pipe://NAME`). Default `[gui] helper_endpoint`, else `pipe://bohio-gui-` and the first 16
    /// hex digits of `--node`.
    #[arg(long, value_name = "ENDPOINT")]
    pub endpoint: Option<String>,
    /// The id of the node (`bohio id` on it), when no endpoint is named: the endpoint's Windows default is made from it.
    #[arg(long, value_name = "ID")]
    pub node: Option<String>,
    /// The account that must serve the endpoint: a SID or name on Windows, a uid or name on Unix. Default `[gui] node_account`
    /// of the lowest layer, else `NT AUTHORITY\LocalService` on Windows and `bohio` on Unix. Any other account serving the
    /// endpoint is refused before anything is sent, with exit 1.
    #[arg(long, value_name = "ACCOUNT")]
    pub node_account: Option<String>,
    /// The name the node lists this session under, after the account. Default `SESSIONNAME` (Windows), `XDG_SESSION_ID`,
    /// `DISPLAY`, else `main`.
    #[arg(long, value_name = "NAME")]
    pub session: Option<String>,
    /// Serve the node over this process's standard input and output instead of an endpoint: the node started the helper through
    /// a login (an ssh source), which is what authenticates it, so no account is checked. Nothing else may write to standard
    /// output.
    #[arg(long)]
    pub stdio: bool,
}

/// The account of a node that was told nothing else.
const DEFAULT_NODE_ACCOUNT: &str = if cfg!(windows) {
    "NT AUTHORITY\\LocalService"
} else {
    "bohio"
};

fn first_set(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
}

/// Runs the helper until the node closes it.
pub(super) fn run(args: &GuiHelperArgs, environment: &Environment) -> Result<(), String> {
    let found = crate::config::discover(&args.config.config).map_err(|error| error.to_string())?;
    let config = found.config();
    if args.stdio {
        return run_stdio(args, config, environment);
    }
    let text = match (&args.endpoint, &config.gui.helper_endpoint, &args.node) {
        (Some(endpoint), _, _) | (None, Some(endpoint), _) => endpoint.clone(),
        (None, None, Some(node)) => {
            let digits = node.rsplit(':').next().unwrap_or("");
            format!("pipe://bohio-gui-{}", &digits[..digits.len().min(16)])
        }
        (None, None, None) => {
            return Err(
                "name the node's helper endpoint: --endpoint, [gui] helper_endpoint, or --node ID"
                    .to_owned(),
            );
        }
    };
    let endpoint = Endpoint::parse(&text).map_err(|error| error.to_string())?;
    let node_account = args
        .node_account
        .clone()
        .or_else(|| config.gui.node_account.clone())
        .unwrap_or_else(|| DEFAULT_NODE_ACCOUNT.to_owned());
    let settings = node_settings(config, environment)?;
    let sources = Sources::from_config(&settings.source, &settings.access)
        .map_err(|(key, message)| format!("{key}: {message}"))?;
    let session = args
        .session
        .clone()
        .or_else(|| first_set(&["SESSIONNAME", "XDG_SESSION_ID", "DISPLAY"]))
        .unwrap_or_else(|| "main".to_owned());
    let helper = HelperSettings {
        endpoint,
        node_account,
        session,
        user: first_set(&["USERNAME", "USER"]),
        gui: config.gui.clone(),
        link: config.link.settings(),
    };
    tracing::info!(endpoint = %helper.endpoint, session = %helper.session, "helping the node");
    helper::run(&helper, Arc::new(sources), &AtomicBool::new(false))
        .map_err(|error| error.to_string())?;
    tracing::info!("the node closed the connection");
    Ok(())
}

/// Serves the node on standard input and output.
fn run_stdio(
    args: &GuiHelperArgs,
    config: &crate::config::Config,
    environment: &Environment,
) -> Result<(), String> {
    let settings = node_settings(config, environment)?;
    let sources = Sources::from_config(&settings.source, &settings.access)
        .map_err(|(key, message)| format!("{key}: {message}"))?;
    let session = args
        .session
        .clone()
        .or_else(|| first_set(&["SESSIONNAME", "XDG_SESSION_ID", "DISPLAY"]))
        .unwrap_or_else(|| "main".to_owned());
    let helper = HelperSettings {
        // Unused here, and one that names nothing of the machine: the connection is the standard streams of the process.
        endpoint: Endpoint::parse("tcp://127.0.0.1:0").map_err(|error| error.to_string())?,
        node_account: String::new(),
        session,
        user: first_set(&["USERNAME", "USER"]),
        gui: config.gui.clone(),
        link: config.link.settings(),
    };
    let channel = bohio_client::link::stdio(&helper.link).map_err(|error| error.to_string())?;
    helper::serve_node(
        &channel,
        &helper,
        Arc::new(sources),
        &AtomicBool::new(false),
    );
    channel.close();
    Ok(())
}
