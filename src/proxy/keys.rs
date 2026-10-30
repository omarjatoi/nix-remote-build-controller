//! SSH key material loaded from the key secret or from files.

use std::path::Path;

use anyhow::{Context as _, anyhow};
use k8s_openapi::api::core::v1::Secret;
use russh::keys::PrivateKey;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};

use crate::secret;

/// Parses the private key stored under `key`. `Ok(None)` when absent.
pub fn private_key_from_secret(ssh_keys: &Secret, key: &str) -> anyhow::Result<Option<PrivateKey>> {
    let Some(data) = secret::entry(ssh_keys, key) else {
        return Ok(None);
    };
    let name = ssh_keys.metadata.name.as_deref().unwrap_or_default();
    parse_private_key(data)
        .with_context(|| format!("parse {key:?} from secret {name}"))
        .map(Some)
}

/// Parses an unencrypted private key (OpenSSH, PKCS#8 or PKCS#1 PEM).
pub fn parse_private_key(data: &[u8]) -> anyhow::Result<PrivateKey> {
    let text = std::str::from_utf8(data).context("private key is not valid UTF-8")?;
    Ok(russh::keys::decode_secret_key(text, None)?)
}

/// Reads a private key from a file.
pub fn load_private_key(path: impl AsRef<Path>) -> anyhow::Result<PrivateKey> {
    let path = path.as_ref();
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    parse_private_key(&data)
}

/// Creates an ephemeral ed25519 key. Used as the proxy host key only when no
/// persistent one is configured; clients then see a changed host key on every
/// restart.
pub fn generate_host_key() -> anyhow::Result<PrivateKey> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("gather randomness: {e}"))?;
    let keypair = Ed25519Keypair::from_seed(&seed);
    PrivateKey::new(KeypairData::Ed25519(keypair), "").context("build host key")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::ByteString;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use russh::keys::ssh_key::LineEnding;

    use super::*;

    #[test]
    fn reads_keys_from_secret() {
        let key = generate_host_key().unwrap();
        let pem = key.to_openssh(LineEnding::LF).unwrap();
        let secret = Secret {
            metadata: ObjectMeta {
                name: Some("keys".into()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([
                (
                    secret::PRIVATE_KEY.to_string(),
                    ByteString(pem.as_bytes().to_vec()),
                ),
                (secret::HOST_KEY.to_string(), ByteString(Vec::new())),
                (
                    secret::BUILDER_HOST_KEY.to_string(),
                    ByteString(b"garbage".to_vec()),
                ),
            ])),
            ..Default::default()
        };
        let parsed = private_key_from_secret(&secret, secret::PRIVATE_KEY)
            .unwrap()
            .unwrap();
        assert_eq!(parsed.public_key().key_data(), key.public_key().key_data());
        assert!(
            private_key_from_secret(&secret, secret::HOST_KEY)
                .unwrap()
                .is_none()
        );
        assert!(
            private_key_from_secret(&secret, "missing")
                .unwrap()
                .is_none()
        );
        let err = private_key_from_secret(&secret, secret::BUILDER_HOST_KEY).unwrap_err();
        assert!(format!("{err:#}").contains("builder-host-key"));
    }

    #[test]
    fn generated_keys_differ() {
        let a = generate_host_key().unwrap();
        let b = generate_host_key().unwrap();
        assert_ne!(a.public_key().key_data(), b.public_key().key_data());
    }
}
