use std::collections::HashMap;

/// This is the signing module of the Rustica project. The module is designed
/// to be easily extended, allowing the creation of new signing submodules with
/// minimal code changes. The interfaces are also async with access to a tokio
/// runtime (provided by the server module) so key signing occuring on remote
/// systems can be simply implemented.
use async_trait::async_trait;

use serde::Deserialize;
use sshcerts::ssh::{CertType, Certificate, PublicKey};

#[cfg(feature = "amazon-kms")]
mod amazon_kms;
mod file;
#[cfg(feature = "yubikey-support")]
mod yubikey;

#[derive(Deserialize)]
#[serde(tag = "kind")]
pub enum SignerType {
    File(file::Config),
    #[cfg(feature = "yubikey-support")]
    Yubikey(yubikey::Config),
    #[cfg(feature = "amazon-kms")]
    AmazonKMS(amazon_kms::Config),
}

impl SignerType {
    async fn into_signer(self) -> Result<Box<dyn Signer + Send + Sync>, SigningError> {
        match self {
            Self::File(x) => x.into_signer().await,
            #[cfg(feature = "yubikey-support")]
            Self::Yubikey(x) => x.into_signer().await,
            #[cfg(feature = "amazon-kms")]
            Self::AmazonKMS(f) => f.into_signer().await,
        }
    }
}

#[async_trait]
pub trait SignerConfig {
    async fn into_signer(self) -> Result<Box<dyn Signer + Send + Sync>, SigningError>;
}

/// Any code that wants to be able to sign certificates for Rustica must implement
/// this trait. The trait is async to allow calls out to external services during
/// sign but fetching public keys must be fast and low cost.
#[async_trait]
pub trait Signer {
    /// Take in a certificate and sign it turning it into a valid certificate. This call
    /// is async allowing calls to be made over the network or to other blocking resources.
    /// This call however should execute as fast as possible and have a strict timeout as
    /// the runtime this is executing on is the one fulfilling certificate requests from
    /// users.
    async fn sign(&self, cert: Certificate) -> Result<Certificate, SigningError>;

    /// This function is intentionally not async. This is to discourage this call being reliant
    /// on further network dependence as it is called earlier in the stack than `sign`. Creating
    /// a `Signer` from a config is async so memoization of the public key should be done in
    /// there. See the AWS signer as an example.
    fn get_signer_public_key(&self, cert_type: CertType) -> Option<PublicKey>;

    /// Return the CA certificate used for signing X509 certificate requests.
    /// This function may hide away async code (as it does in the KMS signer)
    /// due to using the remote KeyPair trait imported from the rcgen crate
    fn get_attested_x509_certificate_authority(&self) -> Option<&rcgen::Certificate>;

    /// Return the CA certificate used for signing X509 certificate requests.
    /// This function may hide away async code (as it does in the KMS signer)
    /// due to using the remote KeyPair trait imported from the rcgen crate
    fn get_client_certificate_authority(&self) -> Option<&rcgen::Certificate>;
}

#[derive(Deserialize)]
pub struct ExternalSigningConfig {
    pub server: String,
    pub port: String,
    pub ca: String,
    pub mtls_cert: String,
    pub mtls_key: String,
}

/// Represents the configuration the Rustica signing system. We have a
/// default authority so if people pass an empty string for key_id(authority)
/// we maintain backwards compatibility.
///
/// The reason this is an enum of one is to support a completely external
/// signing system in the future if none of the internal options work for
/// you.
#[derive(Deserialize)]
pub struct SigningConfiguration {
    pub default_authority: String,
    pub authority_configurations: HashMap<String, SignerType>,
}

/// A `SigningConfiguration` can be coerced into a `SigningMechanism` to
/// handle the signing operations as well as other convenience functions
/// such as fetching public keys or printing info about how signing is
/// configured.
///
pub struct SigningMechanism {
    pub default_authority: String,
    pub authorities: HashMap<String, Box<dyn Signer + Send + Sync>>,
}

#[derive(Debug)]
pub enum SigningError {
    /// Represents when there was an issue accessing the key material. This
    /// could occur when AmazonKMS is unreachable or a Yubikey has been
    /// disconnected during runtime.
    #[allow(dead_code)]
    AccessError(String),
    /// SigningFailure represents the private key material being unable to
    /// sign the provided certificate. This could be because of a key
    /// incompatiblity or a corrupted private key.
    SigningFailure,
    /// ParsingError represents any error that occurs from unexpected data
    /// not being able to be parsed correctly, or code that fails to parse
    /// expected data
    #[allow(dead_code)]
    ParsingError,
    UnknownAuthority(String),
    DuplicatedKey(String, String),
    IdenticalUserAndHostKey(String),
    SignerDoesNotHaveSSHKeys,
    SignerDoesNotAllRequiredSSHKeys,
}

impl std::fmt::Display for SigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessError(e) => write!(f, "Could not access the private key material: {}", e),
            Self::SigningFailure => write!(f, "The signing operation on the provided certificate failed"),
            Self::ParsingError => write!(f, "The signature could not be parsed"),
            Self::UnknownAuthority(authority) => write!(f, "Unknown authority was requested for a signing operation: {authority}"),
            Self::DuplicatedKey(a1, a2) => write!(f, "Authorities {a1} and {a2} share at least one key. This is not allowed as it almost always a misconfiguration leading to access that is not correctly restricted"),
            Self::IdenticalUserAndHostKey(authority) => write!(f, "Authority {authority} has an identical key for both user and host certificates. This is not allowed as it's much safer to use separate keys for both."),
            Self::SignerDoesNotHaveSSHKeys => write!(f, "Signer was not configured with SSH keys so it cannot create an SSH certificate"),
            Self::SignerDoesNotAllRequiredSSHKeys => write!(f, "Signer did not have both user and host keys defined")
        }
    }
}

impl std::fmt::Display for SigningMechanism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::new();
        for signer in self.authorities.iter() {
            output.push_str(&format!("Authority: {}\n", signer.0));

            if let Some(fp) = signer
                .1
                .get_signer_public_key(CertType::User)
                .map(|x| x.fingerprint().hash)
            {
                output.push_str(&format!("\tUser CA Fingerprint (SHA256): {fp}\n"));
            }

            if let Some(fp) = signer
                .1
                .get_signer_public_key(CertType::Host)
                .map(|x| x.fingerprint().hash)
            {
                output.push_str(&format!("\tHost CA Fingerprint (SHA256): {fp}\n"));
            }

            if let Some(attested_x509_authority) =
                signer.1.get_attested_x509_certificate_authority()
            {
                output.push_str(&format!(
                    "\tAttested X509 Certificate Authority:\n{}\n",
                    attested_x509_authority.serialize_pem().unwrap()
                ));
            }

            if let Some(client_certificate_authority) = signer.1.get_client_certificate_authority()
            {
                output.push_str(&format!(
                    "\tClient Certificate Authority:\n{}\n",
                    client_certificate_authority.serialize_pem().unwrap()
                ));
            }
        }
        write!(f, "{}", output)
    }
}

impl SigningMechanism {
    /// Takes in a certificate and handles the getting a signature from the
    /// configured SigningMechanism.
    pub async fn sign(
        &self,
        authority: &str,
        cert: Certificate,
    ) -> Result<Certificate, SigningError> {
        if let Some(authority) = self.authorities.get(authority) {
            authority.sign(cert).await
        } else {
            Err(SigningError::UnknownAuthority(authority.to_string()))
        }
    }

    /// Return an sshcerts::PublicKey type for the signing key asked for,
    /// either User or Host
    pub fn get_signer_public_key(
        &self,
        authority: &str,
        cert_type: CertType,
    ) -> Result<PublicKey, SigningError> {
        let authority = if let Some(authority) = self.authorities.get(authority) {
            authority
        } else {
            return Err(SigningError::UnknownAuthority(authority.to_string()));
        };

        authority
            .get_signer_public_key(cert_type)
            .ok_or(SigningError::SignerDoesNotHaveSSHKeys)
    }

    /// Return the X509 certificate authority certificate to sign attested X509 requests
    pub fn get_attested_x509_certificate_authority(
        &self,
        authority: &str,
    ) -> Result<Option<&rcgen::Certificate>, SigningError> {
        if let Some(authority) = self.authorities.get(authority) {
            Ok(authority.get_attested_x509_certificate_authority())
        } else {
            Err(SigningError::UnknownAuthority(authority.to_string()))
        }
    }

    /// Return the X509 certificate authority certificate to sign new client certificates
    pub fn get_client_certificate_authority(
        &self,
        authority: &str,
    ) -> Result<Option<&rcgen::Certificate>, SigningError> {
        if let Some(authority) = self.authorities.get(authority) {
            Ok(authority.get_client_certificate_authority())
        } else {
            Err(SigningError::UnknownAuthority(authority.to_string()))
        }
    }

    pub fn get_authorities(&self) -> Vec<String> {
        self.authorities.keys().map(|x| x.to_owned()).collect()
    }
}

impl SigningConfiguration {
    pub async fn convert_to_signing_mechanism(self) -> Result<SigningMechanism, SigningError> {
        let authorities = self.authority_configurations;
        // All of the configured signing authorities
        let mut converted_authorities = HashMap::new();

        // The public key fingerprints we've seen while setting up.
        // Used so we can check that someone isn't accidentally using
        // the same key across authorities which is almost certainly
        // a mistake
        let mut public_keys: HashMap<String, String> = HashMap::new();
        for authority in authorities {
            // Convert the SignerType in to a Signer trait object
            let signer = authority.1.into_signer().await?;

            // If this has SSH identities configured, make sure they
            // don't conflict
            let user_hash = signer
                .get_signer_public_key(CertType::User)
                .map(|x| x.fingerprint().hash);
            let host_hash = signer
                .get_signer_public_key(CertType::Host)
                .map(|x| x.fingerprint().hash);

            // If the user is using the same key for user and host
            // certificate authorities, error and tell them not to do
            // this
            if let (Some(user_hash), Some(host_hash)) = (user_hash.as_ref(), host_hash.as_ref()) {
                if user_hash == host_hash {
                    return Err(SigningError::IdenticalUserAndHostKey(authority.0));
                }
            }

            // If the user key is configured
            if let Some(user_hash) = user_hash {
                // If this fingerprint is already known in other
                // authorities
                if let Some(existing) = public_keys.get(&user_hash) {
                    return Err(SigningError::DuplicatedKey(
                        authority.0,
                        existing.to_owned(),
                    ));
                }

                // Remember we've seen this one
                public_keys.insert(user_hash, authority.0.to_owned());
            }

            // If the host key is configured
            if let Some(host_hash) = host_hash {
                // If this fingerprint is already known in other
                // authorities
                if let Some(existing) = public_keys.get(&host_hash) {
                    return Err(SigningError::DuplicatedKey(
                        authority.0,
                        existing.to_owned(),
                    ));
                }

                // Remember we've seen this one
                public_keys.insert(host_hash, authority.0.to_owned());
            }

            converted_authorities.insert(authority.0, signer);
        }

        Ok(SigningMechanism {
            default_authority: self.default_authority,
            authorities: converted_authorities,
        })
    }
}
