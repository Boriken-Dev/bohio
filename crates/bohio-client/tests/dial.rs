//! Endpoint text for every transport, and the command line `stdio://` and `--command` start.

use bohio_client::dial::{Endpoint, SerialOptions, split_command};

#[test]
fn each_scheme_parses_and_prints_back() {
    for text in [
        "tcp://127.0.0.1:7620",
        "ws://127.0.0.1:7621/",
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
fn wss_and_an_unknown_scheme_say_why() {
    let wss = Endpoint::parse("wss://host:443").unwrap_err();
    assert!(wss.contains("TLS"), "{wss}");
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
