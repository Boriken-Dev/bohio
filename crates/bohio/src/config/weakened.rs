//! The link and forward settings an operator weakens: each is a choice, and the host says so once at start.

use super::{Config, ListenConfig};

impl Config {
    /// One line for each link or forward check this configuration weakens, `non-default: [section] key = value: cost`,
    /// for `serve` to log at start. `entries` are the listeners it is about to bind (the file's and the command line's).
    #[must_use]
    pub fn non_default(&self, entries: &[ListenConfig]) -> Vec<String> {
        let mut lines = Vec::new();
        let forward = &self.forward;
        if forward.socket_mode.0 != 0o600 {
            lines.push(format!(
                "non-default: [forward] socket_mode = {:?}: a Unix socket a remote forward makes is open to the accounts that mode allows",
                forward.socket_mode
            ));
        }
        if !forward.ipc_same_user {
            lines.push("non-default: [forward] ipc_same_user = false: a socket or pipe listener serves any account that can reach it, so its mode or access list is the only guard".to_owned());
        }
        if forward.follow_links {
            lines.push("non-default: [forward] follow_links = true: a link in a `unix:` path may lead a rule to a socket it did not name, so whoever can write that directory chooses which".to_owned());
        }
        if self.pairing.max_attempts == 0 {
            lines.push("non-default: [pairing] max_attempts = 0: a wrong PIN guess never uses the PIN up, so only the attempt delay and the PIN's timeout slow a guesser".to_owned());
        }
        for entry in entries {
            let at = &entry.endpoint;
            if let Some(mode) = &entry.socket_mode
                && entry.socket_mode_bits().ok().flatten() != Some(0o600)
            {
                lines.push(format!(
                    "non-default: [[listen]] socket_mode = {mode} on {at}: the socket is open to the accounts that mode allows"
                ));
            }
            if entry.remote_clients == Some(true) {
                lines.push(format!(
                    "non-default: [[listen]] remote_clients = true on {at}: the pipe is served over file sharing to other machines"
                ));
            }
            if entry
                .origins
                .as_ref()
                .is_some_and(|origins| origins.iter().any(|origin| origin == "*"))
            {
                lines.push(format!(
                    "non-default: [[listen]] origins includes \"*\" on {at}: any web page the person opens can reach the listener"
                ));
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(endpoint: &str) -> ListenConfig {
        ListenConfig {
            endpoint: endpoint.to_owned(),
            ..ListenConfig::default()
        }
    }

    #[test]
    fn the_defaults_weaken_nothing() {
        assert!(
            Config::default()
                .non_default(&[entry("tcp://127.0.0.1:0")])
                .is_empty()
        );
    }

    #[test]
    fn each_weakened_check_has_its_line() {
        let mut config = Config::default();
        config.forward.socket_mode = super::super::Mode(0o666);
        config.forward.ipc_same_user = false;
        config.forward.follow_links = true;
        config.pairing.max_attempts = 0;
        let mut pipe = entry("pipe://app");
        pipe.socket_mode = Some("0o660".to_owned());
        pipe.remote_clients = Some(true);
        let mut web = entry("ws://127.0.0.1:0");
        web.origins = Some(vec!["https://a.example".to_owned(), "*".to_owned()]);
        let lines = config.non_default(&[pipe, web]);
        for key in [
            "[forward] socket_mode = 0o666",
            "[forward] ipc_same_user = false",
            "[forward] follow_links = true",
            "[pairing] max_attempts = 0",
            "[[listen]] socket_mode = 0o660 on pipe://app",
            "[[listen]] remote_clients = true on pipe://app",
            "[[listen]] origins includes \"*\" on ws://127.0.0.1:0",
        ] {
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with(&format!("non-default: {key}"))),
                "{key}: {lines:#?}"
            );
        }
        assert_eq!(lines.len(), 7);
    }
}
