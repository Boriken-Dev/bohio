//! Which targets a peer's grant lets the host connect to and bind: the rules of `[peer.<id>.forward] connect` and `listen`.
//!
//! A TCP rule is `cabuya::forward::Rule` (an exact `HOST:PORT`, or `*.suffix`, `*`, an address or a CIDR network, with a
//! port, a range or `*`): nothing is resolved here, so an address or network rule matches only an address target and a name
//! rule only a name target. A `unix:PATH` or `pipe:NAME` rule matches the text exactly, or with `*` and `?`, neither of which
//! crosses a path separator. A requested path with a `..` part or a NUL matches no rule. A `listen` rule has the same
//! grammar, and its port `0` (the host chooses) is allowed only by a rule that names it or by `*`.

use core::str::FromStr;
use std::net::IpAddr;

use bohio_proto::forward::{Bind, Target};
use cabuya::forward::{Rule, Target as TcpTarget};

use crate::policy::{Grant, Permission};

/// One rule of `connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectRule {
    /// A TCP target.
    Tcp(Rule),
    /// A Unix socket path pattern.
    Unix(String),
    /// A Windows pipe name pattern.
    Pipe(String),
}

/// Text that is not a rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct BadRule(String);

impl FromStr for ConnectRule {
    type Err = BadRule;

    fn from_str(text: &str) -> Result<Self, BadRule> {
        if let Some(path) = text.strip_prefix("unix:") {
            return if path.is_empty() {
                Err(BadRule("a `unix:` rule needs a path".to_owned()))
            } else {
                Ok(Self::Unix(path.to_owned()))
            };
        }
        if let Some(name) = text.strip_prefix("pipe:") {
            return if name.is_empty() {
                Err(BadRule("a `pipe:` rule needs a name".to_owned()))
            } else {
                Ok(Self::Pipe(name.to_ascii_lowercase()))
            };
        }
        text.parse::<Rule>()
            .map(Self::Tcp)
            .map_err(|error| BadRule(error.to_string()))
    }
}

impl core::fmt::Display for ConnectRule {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tcp(rule) => write!(f, "{rule}"),
            Self::Unix(path) => write!(f, "unix:{path}"),
            Self::Pipe(name) => write!(f, "pipe:{name}"),
        }
    }
}

impl ConnectRule {
    /// Whether this rule grants `target`.
    pub fn matches(&self, target: &Target) -> bool {
        match (self, target) {
            (Self::Tcp(rule), Target::Tcp { .. }) => {
                tcp_target(target).is_some_and(|target| rule.matches(&target))
            }
            (Self::Unix(pattern), Target::Unix { path }) => {
                plain_path(path) && wildcard(pattern, path)
            }
            // Windows compares pipe names without case.
            (Self::Pipe(pattern), Target::Pipe { name }) => {
                plain_path(name) && wildcard(pattern, &name.to_ascii_lowercase())
            }
            _ => false,
        }
    }
}

/// The target `cabuya` serves, in its one spelling; `None` for a host or port it refuses.
pub fn tcp_target(target: &Target) -> Option<TcpTarget> {
    match target {
        Target::Tcp { .. } => target.to_string().parse().ok(),
        _ => None,
    }
}

/// A path with no NUL and no `.`, `..` or empty part (a leading separator is not one): what a rule can meaningfully match.
/// The system reads `/a/./b` and `/a//b` as the parent's entry, so a wildcard must never reach them.
fn plain_path(path: &str) -> bool {
    let mut parts = path.split(['/', '\\']);
    let rooted = path.starts_with(['/', '\\']);
    if rooted {
        parts.next();
    }
    !path.contains('\0') && parts.all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Whether `text` fits `pattern`, where `*` is any run of characters and `?` one character, neither of them a `/` or a `\`.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (pattern, text): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), text.chars().collect());
    let separator = |c: char| c == '/' || c == '\\';
    // The last `*` seen and where in the text it began matching, for backing up.
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some('?') if !separator(text[t]) => {
                p += 1;
                t += 1;
            }
            Some(&c) if c != '?' && c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((at, from)) if !separator(text[from]) => {
                    star = Some((at, from + 1));
                    p = at + 1;
                    t = from + 1;
                }
                _ => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// One rule of `listen`: what a peer may have the host bind.
///
/// A TCP rule is `BIND:PORT`: `BIND` is an address (an IPv6 one in brackets, the unspecified `0.0.0.0` and `::` included, which
/// `connect` rules cannot name), a CIDR network (`10.0.0.0/8`, `[fd00::]/8`) or `*`, which covers every address, and `PORT` is a
/// number, a range `A-B` or `*`. Nothing is resolved: a bind is an address. The port `0`, which asks the host for a free port,
/// is a number like another: `127.0.0.1:0` allows a host-chosen loopback port and nothing else, and `127.0.0.1:18090-18099`
/// allows those ports and no host-chosen one; `*` allows `0` too. A `unix:PATH` or `pipe:NAME` rule is as in `connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenRule {
    /// A TCP bind.
    Tcp {
        /// The addresses.
        addresses: Addresses,
        /// The ports.
        ports: PortSet,
        /// The rule's own text.
        text: String,
    },
    /// A Unix socket path pattern.
    Unix(String),
    /// A Windows pipe name pattern.
    Pipe(String),
}

/// The addresses a [`ListenRule`] allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addresses {
    /// Every address.
    Any,
    /// The addresses of a network: `prefix` leading bits of `network`. An address alone is the whole width.
    Network {
        /// The network's first address.
        network: IpAddr,
        /// How many leading bits are fixed.
        prefix: u8,
    },
}

/// An address with an IPv4-mapped IPv6 address folded to the IPv4 one.
fn folded(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
        v4 => v4,
    }
}

fn leading_bits(address: IpAddr) -> u128 {
    match address {
        IpAddr::V4(v4) => u128::from(u32::from(v4)),
        IpAddr::V6(v6) => u128::from(v6),
    }
}

impl Addresses {
    fn contains(self, address: IpAddr) -> bool {
        let Self::Network { network, prefix } = self else {
            return true;
        };
        let (address, network) = (folded(address), folded(network));
        if address.is_ipv4() != network.is_ipv4() {
            return false;
        }
        let width = if address.is_ipv4() { 32_u32 } else { 128 };
        let keep = u32::from(prefix).min(width);
        if keep == 0 {
            return true;
        }
        let shift = width - keep;
        leading_bits(address) >> shift == leading_bits(network) >> shift
    }

    fn parse(text: &str) -> Result<Self, BadRule> {
        let bad = || BadRule(format!("`{text}` is not an address, a network or `*`"));
        if text == "*" {
            return Ok(Self::Any);
        }
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let address = address
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(address);
        let network: IpAddr = address.parse().map_err(|_| bad())?;
        let width = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => width,
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= width)
                .ok_or_else(bad)?,
        };
        Ok(Self::Network { network, prefix })
    }
}

/// The ports a [`ListenRule`] allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSet {
    /// Any port, a host-chosen one included.
    Any,
    /// `low` to `high`, both included.
    Range(u16, u16),
}

impl PortSet {
    fn contains(self, port: u16) -> bool {
        match self {
            Self::Any => true,
            Self::Range(low, high) => (low..=high).contains(&port),
        }
    }
}

impl FromStr for ListenRule {
    type Err = BadRule;

    fn from_str(text: &str) -> Result<Self, BadRule> {
        if let Some(path) = text.strip_prefix("unix:") {
            return if path.is_empty() {
                Err(BadRule("a `unix:` rule needs a path".to_owned()))
            } else {
                Ok(Self::Unix(path.to_owned()))
            };
        }
        if let Some(name) = text.strip_prefix("pipe:") {
            return if name.is_empty() {
                Err(BadRule("a `pipe:` rule needs a name".to_owned()))
            } else {
                Ok(Self::Pipe(name.to_ascii_lowercase()))
            };
        }
        let (host, ports) = text
            .rsplit_once(':')
            .ok_or_else(|| BadRule("a listen rule is BIND:PORT".to_owned()))?;
        let ports = match ports.split_once('-') {
            _ if ports == "*" => PortSet::Any,
            Some((low, high)) => match (low.parse::<u16>(), high.parse::<u16>()) {
                (Ok(low), Ok(high)) if low <= high => PortSet::Range(low, high),
                _ => return Err(BadRule(format!("`{ports}` is not a port range"))),
            },
            None => {
                let port = ports
                    .parse::<u16>()
                    .map_err(|_| BadRule(format!("`{ports}` is not a port")))?;
                PortSet::Range(port, port)
            }
        };
        Ok(Self::Tcp {
            addresses: Addresses::parse(host)?,
            ports,
            text: text.to_owned(),
        })
    }
}

impl core::fmt::Display for ListenRule {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tcp { text, .. } => f.write_str(text),
            Self::Unix(path) => write!(f, "unix:{path}"),
            Self::Pipe(name) => write!(f, "pipe:{name}"),
        }
    }
}

impl ListenRule {
    /// Whether this rule grants binding `bind`.
    pub fn matches(&self, bind: &Bind) -> bool {
        match (self, bind) {
            (
                Self::Tcp {
                    addresses, ports, ..
                },
                Bind::Tcp { host, port },
            ) => {
                // A bind that is not an address is for the host to refuse; only `*` covers it.
                let address_ok = match host.parse::<IpAddr>() {
                    Ok(address) => addresses.contains(address),
                    Err(_) => *addresses == Addresses::Any,
                };
                address_ok && ports.contains(*port)
            }
            (Self::Unix(pattern), Bind::Unix { path }) => {
                plain_path(path) && wildcard(pattern, path)
            }
            (Self::Pipe(pattern), Bind::Pipe { name }) => {
                plain_path(name) && wildcard(pattern, &name.to_ascii_lowercase())
            }
            _ => false,
        }
    }
}

impl Grant {
    /// Whether the peer may have the host bind `bind`: it holds [`Permission::ForwardListen`] and a rule of
    /// `[forward] listen` matches. A rule that does not parse matches nothing.
    pub fn permits_listen(&self, bind: &Bind) -> bool {
        self.allows(Permission::ForwardListen)
            && self
                .forward()
                .listen()
                .iter()
                .filter_map(|text| text.parse::<ListenRule>().ok())
                .any(|rule| rule.matches(bind))
    }

    /// Whether the peer may have the host connect to `target`: it holds [`Permission::ForwardConnect`] and a rule of
    /// `[forward] connect` matches. A rule that does not parse matches nothing.
    pub fn permits_connect(&self, target: &Target) -> bool {
        self.allows(Permission::ForwardConnect)
            && self
                .forward()
                .connect()
                .iter()
                .filter_map(|text| text.parse::<ConnectRule>().ok())
                .any(|rule| rule.matches(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(text: &str) -> Target {
        text.parse().expect(text)
    }

    fn rule(text: &str) -> ConnectRule {
        text.parse().expect(text)
    }

    #[test]
    fn a_tcp_rule_matches_what_cabuya_says() {
        assert!(rule("db.lan:5432").matches(&tcp("DB.lan:5432")));
        assert!(!rule("db.lan:5432").matches(&tcp("db.lan:5433")));
        assert!(rule("10.0.0.0/8:22-25").matches(&tcp("10.1.2.3:24")));
        assert!(!rule("10.0.0.0/8:22-25").matches(&tcp("11.1.2.3:24")));
        assert!(rule("*.lan:*").matches(&tcp("a.lan:1")));
        assert!(!rule("*.lan:*").matches(&tcp("lan:1")));
        // Nothing resolves: a name never matches a network, and localhost is not 127.0.0.1.
        assert!(!rule("127.0.0.0/8:80").matches(&tcp("localhost:80")));
        assert!(!rule("127.0.0.1:80").matches(&tcp("localhost:80")));
        assert!(rule("[fd00::]/8:443").matches(&tcp("[fd00::5]:443")));
        assert!(rule("*:*").matches(&tcp("anything:9")));
        assert!(!rule("*:*").matches(&Target::Unix { path: "/x".into() }));
    }

    #[test]
    fn a_unix_or_pipe_rule_is_exact_or_a_wildcard_within_one_component() {
        let unix = |path: &str| Target::Unix { path: path.into() };
        assert!(rule("unix:/run/docker.sock").matches(&unix("/run/docker.sock")));
        assert!(!rule("unix:/run/docker.sock").matches(&unix("/run/docker.sock2")));
        assert!(rule("unix:/run/*.sock").matches(&unix("/run/a.sock")));
        assert!(!rule("unix:/run/*.sock").matches(&unix("/run/x/a.sock")));
        assert!(rule("unix:/run/user/*/bus").matches(&unix("/run/user/1000/bus")));
        assert!(rule("unix:/run/s?.sock").matches(&unix("/run/s1.sock")));
        assert!(!rule("unix:/run/s?.sock").matches(&unix("/run/s/.sock")));
        assert!(!rule("unix:/run/*").matches(&unix("/run/../etc/x")));
        assert!(!rule("unix:/run/*").matches(&unix("/run/a\0b")));
        let pipe = |name: &str| Target::Pipe { name: name.into() };
        assert!(rule("pipe:docker_engine").matches(&pipe("Docker_Engine")));
        assert!(rule("pipe:docker*").matches(&pipe("docker_engine")));
        assert!(!rule("pipe:docker*").matches(&pipe("other")));
        assert!(!rule("pipe:docker_engine").matches(&unix("docker_engine")));
    }

    #[test]
    fn a_wildcard_never_matches_an_empty_or_dot_component() {
        let unix = |path: &str| Target::Unix { path: path.into() };
        let listen = |path: &str| Bind::Unix { path: path.into() };
        let listen_rule = "unix:/run/*/docker.sock".parse::<ListenRule>().unwrap();
        assert!(rule("unix:/run/*/docker.sock").matches(&unix("/run/user/docker.sock")));
        assert!(listen_rule.matches(&listen("/run/user/docker.sock")));
        for path in ["/run/./docker.sock", "/run//docker.sock", "/run/user/docker.sock/"] {
            assert!(!rule("unix:/run/*/docker.sock").matches(&unix(path)), "{path}");
            assert!(!listen_rule.matches(&listen(path)), "{path}");
        }
        assert!(!rule("unix:/run/*").matches(&unix("/run/./")));
        assert!(!rule("unix:/srv/a/*/x.sock").matches(&unix("/srv/a/./x.sock")));
    }

    #[test]
    fn a_grant_needs_the_permission_and_a_rule() {
        let rules = ["db.lan:5432", "unix:/run/docker.sock"];
        let granted = Grant::allowing([Permission::ForwardConnect]).with_connect(rules);
        assert!(granted.permits_connect(&tcp("db.lan:5432")));
        assert!(!granted.permits_connect(&tcp("db.lan:22")));
        assert!(
            !Grant::allowing([Permission::Info])
                .with_connect(rules)
                .permits_connect(&tcp("db.lan:5432")),
            "rules without the permission allow nothing"
        );
        assert!(
            !Grant::allowing([Permission::ForwardConnect]).permits_connect(&tcp("db.lan:5432")),
            "the permission without a rule allows nothing"
        );
    }

    #[test]
    fn text_that_is_not_a_rule_is_refused() {
        for text in ["", "db.lan", "unix:", "pipe:", "10.0.0.0/33:22", "db.lan:0"] {
            assert!(text.parse::<ConnectRule>().is_err(), "{text}");
        }
    }

    #[test]
    fn a_listen_rule_matches_an_address_and_ports_and_a_host_chosen_port_only_when_named() {
        let tcp = |text: &str| text.parse::<Bind>().expect(text);
        let rule = |text: &str| text.parse::<ListenRule>().expect(text);
        assert!(rule("127.0.0.1:0").matches(&tcp("127.0.0.1:0")));
        assert!(!rule("127.0.0.1:0").matches(&tcp("127.0.0.1:80")));
        assert!(rule("127.0.0.1:18090-18099").matches(&tcp("127.0.0.1:18095")));
        assert!(!rule("127.0.0.1:18090-18099").matches(&tcp("127.0.0.1:0")));
        assert!(!rule("127.0.0.1:18090-18099").matches(&tcp("127.0.0.1:18100")));
        assert!(rule("127.0.0.1:*").matches(&tcp("127.0.0.1:0")));
        // The loopback rule is not a wildcard rule: another address needs its own.
        assert!(!rule("127.0.0.1:8080").matches(&tcp("0.0.0.0:8080")));
        assert!(!rule("127.0.0.1:8080").matches(&tcp("[::1]:8080")));
        assert!(rule("0.0.0.0:8080").matches(&tcp("0.0.0.0:8080")));
        assert!(rule("*:8080").matches(&tcp("0.0.0.0:8080")));
        assert!(rule("[::1]:8080").matches(&tcp("[::1]:8080")));
        assert!(rule("127.0.0.0/8:*").matches(&tcp("127.0.0.2:5")));
        assert!(!rule("127.0.0.0/8:*").matches(&tcp("10.0.0.2:5")));
        assert!(!rule("127.0.0.1:*").matches(&Bind::Unix { path: "/x".into() }));
        let unix = |path: &str| Bind::Unix { path: path.into() };
        assert!(rule("unix:/run/user/*/app.sock").matches(&unix("/run/user/1000/app.sock")));
        assert!(!rule("unix:/run/user/*/app.sock").matches(&unix("/run/user/1000/x/app.sock")));
        assert!(!rule("unix:/run/*").matches(&unix("/run/../etc/x")));
        let pipe = |name: &str| Bind::Pipe { name: name.into() };
        assert!(rule("pipe:app*").matches(&pipe("App1")));
        assert!(!rule("pipe:app*").matches(&pipe("other")));
    }

    #[test]
    fn a_listen_grant_needs_the_permission_and_a_rule() {
        let bind = "127.0.0.1:0".parse::<Bind>().unwrap();
        let rules = ["127.0.0.1:0"];
        let granted = Grant::allowing([Permission::ForwardListen]).with_listen(rules);
        assert!(granted.permits_listen(&bind));
        assert!(
            !Grant::allowing([Permission::Info])
                .with_listen(rules)
                .permits_listen(&bind)
        );
        assert!(!Grant::allowing([Permission::ForwardListen]).permits_listen(&bind));
        // A connect rule is not a listen rule.
        assert!(
            !Grant::allowing([Permission::ForwardListen])
                .with_connect(rules)
                .permits_listen(&bind)
        );
    }

    #[test]
    fn text_that_is_not_a_listen_rule_is_refused() {
        for text in [
            "",
            "127.0.0.1",
            "127.0.0.1:x",
            "127.0.0.1:9-1",
            "unix:",
            "pipe:",
            "10.0.0.0/33:22",
        ] {
            assert!(text.parse::<ListenRule>().is_err(), "{text}");
        }
    }
}
