//! Secret references: a setting a provider marks sensitive holds where the secret is kept, never the secret.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use bohio::access::Sources;
use bohio::config::{AccessConfig, SourceTables};
use bohio::secret::{Secret, SecretError, SecretResolver};
use bohio_api::guatiao::library::ProviderError;
use bohio_api::guatiao::{FromValue, Provider, Schema, ToValue};
use bohio_api::model::{PowerState, SourceInfo};
use bohio_api::{PowerSource, vocab};

/// What the test provider was given for its password.
static GIVEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[derive(ToValue, FromValue, Schema, Debug, Clone)]
#[map(crate = ::bohio_api::guatiao)]
struct VaultConfig {
    host: String,
    #[schema(sensitive)]
    password: String,
}

#[derive(Default, Provider)]
#[provider(
    PowerSource,
    id = "vault_power",
    name = "vault power",
    crate = ::bohio_api::guatiao,
    config = VaultConfig
)]
struct Vault;

impl TryFrom<VaultConfig> for Vault {
    type Error = ProviderError;

    fn try_from(config: VaultConfig) -> Result<Self, Self::Error> {
        GIVEN.lock().unwrap().push(config.password);
        Ok(Self)
    }
}

impl PowerSource for Vault {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new("vault_power", &[vocab::power::STATUS], &[]))
    }

    fn state(&self) -> Result<PowerState, ProviderError> {
        Ok(PowerState {
            state: "on".to_owned(),
            done: true,
        })
    }

    fn act(&self, _action: &str) -> Result<PowerState, ProviderError> {
        Err(bohio_api::unsupported("no actions"))
    }
}

bohio_api::guatiao::local_providers!(id = "secrets-test", version = "0", providers = [Vault]);

struct Reads(Mutex<Vec<String>>);

impl SecretResolver for Reads {
    fn resolve(&self, reference: &str, max_bytes: usize) -> Result<Secret, SecretError> {
        self.0.lock().unwrap().push(reference.to_owned());
        match reference {
            "credential:bmc" => Ok(Secret::new("s3cr3t-from-the-store")),
            "file:/too/big" => Err(SecretError::new(format!(
                "the secret file is larger than [secrets] max_bytes ({max_bytes})"
            ))),
            _ => Err(SecretError::new("no such secret")),
        }
    }
}

fn tables(password: &str) -> SourceTables {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    toml::from_str::<Whole>(&format!(
        "[source.bmc]\nprovider = \"vault_power\"\nhost = \"bmc.invalid\"\npassword = \"{password}\"\n"
    ))
    .expect("source tables")
    .source
}

fn sources() -> Sources {
    let sources = Sources::unconfigured();
    sources
        .register_library("secrets-test", library)
        .expect("the test provider registers");
    sources
}

fn access() -> AccessConfig {
    AccessConfig {
        power: vec!["bmc".to_owned()],
        ..AccessConfig::default()
    }
}

#[test]
fn a_literal_for_a_sensitive_setting_is_refused_by_key_and_never_quoted() {
    let literal = "hunter2-the-actual-password";
    let sources = sources();
    sources
        .set_secrets(Arc::new(Reads(Mutex::default())), 64)
        .unwrap();
    let (key, message) = sources
        .reconfigure(&tables(literal), &access())
        .expect_err("a literal is refused");
    assert_eq!(key, "source.bmc.password");
    assert!(!message.contains(literal), "{message}");
    assert!(!format!("{key} {message}").contains(literal));
    assert!(
        GIVEN.lock().unwrap().iter().all(|given| given != literal),
        "the provider was never built with it"
    );
}

#[test]
fn a_reference_is_read_when_the_source_is_built_and_the_provider_gets_the_value() {
    let reads = Arc::new(Reads(Mutex::default()));
    let sources = sources();
    sources.set_secrets(reads.clone(), 64).unwrap();
    sources
        .reconfigure(&tables("credential:bmc"), &access())
        .expect("the reference resolves");
    assert_eq!(reads.0.lock().unwrap().as_slice(), ["credential:bmc"]);
    assert!(
        GIVEN
            .lock()
            .unwrap()
            .iter()
            .any(|given| given == "s3cr3t-from-the-store"),
        "the provider received the value, not the reference"
    );
    assert!(sources.power(None).is_ok());
}

#[test]
fn a_refused_reference_names_its_key_and_the_log_and_error_hold_no_secret() {
    let sources = sources();
    sources
        .set_secrets(Arc::new(Reads(Mutex::default())), 64)
        .unwrap();
    let (key, message) = sources
        .reconfigure(&tables("file:/too/big"), &access())
        .expect_err("refused");
    assert_eq!(key, "source.bmc.password");
    assert!(message.contains("max_bytes"), "{message}");
    let secret = Secret::new("s3cr3t-from-the-store");
    assert!(!format!("{secret:?} {secret}").contains("s3cr3t"));
}

#[test]
fn a_node_without_a_resolver_leaves_such_a_source_out_until_it_has_one() {
    let sources = sources();
    sources.defer_secrets();
    sources
        .reconfigure(&tables("credential:bmc"), &AccessConfig::default())
        .expect("the source waits");
    assert!(sources.power(Some("bmc")).is_err(), "not built yet");
    sources
        .set_secrets(Arc::new(Reads(Mutex::default())), 64)
        .expect("built once a resolver arrives");
    assert!(sources.power(Some("bmc")).is_ok());
}
