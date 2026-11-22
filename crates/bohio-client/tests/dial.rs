//! Endpoint text for every transport, and the command line `stdio://` and `--command` start.

use bohio_client::dial::{Endpoint, SerialOptions, split_command};

#[test]
fn each_scheme_parses_and_prints_back() {
    for text in [
        "tcp://127.0.0.1:7620",
        "ws://127.0.0.1:7621/",
        "wss://127.0.0.1:7622/",
        "serial:///dev/ttyUSB0?baud=9600",
        "serial://COM3",
        "stdio://docker exec -i box bohio serve --stdio",
    ] {
        let endpoint = Endpoint::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
        assert_eq!(endpoint.to_string(), text);
    }
    let pipe = Endpoint::parse("pipe:///run/bohio/host.sock");
    if cfg!(unix) {
        assert_eq!(pipe.unwrap().to_string(), "pipe:///run/bohio/host.sock");
    }
    let named = Endpoint::parse("pipe://bohio-host");
    if cfg!(windows) {
        assert!(named.unwrap().to_string().ends_with("bohio-host"));
    }
}

#[test]
fn a_serial_query_sets_the_line_and_refuses_what_it_does_not_know() {
    let Endpoint::Serial { path, options } = Endpoint::parse(
        "serial:///dev/ttyS1?baud=57600&parity=even&stop_bits=2&data_bits=8&flow_control=hardware",
    )
    .unwrap() else {
        panic!("a serial endpoint");
    };
    assert_eq!(path, "/dev/ttyS1");
    let mut expected = SerialOptions::default();
    for (key, value) in [
        ("baud", "57600"),
        ("parity", "even"),
        ("stop_bits", "2"),
        ("data_bits", "8"),
        ("flow_control", "hardware"),
    ] {
        expected.set(key, value).unwrap();
    }
    assert_eq!(options, expected);
    for bad in [
        "serial:///dev/x?baud=0",
        "serial:///dev/x?baud=fast",
        "serial:///dev/x?flow_control=software",
        "serial:///dev/x?speed=1",
        "serial:///dev/x?baud",
        "serial://?baud=9600",
    ] {
        assert!(Endpoint::parse(bad).is_err(), "{bad} was accepted");
    }
}

#[test]
fn wss_is_a_web_socket_and_an_unknown_scheme_says_why() {
    let Endpoint::WebSocket(url) = Endpoint::parse("wss://127.0.0.1:443/bohio").unwrap() else {
        panic!("wss is not a WebSocket endpoint");
    };
    assert_eq!(url.scheme(), "wss");
    assert!(Endpoint::parse("wss://").is_err());
    let other = Endpoint::parse("gopher://host").unwrap_err();
    assert!(other.contains("not served"), "{other}");
    assert!(Endpoint::parse("nowhere").is_err());
}

#[test]
fn a_command_line_splits_on_spaces_and_groups_quotes_without_escapes() {
    assert_eq!(
        split_command(r#"docker exec -i "my box" bohio serve --stdio"#).unwrap(),
        [
            "docker", "exec", "-i", "my box", "bohio", "serve", "--stdio"
        ]
    );
    assert_eq!(
        split_command(r#""C:\Program Files\bohio\bohio.exe" serve"#).unwrap(),
        [r"C:\Program Files\bohio\bohio.exe", "serve"]
    );
    assert_eq!(split_command("a '' b").unwrap(), ["a", "", "b"]);
    assert!(split_command("   ").is_err());
    assert!(split_command("a \"b").is_err());
}

#[test]
fn a_certificate_pin_is_a_sha256_digest() {
    let mut trust = bohio_client::dial::ServerTrust::default();
    let digest = "ab".repeat(32);
    trust.pin(&format!("sha256:{digest}")).unwrap();
    assert_eq!(trust.pins.len(), 1);
    for bad in [
        "",
        "sha256:abcd",
        "md5:d41d8cd98f00b204e9800998ecf8427e",
        &format!("sha384:{}", "ab".repeat(48)),
        &format!("sha256:{}", "zz".repeat(32)),
    ] {
        assert!(trust.pin(bad).is_err(), "{bad} was accepted");
    }
    assert_eq!(trust.pins.len(), 1);
}

#[test]
fn a_trust_file_that_cannot_be_read_is_named() {
    let mut trust = bohio_client::dial::ServerTrust::default();
    trust.ca_files.push("no-such-dir/no-such-ca.pem".into());
    let reason = trust.client_config().unwrap_err();
    assert!(reason.contains("no-such-ca.pem"), "{reason}");
}

#[test]
fn webtransport_needs_a_udp_port_and_takes_a_websocket_fallback_wherever_it_is_asked() {
    let wt = Endpoint::parse("webtransport://127.0.0.1:4433/bohio").unwrap();
    assert_eq!(wt.to_string(), "webtransport://127.0.0.1:4433/bohio");
    assert!(Endpoint::parse("webtransport://127.0.0.1").is_err());
    assert!(Endpoint::parse("webtransport://:4433").is_err());

    let ws = Endpoint::parse("ws://127.0.0.1:7621").unwrap();
    let paired = wt.clone().with_fallback(ws.clone()).unwrap();
    assert_eq!(
        paired.to_string(),
        "webtransport://127.0.0.1:4433/bohio (or ws://127.0.0.1:7621/)"
    );
    // A fallback on another host is the caller's choice, plain or not.
    let other = Endpoint::parse("ws://192.0.2.7:7621").unwrap();
    let elsewhere = wt.clone().with_fallback(other).unwrap();
    assert_eq!(
        elsewhere.to_string(),
        "webtransport://127.0.0.1:4433/bohio (or ws://192.0.2.7:7621/)"
    );
    let secure = Endpoint::parse("wss://other.example:7621").unwrap();
    assert!(wt.clone().with_fallback(secure).is_ok());
    // A non-WebSocket fallback and a fallback on a non-WebTransport endpoint are refused.
    assert!(
        wt.with_fallback(Endpoint::parse("tcp://127.0.0.1:7620").unwrap())
            .is_err()
    );
    assert!(ws.clone().with_fallback(ws).is_err());
}

#[test]
fn webtransport_takes_mtu_sizes_that_quic_can_start_from_and_no_other_endpoint_does() {
    use bohio_client::dial::{MIN_MTU, Mtu};
    let wt = Endpoint::parse("webtransport://127.0.0.1:4433").unwrap();
    let mut mtu = Mtu::default();
    mtu.upper_bound = 1300;
    mtu.discovery = false;
    let lowered = wt.clone().with_mtu(mtu).unwrap();
    // The size is a setting of the connection: the endpoint still reads the same, and a fallback keeps it.
    assert_eq!(lowered.to_string(), wt.to_string());
    let ws = Endpoint::parse("ws://127.0.0.1:7621").unwrap();
    assert_eq!(
        lowered.clone().with_fallback(ws.clone()).unwrap(),
        wt.clone()
            .with_fallback(ws.clone())
            .unwrap()
            .with_mtu(mtu)
            .unwrap()
    );
    assert_ne!(lowered, wt);
    mtu.initial = MIN_MTU - 1;
    let refused = wt.with_mtu(mtu).unwrap_err();
    assert!(
        refused.contains("1199") && refused.contains("1200"),
        "{refused}"
    );
    assert!(
        ws.with_mtu(Mtu::default())
            .unwrap_err()
            .contains("webtransport://")
    );
}
