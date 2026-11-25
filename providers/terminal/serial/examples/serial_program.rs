//! The `serial` provider as a program: a provider a node starts with `[[providers.program]]` and talks to over this process's
//! standard input and output, in guatiao's remote protocol.
//!
//! A provider served from another process is one instance, configured where it runs, so the settings of a `[source.<name>]`
//! are this program's arguments: `serial_program --path /dev/ttyUSB0 --baud_rate 9600`, each `--KEY VALUE` one setting of the
//! `serial` provider (a number, `true` or `false`, or text). The node's own table is then `provider = "serial-program"` alone.
//! Nothing else may be written to standard output: it is the link.

#![forbid(unsafe_code)]

use std::sync::OnceLock;
use std::time::Duration;

use bohio_api::guatiao::library::{Object, ProviderError, Registry};
use bohio_api::guatiao::{FromValue, Map, Provider, Value};
use bohio_api::model::{SourceInfo, TerminalOpen};
use bohio_api::{Terminal, TerminalSource, unavailable};
use bohio_provider_serial::{Serial, SerialConfig};
use guatiao_source_remote::transport::child;
use guatiao_source_remote::{Kinds, LinkOptions, Server, Trust};

/// The line this process serves, made from its arguments before the link starts.
static LINE: OnceLock<Serial> = OnceLock::new();

/// The provider the node names: it hands every call to the line the arguments configured.
#[derive(Default, Provider)]
#[provider(
    TerminalSource,
    id = "serial-program",
    name = "serial console program",
    crate = ::bohio_api::guatiao
)]
struct Hosted;

fn line() -> Result<&'static Serial, ProviderError> {
    LINE.get()
        .ok_or_else(|| unavailable("the serial line is not configured"))
}

impl TerminalSource for Hosted {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        line()?.describe()
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        line()?.open(request)
    }
}

bohio_api::guatiao::local_providers!(
    id = "serial-program",
    version = env!("CARGO_PKG_VERSION"),
    providers = [Hosted]
);

/// `--KEY VALUE` pairs as the map a `[source.<name>]` table would be.
fn settings(args: impl Iterator<Item = String>) -> Result<Map, String> {
    let mut map = Map::new();
    let mut args = args;
    while let Some(flag) = args.next() {
        let key = flag
            .strip_prefix("--")
            .ok_or_else(|| format!("{flag} is not a setting (--KEY VALUE)"))?;
        let text = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let value = match text.parse::<i64>() {
            Ok(number) => Value::from(number),
            Err(_) => match text.as_str() {
                "true" => Value::from(true),
                "false" => Value::from(false),
                _ => Value::from(text.as_str()),
            },
        };
        map.set(key, value);
    }
    Ok(map)
}

fn main() -> Result<(), String> {
    let map = settings(std::env::args().skip(1))?;
    let config = SerialConfig::from_value(&Value::from(map)).map_err(|error| error.to_string())?;
    let serial = Serial::try_from(config).map_err(|error| error.to_string())?;
    let _ = LINE.set(serial);
    let mut registry = Registry::new("serial-program", env!("CARGO_PKG_VERSION"));
    registry
        .register_local("serial-program", library)
        .map_err(|error| error.to_string())?;
    let server = Server::new(registry, Kinds::new(), LinkOptions::new(Trust::Local));
    let link = server
        .serve(child::stdio())
        .map_err(|error| error.to_string())?;
    // The node closes this process's input when it is done with it, which ends the link.
    while link.alive() {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
