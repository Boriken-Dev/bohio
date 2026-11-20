//! Which targets a peer's grant lets the host connect to: the rules of `[peer.<id>.forward] connect`.
//!
//! A TCP rule is `coqui_link::forward::Rule` (an exact `HOST:PORT`, or `*.suffix`, `*`, an address or a CIDR network, with a
//! port, a range or `*`): nothing is resolved here, so an address or network rule matches only an address target and a name
//! rule only a name target. A `unix:PATH` or `pipe:NAME` rule matches the text exactly, or with `*` and `?`, neither of which
//! crosses a path separator. A requested path with a `..` part, a NUL or a relative path matches no rule.

use core::str::FromStr;

use bohio_proto::forward::Target;
use coqui_link::forward::{Rule, Target as TcpTarget};

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

/// The target `coqui-link` serves, in its one spelling; `None` for a host or port it refuses.
pub fn tcp_target(target: &Target) -> Option<TcpTarget> {
    match target {
        Target::Tcp { .. } => target.to_string().parse().ok(),
        _ => None,
    }
}

/// A path with no NUL and no `..` part: what a rule can meaningfully match.
fn plain_path(path: &str) -> bool {
    !path.contains('\0') && !path.split(['/', '\\']).any(|part| part == "..")
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

impl Grant {
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
    fn a_tcp_rule_matches_what_coqui_link_says() {
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
}
