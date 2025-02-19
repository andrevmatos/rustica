use crate::auth::AuthorizationConfiguration;
use crate::logging::{Log, LoggingConfiguration};
use crate::server::{AllowedSignersCache, RusticaServer};
use crate::signing::{SigningConfiguration, SigningError};

use clap::{Arg, Command};

use crossbeam_channel::{unbounded, Receiver};
use lru::LruCache;
use ring::{hmac, rand};
use serde::Deserialize;

use std::convert::TryInto;
use std::net::SocketAddr;
use std::time::Duration;
use std::num::NonZeroUsize;

use tokio::sync::{RwLock, Mutex};

use sshcerts::{ssh::KeyTypeKind, CertType, PrivateKey};

#[derive(Deserialize)]
pub struct ClientAuthorityConfiguration {
    pub authority: String,
    pub validity_length: u64,
    pub expiration_renewal_period: u64,
}

#[derive(Deserialize)]
pub struct AllowedSignersConfiguration {
    pub cache_validity_length: Duration,
    pub lru_rate_limiter_size: NonZeroUsize,
    pub rate_limit_cooldown: Duration,
}

#[derive(Deserialize)]
pub struct Configuration {
    pub server_cert: String,
    pub server_key: String,
    pub client_authority: ClientAuthorityConfiguration,
    pub listen_address: String,
    pub authorization: AuthorizationConfiguration,
    pub signing: SigningConfiguration,
    pub require_rustica_proof: bool,
    pub require_attestation_chain: bool,
    pub logging: LoggingConfiguration,
    pub allowed_signers: AllowedSignersConfiguration,
}

pub struct RusticaSettings {
    pub server: RusticaServer,
    pub client_ca_cert: String,
    pub server_cert: String,
    pub server_key: String,
    pub address: SocketAddr,
    pub log_receiver: Receiver<Log>,
    pub logging_configuration: LoggingConfiguration,
}

pub enum ConfigurationError {
    FileError,
    ParsingError,
    SSHKeyError,
    InvalidListenAddress,
    AuthorizerError,
    SigningMechanismError(SigningError),
    ValidateOnly,
    DefaultAuthorityDoesNotHaveSSHKeys,
    NoSuchSigningMechanismForClientCa(String, Vec<String>),
}

impl From<sshcerts::error::Error> for ConfigurationError {
    fn from(_: sshcerts::error::Error) -> ConfigurationError {
        ConfigurationError::SSHKeyError
    }
}

impl std::error::Error for ConfigurationError {
    fn description(&self) -> &str {
        ""
    }
}

impl std::fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileError => write!(f, "Could not read configuration file"),
            Self::ParsingError => write!(f, "Could not parse the configuration file"),
            Self::SSHKeyError => write!(f, "Could not parse the provided SSH keys file"),
            Self::InvalidListenAddress => write!(f, "Invalid address and/or port to listen on"),
            Self::AuthorizerError => write!(f, "Configuration for authorization was invalid"),
            Self::SigningMechanismError(ref e) => write!(f, "{}", e),
            Self::ValidateOnly => write!(f, "Configuration was validated"),
            Self::DefaultAuthorityDoesNotHaveSSHKeys => write!(
                f,
                "The default authority must provide SSH keys"
            ),
            Self::NoSuchSigningMechanismForClientCa(chosen, options) => write!(
                f,
                "The requested signing mechanism to issue client certificates ({chosen}) is not configured. Options are: {}", options.join(", ")
            ),
        }
    }
}

impl std::fmt::Debug for ConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

pub async fn configure() -> Result<RusticaSettings, ConfigurationError> {
    let matches = Command::new("Rustica")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Mitchell Grenier <mitchell@confurious.io>")
        .about("Rustica is a Yubikey backed SSHCA")
        .arg(
            Arg::new("config")
                .help("Path to Rustica configuration toml file")
                .long("config")
                .default_value("/etc/rustica/rustica.toml")
                .takes_value(true),
        )
        .arg(
            Arg::new("validate")
                .help("Only validate the configuration and then quit. Useful for testing configuration changes.")
                .long("validate-config")
                .short('v')
                .action(clap::ArgAction::Count)
                .takes_value(false),
        )
        .get_matches();

    // Read the configuration file
    let config = match tokio::fs::read(matches.value_of("config").unwrap()).await {
        Ok(config) => config,
        Err(_) => return Err(ConfigurationError::FileError),
    };

    // Parse the TOML into our configuration structures
    let config: Configuration = match toml::from_slice(&config) {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to parse config: {}", e);
            return Err(ConfigurationError::ParsingError);
        }
    };

    // Only validate that the configuration parses correctly
    // Do not check that we could access keys and build certificates.
    if matches.get_count("validate") == 1 {
        return Err(ConfigurationError::ValidateOnly);
    }

    let address = match config.listen_address.parse() {
        Ok(addr) => addr,
        Err(_) => return Err(ConfigurationError::InvalidListenAddress),
    };

    let (log_sender, log_receiver) = unbounded();

    let authorizer = match config.authorization.try_into() {
        Ok(authorizer) => authorizer,
        _ => return Err(ConfigurationError::AuthorizerError),
    };

    let signer = match config.signing.convert_to_signing_mechanism().await {
        Ok(signer) => signer,
        Err(e) => return Err(ConfigurationError::SigningMechanismError(e)),
    };

    if signer
        .get_signer_public_key(&signer.default_authority, CertType::User)
        .is_err()
    {
        return Err(ConfigurationError::DefaultAuthorityDoesNotHaveSSHKeys);
    }

    let rng = rand::SystemRandom::new();
    let hmac_key = hmac::Key::generate(hmac::HMAC_SHA256, &rng).unwrap();
    let challenge_key = PrivateKey::new(KeyTypeKind::Ed25519, "RusticaChallengeKey").unwrap();

    let client_ca_cert = signer
        .get_client_certificate_authority(&config.client_authority.authority)
        .map_err(|e| ConfigurationError::SigningMechanismError(e))?
        .ok_or(ConfigurationError::NoSuchSigningMechanismForClientCa(config.client_authority.authority.clone(), signer.get_authorities()))?
        .serialize_pem()
        .map_err(|e| {
            ConfigurationError::SigningMechanismError(SigningError::AccessError(format!(
                "Could not create a PEM from the requested signing system: {e}"
            )))
        })?;

    let allowed_signers_rate_limiter = LruCache::new(config.allowed_signers.lru_rate_limiter_size);

    let allowed_signers_cache = AllowedSignersCache {
        compressed_allowed_signers: vec![],
        expiry_timestamp: Duration::ZERO,
    };
    
    // We're only validating that we can use this configuration so do not start
    // This happens after we've parsed the config but also confirmed access to
    // keys and created certificates.
    if matches.get_count("validate") > 1 {
        return Err(ConfigurationError::ValidateOnly);
    }

    let server = RusticaServer {
        log_sender,
        hmac_key,
        challenge_key,
        authorizer,
        signer,
        require_rustica_proof: config.require_rustica_proof,
        require_attestation_chain: config.require_attestation_chain,
        client_authority: config.client_authority,
        allowed_signers: config.allowed_signers,
        allowed_signers_rate_limiter: Mutex::new(allowed_signers_rate_limiter).into(),
        allowed_signers_cache: RwLock::new(allowed_signers_cache).into(),
    };

    Ok(RusticaSettings {
        server,
        client_ca_cert,
        server_cert: config.server_cert,
        server_key: config.server_key,
        address,
        log_receiver,
        logging_configuration: config.logging,
    })
}
